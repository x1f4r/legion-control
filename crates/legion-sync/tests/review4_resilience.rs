//! Fourth independent review regression tests: whole version-vector comparisons (no key
//! pruning), 64-incarnation ceiling truth, evidence-backed per-claimant rollback recovery,
//! honest sync status, and file/dir swap resilience on real disk and redb.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, dead_code)]
mod safety_common;
use legion_sync::{IndexStore, MemberState, ProblemKind, RealFs, RedbIndexStore, SyncEngine};
use safety_common::*;
use std::fs;
use std::path::Path;
type E = SyncEngine<RealFs, RedbIndexStore>;

fn copy_tree(src: &Path, dst: &Path) {
    fs::create_dir_all(dst).unwrap();
    for entry in fs::read_dir(src).unwrap() {
        let entry = entry.unwrap();
        let ty = entry.file_type().unwrap();
        let from = entry.path();
        let to = dst.join(entry.file_name());
        if ty.is_dir() {
            copy_tree(&from, &to);
        } else if ty.is_file() {
            fs::copy(&from, &to).unwrap();
        }
    }
}
fn pull(a: &mut E, b: &E, now: i64) -> Option<String> {
    let caller = a.meta().unwrap().local_member;
    match a.pull_from_peer(&EnginePeer::new(b, &caller), ts(now)) {
        Ok(_) => None,
        Err(e) => Some(format!("{caller}<-{}: {e}", b.meta().unwrap().local_member)),
    }
}
fn sync_all(engines: &mut [&mut E], t0: i64) {
    let n = engines.len();
    for round in 0..3 {
        for i in 0..n {
            for j in 0..n {
                if i == j {
                    continue;
                }
                let (a, b) = if i < j {
                    let (l, r) = engines.split_at_mut(j);
                    (&mut l[i], &r[0])
                } else {
                    let (l, r) = engines.split_at_mut(i);
                    (&mut r[0], &l[j])
                };
                let caller = a.meta().unwrap().local_member;
                if let Err(e) = a.pull_from_peer(&EnginePeer::new(&**b, &caller), ts(t0 + round)) {
                    eprintln!("pull err {caller}: {e}");
                }
            }
        }
    }
}

/// Deleter's incarnation retires (pi rebuilds its index), the deleter key gets pruned, then a
/// new member arrives with an old copy of the deleted file.
#[test]
fn p1_pruned_deleter_key_lets_old_copy_resurrect() {
    p1_case(true)
}
#[test]
fn p1_control_without_rebuild() {
    p1_case(false)
}
fn p1_case(rebuild: bool) {
    let tmp = scratch();
    let t = tmp.path();
    let all = ["tower", "pi"];
    fs::create_dir_all(t.join("tower")).unwrap();
    fs::create_dir_all(t.join("pi")).unwrap();
    fs::write(t.join("tower/keep.txt"), b"keep").unwrap();
    fs::write(t.join("pi/tax.pdf"), b"deleted on purpose").unwrap();
    fs::write(t.join("pi/shared.txt"), b"v1").unwrap();
    let mut tower = open_real(&t.join("tower"), &t.join("tower.redb"), "tower", &all);
    let mut pi = open_real(&t.join("pi"), &t.join("pi.redb"), "pi", &all);
    tower.scan(ts(0)).unwrap();
    pi.scan(ts(0)).unwrap();
    sync_all(&mut [&mut tower, &mut pi], 1);
    assert!(t.join("tower/tax.pdf").exists());
    copy_tree(&t.join("tower"), &t.join("usb"));
    fs::remove_dir_all(t.join("usb/.legion-sync")).unwrap();

    fs::write(t.join("tower/shared.txt"), b"v2 by tower").unwrap();
    tower.scan(ts(5)).unwrap();
    fs::remove_file(t.join("pi/tax.pdf")).unwrap();
    pi.scan(ts(10)).unwrap();
    pi.flush_settled_deletions(ts(100)).unwrap();
    sync_all(&mut [&mut tower, &mut pi], 101);
    sync_all(&mut [&mut tower, &mut pi], 110);
    for e in [&mut tower, &mut pi] {
        let _ = e.gc_tombstones().unwrap();
    }
    assert!(!t.join("tower/tax.pdf").exists());
    eprintln!(
        "tower del vv: {:?}",
        tower
            .store()
            .get_deleted_history_version("tax.pdf")
            .unwrap()
    );
    eprintln!(
        "tower record: {:?}",
        tower
            .store()
            .get_record("tax.pdf")
            .unwrap()
            .map(|r| r.version)
    );

    // pi reinstalls: index lost, rebuilt -> new incarnation.
    if rebuild {
        drop(pi);
        fs::remove_file(t.join("pi.redb")).unwrap();
        pi = open_real(&t.join("pi"), &t.join("pi.redb"), "pi", &all);
        pi.rebuild_from_disk(ts(200)).unwrap();
    }
    for k in 0..6 {
        sync_all(&mut [&mut tower, &mut pi], 300 + 10 * k);
        for e in [&mut tower, &mut pi] {
            let _ = e.gc_tombstones();
        }
    }
    // Remembered deletion version vector must retain the deleter's key whole.
    let del_vv = tower
        .store()
        .get_deleted_history_version("tax.pdf")
        .unwrap()
        .expect("tower must retain deleted-history version for tax.pdf");
    assert!(
        !del_vv.is_empty(),
        "remembered deletion version vector must never lose its keys"
    );

    // New member with the old copy.
    let all3 = ["tower", "pi", "laptop"];
    tower.add_member("laptop").unwrap();
    pi.add_member("laptop").unwrap();
    fs::rename(t.join("usb"), t.join("laptop")).unwrap();
    let mut laptop = open_real(&t.join("laptop"), &t.join("laptop.redb"), "laptop", &all3);
    laptop.scan(ts(500)).unwrap();
    sync_all(&mut [&mut tower, &mut pi, &mut laptop], 600);
    for n in ["tower", "pi", "laptop"] {
        assert!(
            !t.join(format!("{n}/tax.pdf")).exists(),
            "deleted file came back on {n}"
        );
    }
}

fn live_with(root: &Path, content: &[u8]) -> Vec<String> {
    where_is(root, content)
        .into_iter()
        .filter(|p| !p.starts_with(".legion-sync"))
        .collect()
}

/// laptop's last edit reaches only pi; laptop is removed; pi is removed while offline; tower and
/// nas sync; pi is re-added and syncs. Both `LAPTOP EDIT` and `nas edit` must survive live on all
/// three members (never silently replaced into a bin).
fn p2_case(settle_window: bool, pi_pulls_first: bool) {
    let tmp = scratch();
    let t = tmp.path();
    let all = ["tower", "nas", "laptop", "pi"];
    for n in all {
        fs::create_dir_all(t.join(n)).unwrap();
    }
    fs::write(t.join("laptop/doc.txt"), b"v1").unwrap();
    let mut tower = open_real(&t.join("tower"), &t.join("tower.redb"), "tower", &all);
    let mut nas = open_real(&t.join("nas"), &t.join("nas.redb"), "nas", &all);
    let mut laptop = open_real(&t.join("laptop"), &t.join("laptop.redb"), "laptop", &all);
    let mut pi = open_real(&t.join("pi"), &t.join("pi.redb"), "pi", &all);
    laptop.scan(ts(0)).unwrap();
    sync_all(&mut [&mut tower, &mut nas, &mut laptop, &mut pi], 1);
    fs::write(t.join("tower/doc.txt"), b"v2 tower").unwrap();
    tower.scan(ts(5)).unwrap();
    sync_all(&mut [&mut tower, &mut nas, &mut laptop, &mut pi], 6);
    // laptop's edit reaches only pi.
    fs::write(t.join("laptop/doc.txt"), b"LAPTOP EDIT").unwrap();
    laptop.scan(ts(10)).unwrap();
    assert!(pull(&mut pi, &laptop, 11).is_none());
    assert_eq!(fs::read(t.join("pi/doc.txt")).unwrap(), b"LAPTOP EDIT");
    // nas edits concurrently.
    fs::write(t.join("nas/doc.txt"), b"nas edit").unwrap();
    nas.scan(ts(12)).unwrap();
    drop(laptop);
    // laptop removed everywhere (pi learns too); pi removed on tower and nas while offline.
    for e in [&mut tower, &mut nas, &mut pi] {
        e.remove_member("laptop").unwrap();
    }
    for e in [&mut tower, &mut nas] {
        e.remove_member("pi").unwrap();
    }
    if settle_window {
        for k in 0..4 {
            sync_all(&mut [&mut tower, &mut nas], 20 + 10 * k);
            for e in [&mut tower, &mut nas] {
                let _ = e.gc_tombstones();
            }
        }
    }
    for e in [&mut tower, &mut nas] {
        e.add_member("pi").unwrap();
    }
    if pi_pulls_first {
        let _ = pull(&mut pi, &tower, 100);
    }
    assert!(pull(&mut tower, &pi, 101).is_none());
    assert!(pull(&mut nas, &pi, 102).is_none());
    for k in 0..3 {
        assert!(pull(&mut tower, &pi, 103 + k * 4).is_none());
        assert!(pull(&mut nas, &pi, 104 + k * 4).is_none());
        assert!(pull(&mut pi, &nas, 105 + k * 4).is_none());
        assert!(pull(&mut pi, &tower, 106 + k * 4).is_none());
    }
    for k in 0..4 {
        sync_all(&mut [&mut tower, &mut nas, &mut pi], 110 + 10 * k);
    }
    for n in ["tower", "nas", "pi"] {
        assert!(
            !live_with(&t.join(n), b"LAPTOP EDIT").is_empty(),
            "settle_window={settle_window} pi_pulls_first={pi_pulls_first}: LAPTOP EDIT missing live on {n}"
        );
        assert!(
            !live_with(&t.join(n), b"nas edit").is_empty(),
            "settle_window={settle_window} pi_pulls_first={pi_pulls_first}: nas edit missing live on {n}"
        );
    }
}

#[test]
fn p2_removed_relay_member_returns() {
    for (pw, first) in [(false, false), (true, false), (true, true)] {
        p2_case(pw, first);
    }
}

struct NoPages<'a>(EnginePeer<'a, RealFs, RedbIndexStore>);
impl legion_sync::Peer for NoPages<'_> {
    fn member_id(&self) -> &str {
        self.0.member_id.as_str()
    }
    fn handshake(
        &self,
        c: &legion_sync::HandshakeHello,
    ) -> legion_sync::Result<legion_sync::PeerHandshake> {
        self.0.handshake(c)
    }
    fn records_since(
        &self,
        _i: u64,
        _s: u64,
        _l: usize,
        _c: Option<&std::sync::atomic::AtomicBool>,
    ) -> legion_sync::Result<legion_sync::RecordsPage> {
        Err(legion_sync::SyncError::NotFound {
            what: "network dropped".into(),
        })
    }
    fn fetch_file(
        &self,
        p: &str,
        _h: legion_xfer::Hash,
        _o: &legion_sync::FetchOptions,
    ) -> legion_sync::Result<Box<dyn std::io::Read + Send>> {
        Err(legion_sync::SyncError::NotFound { what: p.into() })
    }
}

/// Same as p2, but pi's first pull from tower after re-add loses its connection after the
/// handshake before seeing nas's concurrent version. Both edits must still survive live everywhere.
#[test]
fn p2b_relay_edit_with_dropped_page() {
    for settle_window in [false, true] {
        let tmp = scratch();
        let t = tmp.path();
        let all = ["tower", "nas", "laptop", "pi"];
        for n in all {
            fs::create_dir_all(t.join(n)).unwrap();
        }
        fs::write(t.join("laptop/doc.txt"), b"v1").unwrap();
        let mut tower = open_real(&t.join("tower"), &t.join("tower.redb"), "tower", &all);
        let mut nas = open_real(&t.join("nas"), &t.join("nas.redb"), "nas", &all);
        let mut laptop = open_real(&t.join("laptop"), &t.join("laptop.redb"), "laptop", &all);
        let mut pi = open_real(&t.join("pi"), &t.join("pi.redb"), "pi", &all);
        laptop.scan(ts(0)).unwrap();
        sync_all(&mut [&mut tower, &mut nas, &mut laptop, &mut pi], 1);
        fs::write(t.join("tower/doc.txt"), b"v2 tower").unwrap();
        tower.scan(ts(5)).unwrap();
        sync_all(&mut [&mut tower, &mut nas, &mut laptop, &mut pi], 6);
        fs::write(t.join("laptop/doc.txt"), b"LAPTOP EDIT").unwrap();
        laptop.scan(ts(10)).unwrap();
        assert!(pull(&mut pi, &laptop, 11).is_none());
        fs::write(t.join("nas/doc.txt"), b"nas edit").unwrap();
        nas.scan(ts(12)).unwrap();
        drop(laptop);
        for e in [&mut tower, &mut nas, &mut pi] {
            e.remove_member("laptop").unwrap();
        }
        for e in [&mut tower, &mut nas] {
            e.remove_member("pi").unwrap();
        }
        if settle_window {
            for k in 0..4 {
                sync_all(&mut [&mut tower, &mut nas], 20 + 10 * k);
                for e in [&mut tower, &mut nas] {
                    let _ = e.gc_tombstones();
                }
            }
        }
        for e in [&mut tower, &mut nas] {
            e.add_member("pi").unwrap();
        }
        assert!(pull(&mut tower, &pi, 101).is_none());
        assert!(pull(&mut nas, &pi, 102).is_none());
        let r = pi.pull_from_peer(&NoPages(EnginePeer::new(&tower, "pi")), ts(103));
        assert!(r.is_err(), "NoPages pull must fail");
        assert!(pull(&mut pi, &nas, 104).is_none());
        for k in 0..4 {
            sync_all(&mut [&mut tower, &mut nas, &mut pi], 110 + 10 * k);
        }
        for n in ["tower", "nas", "pi"] {
            assert!(
                !live_with(&t.join(n), b"LAPTOP EDIT").is_empty(),
                "settle_window={settle_window}: LAPTOP EDIT missing live on {n}"
            );
            assert!(
                !live_with(&t.join(n), b"nas edit").is_empty(),
                "settle_window={settle_window}: nas edit missing live on {n}"
            );
        }
        assert!(
            !pi.list_conflicts().unwrap().is_empty(),
            "concurrent edits must produce a visible conflict copy"
        );
    }
}

/// Without VV pruning, 64 rebuilds+edits of one machine all sync cleanly (all keys preserved
/// whole), and on the 65th rebuild that file's record hits the 64-incarnation cap with a truthful
/// message while unrelated files keep syncing and nothing is lost.
#[test]
fn p3_sixty_four_incarnation_cap_truthful_and_isolated() {
    let tmp = scratch();
    let t = tmp.path();
    let all = ["tower", "pi"];
    fs::create_dir_all(t.join("tower")).unwrap();
    fs::write(t.join("tower/f.txt"), b"init").unwrap();
    let mut tower = open_real(&t.join("tower"), &t.join("tower.redb"), "tower", &all);
    let mut pi = open_real(&t.join("pi"), &t.join("pi.redb"), "pi", &all);
    tower.scan(ts(0)).unwrap();
    sync_all(&mut [&mut tower, &mut pi], 1);
    // Authored post-join file so unchanged files do not accumulate rebuild incarnation keys.
    fs::write(t.join("tower/other.txt"), b"other init").unwrap();
    tower.scan(ts(2)).unwrap();
    sync_all(&mut [&mut tower, &mut pi], 3);

    // 64 rebuilds + edits on pi all succeed (keys 1..=64 for pi on f.txt).
    for n in 1..=64i64 {
        drop(pi);
        fs::remove_file(t.join("pi.redb")).unwrap();
        let mut p = open_real(&t.join("pi"), &t.join("pi.redb"), "pi", &all);
        p.rebuild_from_disk(ts(100 * n)).unwrap();
        pi = p;
        assert!(pull(&mut pi, &tower, 100 * n + 1).is_none());
        assert!(pull(&mut tower, &pi, 100 * n + 2).is_none());
        let body = format!("rebuild {n}");
        fs::write(t.join("pi/f.txt"), &body).unwrap();
        pi.scan(ts(100 * n + 5)).unwrap();
        assert!(pull(&mut tower, &pi, 100 * n + 6).is_none());
        assert!(pull(&mut pi, &tower, 100 * n + 7).is_none());
        assert_eq!(fs::read_to_string(t.join("tower/f.txt")).unwrap(), body);
    }
    let pi_keys = tower
        .store()
        .get_record("f.txt")
        .unwrap()
        .unwrap()
        .version
        .iter()
        .filter(|(k, _)| k.member == "pi")
        .count();
    assert_eq!(
        pi_keys, 64,
        "all 64 incarnation keys must be preserved whole"
    );

    // 65th rebuild + edit hits the 64-incarnation cap on f.txt, while other.txt is unaffected.
    drop(pi);
    fs::remove_file(t.join("pi.redb")).unwrap();
    pi = open_real(&t.join("pi"), &t.join("pi.redb"), "pi", &all);
    pi.rebuild_from_disk(ts(10_000)).unwrap();
    assert!(pull(&mut pi, &tower, 10_001).is_none());
    assert!(pull(&mut tower, &pi, 10_002).is_none());
    fs::write(t.join("pi/f.txt"), b"rebuild 65").unwrap();
    fs::write(t.join("pi/other.txt"), b"other still syncs").unwrap();
    pi.scan(ts(10_005)).unwrap();
    let rep = tower
        .pull_from_peer(&EnginePeer::new(&pi, "tower"), ts(10_006))
        .unwrap();
    assert_eq!(
        rep.invalid_skipped, 1,
        "65th incarnation record must be refused"
    );
    // Unrelated file synced cleanly and previous f.txt on tower was not lost.
    assert_eq!(
        fs::read(t.join("tower/other.txt")).unwrap(),
        b"other still syncs"
    );
    assert_eq!(fs::read(t.join("tower/f.txt")).unwrap(), b"rebuild 64");
    assert_eq!(fs::read(t.join("pi/f.txt")).unwrap(), b"rebuild 65");

    // Tower reports a truthful message (does NOT falsely promise remove + re-add clears it).
    match tower.status().unwrap().state {
        MemberState::Problem { problems, .. } => {
            assert!(
                problems.iter().any(|p| {
                    p.kind == ProblemKind::InvalidPeerRecord
                        && p.sentence.contains("keep that machine's index")
                        && p.sentence.contains("set up afresh")
                        && !p.sentence.contains("add it again")
                }),
                "expected truthful 64-incarnation message, got {problems:?}"
            );
        }
        other => panic!("expected Problem on tower after 65th incarnation, got {other:?}"),
    }
    // pi learns of the refusal on its next pull.
    assert!(pull(&mut pi, &tower, 10_007).is_none());
    match pi.status().unwrap().state {
        MemberState::Problem { problems, .. } => {
            assert!(
                problems
                    .iter()
                    .any(|p| p.kind == ProblemKind::PeerRefusedRecords),
                "expected PeerRefusedRecords on pi, got {problems:?}"
            );
        }
        other => panic!("expected PeerRefusedRecords on pi, got {other:?}"),
    }
}

fn rehash_counts(e: &mut E, t0: i64) -> Vec<usize> {
    (0..3).map(|i| e.scan(ts(t0 + i)).unwrap().hashed).collect()
}

/// After each way a record can be written, the second and later scans must not re-hash.
#[test]
fn p4_rehash_after_each_record_writer() {
    let tmp = scratch();
    let t = tmp.path();
    let all = ["tower", "pi"];
    fs::create_dir_all(t.join("tower")).unwrap();
    fs::create_dir_all(t.join("pi")).unwrap();
    for i in 0..50 {
        fs::write(t.join(format!("tower/f{i}.txt")), format!("body {i}")).unwrap();
    }
    // same content on pi already (join merge)
    for i in 0..20 {
        fs::write(t.join(format!("pi/f{i}.txt")), format!("body {i}")).unwrap();
    }
    let mut tower = open_real(&t.join("tower"), &t.join("tower.redb"), "tower", &all);
    let mut pi = open_real(&t.join("pi"), &t.join("pi.redb"), "pi", &all);
    tower.scan(ts(0)).unwrap();
    pi.scan(ts(0)).unwrap();
    sync_all(&mut [&mut tower, &mut pi], 1);
    assert_eq!(rehash_counts(&mut tower, 10), vec![0, 0, 0]);
    assert_eq!(rehash_counts(&mut pi, 10), vec![0, 0, 0]);
    // metadata-only: tower touches mtime without changing content
    for i in 0..10 {
        let f = fs::OpenOptions::new()
            .write(true)
            .open(t.join(format!("tower/f{i}.txt")))
            .unwrap();
        f.set_modified(std::time::SystemTime::now() - std::time::Duration::from_secs(5000))
            .unwrap();
    }
    tower.scan(ts(20)).unwrap();
    sync_all(&mut [&mut tower, &mut pi], 21);
    assert_eq!(rehash_counts(&mut tower, 30), vec![0, 0, 0]);
    assert_eq!(rehash_counts(&mut pi, 30), vec![0, 0, 0]);
    // conflict
    fs::write(t.join("tower/f30.txt"), b"tower side").unwrap();
    fs::write(t.join("pi/f30.txt"), b"pi side").unwrap();
    tower.scan(ts(40)).unwrap();
    pi.scan(ts(40)).unwrap();
    sync_all(&mut [&mut tower, &mut pi], 41);
    assert_eq!(rehash_counts(&mut tower, 50), vec![0, 0, 0]);
    assert_eq!(rehash_counts(&mut pi, 50), vec![0, 0, 0]);
    // deletion + bin
    fs::remove_file(t.join("tower/f40.txt")).unwrap();
    tower.scan(ts(60)).unwrap();
    tower.flush_settled_deletions(ts(200)).unwrap();
    sync_all(&mut [&mut tower, &mut pi], 201);
    assert_eq!(pi.list_bin().unwrap().len(), 1);
    assert_eq!(rehash_counts(&mut tower, 210), vec![0, 0, 0]);
    assert_eq!(rehash_counts(&mut pi, 210), vec![0, 0, 0]);
    // rebuild
    drop(pi);
    fs::remove_file(t.join("pi.redb")).unwrap();
    pi = open_real(&t.join("pi"), &t.join("pi.redb"), "pi", &all);
    pi.rebuild_from_disk(ts(300)).unwrap();
    sync_all(&mut [&mut tower, &mut pi], 301);
    assert_eq!(rehash_counts(&mut tower, 310), vec![0, 0, 0]);
    assert_eq!(rehash_counts(&mut pi, 310), vec![0, 0, 0]);
}

/// A directory deleted on pi while tower's copy holds an entry tower's scan does not index.
#[cfg(unix)]
fn p5_case(kind: &str) {
    use std::os::unix::fs::PermissionsExt;
    let tmp = scratch();
    let t = tmp.path();
    let all = ["tower", "pi"];
    fs::create_dir_all(t.join("tower/d")).unwrap();
    fs::write(t.join("tower/d/x.txt"), b"x").unwrap();
    fs::write(t.join("tower/other.txt"), b"o").unwrap();
    let mut tower = open_real(&t.join("tower"), &t.join("tower.redb"), "tower", &all);
    let mut pi = open_real(&t.join("pi"), &t.join("pi.redb"), "pi", &all);
    tower.scan(ts(0)).unwrap();
    sync_all(&mut [&mut tower, &mut pi], 1);
    let special = t.join("tower/d/special");
    match kind {
        "fifo" => {
            let st = std::process::Command::new("mkfifo")
                .arg(&special)
                .status()
                .unwrap();
            assert!(st.success());
        }
        "unreadable" => {
            fs::write(&special, b"secret local").unwrap();
            fs::set_permissions(&special, fs::Permissions::from_mode(0o000)).unwrap();
        }
        "unreadable_dir" => {
            fs::create_dir(&special).unwrap();
            fs::write(special.join("inner.txt"), b"inner").unwrap();
            fs::set_permissions(&special, fs::Permissions::from_mode(0o000)).unwrap();
        }
        "backslash" => {
            fs::write(t.join("tower/d/a\\b.txt"), b"bs").unwrap();
        }
        "abs_symlink" => {
            std::os::unix::fs::symlink("/etc/hostname", &special).unwrap();
        }
        _ => unreachable!(),
    }
    let _ = tower.scan(ts(5)).unwrap();
    fs::remove_dir_all(t.join("pi/d")).unwrap();
    pi.scan(ts(10)).unwrap();
    pi.flush_settled_deletions(ts(100)).unwrap();
    fs::write(t.join("pi/after.txt"), b"after").unwrap();
    pi.scan(ts(101)).unwrap();
    for k in 0..4 {
        tower
            .pull_from_peer(&EnginePeer::new(&pi, "tower"), ts(110 + k))
            .unwrap();
    }
    assert!(
        t.join("tower/after.txt").exists(),
        "[{kind}] later file after.txt must not be blocked by kept directory"
    );
    assert!(
        t.join("tower/d").exists(),
        "[{kind}] directory holding unindexed entry must be kept"
    );
    assert!(
        !t.join("tower/d/x.txt").exists(),
        "[{kind}] synced file x.txt inside d must be deleted to bin"
    );
    let kept_path = if kind == "backslash" {
        t.join("tower/d/a\\b.txt")
    } else {
        special.clone()
    };
    assert!(
        fs::symlink_metadata(&kept_path).is_ok(),
        "[{kind}] unindexed entry must remain untouched on disk"
    );
    let _ = fs::set_permissions(&special, fs::Permissions::from_mode(0o755));
}

#[cfg(unix)]
#[test]
fn p5_deleted_dir_with_unindexed_entries() {
    for k in [
        "fifo",
        "unreadable",
        "unreadable_dir",
        "backslash",
        "abs_symlink",
    ] {
        p5_case(k);
    }
}

/// pi is added on nas and laptop but tower has not heard of it yet. laptop's last edit reaches
/// only pi; laptop is then removed. Without VV pruning, tower and nas retain laptop's key whole so
/// when pi comes back both `LAPTOP EDIT` and `nas edit` survive live on all three members.
#[test]
fn p6_unknown_member_and_adoption_skip_phase_two() {
    for settle_window in [false, true] {
        let tmp = scratch();
        let t = tmp.path();
        let three = ["tower", "nas", "laptop"];
        let four = ["tower", "nas", "laptop", "pi"];
        for n in four {
            fs::create_dir_all(t.join(n)).unwrap();
        }
        fs::write(t.join("tower/doc.txt"), b"v1").unwrap();
        fs::write(t.join("laptop/other.txt"), b"o1").unwrap();
        let mut tower = open_real(&t.join("tower"), &t.join("tower.redb"), "tower", &three);
        let mut nas = open_real(&t.join("nas"), &t.join("nas.redb"), "nas", &three);
        let mut laptop = open_real(&t.join("laptop"), &t.join("laptop.redb"), "laptop", &three);
        tower.scan(ts(0)).unwrap();
        laptop.scan(ts(0)).unwrap();
        sync_all(&mut [&mut tower, &mut nas, &mut laptop], 1);
        fs::write(t.join("tower/other.txt"), b"o2 tower").unwrap();
        tower.scan(ts(4)).unwrap();
        sync_all(&mut [&mut tower, &mut nas, &mut laptop], 5);
        // pi joins on nas and laptop only.
        nas.add_member("pi").unwrap();
        laptop.add_member("pi").unwrap();
        let mut pi = open_real(&t.join("pi"), &t.join("pi.redb"), "pi", &four);
        sync_all(&mut [&mut nas, &mut laptop, &mut pi], 10);
        // laptop edits doc; only pi gets it.
        fs::write(t.join("laptop/doc.txt"), b"LAPTOP EDIT").unwrap();
        laptop.scan(ts(20)).unwrap();
        assert!(pull(&mut pi, &laptop, 21).is_none());
        // nas edits doc concurrently.
        fs::write(t.join("nas/doc.txt"), b"nas edit").unwrap();
        nas.scan(ts(22)).unwrap();
        drop(laptop);
        for e in [&mut tower, &mut nas, &mut pi] {
            e.remove_member("laptop").unwrap();
        }
        if settle_window {
            for k in 0..4 {
                sync_all(&mut [&mut tower, &mut nas], 30 + 10 * k);
                for e in [&mut tower, &mut nas] {
                    let _ = e.gc_tombstones();
                }
            }
        }
        tower.add_member("pi").unwrap();
        assert!(pull(&mut nas, &pi, 101).is_none());
        assert!(pull(&mut tower, &pi, 102).is_none());
        assert!(
            pi.pull_from_peer(&NoPages(EnginePeer::new(&tower, "pi")), ts(103))
                .is_err()
        );
        assert!(pull(&mut pi, &nas, 104).is_none());
        for k in 0..4 {
            sync_all(&mut [&mut tower, &mut nas, &mut pi], 110 + 10 * k);
        }
        for n in ["tower", "nas", "pi"] {
            assert!(
                !live_with(&t.join(n), b"LAPTOP EDIT").is_empty(),
                "settle_window={settle_window}: LAPTOP EDIT missing live on {n}"
            );
            assert!(
                !live_with(&t.join(n), b"nas edit").is_empty(),
                "settle_window={settle_window}: nas edit missing live on {n}"
            );
        }
    }
}

#[test]
fn p6b_same_with_a_deletion() {
    for settle_window in [false, true] {
        let tmp = scratch();
        let t = tmp.path();
        let three = ["tower", "nas", "laptop"];
        let four = ["tower", "nas", "laptop", "pi"];
        for n in four {
            fs::create_dir_all(t.join(n)).unwrap();
        }
        fs::write(t.join("tower/doc.txt"), b"v1").unwrap();
        fs::write(t.join("laptop/other.txt"), b"o1").unwrap();
        let mut tower = open_real(&t.join("tower"), &t.join("tower.redb"), "tower", &three);
        let mut nas = open_real(&t.join("nas"), &t.join("nas.redb"), "nas", &three);
        let mut laptop = open_real(&t.join("laptop"), &t.join("laptop.redb"), "laptop", &three);
        tower.scan(ts(0)).unwrap();
        laptop.scan(ts(0)).unwrap();
        sync_all(&mut [&mut tower, &mut nas, &mut laptop], 1);
        fs::write(t.join("tower/other.txt"), b"o2 tower").unwrap();
        tower.scan(ts(4)).unwrap();
        sync_all(&mut [&mut tower, &mut nas, &mut laptop], 5);
        // pi joins on nas and laptop only.
        nas.add_member("pi").unwrap();
        laptop.add_member("pi").unwrap();
        let mut pi = open_real(&t.join("pi"), &t.join("pi.redb"), "pi", &four);
        sync_all(&mut [&mut nas, &mut laptop, &mut pi], 10);
        // laptop edits doc; only pi gets it.
        fs::write(t.join("laptop/doc.txt"), b"LAPTOP EDIT").unwrap();
        laptop.scan(ts(20)).unwrap();
        assert!(pull(&mut pi, &laptop, 21).is_none());
        // nas deletes doc concurrently.
        fs::remove_file(t.join("nas/doc.txt")).unwrap();
        nas.scan(ts(22)).unwrap();
        let _ = nas.flush_settled_deletions(ts(29));
        drop(laptop);
        for e in [&mut tower, &mut nas, &mut pi] {
            e.remove_member("laptop").unwrap();
        }
        if settle_window {
            for k in 0..4 {
                sync_all(&mut [&mut tower, &mut nas], 30 + 10 * k);
                for e in [&mut tower, &mut nas] {
                    let _ = e.gc_tombstones();
                }
            }
        }
        tower.add_member("pi").unwrap();
        assert!(pull(&mut nas, &pi, 101).is_none());
        assert!(pull(&mut tower, &pi, 102).is_none());
        assert!(
            pi.pull_from_peer(&NoPages(EnginePeer::new(&tower, "pi")), ts(103))
                .is_err()
        );
        assert!(pull(&mut pi, &nas, 104).is_none());
        for k in 0..4 {
            sync_all(&mut [&mut tower, &mut nas, &mut pi], 110 + 10 * k);
        }
        // Delete vs concurrent edit: the edit MUST win everywhere!
        for n in ["tower", "nas", "pi"] {
            assert_eq!(
                fs::read(t.join(n).join("doc.txt")).unwrap(),
                b"LAPTOP EDIT",
                "settle_window={settle_window}: concurrent edit must win over deletion on {n}"
            );
        }
    }
}

struct Liar<'a>(EnginePeer<'a, RealFs, RedbIndexStore>);
impl legion_sync::Peer for Liar<'_> {
    fn member_id(&self) -> &str {
        self.0.member_id.as_str()
    }
    fn handshake(
        &self,
        c: &legion_sync::HandshakeHello,
    ) -> legion_sync::Result<legion_sync::PeerHandshake> {
        let mut h = self.0.handshake(c)?;
        h.caller_regressed = true;
        Ok(h)
    }
    fn records_since(
        &self,
        i: u64,
        s: u64,
        l: usize,
        c: Option<&std::sync::atomic::AtomicBool>,
    ) -> legion_sync::Result<legion_sync::RecordsPage> {
        self.0.records_since(i, s, l, c)
    }
    fn fetch_file(
        &self,
        p: &str,
        h: legion_xfer::Hash,
        o: &legion_sync::FetchOptions,
    ) -> legion_sync::Result<Box<dyn std::io::Read + Send>> {
        self.0.fetch_file(p, h, o)
    }
}

struct ForgedEvidenceLiar<'a>(EnginePeer<'a, RealFs, RedbIndexStore>);
impl legion_sync::Peer for ForgedEvidenceLiar<'_> {
    fn member_id(&self) -> &str {
        self.0.member_id.as_str()
    }
    fn handshake(
        &self,
        c: &legion_sync::HandshakeHello,
    ) -> legion_sync::Result<legion_sync::PeerHandshake> {
        let mut h = self.0.handshake(c)?;
        h.caller_pulled = Some(legion_sync::PeerAckView {
            incarnation: c.incarnation,
            seq: c.snapshot_seq.max(1),
            fingerprint: Some(legion_xfer::hash::hash_bytes(b"forged-fp")),
        });
        Ok(h)
    }
    fn records_since(
        &self,
        i: u64,
        s: u64,
        l: usize,
        c: Option<&std::sync::atomic::AtomicBool>,
    ) -> legion_sync::Result<legion_sync::RecordsPage> {
        self.0.records_since(i, s, l, c)
    }
    fn fetch_file(
        &self,
        p: &str,
        h: legion_xfer::Hash,
        o: &legion_sync::FetchOptions,
    ) -> legion_sync::Result<Box<dyn std::io::Read + Send>> {
        self.0.fetch_file(p, h, o)
    }
}

/// 1) Bare `caller_regressed = true` claims from `nas` rotate nothing.
/// 2) Even if `nas` forges checkable evidence on three consecutive days to exhaust `nas`'s own
///    per-claimant budget, `pi`'s real backup restore on day 4 recovers immediately via `tower`
///    (never blocked by `nas`), and both sides report honest backup-restore sentences (never
///    blaming two machines sharing a name).
#[test]
fn p7_liar_spends_weekly_budget_then_real_restore() {
    const DAY: i64 = 86_400;
    for forged in [false, true] {
        let tmp = scratch();
        let t = tmp.path();
        let all = ["tower", "pi", "nas"];
        fs::create_dir_all(t.join("tower")).unwrap();
        for i in 0..5 {
            fs::write(t.join(format!("tower/f{i}.txt")), format!("base {i}")).unwrap();
        }
        let mut tower = open_real(&t.join("tower"), &t.join("tower.redb"), "tower", &all);
        let mut pi = open_real(&t.join("pi"), &t.join("pi.redb"), "pi", &all);
        let mut nas = open_real(&t.join("nas"), &t.join("nas.redb"), "nas", &all);
        let base = 1_795_000_000i64;
        tower.scan(ts(base)).unwrap();
        sync_all(&mut [&mut tower, &mut pi, &mut nas], base + 1);

        let inc_initial = pi.meta().unwrap().incarnation;
        let mut liar_rotations = 0usize;
        let mut prev_inc = inc_initial;
        for d in 1..=3 {
            let now = base + d * DAY + 10;
            let r = if forged {
                pi.pull_from_peer(&ForgedEvidenceLiar(EnginePeer::new(&nas, "pi")), ts(now))
            } else {
                pi.pull_from_peer(&Liar(EnginePeer::new(&nas, "pi")), ts(now))
            };
            if !forged {
                assert!(r.is_err(), "bare claim without evidence must be rejected");
            }
            let cur_inc = pi.meta().unwrap().incarnation;
            if cur_inc != prev_inc {
                liar_rotations += 1;
                prev_inc = cur_inc;
            }
            sync_all(&mut [&mut tower, &mut pi, &mut nas], now + 100);
        }
        if !forged {
            assert_eq!(
                liar_rotations, 0,
                "bare caller_regressed claim must cause 0 rotations"
            );
        } else {
            assert_eq!(
                liar_rotations, 3,
                "forged-evidence liar spends its own 3/week per-peer budget"
            );
            // A 4th forged lie from nas inside 7 days is now ignored & reported as faulty!
            let r4 = pi.pull_from_peer(
                &ForgedEvidenceLiar(EnginePeer::new(&nas, "pi")),
                ts(base + 4 * DAY - 100),
            );
            assert!(r4.is_err(), "4th claim from nas must be rate-limited");
            assert_eq!(pi.meta().unwrap().incarnation, prev_inc);
        }

        // Back up pi's index, make pi work, sync to tower & nas, then restore the backup.
        drop(pi);
        fs::copy(t.join("pi.redb"), t.join("pi.redb.bak")).unwrap();
        let mut pi = open_real(&t.join("pi"), &t.join("pi.redb"), "pi", &all);
        for i in 0..3 {
            fs::write(t.join(format!("pi/new{i}.txt")), format!("pi work {i}")).unwrap();
        }
        pi.scan(ts(base + 3 * DAY + 500)).unwrap();
        sync_all(&mut [&mut tower, &mut pi, &mut nas], base + 3 * DAY + 600);
        assert!(t.join("tower/new0.txt").exists());
        drop(pi);
        fs::copy(t.join("pi.redb.bak"), t.join("pi.redb")).unwrap();
        let mut pi = open_real(&t.join("pi"), &t.join("pi.redb"), "pi", &all);
        fs::write(t.join("pi/after-restore.txt"), b"written after restore").unwrap();

        // Day 4: first let tower pull from pi (healthy peer observing restored pi) -> must say
        // backup restore, NEVER "Two different machines".
        let day4 = base + 4 * DAY;
        pi.scan(ts(day4)).unwrap();
        let tower_first = pull(&mut tower, &pi, day4 + 1);
        let tower_err =
            tower_first.expect("tower must pause pulling from pre-rotation restored pi");
        assert!(
            tower_err.contains("restored from a backup")
                && !tower_err.contains("Two different machines"),
            "tower sentence must explain backup restore, not duplicate name: {tower_err}"
        );

        // Now pi pulls from tower -> recovers immediately on day 4 despite nas's spent budget!
        assert!(
            pull(&mut pi, &tower, day4 + 2).is_none(),
            "forged={forged}: pi must recover immediately via tower on day 4"
        );
        assert!(
            pull(&mut tower, &pi, day4 + 3).is_none(),
            "forged={forged}: tower must pull from rejoined pi on day 4"
        );
        assert!(
            t.join("tower/after-restore.txt").exists(),
            "forged={forged}: after-restore.txt must reach tower on day 4"
        );
        assert_eq!(
            fs::read(t.join("tower/after-restore.txt")).unwrap(),
            b"written after restore"
        );

        // Verify the exact honest sentence on the restored machine (pi).
        let pi_probs = pi.store().list_problems().unwrap();
        assert!(
            pi_probs.iter().any(|(_, p)| {
                p.kind == ProblemKind::PeerForcedRotation
                    && p.sentence
                        == "This machine's copy of the folder is older than what tower has already received from it; it looks like it was restored from a backup. Legion is joining the folder again. Nothing is deleted."
            }),
            "expected exact backup-restore sentence on pi, got {pi_probs:?}"
        );
        // Tower's temporary waiting-for-rejoin problem cleared once pi rejoined!
        let tower_probs = tower.store().list_problems().unwrap();
        assert!(
            !tower_probs
                .iter()
                .any(|(_, p)| p.kind == ProblemKind::DuplicateMemberId),
            "tower's waiting problem should clear once pi rejoins, got {tower_probs:?}"
        );
    }
}

fn st(e: &E) -> String {
    let s = e.status().unwrap();
    format!(
        "local={:?} members={:?}",
        s.state,
        s.members
            .iter()
            .map(|m| format!("{}:{:?}", m.member, m.state))
            .collect::<Vec<_>>()
    )
    .chars()
    .take(400)
    .collect()
}

#[test]
fn p8_status_honesty() {
    let tmp = scratch();
    let t = tmp.path();
    let all = ["tower", "pi"];
    fs::create_dir_all(t.join("tower")).unwrap();
    fs::write(t.join("tower/a.txt"), b"a").unwrap();
    let mut tower = open_real(&t.join("tower"), &t.join("tower.redb"), "tower", &all);
    tower.set_deletion_brake(legion_sync::DeletionBrakeConfig {
        min_deleted_files: 1,
        min_deleted_percent: 25,
        settle_secs: 30,
        window_secs: 600,
    });

    // 0) Fresh folder before any scan or peer contact: pi has never been reached!
    let s0 = tower.status().unwrap();
    assert_eq!(s0.state, MemberState::NeverReached);
    assert!(
        s0.summary.contains("pi has never been reached"),
        "unexpected summary before first contact: {}",
        s0.summary
    );
    let pi_m0 = s0.members.iter().find(|m| m.member == "pi").unwrap();
    assert_eq!(pi_m0.state, MemberState::NeverReached);
    assert_eq!(pi_m0.last_synced_at, None);

    // 1) Never contacted pi, 1 unsent local file: MUST NOT say UpToDate!
    tower.scan(ts(0)).unwrap();
    let s1 = tower.status().unwrap();
    assert_ne!(s1.state, MemberState::UpToDate);
    assert_eq!(s1.up_to_date_as_of, None);
    let pi_m1 = s1.members.iter().find(|m| m.member == "pi").unwrap();
    assert_eq!(pi_m1.state, MemberState::NeverReached);
    assert_eq!(pi_m1.pending_outgoing, 1);
    assert_eq!(pi_m1.pending_outgoing_since, Some(ts(0)));
    assert_eq!(pi_m1.last_synced_at, None);
    assert!(
        s1.summary.contains("1 change waiting for pi") && s1.summary.contains("never reached"),
        "unexpected summary with unsent file and never-reached peer: {}",
        s1.summary
    );

    // 2) Full sync: BOTH members have acked everything -> UpToDate with as-of timestamp!
    let mut pi = open_real(&t.join("pi"), &t.join("pi.redb"), "pi", &all);
    sync_all(&mut [&mut tower, &mut pi], 1);
    let s2 = tower.status().unwrap();
    assert_eq!(s2.state, MemberState::UpToDate);
    assert!(s2.up_to_date_as_of.is_some());
    assert!(
        s2.summary.starts_with("Up to date as of "),
        "expected 'Up to date as of ...', got {}",
        s2.summary
    );
    let pi_m2 = s2.members.iter().find(|m| m.member == "pi").unwrap();
    assert_eq!(pi_m2.state, MemberState::UpToDate);
    assert_eq!(pi_m2.pending_outgoing, 0);
    assert_eq!(pi_m2.pending_incoming, 0);
    assert!(pi_m2.last_synced_at.is_some());

    // 3) New local change not yet pulled by pi: MUST NOT say UpToDate, reports exact count & age!
    fs::write(t.join("tower/b.txt"), b"b").unwrap();
    tower.scan(ts(10)).unwrap();
    let s3 = tower.status_at(ts(10 + 2 * 86_400)).unwrap();
    assert_eq!(
        s3.state,
        MemberState::WaitingForMember {
            members: vec!["pi".to_string()]
        }
    );
    let pi_m3 = s3.members.iter().find(|m| m.member == "pi").unwrap();
    assert_eq!(pi_m3.pending_outgoing, 1);
    assert_eq!(pi_m3.pending_outgoing_since, Some(ts(10)));
    assert!(
        s3.summary
            .contains("1 change waiting for pi, last reached 2 days ago"),
        "expected waiting summary with age, got {}",
        s3.summary
    );

    // 4) Local deletion in 30-second settle window ALSO counts as waiting outbound change!
    fs::remove_file(t.join("tower/a.txt")).unwrap();
    tower.scan(ts(11)).unwrap();
    assert_eq!(tower.meta().unwrap().pending_deletions.len(), 1);
    let s4 = tower.status_at(ts(11 + 2 * 86_400)).unwrap();
    assert_ne!(s4.state, MemberState::UpToDate);
    let pi_m4 = s4.members.iter().find(|m| m.member == "pi").unwrap();
    assert_eq!(pi_m4.pending_outgoing, 2);
    assert!(
        s4.summary
            .contains("2 changes waiting for pi, last reached 2 days ago"),
        "expected 2 changes waiting (edit + pending deletion), got {}",
        s4.summary
    );

    // 5a) Failed pull on pi records a durable Problem naming tower (NOT UpToDate).
    let r = pi.pull_from_peer(&NoPages(EnginePeer::new(&tower, "pi")), ts(12));
    assert!(r.is_err());
    let s5a = pi.status().unwrap();
    assert!(
        matches!(s5a.state, MemberState::Problem { .. }),
        "5a: pi must report Problem after failed pull, got {:?}",
        s5a.state
    );
    let tw_m5a = s5a.members.iter().find(|m| m.member == "tower").unwrap();
    assert!(matches!(tw_m5a.state, MemberState::Problem { .. }));
    assert!(tw_m5a.last_error.is_some());

    // 5b) Restarting pi MUST NOT forget that pulls from tower are failing!
    drop(pi);
    let mut pi = open_real(&t.join("pi"), &t.join("pi.redb"), "pi", &all);
    let s5b = pi.status().unwrap();
    assert!(
        matches!(s5b.state, MemberState::Problem { .. }),
        "5b: pi after restart must still report persisted pull failure, got {:?}",
        s5b.state
    );
    let tw_m5b = s5b.members.iter().find(|m| m.member == "tower").unwrap();
    assert!(matches!(tw_m5b.state, MemberState::Problem { .. }));
    assert!(tw_m5b.last_error.is_some());

    // 6) Once the pull succeeds cleanly and deletions settle & sync, the persisted error clears
    //    and both members return to honest UpToDate!
    tower.flush_settled_deletions(ts(100)).unwrap();
    sync_all(&mut [&mut tower, &mut pi], 101);
    assert_eq!(pi.status().unwrap().state, MemberState::UpToDate);
    assert_eq!(tower.status().unwrap().state, MemberState::UpToDate);
    assert_eq!(
        pi.status()
            .unwrap()
            .members
            .iter()
            .find(|m| m.member == "tower")
            .unwrap()
            .last_error,
        None
    );
}

/// tower on a case-sensitive btrfs folder, pi on a real case-folding, normalising ext4 folder.
#[test]
#[ignore]
fn p9_real_casefold_volume() {
    let cf = std::path::PathBuf::from(std::env::var("CF_ROOT").unwrap());
    let tmp = scratch();
    let t = tmp.path();
    let all = ["tower", "pi"];
    let tw = t.join("tower");
    let pw = cf.join(format!("pi-{}", std::process::id()));
    fs::create_dir_all(&tw).unwrap();
    fs::create_dir_all(&pw).unwrap();
    fs::write(tw.join("Readme.txt"), b"UPPER readme").unwrap();
    fs::write(tw.join("README.txt"), b"lower README").unwrap();
    fs::write(tw.join("caf\u{e9}.txt"), b"NFC cafe").unwrap();
    fs::write(tw.join("cafe\u{301}.txt"), b"NFD cafe").unwrap();
    fs::write(tw.join("photo.JPG"), b"photo bytes").unwrap();
    fs::create_dir_all(tw.join("Dir")).unwrap();
    fs::write(tw.join("Dir/a.txt"), b"in Dir").unwrap();
    fs::create_dir_all(tw.join("dir")).unwrap();
    fs::write(tw.join("dir/a.txt"), b"in dir").unwrap();
    let mut tower = open_real(&tw, &t.join("tower.redb"), "tower", &all);
    let mut pi = open_real(&pw, &t.join("pi.redb"), "pi", &all);
    eprintln!("pi caps {:?}", legion_sync::SyncFs::capabilities(pi.fs()));
    tower.scan(ts(0)).unwrap();
    for k in 0..3 {
        let a = pull(&mut pi, &tower, 1 + 3 * k);
        let b = pull(&mut tower, &pi, 2 + 3 * k);
        if a.is_some() || b.is_some() {
            eprintln!("errs {a:?} {b:?}");
        }
    }
    let dump = |root: &Path| {
        let mut v: Vec<String> = all_bytes(root)
            .into_iter()
            .filter(|(p, _)| {
                !p.starts_with(".legion-sync/tmp")
                    && !p.ends_with(".json")
                    && !p.contains("marker")
                    && !p.contains("deletions.log")
            })
            .map(|(p, b)| format!("{p}={}", String::from_utf8_lossy(&b)))
            .collect();
        v.sort();
        v
    };
    eprintln!(
        "after first sync\n tower {:?}\n pi    {:?}\n pi status {}",
        dump(&tw),
        dump(&pw),
        st(&pi)
    );
    // case-only rename on tower, and pi edits its copy of the readme
    fs::rename(tw.join("photo.JPG"), tw.join("photo.jpg")).unwrap();
    tower.scan(ts(20)).unwrap();
    let readme_on_pi = fs::read_dir(&pw)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
        .filter(|n| n.to_lowercase() == "readme.txt")
        .collect::<Vec<_>>();
    eprintln!("readme names on pi: {readme_on_pi:?}");
    fs::write(pw.join(&readme_on_pi[0]), b"PI EDITED readme").unwrap();
    pi.scan(ts(21)).unwrap();
    for k in 0..3 {
        let a = pull(&mut pi, &tower, 30 + 3 * k);
        let b = pull(&mut tower, &pi, 31 + 3 * k);
        if a.is_some() || b.is_some() {
            eprintln!("errs {a:?} {b:?}");
        }
    }
    eprintln!(
        "after rename+edit\n tower {:?}\n pi    {:?}\n tower status {}\n pi status {}",
        dump(&tw),
        dump(&pw),
        st(&tower),
        st(&pi)
    );
    for c in [
        &b"UPPER readme"[..],
        b"lower README",
        b"NFC cafe",
        b"NFD cafe",
        b"photo bytes",
        b"in Dir",
        b"in dir",
        b"PI EDITED readme",
    ] {
        let on_t = where_is(&tw, c);
        let on_p = where_is(&pw, c);
        assert!(
            !(on_t.is_empty() && on_p.is_empty()),
            "lost {:?}",
            String::from_utf8_lossy(c)
        );
    }
    let _ = fs::remove_dir_all(&pw);
}

/// File replaced by a directory on tower while pi edits the file, and a directory replaced by a
/// file on tower while pi adds a file inside it.
#[test]
fn p10_file_dir_swaps_against_concurrent_edits() {
    let tmp = scratch();
    let t = tmp.path();
    let all = ["tower", "pi"];
    fs::create_dir_all(t.join("tower/d")).unwrap();
    fs::write(t.join("tower/f"), b"file f v1").unwrap();
    fs::write(t.join("tower/d/x.txt"), b"x in d").unwrap();
    let mut tower = open_real(&t.join("tower"), &t.join("tower.redb"), "tower", &all);
    let mut pi = open_real(&t.join("pi"), &t.join("pi.redb"), "pi", &all);
    tower.scan(ts(0)).unwrap();
    sync_all(&mut [&mut tower, &mut pi], 1);
    // tower: f -> dir, d -> file
    fs::remove_file(t.join("tower/f")).unwrap();
    fs::create_dir(t.join("tower/f")).unwrap();
    fs::write(t.join("tower/f/inner.txt"), b"inner of f-dir").unwrap();
    fs::remove_dir_all(t.join("tower/d")).unwrap();
    fs::write(t.join("tower/d"), b"d is a file now").unwrap();
    tower.scan(ts(10)).unwrap();
    let _ = tower.flush_settled_deletions(ts(100));
    // pi: edit f, add file in d
    fs::write(t.join("pi/f"), b"PI EDIT of f").unwrap();
    fs::write(t.join("pi/d/new.txt"), b"PI NEW in d").unwrap();
    pi.scan(ts(11)).unwrap();
    for k in 0..4 {
        sync_all(&mut [&mut tower, &mut pi], 110 + 10 * k);
    }
    let mut lost = Vec::new();
    for c in [
        &b"inner of f-dir"[..],
        b"d is a file now",
        b"PI EDIT of f",
        b"PI NEW in d",
    ] {
        let tl = live_with(&t.join("tower"), c);
        let pl = live_with(&t.join("pi"), c);
        eprintln!(
            "{:?}: tower {:?} pi {:?}",
            String::from_utf8_lossy(c),
            tl,
            pl
        );
        if tl.is_empty() || pl.is_empty() {
            lost.push(String::from_utf8_lossy(c).to_string());
        }
    }
    eprintln!("tower {}\npi {}", st(&tower), st(&pi));
    assert!(lost.is_empty(), "not live on both: {lost:?}");
}
