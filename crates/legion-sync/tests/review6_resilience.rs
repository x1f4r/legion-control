//! Asserting regression tests for every probe in the sixth independent review and for Decisions
//! 1 through 4:
//! - Decision 1: one store in `.legion-sync/index.redb`, in-place schema migration, no
//!   `deletions.log`, durable-commit refusal.
//! - Decision 2: history is facts and is always kept; identity belongs to one install via a
//!   machine-local instance token (folder copy, reinstall with unsynced edits/deletions,
//!   same-machine backup restore with edits before first exchange).
//! - Decision 3: unversioned provenance on true join (`RecordProvenance::Unversioned`), winning
//!   nothing and growing zero keys, with edits and deletions before first exchange (2 & 3 members).
//! - Decision 4: content-based `base_hash` precedence before conflicts, plus the explicit accepted
//!   limit when every member knowing a re-save destroys its entire `.legion-sync`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod safety_common;

use std::fs;
use std::path::{Path, PathBuf};

use legion_sync::{
    FileInstanceTokenStore, IndexStore, MIN_MIGRATABLE_SCHEMA_VERSION, MemberState, ProblemKind,
    RealFs, RecordProvenance, RedbIndexStore, SYNC_INDEX_FILE, SyncEngine, SyncError,
};
use safety_common::*;

type E = SyncEngine<RealFs, RedbIndexStore>;

fn copy_tree(src: &Path, dst: &Path) {
    fs::create_dir_all(dst).unwrap();
    for entry in fs::read_dir(src).unwrap() {
        let entry = entry.unwrap();
        let ty = entry.file_type().unwrap();
        let to = dst.join(entry.file_name());
        if ty.is_dir() {
            copy_tree(&entry.path(), &to);
        } else if ty.is_file() {
            fs::copy(entry.path(), &to).unwrap();
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

fn sync_all(engines: &mut [&mut E], t0: i64) -> Vec<String> {
    let n = engines.len();
    let mut errs = Vec::new();
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
                    errs.push(format!("{caller}: {e}"));
                }
            }
        }
    }
    errs
}

fn open_member(base: &Path, name: &str, all: &[&str]) -> E {
    let root = base.join(name);
    fs::create_dir_all(&root).unwrap();
    open_folder(&root, &token_store_dir(&root, name), name, all)
}

fn rebuild_member(base: &Path, name: &str, all: &[&str], now: i64) -> E {
    let root = base.join(name);
    SyncEngine::rebuild_folder_from_disk(
        cfg(name, all),
        &root,
        token_store_for(&root, name),
        ts(now),
    )
    .unwrap()
    .0
}

fn conflict_copies(root: &Path) -> Vec<String> {
    let mut out = Vec::new();
    for e in fs::read_dir(root).unwrap() {
        let n = e.unwrap().file_name().to_string_lossy().to_string();
        if n.contains("(changed on") {
            out.push(n);
        }
    }
    out
}

fn provenance_tag(e: &E, path: &str) -> Option<String> {
    e.store().get_record(path).unwrap().map(|r| {
        let state = if r.deleted { "DEL " } else { "" };
        match &r.provenance {
            RecordProvenance::Unversioned => format!("{state}UNVERSIONED"),
            RecordProvenance::Versioned(vv) => format!("{state}{vv}"),
        }
    })
}

/// Review 6 Probe 1: `f.txt` is deleted everywhere, later re-created on tower with the same bytes
/// and synced to pi while laptop is off. Under Decisions 1 & 2, schema bumps migrate in place and
/// reinstalls rotate preserving history (and even if one member loses its `.legion-sync`, the
/// surviving member's history dominates laptop's older deletion). When laptop returns, the
/// re-created `f.txt` MUST stay live everywhere.
#[test]
fn r6_p1_recreated_same_content_survives_upgrade_reinstall_and_single_rebuild() {
    for mode in [
        "in-place-upgrade",
        "reinstall-token-loss",
        "pi-destroys-index",
    ] {
        let tmp = scratch();
        let t = tmp.path();
        let all = ["tower", "pi", "laptop"];
        let mut tower = open_member(t, "tower", &all);
        let mut pi = open_member(t, "pi", &all);
        let mut laptop = open_member(t, "laptop", &all);

        fs::write(t.join("tower/f.txt"), b"same bytes").unwrap();
        tower.scan(ts(0)).unwrap();
        assert!(sync_all(&mut [&mut tower, &mut pi, &mut laptop], 1).is_empty());

        fs::remove_file(t.join("tower/f.txt")).unwrap();
        tower.scan(ts(10)).unwrap();
        tower.flush_settled_deletions(ts(100)).unwrap();
        assert!(sync_all(&mut [&mut tower, &mut pi, &mut laptop], 101).is_empty());
        assert!(!t.join("laptop/f.txt").exists());
        // laptop goes offline.

        fs::write(t.join("tower/f.txt"), b"same bytes").unwrap();
        tower.scan(ts(200)).unwrap();
        assert!(sync_all(&mut [&mut tower, &mut pi], 201).is_empty());
        assert_eq!(fs::read(t.join("pi/f.txt")).unwrap(), b"same bytes");

        match mode {
            "in-place-upgrade" => {
                tower
                    .store_mut()
                    .write_schema_version_for_test(MIN_MIGRATABLE_SCHEMA_VERSION)
                    .unwrap();
                pi.store_mut()
                    .write_schema_version_for_test(MIN_MIGRATABLE_SCHEMA_VERSION)
                    .unwrap();
                drop(tower);
                drop(pi);
                tower = open_member(t, "tower", &all);
                pi = open_member(t, "pi", &all);
            }
            "reinstall-token-loss" => {
                drop(tower);
                drop(pi);
                let _ = fs::remove_dir_all(token_store_dir(&t.join("tower"), "tower"));
                let _ = fs::remove_dir_all(token_store_dir(&t.join("pi"), "pi"));
                tower = open_member(t, "tower", &all);
                pi = open_member(t, "pi", &all);
            }
            _ => {
                drop(pi);
                let _ = fs::remove_dir_all(t.join("pi/.legion-sync"));
                pi = rebuild_member(t, "pi", &all, 300);
            }
        }
        assert!(sync_all(&mut [&mut tower, &mut pi], 301).is_empty());

        // laptop returns and meets pi first.
        let _ = pull(&mut laptop, &pi, 400);
        let _ = pull(&mut pi, &laptop, 401);
        assert!(sync_all(&mut [&mut tower, &mut pi, &mut laptop], 402).is_empty());
        assert!(sync_all(&mut [&mut tower, &mut pi, &mut laptop], 410).is_empty());
        for n in all {
            assert_eq!(
                fs::read(t.join(n).join("f.txt")).ok().as_deref(),
                Some(&b"same bytes"[..]),
                "mode={mode}: re-created f.txt lost on {n} (found {:?})",
                where_is(&t.join(n), b"same bytes")
            );
        }
    }
}

/// Review 6 Probe 2: restoring pi's folder from a backup brings its `.legion-sync/index.redb`
/// back with it (Decision 1: one store inside the folder). tower's newer `a.txt` must stay and
/// deleted `g.txt` must NOT come back.
#[test]
fn r6_p2_folder_restore_does_not_win_over_newer_edit_or_deletion() {
    let tmp = scratch();
    let t = tmp.path();
    let all = ["tower", "pi"];
    let mut tower = open_member(t, "tower", &all);
    let mut pi = open_member(t, "pi", &all);
    fs::write(t.join("tower/a.txt"), b"a version 1").unwrap();
    fs::write(t.join("tower/g.txt"), b"to be deleted").unwrap();
    tower.scan(ts(0)).unwrap();
    assert!(sync_all(&mut [&mut tower, &mut pi], 1).is_empty());

    drop(pi);
    copy_tree(&t.join("pi"), &t.join("pi.bak"));
    let mut pi = open_member(t, "pi", &all);

    fs::write(t.join("tower/a.txt"), b"a version 2, newer").unwrap();
    fs::remove_file(t.join("tower/g.txt")).unwrap();
    tower.scan(ts(10)).unwrap();
    tower.flush_settled_deletions(ts(100)).unwrap();
    assert!(sync_all(&mut [&mut tower, &mut pi], 101).is_empty());
    assert_eq!(fs::read(t.join("pi/a.txt")).unwrap(), b"a version 2, newer");

    drop(pi);
    fs::remove_dir_all(t.join("pi")).unwrap();
    fs::rename(t.join("pi.bak"), t.join("pi")).unwrap();
    let mut pi = open_member(t, "pi", &all);
    pi.scan(ts(200)).unwrap();
    let _ = sync_all(&mut [&mut tower, &mut pi], 201);
    let e2 = sync_all(&mut [&mut tower, &mut pi], 210);
    assert!(e2.is_empty(), "{e2:?}");
    for n in all {
        assert_eq!(
            fs::read(t.join(n).join("a.txt")).unwrap(),
            b"a version 2, newer",
            "restored backup overwrote newer a.txt on {n}"
        );
        assert!(
            !t.join(n).join("g.txt").exists(),
            "restored backup resurrected deleted g.txt on {n}"
        );
    }
}

/// Review 6 Probe 3: the lost part is exactly the last record, and pi edits once after the restore
/// (so its sequence is back where tower's cursor is). tower pulls first. The post-restore edit
/// must reach tower, tower's copy of the lost record must come back, and both converge cleanly.
#[test]
fn r6_p3_restore_lost_exactly_last_record_then_one_edit() {
    for edit_same_path in [false, true] {
        let tmp = scratch();
        let t = tmp.path();
        let all = ["tower", "pi"];
        let mut tower = open_member(t, "tower", &all);
        let mut pi = open_member(t, "pi", &all);
        fs::write(t.join("pi/p.txt"), b"p").unwrap();
        pi.scan(ts(0)).unwrap();
        assert!(sync_all(&mut [&mut tower, &mut pi], 1).is_empty());

        drop(pi);
        copy_tree(&t.join("pi"), &t.join("pi.bak"));
        let mut pi = open_member(t, "pi", &all);
        fs::write(t.join("pi/q.txt"), b"lost on pi, kept on tower").unwrap();
        pi.scan(ts(10)).unwrap();
        assert!(pull(&mut tower, &pi, 11).is_none());
        assert!(t.join("tower/q.txt").exists());

        drop(pi);
        fs::remove_dir_all(t.join("pi")).unwrap();
        fs::rename(t.join("pi.bak"), t.join("pi")).unwrap();
        let edit_path = if edit_same_path { "q.txt" } else { "r.txt" };
        fs::write(t.join("pi").join(edit_path), b"post-restore edit").unwrap();
        let mut pi = open_member(t, "pi", &all);
        pi.scan(ts(20)).unwrap();

        let _tower_first = pull(&mut tower, &pi, 30);
        let _pi_first = pull(&mut pi, &tower, 31);
        let tower_second = pull(&mut tower, &pi, 32);
        let late = sync_all(&mut [&mut tower, &mut pi], 100);
        assert!(
            late.is_empty(),
            "edit_same_path={edit_same_path}: stalled: {late:?}"
        );
        assert!(
            tower_second.is_none(),
            "edit_same_path={edit_same_path}: tower's second pull still failed: {tower_second:?}"
        );

        for n in all {
            assert!(
                !where_is(&t.join(n), b"post-restore edit")
                    .iter()
                    .all(|p| p.starts_with(".legion-sync")),
                "edit_same_path={edit_same_path}: post-restore edit not live on {n}"
            );
            assert!(
                !where_is(&t.join(n), b"lost on pi, kept on tower")
                    .iter()
                    .all(|p| p.starts_with(".legion-sync")),
                "edit_same_path={edit_same_path}: lost pre-restore q.txt not live on {n}"
            );
        }
        assert_eq!(
            tower.status_at(ts(101)).unwrap().state,
            MemberState::UpToDate
        );
    }
}

/// Review 6 Probe 4: both members restored from backups taken at different times, then both make
/// new files before syncing. Both converge with all files, zero conflict copies, and UpToDate.
#[test]
fn r6_p4_two_members_restored_from_different_times() {
    let tmp = scratch();
    let t = tmp.path();
    let all = ["tower", "pi"];
    let mut tower = open_member(t, "tower", &all);
    let mut pi = open_member(t, "pi", &all);
    fs::write(t.join("tower/a.txt"), b"a").unwrap();
    tower.scan(ts(0)).unwrap();
    assert!(sync_all(&mut [&mut tower, &mut pi], 1).is_empty());

    drop(tower);
    copy_tree(&t.join("tower"), &t.join("tower.bak"));
    let mut tower = open_member(t, "tower", &all);
    fs::write(t.join("tower/t1.txt"), b"t1").unwrap();
    tower.scan(ts(10)).unwrap();
    assert!(sync_all(&mut [&mut tower, &mut pi], 11).is_empty());

    drop(pi);
    copy_tree(&t.join("pi"), &t.join("pi.bak"));
    let mut pi = open_member(t, "pi", &all);
    fs::write(t.join("pi/p1.txt"), b"p1").unwrap();
    pi.scan(ts(20)).unwrap();
    fs::write(t.join("tower/t2.txt"), b"t2").unwrap();
    tower.scan(ts(20)).unwrap();
    assert!(sync_all(&mut [&mut tower, &mut pi], 21).is_empty());

    drop(tower);
    drop(pi);
    for n in all {
        fs::remove_dir_all(t.join(n)).unwrap();
        fs::rename(t.join(format!("{n}.bak")), t.join(n)).unwrap();
    }
    let mut tower = open_member(t, "tower", &all);
    let mut pi = open_member(t, "pi", &all);
    fs::write(t.join("tower/after_t.txt"), b"after t").unwrap();
    fs::write(t.join("pi/after_p.txt"), b"after p").unwrap();
    tower.scan(ts(30)).unwrap();
    pi.scan(ts(30)).unwrap();

    let mut last = Vec::new();
    for k in 0..4 {
        last = sync_all(&mut [&mut tower, &mut pi], 100 + 10 * k);
    }
    assert!(last.is_empty(), "stalled: {last:?}");
    for n in all {
        for f in ["a.txt", "t1.txt", "after_t.txt", "after_p.txt"] {
            assert!(t.join(n).join(f).exists(), "{n} lacks {f}");
        }
        assert!(
            conflict_copies(&t.join(n)).is_empty(),
            "{n} has conflict copies"
        );
    }
    assert_eq!(pi.status_at(ts(200)).unwrap().state, MemberState::UpToDate);
}

/// Review 6 Probe 5 (Decisions 3 & 4): tower and pi truly rebuild from disk (index destroyed)
/// while nas is off; pi edits or deletes `f.txt` either before or after meeting tower, and nas
/// either stays healthy or also rebuilds. Afterwards every member holds the exact same provenance
/// for every path, edits win cleanly without conflict copies, deletions stay deleted, and
/// untouched `u.txt` keeps its exact provenance when nas did not rebuild.
#[test]
fn r6_p5_three_members_true_join_then_edit_or_delete() {
    let mut failures: Vec<String> = Vec::new();
    for (action, nas_also_rebuilds, edit_before_meeting) in [
        ("edit", false, false),
        ("delete", false, false),
        ("edit", true, false),
        ("delete", true, false),
        ("edit", true, true),
        ("delete", true, true),
        ("edit", false, true),
        ("delete", false, true),
    ] {
        let tmp = scratch();
        let t = tmp.path();
        let all = ["tower", "pi", "nas"];
        let mut tower = open_member(t, "tower", &all);
        let mut pi = open_member(t, "pi", &all);
        let mut nas = open_member(t, "nas", &all);

        // Author both files post-join so nas holds versioned records.
        tower.scan(ts(0)).unwrap();
        fs::write(t.join("tower/f.txt"), b"original").unwrap();
        fs::write(t.join("tower/u.txt"), b"untouched").unwrap();
        tower.scan(ts(1)).unwrap();
        assert!(sync_all(&mut [&mut tower, &mut pi, &mut nas], 2).is_empty());
        let u_before = provenance_tag(&tower, "u.txt");

        drop(tower);
        drop(pi);
        let _ = fs::remove_dir_all(t.join("tower/.legion-sync"));
        let _ = fs::remove_dir_all(t.join("pi/.legion-sync"));
        tower = rebuild_member(t, "tower", &all, 50);
        pi = rebuild_member(t, "pi", &all, 50);
        if nas_also_rebuilds {
            drop(nas);
            let _ = fs::remove_dir_all(t.join("nas/.legion-sync"));
            nas = rebuild_member(t, "nas", &all, 50);
        }

        let act = |pi: &mut E, now: i64| {
            if action == "edit" {
                fs::write(t.join("pi/f.txt"), b"edited on pi").unwrap();
                pi.scan(ts(now)).unwrap();
            } else {
                fs::remove_file(t.join("pi/f.txt")).unwrap();
                pi.scan(ts(now)).unwrap();
                pi.flush_settled_deletions(ts(now + 60)).unwrap();
            }
        };
        if edit_before_meeting {
            act(&mut pi, 55);
            let e = sync_all(&mut [&mut tower, &mut pi], 200);
            assert!(e.is_empty(), "{e:?}");
        } else {
            let e = sync_all(&mut [&mut tower, &mut pi], 60);
            assert!(e.is_empty(), "{e:?}");
            act(&mut pi, 100);
            assert!(pull(&mut tower, &pi, 200).is_none());
        }

        let _ = pull(&mut nas, &tower, 300);
        let _ = pull(&mut tower, &nas, 301);
        for k in 0..3 {
            let e = sync_all(&mut [&mut tower, &mut pi, &mut nas], 400 + 10 * k);
            assert!(e.is_empty(), "{e:?}");
        }
        let tag = format!(
            "action={action} nas_rebuilds={nas_also_rebuilds} edit_before_meeting={edit_before_meeting}"
        );
        for n in all {
            let f = fs::read(t.join(n).join("f.txt")).ok();
            if action == "edit" && f.as_deref() != Some(&b"edited on pi"[..]) {
                failures.push(format!(
                    "{tag}: {n} f.txt = {:?}",
                    f.as_deref().map(String::from_utf8_lossy)
                ));
            }
            if action == "delete" && f.is_some() {
                failures.push(format!(
                    "{tag}: f.txt came back on {n} = {:?}",
                    f.as_deref().map(String::from_utf8_lossy)
                ));
            }
            let c = conflict_copies(&t.join(n));
            if !c.is_empty() {
                failures.push(format!("{tag}: {n} has conflict copies {c:?}"));
            }
        }
        for p in ["f.txt", "u.txt"] {
            let vt = provenance_tag(&tower, p);
            let vp = provenance_tag(&pi, p);
            let vn = provenance_tag(&nas, p);
            if !(vt == vp && vp == vn) {
                failures.push(format!(
                    "{tag}: different provenance for {p}: tower={vt:?} pi={vp:?} nas={vn:?}"
                ));
            }
        }
        if !nas_also_rebuilds && provenance_tag(&tower, "u.txt") != u_before {
            failures.push(format!(
                "{tag}: untouched u.txt provenance changed {u_before:?} -> {:?}",
                provenance_tag(&tower, "u.txt")
            ));
        }
    }
    assert!(failures.is_empty(), "{failures:#?}");
}

/// Review 6 Probe 6 (Decisions 3 & 4): two members; tower destroys its index and rebuilds while pi
/// is off, then deletes or edits `f.txt` BEFORE pi returns. When pi returns, the deletion stays
/// deleted and the edit wins without a conflict copy.
#[test]
fn r6_p6_two_members_true_join_while_peer_off_then_delete_or_edit() {
    for action in ["delete", "edit"] {
        let tmp = scratch();
        let t = tmp.path();
        let all = ["tower", "pi"];
        let mut tower = open_member(t, "tower", &all);
        let mut pi = open_member(t, "pi", &all);
        tower.scan(ts(0)).unwrap();
        fs::write(t.join("tower/f.txt"), b"original").unwrap();
        tower.scan(ts(1)).unwrap();
        assert!(sync_all(&mut [&mut tower, &mut pi], 2).is_empty());

        // pi goes off; tower loses its `.legion-sync` and rebuilds as a true join.
        drop(tower);
        let _ = fs::remove_dir_all(t.join("tower/.legion-sync"));
        let mut tower = rebuild_member(t, "tower", &all, 50);
        if action == "delete" {
            fs::remove_file(t.join("tower/f.txt")).unwrap();
            tower.scan(ts(60)).unwrap();
            tower.flush_settled_deletions(ts(200)).unwrap();
        } else {
            fs::write(t.join("tower/f.txt"), b"edited on tower").unwrap();
            tower.scan(ts(60)).unwrap();
        }

        for k in 0..3 {
            let e = sync_all(&mut [&mut tower, &mut pi], 300 + 10 * k);
            assert!(e.is_empty(), "{e:?}");
        }
        for n in all {
            let f = fs::read(t.join(n).join("f.txt")).ok();
            match action {
                "delete" => assert!(
                    f.is_none(),
                    "delete: f.txt came back on {n} = {:?}",
                    f.as_deref().map(String::from_utf8_lossy)
                ),
                "edit" => assert_eq!(
                    f.as_deref(),
                    Some(&b"edited on tower"[..]),
                    "edit: wrong f.txt on {n}"
                ),
                _ => unreachable!(),
            }
            assert!(
                conflict_copies(&t.join(n)).is_empty(),
                "{action}: {n} has unexpected conflict copies"
            );
        }
    }
}

/// Review 6 Probe 7 (Decision 1): there is ONE store (`.legion-sync/index.redb`) and NO
/// `deletions.log`. Deletions live permanently inside `.legion-sync/index.redb` and survive both
/// reinstall (token loss) and in-place schema upgrade.
#[test]
fn r6_p7_single_store_no_deletions_log_and_history_survives_reinstall() {
    let tmp = scratch();
    let t = tmp.path();
    let all = ["tower", "pi"];
    let mut tower = open_member(t, "tower", &all);
    let mut pi = open_member(t, "pi", &all);
    fs::write(t.join("tower/g.txt"), b"gone").unwrap();
    tower.scan(ts(0)).unwrap();
    assert!(sync_all(&mut [&mut tower, &mut pi], 1).is_empty());

    fs::remove_file(t.join("tower/g.txt")).unwrap();
    tower.scan(ts(10)).unwrap();
    tower.flush_settled_deletions(ts(100)).unwrap();
    assert!(sync_all(&mut [&mut tower, &mut pi], 101).is_empty());

    for n in all {
        assert!(t.join(n).join(SYNC_INDEX_FILE).exists());
        assert!(!t.join(n).join(".legion-sync/deletions.log").exists());
    }

    // Reinstall tower (token lost) + in-place upgrade pi -> both keep the deletion permanently.
    drop(tower);
    let _ = fs::remove_dir_all(token_store_dir(&t.join("tower"), "tower"));
    let mut tower = open_member(t, "tower", &all);
    pi.store_mut()
        .write_schema_version_for_test(MIN_MIGRATABLE_SCHEMA_VERSION)
        .unwrap();
    drop(pi);
    let mut pi = open_member(t, "pi", &all);

    tower.scan(ts(200)).unwrap();
    pi.scan(ts(200)).unwrap();
    assert!(sync_all(&mut [&mut tower, &mut pi], 201).is_empty());
    assert!(
        tower
            .store()
            .get_record("g.txt")
            .unwrap()
            .is_some_and(|r| r.deleted)
    );
    assert!(
        pi.store()
            .get_record("g.txt")
            .unwrap()
            .is_some_and(|r| r.deleted)
    );
}

/// Review 6 Probe 8: three members, nas off after the first exchange. tower writes `b.txt`, pi
/// pulls it and tower hears pi's ack. Neither tower nor pi may report `UpToDate`.
#[test]
fn r6_p8_status_not_up_to_date_with_one_member_off() {
    let tmp = scratch();
    let t = tmp.path();
    let all = ["tower", "pi", "nas"];
    let mut tower = open_member(t, "tower", &all);
    let mut pi = open_member(t, "pi", &all);
    let mut nas = open_member(t, "nas", &all);

    fs::write(t.join("tower/a.txt"), b"a").unwrap();
    tower.scan(ts(0)).unwrap();
    assert!(sync_all(&mut [&mut tower, &mut pi, &mut nas], 1).is_empty());
    assert_eq!(tower.status_at(ts(5)).unwrap().state, MemberState::UpToDate);

    fs::write(t.join("tower/b.txt"), b"b").unwrap();
    tower.scan(ts(10)).unwrap();
    assert!(pull(&mut pi, &tower, 11).is_none());
    assert!(pull(&mut tower, &pi, 12).is_none());
    assert!(pull(&mut pi, &tower, 13).is_none());

    let st = tower.status_at(ts(20)).unwrap();
    assert_ne!(st.state, MemberState::UpToDate, "tower: {}", st.summary);
    let sp = pi.status_at(ts(20)).unwrap();
    assert_ne!(sp.state, MemberState::UpToDate, "pi: {}", sp.summary);
}

/// Decision 2: copying an entire folder WITH `.legion-sync` (including `index.redb`) to a second
/// member and running both rotates the copy cleanly on open (unknown instance token), keeps every
/// record and deletion intact, and lets both members edit, delete, and converge without identity
/// errors or lost saves.
#[test]
fn copy_folder_with_internal_store_to_second_member_and_run_both() {
    let tmp = scratch();
    let t = tmp.path();
    let all = ["tower", "laptop"];
    let mut tower = open_member(t, "tower", &all);
    tower.scan(ts(0)).unwrap();
    fs::write(t.join("tower/shared.txt"), b"v1").unwrap();
    fs::write(t.join("tower/deleted_before_copy.txt"), b"gone").unwrap();
    fs::write(t.join("tower/tower_deletes.txt"), b"remove on tower").unwrap();
    fs::write(t.join("tower/laptop_deletes.txt"), b"remove on laptop").unwrap();
    tower.scan(ts(1)).unwrap();

    fs::remove_file(t.join("tower/deleted_before_copy.txt")).unwrap();
    tower.scan(ts(2)).unwrap();
    tower.flush_settled_deletions(ts(40)).unwrap();
    let tower_inc_before = tower.meta().unwrap().incarnation;
    let tower_tok_before = tower.meta().unwrap().instance_token;

    // Copy the entire folder INCLUDING `.legion-sync/index.redb` and `marker.json` to laptop!
    drop(tower);
    copy_tree(&t.join("tower"), &t.join("laptop"));

    let mut tower = open_member(t, "tower", &all);
    let mut laptop = open_member(t, "laptop", &all);
    assert_eq!(
        tower.meta().unwrap().incarnation,
        tower_inc_before,
        "original machine must keep its incarnation"
    );
    assert_ne!(
        laptop.meta().unwrap().incarnation,
        tower_inc_before,
        "copied folder on second member must rotate incarnation on open"
    );
    assert_ne!(
        laptop.meta().unwrap().instance_token,
        tower_tok_before,
        "copied folder must mint a fresh instance token"
    );
    assert!(
        !laptop.meta().unwrap().join_mode,
        "rotation must keep history and NEVER enter join mode"
    );
    assert!(
        laptop
            .store()
            .get_record("deleted_before_copy.txt")
            .unwrap()
            .is_some_and(|r| r.deleted),
        "copied folder must preserve deletion history across rotation"
    );

    // Both sides now make edits and deletions independently before syncing.
    fs::write(t.join("tower/tower_new.txt"), b"from tower").unwrap();
    fs::remove_file(t.join("tower/tower_deletes.txt")).unwrap();
    fs::write(t.join("laptop/laptop_new.txt"), b"from laptop").unwrap();
    fs::remove_file(t.join("laptop/laptop_deletes.txt")).unwrap();
    fs::write(t.join("laptop/shared.txt"), b"v2 from laptop").unwrap();
    tower.scan(ts(50)).unwrap();
    tower.flush_settled_deletions(ts(90)).unwrap();
    laptop.scan(ts(50)).unwrap();
    laptop.flush_settled_deletions(ts(90)).unwrap();

    assert!(sync_all(&mut [&mut tower, &mut laptop], 100).is_empty());
    assert!(sync_all(&mut [&mut tower, &mut laptop], 110).is_empty());

    for n in all {
        assert_eq!(
            fs::read(t.join(n).join("shared.txt")).unwrap(),
            b"v2 from laptop"
        );
        assert_eq!(
            fs::read(t.join(n).join("tower_new.txt")).unwrap(),
            b"from tower"
        );
        assert_eq!(
            fs::read(t.join(n).join("laptop_new.txt")).unwrap(),
            b"from laptop"
        );
        assert!(!t.join(n).join("deleted_before_copy.txt").exists());
        assert!(!t.join(n).join("tower_deletes.txt").exists());
        assert!(!t.join(n).join("laptop_deletes.txt").exists());
        assert!(conflict_copies(&t.join(n)).is_empty());
    }
    assert_eq!(
        tower.status_at(ts(120)).unwrap().state,
        MemberState::UpToDate
    );
    assert_eq!(
        laptop.status_at(ts(120)).unwrap().state,
        MemberState::UpToDate
    );
}

/// Decision 2: reinstall (machine instance token lost while folder + `.legion-sync/index.redb`
/// stay) with unsynced edits and deletions on BOTH sides. Neither side loses a save or resurrects
/// a deletion.
#[test]
fn reinstall_token_lost_with_unsynced_edits_and_deletions_on_both_sides() {
    let tmp = scratch();
    let t = tmp.path();
    let all = ["tower", "pi"];
    let mut tower = open_member(t, "tower", &all);
    let mut pi = open_member(t, "pi", &all);
    tower.scan(ts(0)).unwrap();
    for name in [
        "both_edit.txt",
        "pi_del.txt",
        "tower_del.txt",
        "pi_edit.txt",
        "tower_edit.txt",
    ] {
        fs::write(t.join("tower").join(name), format!("base {name}")).unwrap();
    }
    tower.scan(ts(1)).unwrap();
    assert!(sync_all(&mut [&mut tower, &mut pi], 2).is_empty());

    // Offline work on BOTH sides.
    fs::write(t.join("tower/both_edit.txt"), b"tower concurrent").unwrap();
    fs::write(t.join("tower/tower_edit.txt"), b"tower updated").unwrap();
    fs::remove_file(t.join("tower/tower_del.txt")).unwrap();
    tower.scan(ts(10)).unwrap();
    tower.flush_settled_deletions(ts(50)).unwrap();

    fs::write(t.join("pi/both_edit.txt"), b"pi concurrent").unwrap();
    fs::write(t.join("pi/pi_edit.txt"), b"pi updated").unwrap();
    fs::remove_file(t.join("pi/pi_del.txt")).unwrap();
    pi.scan(ts(10)).unwrap();
    pi.flush_settled_deletions(ts(50)).unwrap();

    // pi reinstalls: machine state directory (instance token) is wiped, folder stays intact.
    let old_pi_inc = pi.meta().unwrap().incarnation;
    drop(pi);
    fs::remove_dir_all(token_store_dir(&t.join("pi"), "pi")).unwrap();
    let mut pi = open_member(t, "pi", &all);
    assert_ne!(pi.meta().unwrap().incarnation, old_pi_inc);
    assert!(!pi.meta().unwrap().join_mode);

    assert!(sync_all(&mut [&mut tower, &mut pi], 60).is_empty());
    assert!(sync_all(&mut [&mut tower, &mut pi], 70).is_empty());

    for n in all {
        assert_eq!(
            fs::read(t.join(n).join("tower_edit.txt")).unwrap(),
            b"tower updated"
        );
        assert_eq!(
            fs::read(t.join(n).join("pi_edit.txt")).unwrap(),
            b"pi updated"
        );
        assert!(!t.join(n).join("pi_del.txt").exists());
        assert!(!t.join(n).join("tower_del.txt").exists());
        assert!(
            !where_is(&t.join(n), b"tower concurrent")
                .iter()
                .all(|p| p.starts_with(".legion-sync")),
            "tower's concurrent save lost on {n}"
        );
        assert!(
            !where_is(&t.join(n), b"pi concurrent")
                .iter()
                .all(|p| p.starts_with(".legion-sync")),
            "pi's concurrent save lost on {n}"
        );
    }
    assert_eq!(
        tower.status_at(ts(80)).unwrap().state,
        MemberState::UpToDate
    );
    assert_eq!(pi.status_at(ts(80)).unwrap().state, MemberState::UpToDate);
}

/// Decision 2 + Mechanism X: restoring a folder (including `.legion-sync/index.redb`) to an older
/// state on the same machine (token still matches), then making edits and deletions BEFORE first
/// exchange across `new < lost`, `new == lost`, and `new > lost`. Zero saves lost and clean
/// convergence in every case.
#[test]
fn restore_folder_to_older_state_then_edits_before_first_exchange() {
    for (lost, new_edits) in [(3usize, 1usize), (3, 3), (3, 5)] {
        let tmp = scratch();
        let t = tmp.path();
        let all = ["tower", "pi"];
        let mut tower = open_member(t, "tower", &all);
        let mut pi = open_member(t, "pi", &all);
        tower.scan(ts(0)).unwrap();
        for i in 0..6 {
            fs::write(t.join(format!("tower/f{i}.txt")), format!("base {i}")).unwrap();
        }
        tower.scan(ts(1)).unwrap();
        assert!(sync_all(&mut [&mut tower, &mut pi], 2).is_empty());

        drop(pi);
        copy_tree(&t.join("pi"), &t.join("pi.bak"));
        let mut pi = open_member(t, "pi", &all);

        for i in 0..lost {
            fs::write(t.join(format!("pi/f{i}.txt")), format!("pre-restore {i}")).unwrap();
        }
        pi.scan(ts(10)).unwrap();
        assert!(pull(&mut tower, &pi, 11).is_none());

        // Restore pi's folder (with `.legion-sync/index.redb`) on the same machine.
        drop(pi);
        fs::remove_dir_all(t.join("pi")).unwrap();
        fs::rename(t.join("pi.bak"), t.join("pi")).unwrap();
        let mut pi = open_member(t, "pi", &all);

        // Make post-restore edits BEFORE first exchange (overlapping f0.. plus fresh files).
        for i in 0..new_edits {
            fs::write(t.join(format!("pi/f{i}.txt")), format!("post-restore {i}")).unwrap();
        }
        pi.scan(ts(20)).unwrap();

        assert!(
            pull(&mut tower, &pi, 30).is_some(),
            "lost={lost} new={new_edits}: tower must refuse rolled-back pi before pi rotates"
        );
        assert!(
            pull(&mut pi, &tower, 31).is_none(),
            "lost={lost} new={new_edits}: pi must detect own rollback and rotate cleanly"
        );
        assert!(sync_all(&mut [&mut tower, &mut pi], 35).is_empty());
        assert!(sync_all(&mut [&mut tower, &mut pi], 45).is_empty());

        for n in all {
            for i in 0..lost {
                let b = format!("pre-restore {i}");
                assert!(
                    !where_is(&t.join(n), b.as_bytes())
                        .iter()
                        .all(|p| p.starts_with(".legion-sync")),
                    "lost={lost} new={new_edits}: {b:?} lost on {n}"
                );
            }
            for i in 0..new_edits {
                let b = format!("post-restore {i}");
                assert!(
                    !where_is(&t.join(n), b.as_bytes())
                        .iter()
                        .all(|p| p.starts_with(".legion-sync")),
                    "lost={lost} new={new_edits}: {b:?} lost on {n}"
                );
            }
        }
        assert_eq!(
            tower.status_at(ts(60)).unwrap().state,
            MemberState::UpToDate
        );
        let pi_st = pi.status_at(ts(60)).unwrap();
        assert!(
            matches!(
                pi_st.state,
                MemberState::Problem { ref problems, .. }
                    if problems.len() == 1 && problems[0].kind == ProblemKind::PeerForcedRotation
            ),
            "lost={lost} new={new_edits}: pi should report only the honest rollback notice, got {:?}",
            pi_st.state
        );
        assert_eq!(pi.store().held_back_count().unwrap(), 0);
    }
}

/// Decisions 3 & 4: true join (no `.legion-sync` at all) with edits, deletions, new files, and
/// untouched files made BEFORE the first exchange while peers are off — tested for both 2 members
/// and 3 members.
#[test]
fn true_join_edits_and_deletions_before_first_exchange_two_and_three_members() {
    for three_members in [false, true] {
        let tmp = scratch();
        let t = tmp.path();
        let all: Vec<&str> = if three_members {
            vec!["tower", "pi", "nas"]
        } else {
            vec!["tower", "pi"]
        };
        let mut tower = open_member(t, "tower", &all);
        let mut pi = open_member(t, "pi", &all);
        let mut nas_opt = three_members.then(|| open_member(t, "nas", &all));

        tower.scan(ts(0)).unwrap();
        for name in [
            "edit_me.txt",
            "del_me.txt",
            "untouched.txt",
            "both_edit.txt",
        ] {
            fs::write(t.join("tower").join(name), format!("base {name}")).unwrap();
        }
        tower.scan(ts(1)).unwrap();
        if let Some(ref mut nas) = nas_opt {
            assert!(sync_all(&mut [&mut tower, &mut pi, nas], 2).is_empty());
        } else {
            assert!(sync_all(&mut [&mut tower, &mut pi], 2).is_empty());
        }
        let untouched_before = provenance_tag(&tower, "untouched.txt");

        // Concurrent peer edit on `both_edit.txt` while pi is offline.
        fs::write(t.join("tower/both_edit.txt"), b"tower concurrent").unwrap();
        tower.scan(ts(10)).unwrap();
        if let Some(ref mut nas) = nas_opt {
            assert!(sync_all(&mut [&mut tower, nas], 11).is_empty());
        }

        // pi loses `.legion-sync` entirely and joins from disk, then edits & deletes BEFORE meeting
        // any peer.
        drop(pi);
        fs::remove_dir_all(t.join("pi/.legion-sync")).unwrap();
        let _ = fs::remove_dir_all(token_store_dir(&t.join("pi"), "pi"));
        let mut pi = open_member(t, "pi", &all);
        pi.scan(ts(20)).unwrap();
        assert_eq!(
            pi.store()
                .get_record("untouched.txt")
                .unwrap()
                .unwrap()
                .provenance,
            RecordProvenance::Unversioned,
            "true join must index pre-existing files as Unversioned"
        );

        fs::write(t.join("pi/edit_me.txt"), b"edited after join").unwrap();
        fs::write(t.join("pi/both_edit.txt"), b"pi concurrent").unwrap();
        fs::remove_file(t.join("pi/del_me.txt")).unwrap();
        fs::write(t.join("pi/brand_new.txt"), b"brand new on pi").unwrap();
        pi.scan(ts(30)).unwrap();
        pi.flush_settled_deletions(ts(90)).unwrap();

        if let Some(ref mut nas) = nas_opt {
            assert!(sync_all(&mut [&mut tower, &mut pi, nas], 100).is_empty());
            assert!(sync_all(&mut [&mut tower, &mut pi, nas], 110).is_empty());
        } else {
            assert!(sync_all(&mut [&mut tower, &mut pi], 100).is_empty());
            assert!(sync_all(&mut [&mut tower, &mut pi], 110).is_empty());
        }

        for n in &all {
            assert_eq!(
                fs::read(t.join(n).join("edit_me.txt")).unwrap(),
                b"edited after join",
                "three={three_members}: edit_me.txt wrong on {n}"
            );
            assert!(
                !t.join(n).join("del_me.txt").exists(),
                "three={three_members}: del_me.txt resurrected on {n}"
            );
            assert_eq!(
                fs::read(t.join(n).join("brand_new.txt")).unwrap(),
                b"brand new on pi"
            );
            assert_eq!(
                fs::read(t.join(n).join("untouched.txt")).unwrap(),
                b"base untouched.txt"
            );
            assert!(
                !where_is(&t.join(n), b"tower concurrent")
                    .iter()
                    .all(|p| p.starts_with(".legion-sync")),
                "three={three_members}: tower concurrent lost on {n}"
            );
            assert!(
                !where_is(&t.join(n), b"pi concurrent")
                    .iter()
                    .all(|p| p.starts_with(".legion-sync")),
                "three={three_members}: pi concurrent lost on {n}"
            );
        }
        // Untouched file kept its exact version vector with zero key growth!
        assert_eq!(provenance_tag(&tower, "untouched.txt"), untouched_before);
        assert_eq!(provenance_tag(&pi, "untouched.txt"), untouched_before);
        if let Some(ref nas) = nas_opt {
            assert_eq!(provenance_tag(nas, "untouched.txt"), untouched_before);
        }
    }
}

/// Accepted limit: `f.txt` is deleted everywhere, laptop goes offline, tower re-saves `f.txt` with
/// the exact same bytes and syncs to pi, and then EVERY member that knew about the re-save (both
/// tower and pi) destroys its entire `.legion-sync`. When laptop returns, its deletion wins and
/// moves the re-saved file to the bin — it is in the bin, not gone.
#[test]
fn accepted_limit_all_members_knowing_resave_destroy_internal_store_bins_file() {
    let tmp = scratch();
    let t = tmp.path();
    let all = ["tower", "pi", "laptop"];
    let mut tower = open_member(t, "tower", &all);
    let mut pi = open_member(t, "pi", &all);
    let mut laptop = open_member(t, "laptop", &all);

    tower.scan(ts(0)).unwrap();
    fs::write(t.join("tower/f.txt"), b"same bytes").unwrap();
    tower.scan(ts(1)).unwrap();
    assert!(sync_all(&mut [&mut tower, &mut pi, &mut laptop], 2).is_empty());

    fs::remove_file(t.join("tower/f.txt")).unwrap();
    tower.scan(ts(10)).unwrap();
    tower.flush_settled_deletions(ts(100)).unwrap();
    assert!(sync_all(&mut [&mut tower, &mut pi, &mut laptop], 101).is_empty());
    // laptop goes offline.

    fs::write(t.join("tower/f.txt"), b"same bytes").unwrap();
    tower.scan(ts(200)).unwrap();
    assert!(sync_all(&mut [&mut tower, &mut pi], 201).is_empty());

    // BOTH tower and pi destroy their entire `.legion-sync` and rebuild from disk.
    drop(tower);
    drop(pi);
    fs::remove_dir_all(t.join("tower/.legion-sync")).unwrap();
    fs::remove_dir_all(t.join("pi/.legion-sync")).unwrap();
    let mut tower = rebuild_member(t, "tower", &all, 300);
    let mut pi = rebuild_member(t, "pi", &all, 300);
    assert!(sync_all(&mut [&mut tower, &mut pi], 301).is_empty());

    // laptop returns with its deletion of `same bytes`.
    assert!(sync_all(&mut [&mut tower, &mut pi, &mut laptop], 400).is_empty());
    assert!(sync_all(&mut [&mut tower, &mut pi, &mut laptop], 410).is_empty());

    for n in ["tower", "pi"] {
        assert!(!t.join(n).join("f.txt").exists());
        let in_bin: Vec<String> = where_is(&t.join(n), b"same bytes")
            .into_iter()
            .filter(|p| p.starts_with(".legion-sync/bin/"))
            .collect();
        assert!(
            !in_bin.is_empty(),
            "accepted limit: re-saved file must be preserved in {n}'s bin, not gone"
        );
    }
}

/// Decision 1: if the filesystem cannot open and durably commit `.legion-sync/index.redb`,
/// `SyncEngine::open_folder` refuses the folder with `SyncError::Unsupported` and a clear
/// plain-language sentence instead of silently falling back outside the folder.
#[test]
fn refuse_folder_when_durable_index_commits_unavailable() {
    let tmp = scratch();
    let root = tmp.path().join("readonly_folder");
    let state = tmp.path().join("state");
    fs::create_dir_all(&root).unwrap();

    // Initialize once so `.legion-sync/index.redb` exists, then mark it read-only so durable
    // read-write commits fail on Unix and Windows alike.
    {
        let mut eng = SyncEngine::open_folder(
            cfg("tower", &["tower"]),
            &root,
            FileInstanceTokenStore::new(&state),
        )
        .unwrap();
        fs::write(root.join("doc.txt"), b"hello").unwrap();
        eng.scan(ts(0)).unwrap();
    }

    let index_path: PathBuf = root.join(SYNC_INDEX_FILE);
    let orig_perms = fs::metadata(&index_path).unwrap().permissions();
    let mut ro_perms = orig_perms.clone();
    ro_perms.set_readonly(true);
    fs::set_permissions(&index_path, ro_perms).unwrap();

    let res = SyncEngine::open_folder(
        cfg("tower", &["tower"]),
        &root,
        FileInstanceTokenStore::new(&state),
    );

    // Restore permissions before asserting so temp cleanup always succeeds.
    let _ = fs::set_permissions(&index_path, orig_perms);

    match res {
        Err(SyncError::Unsupported { reason, hint }) => {
            assert!(
                reason.contains("durable index commits"),
                "expected plain-language durable-commit explanation, got: {reason}"
            );
            assert!(hint.is_some());
        }
        Err(other) => panic!("expected SyncError::Unsupported, got {other:?}"),
        Ok(_) => panic!("expected read-only index open to be refused with SyncError::Unsupported"),
    }
}
