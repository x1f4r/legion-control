//! Real-disk safety regression tests: a person's programs interfering with apply on a
//! real btrfs directory, at exact operation boundaries.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod safety_common;

use std::fs;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use legion_sync::{FsCapabilities, FsErrorKind, IndexStore, SyncError, SyncFs};

use safety_common::*;

#[cfg(unix)]
const ATOMIC_REPLACE_OP: &str = "exchange_tmp_with_live";
#[cfg(windows)]
const ATOMIC_REPLACE_OP: &str = "replace_live_with_tmp_backup";

fn two_members(
    t: &std::path::Path,
) -> (
    legion_sync::SyncEngine<legion_sync::RealFs, legion_sync::RedbIndexStore>,
    legion_sync::SyncEngine<HookFs, legion_sync::RedbIndexStore>,
) {
    let mut tower = open_real(
        &t.join("tower"),
        &t.join("tower.redb"),
        "tower",
        &["tower", "pi"],
    );
    let mut pi = open_hook(&t.join("pi"), &t.join("pi.redb"), "pi", &["tower", "pi"]);
    fs::write(t.join("tower/doc.txt"), b"v1 original").unwrap();
    tower.scan(ts(0)).unwrap();
    pi.pull_from_peer(&EnginePeer::new(&tower, "pi"), ts(1))
        .unwrap();
    assert_eq!(fs::read(t.join("pi/doc.txt")).unwrap(), b"v1 original");
    (tower, pi)
}

/// Two quick saves by a person's program while a remote replacement is applied: the first save
/// lands before the atomic exchange, the second (write-temp-then-rename) right after it.
#[test]
fn r1_two_saves_around_exchange() {
    let tmp = scratch();
    let t = tmp.path();
    let (mut tower, mut pi) = two_members(t);
    fs::write(t.join("tower/doc.txt"), b"tower edit v2").unwrap();
    tower.scan(ts(10)).unwrap();

    let p1 = t.join("pi/doc.txt");
    pi.fs().add(ATOMIC_REPLACE_OP, When::Before, 1, move || {
        fs::write(&p1, b"pi save 1 (in place)").unwrap();
        Act::Continue
    });
    let p2 = t.join("pi/doc.txt");
    pi.fs().add(ATOMIC_REPLACE_OP, When::After, 1, move || {
        write_via_rename(&p2, b"pi save 2 NEWEST");
        Act::Continue
    });
    let r = pi.pull_from_peer(&EnginePeer::new(&tower, "pi"), ts(20));
    println!("pull: {r:?}");
    assert!(pi.fs().all_fired());
    pi.fs().clear();
    let _ = pi.scan(ts(30));
    println!("pi after:\n{}", dump(&t.join("pi")));
    let w1 = where_is(&t.join("pi"), b"pi save 1 (in place)");
    let w2 = where_is(&t.join("pi"), b"pi save 2 NEWEST");
    println!("save1 at {w1:?}; save2 at {w2:?}");
    assert!(
        !w2.is_empty(),
        "the newest save is gone; the older save is at {w1:?}"
    );
}

/// A save lands before the exchange, and the person moves the file right after it.
#[test]
fn r2_save_then_move_around_exchange() {
    let tmp = scratch();
    let t = tmp.path();
    let (mut tower, mut pi) = two_members(t);
    fs::write(t.join("tower/doc.txt"), b"tower edit v2").unwrap();
    tower.scan(ts(10)).unwrap();

    let p1 = t.join("pi/doc.txt");
    pi.fs().add(ATOMIC_REPLACE_OP, When::Before, 1, move || {
        fs::write(&p1, b"pi save 1 (in place)").unwrap();
        Act::Continue
    });
    let p2 = t.join("pi/doc.txt");
    let p2b = t.join("pi/moved.txt");
    pi.fs().add(ATOMIC_REPLACE_OP, When::After, 1, move || {
        fs::rename(&p2, &p2b).unwrap();
        Act::Continue
    });
    let r = pi.pull_from_peer(&EnginePeer::new(&tower, "pi"), ts(20));
    println!("pull: {r:?}");
    assert!(pi.fs().all_fired());
    pi.fs().clear();
    let _ = pi.scan(ts(30));
    println!("pi after:\n{}", dump(&t.join("pi")));
    let w1 = where_is(&t.join("pi"), b"pi save 1 (in place)");
    assert!(!w1.is_empty(), "the person's save is gone");
}

/// A same-size save lands just after the exchange succeeds: it must be noticed on the next scan.
#[test]
fn r3_same_size_save_right_after_exchange() {
    let tmp = scratch();
    let t = tmp.path();
    let (mut tower, mut pi) = two_members(t);
    fs::write(t.join("tower/doc.txt"), b"tower edit v2").unwrap();
    tower.scan(ts(10)).unwrap();

    let p2 = t.join("pi/doc.txt");
    pi.fs().add(ATOMIC_REPLACE_OP, When::After, 1, move || {
        std::thread::sleep(std::time::Duration::from_millis(1200));
        write_via_rename(&p2, b"PI SAME SIZE!");
        Act::Continue
    });
    assert_eq!(b"PI SAME SIZE!".len(), b"tower edit v2".len());
    let r = pi.pull_from_peer(&EnginePeer::new(&tower, "pi"), ts(20));
    println!("pull: {r:?}");
    assert!(pi.fs().all_fired());
    pi.fs().clear();
    let tower_mt = fs::metadata(t.join("tower/doc.txt"))
        .unwrap()
        .modified()
        .unwrap();
    println!(
        "tower doc.txt mtime: {:?}",
        tower_mt.duration_since(std::time::UNIX_EPOCH).unwrap()
    );
    let mt = fs::metadata(t.join("pi/doc.txt"))
        .unwrap()
        .modified()
        .unwrap();
    println!(
        "pi doc.txt mtime after apply: {:?}",
        mt.duration_since(std::time::UNIX_EPOCH).unwrap()
    );
    let s = pi.scan(ts(30)).unwrap();
    println!("pi scan: {s:?}");
    let s2 = pi.scan(ts(40)).unwrap();
    println!("pi scan2: {s2:?}");
    tower
        .pull_from_peer(&EnginePeer::new(&pi, "tower"), ts(50))
        .unwrap();
    println!(
        "pi:\n{}tower:\n{}",
        dump(&t.join("pi")),
        dump(&t.join("tower"))
    );
    let rec = pi.store().get_record("doc.txt").unwrap().unwrap();
    println!(
        "pi index hash {:?} vs disk {:?}",
        rec.hash,
        legion_xfer::hash::hash_bytes(b"PI SAME SIZE!")
    );
    assert!(
        !where_is(&t.join("tower"), b"PI SAME SIZE!").is_empty(),
        "pi's save never left pi: pi's index still says doc.txt is tower's version"
    );
}

/// Remote deletion: a save lands just before the claim, a second save (rename-over) right after.
#[test]
fn r4_delete_claim_putback_fails_then_restart() {
    let tmp = scratch();
    let t = tmp.path();
    let (mut tower, mut pi) = two_members(t);
    fs::remove_file(t.join("tower/doc.txt")).unwrap();
    tower.scan(ts(10)).unwrap();

    let p1 = t.join("pi/doc.txt");
    pi.fs()
        .add("move_live_to_tmp_no_replace", When::Before, 1, move || {
            fs::write(&p1, b"pi save 1 (in place)").unwrap();
            Act::Continue
        });
    let p2 = t.join("pi/doc.txt");
    pi.fs()
        .add("move_live_to_tmp_no_replace", When::After, 1, move || {
            write_via_rename(&p2, b"pi save 2 NEWEST");
            Act::Continue
        });
    let r = pi.pull_from_peer(&EnginePeer::new(&tower, "pi"), ts(20));
    println!("pull: {r:?}");
    assert!(pi.fs().all_fired());
    println!("pi before restart:\n{}", dump(&t.join("pi")));
    drop(pi);
    let pi = open_hook(&t.join("pi"), &t.join("pi.redb"), "pi", &["tower", "pi"]);
    println!("pi after restart:\n{}", dump(&t.join("pi")));
    let w1 = where_is(&t.join("pi"), b"pi save 1 (in place)");
    drop(pi);
    assert!(
        !w1.is_empty(),
        "save 1 was deleted by the restart's tmp sweep"
    );
}

/// Crash after a remote deletion claimed the file; the person writes a new file of that name
/// before the runner restarts.
#[test]
fn r5_crash_after_delete_claim_then_new_file() {
    let tmp = scratch();
    let t = tmp.path();
    let (mut tower, mut pi) = two_members(t);
    fs::remove_file(t.join("tower/doc.txt")).unwrap();
    tower.scan(ts(10)).unwrap();

    pi.fs()
        .add("move_live_to_tmp_no_replace", When::After, 1, || Act::Crash);
    let r = pi.pull_from_peer(&EnginePeer::new(&tower, "pi"), ts(20));
    println!("pull: {r:?}");
    assert!(pi.fs().all_fired());
    drop(pi);
    fs::write(t.join("pi/doc.txt"), b"new doc by the person").unwrap();
    let pi = open_hook(&t.join("pi"), &t.join("pi.redb"), "pi", &["tower", "pi"]);
    println!("pi after restart:\n{}", dump(&t.join("pi")));
    let w = where_is(&t.join("pi"), b"v1 original");
    drop(pi);
    assert!(
        !w.is_empty(),
        "the deleted file never reached the bin: erased"
    );
}

/// Crash after the exchange placed the remote version (old version still in tmp); the person
/// edits the file before the runner restarts.
#[test]
fn r6_crash_after_exchange_then_edit() {
    let tmp = scratch();
    let t = tmp.path();
    let (mut tower, mut pi) = two_members(t);
    fs::write(t.join("tower/doc.txt"), b"tower edit v2").unwrap();
    tower.scan(ts(10)).unwrap();

    pi.fs()
        .add(ATOMIC_REPLACE_OP, When::After, 1, || Act::Crash);
    let r = pi.pull_from_peer(&EnginePeer::new(&tower, "pi"), ts(20));
    println!("pull: {r:?}");
    assert!(pi.fs().all_fired());
    drop(pi);
    fs::write(t.join("pi/doc.txt"), b"tower edit v2 + person's line").unwrap();
    let pi = open_hook(&t.join("pi"), &t.join("pi.redb"), "pi", &["tower", "pi"]);
    println!("pi after restart:\n{}", dump(&t.join("pi")));
    let w = where_is(&t.join("pi"), b"v1 original");
    drop(pi);
    assert!(
        !w.is_empty(),
        "pi's previous version never reached the bin: erased"
    );
}

/// Same as r1 on a file system without atomic exchange (claim-then-rename fallback): the person
/// edits in place just before the claim and saves again (rename-over) right after it.
#[test]
fn r7_fallback_claim_putback_fails() {
    let tmp = scratch();
    let t = tmp.path();
    let (mut tower, mut pi) = two_members(t);
    pi.fs_mut().no_exchange = true;
    pi.fs_mut().no_replace_backup = true;
    fs::write(t.join("tower/doc.txt"), b"tower edit v2").unwrap();
    tower.scan(ts(10)).unwrap();
    let p1 = t.join("pi/doc.txt");
    pi.fs()
        .add("move_live_to_tmp_no_replace", When::Before, 1, move || {
            fs::write(&p1, b"pi save 1 (in place)").unwrap();
            Act::Continue
        });
    let p2 = t.join("pi/doc.txt");
    pi.fs()
        .add("move_live_to_tmp_no_replace", When::After, 1, move || {
            write_via_rename(&p2, b"pi save 2 NEWEST");
            Act::Continue
        });
    let r = pi.pull_from_peer(&EnginePeer::new(&tower, "pi"), ts(20));
    println!("pull: {r:?}");
    assert!(pi.fs().all_fired());
    pi.fs().clear();
    println!("pi after:\n{}", dump(&t.join("pi")));
    let w1 = where_is(&t.join("pi"), b"pi save 1 (in place)");
    assert!(!w1.is_empty(), "save 1 erased");
}

/// A person saving the previous version's bytes (e.g. hitting Undo or renaming a conflict copy
/// back into place) right after sync displaced the old file must bump the version so both
/// versions survive live instead of the remote edit silently clobbering the person's save.
#[test]
fn r8_same_hash_undo_after_displace_survives_live() {
    let tmp = scratch();
    let t = tmp.path();
    let (mut tower, mut pi) = two_members(t);
    pi.fs_mut().no_exchange = true;
    pi.fs_mut().no_replace_backup = true;
    fs::write(t.join("tower/doc.txt"), b"tower edit v2").unwrap();
    tower.scan(ts(10)).unwrap();

    // Right after pi claims & removes `doc.txt` ("v1 original") during claim-then-rename, the
    // person puts "v1 original" back into `doc.txt` (identical hash to the displaced file).
    let p = t.join("pi/doc.txt");
    pi.fs()
        .add("move_live_to_tmp_no_replace", When::After, 1, move || {
            write_via_rename(&p, b"v1 original");
            Act::Continue
        });
    pi.pull_from_peer(&EnginePeer::new(&tower, "pi"), ts(20))
        .unwrap();
    assert!(pi.fs().all_fired());
    pi.fs().clear();

    // Converge both directions; BOTH "v1 original" (person's post-displace save) AND
    // "tower edit v2" must survive LIVE on both machines!
    for step in 0..3 {
        pi.scan(ts(30 + step)).unwrap();
        tower.scan(ts(30 + step)).unwrap();
        tower
            .pull_from_peer(&EnginePeer::new(&pi, "tower"), ts(30 + step))
            .unwrap();
        pi.pull_from_peer(&EnginePeer::new(&tower, "pi"), ts(30 + step))
            .unwrap();
    }
    let live_v1_pi: Vec<_> = where_is(&t.join("pi"), b"v1 original")
        .into_iter()
        .filter(|p| !p.starts_with(".legion-sync"))
        .collect();
    let live_v2_pi: Vec<_> = where_is(&t.join("pi"), b"tower edit v2")
        .into_iter()
        .filter(|p| !p.starts_with(".legion-sync"))
        .collect();
    let live_v1_tower: Vec<_> = where_is(&t.join("tower"), b"v1 original")
        .into_iter()
        .filter(|p| !p.starts_with(".legion-sync"))
        .collect();
    assert!(
        !live_v1_pi.is_empty(),
        "person's post-displace v1 save lost on pi"
    );
    assert!(!live_v2_pi.is_empty(), "tower edit v2 lost on pi");
    assert!(
        !live_v1_tower.is_empty(),
        "person's post-displace v1 save never reached tower"
    );
}

fn configure_simulated_windows_replace(
    pi: &mut legion_sync::SyncEngine<HookFs, legion_sync::RedbIndexStore>,
) {
    pi.fs().clear();
    pi.fs_mut().no_exchange = true;
    pi.fs_mut().simulate_replace_with_backup = true;
    pi.fs_mut().capabilities_override = Some(FsCapabilities::windows());
}

/// Exercises Path 2 (`replace_live_with_tmp_backup`, Windows `ReplaceFileW`) on Linux too:
/// r1 (two saves around ReplaceFileW) and r6 (crash after ReplaceFileW + user edit before restart).
#[test]
fn w1_replace_with_backup_races_and_crash_simulated() {
    // Subcase A: two saves around replace_live_with_tmp_backup.
    {
        let tmp = scratch();
        let t = tmp.path();
        let (mut tower, mut pi) = two_members(t);
        configure_simulated_windows_replace(&mut pi);
        fs::write(t.join("tower/doc.txt"), b"tower edit v2").unwrap();
        tower.scan(ts(10)).unwrap();

        let p1 = t.join("pi/doc.txt");
        pi.fs()
            .add("replace_live_with_tmp_backup", When::Before, 1, move || {
                fs::write(&p1, b"pi save 1 (in place)").unwrap();
                Act::Continue
            });
        let p2 = t.join("pi/doc.txt");
        pi.fs()
            .add("replace_live_with_tmp_backup", When::After, 1, move || {
                write_via_rename(&p2, b"pi save 2 NEWEST");
                Act::Continue
            });
        pi.pull_from_peer(&EnginePeer::new(&tower, "pi"), ts(20))
            .unwrap();
        assert!(pi.fs().all_fired());
        pi.fs().clear();
        pi.scan(ts(30)).unwrap();
        assert!(
            !where_is(&t.join("pi"), b"pi save 1 (in place)").is_empty(),
            "save 1 lost under ReplaceFileW path"
        );
        assert!(
            !where_is(&t.join("pi"), b"pi save 2 NEWEST").is_empty(),
            "save 2 lost under ReplaceFileW path"
        );
    }

    // Subcase B: crash right after replace_live_with_tmp_backup + person edits doc.txt before restart.
    {
        let tmp = scratch();
        let t = tmp.path();
        let (mut tower, mut pi) = two_members(t);
        configure_simulated_windows_replace(&mut pi);
        fs::write(t.join("tower/doc.txt"), b"tower edit v2").unwrap();
        tower.scan(ts(10)).unwrap();

        pi.fs()
            .add("replace_live_with_tmp_backup", When::After, 1, || {
                Act::Crash
            });
        let r = pi.pull_from_peer(&EnginePeer::new(&tower, "pi"), ts(20));
        assert!(r.is_err());
        assert!(pi.fs().all_fired());
        drop(pi);

        fs::write(t.join("pi/doc.txt"), b"tower edit v2 + person's line").unwrap();
        let pi = open_hook(&t.join("pi"), &t.join("pi.redb"), "pi", &["tower", "pi"]);
        assert!(
            !where_is(&t.join("pi"), b"v1 original").is_empty(),
            "v1 original never reached bin after ReplaceFileW crash"
        );
        assert_eq!(
            fs::read(t.join("pi/doc.txt")).unwrap(),
            b"tower edit v2 + person's line"
        );
        drop(pi);
    }
}

/// Simulates Windows `ReplaceFileW` `ERROR_UNABLE_TO_REMOVE_REPLACED` (1175): target and staged
/// file remain untouched, the locked file defers cleanly without aborting the page, other files
/// in the same pull still arrive, and the next pull converges once unlocked.
#[test]
fn w2_replace_unable_to_remove_replaced_1175_defers_without_blocking_page() {
    let tmp = scratch();
    let t = tmp.path();
    let (mut tower, mut pi) = two_members(t);
    configure_simulated_windows_replace(&mut pi);

    fs::write(t.join("tower/doc.txt"), b"tower edit v2").unwrap();
    fs::write(t.join("tower/other.txt"), b"other file in same page").unwrap();
    tower.scan(ts(10)).unwrap();

    let locked = Arc::new(AtomicBool::new(true));
    let locked_cb = Arc::clone(&locked);
    pi.fs()
        .set_replace_backup_override(move |f, staged, live, backup| {
            if live == "doc.txt" && locked_cb.load(Ordering::Relaxed) {
                return Err(SyncError::Fs {
                    path: Some(live.to_string()),
                    detail: "simulated ERROR_UNABLE_TO_REMOVE_REPLACED (1175)".to_string(),
                    kind: FsErrorKind::ReplaceUnableToRemoveReplaced,
                });
            }
            f.move_live_to_tmp_no_replace(live, backup)?;
            f.commit_tmp_to_live_no_replace(staged, live)?;
            Ok(true)
        });

    let report = pi
        .pull_from_peer(&EnginePeer::new(&tower, "pi"), ts(20))
        .unwrap();
    assert!(
        report.deferred >= 1,
        "doc.txt must defer on 1175: {report:?}"
    );
    assert_eq!(
        fs::read(t.join("pi/other.txt")).unwrap(),
        b"other file in same page",
        "other files in the same pull page must still arrive"
    );
    assert_eq!(fs::read(t.join("pi/doc.txt")).unwrap(), b"v1 original");
    assert!(pi.store().load_pending_op().unwrap().is_none());
    let defs = pi.store().list_deferrals().unwrap();
    assert!(
        defs.iter().any(|d| d.path == "doc.txt"),
        "expected visible deferral for doc.txt: {defs:?}"
    );

    locked.store(false, Ordering::Relaxed);
    pi.pull_from_peer(&EnginePeer::new(&tower, "pi"), ts(30))
        .unwrap();
    assert_eq!(fs::read(t.join("pi/doc.txt")).unwrap(), b"tower edit v2");
    assert!(pi.store().list_deferrals().unwrap().is_empty());
    assert!(!where_is(&t.join("pi"), b"v1 original").is_empty());
}

/// Simulates Windows `ReplaceFileW` `ERROR_UNABLE_TO_MOVE_REPLACEMENT` (1176) in both subcases:
/// (a) Windows rolled backup back to live path, and (b) backup was still left in `.legion-sync/tmp/`.
#[test]
fn w3_replace_unable_to_move_replacement_1176_rolled_back_and_leftover_backup() {
    for leave_backup_stranded in [false, true] {
        let tmp = scratch();
        let t = tmp.path();
        let (mut tower, mut pi) = two_members(t);
        configure_simulated_windows_replace(&mut pi);

        fs::write(t.join("tower/doc.txt"), b"tower edit v2").unwrap();
        fs::write(t.join("tower/other.txt"), b"other arrives").unwrap();
        tower.scan(ts(10)).unwrap();

        let inject = Arc::new(AtomicBool::new(true));
        let inject_cb = Arc::clone(&inject);
        pi.fs()
            .set_replace_backup_override(move |f, staged, live, backup| {
                if live == "doc.txt" && inject_cb.swap(false, Ordering::Relaxed) {
                    if leave_backup_stranded {
                        f.move_live_to_tmp_no_replace(live, backup)?;
                    }
                    return Err(SyncError::Fs {
                        path: Some(live.to_string()),
                        detail: "simulated ERROR_UNABLE_TO_MOVE_REPLACEMENT (1176)".to_string(),
                        kind: FsErrorKind::ReplaceUnableToMoveReplacement,
                    });
                }
                f.move_live_to_tmp_no_replace(live, backup)?;
                f.commit_tmp_to_live_no_replace(staged, live)?;
                Ok(true)
            });

        let report = pi
            .pull_from_peer(&EnginePeer::new(&tower, "pi"), ts(20))
            .unwrap();
        assert!(report.deferred >= 1);
        assert_eq!(fs::read(t.join("pi/other.txt")).unwrap(), b"other arrives");
        assert_eq!(
            fs::read(t.join("pi/doc.txt")).unwrap(),
            b"v1 original",
            "doc.txt must remain intact after 1176 (leave_backup_stranded={leave_backup_stranded})"
        );
        assert!(pi.store().load_pending_op().unwrap().is_none());

        pi.pull_from_peer(&EnginePeer::new(&tower, "pi"), ts(30))
            .unwrap();
        assert_eq!(fs::read(t.join("pi/doc.txt")).unwrap(), b"tower edit v2");
        assert!(pi.store().list_deferrals().unwrap().is_empty());
    }
}

/// Simulates Windows `ReplaceFileW` `ERROR_UNABLE_TO_MOVE_REPLACEMENT_2` (1177), where Windows
/// moved `live_path` into `backup` (`claim_tmp`), left `staged_tmp` in place, and failed rollback
/// leaving `live_path` absent on disk:
/// (a) displaced file matches `expected_local` -> restored straight back to `live_path` and deferred;
/// (b) displaced file was an unseen user edit -> restored back to `live_path`, rescanned, both
///     versions survive live;
/// (c) concurrent new file appeared at `live_path` after 1177 -> displaced unseen edit preserved
///     as conflict copy while the new file stays live;
/// (d) crash immediately after 1177 -> restart recovery restores without losing a byte.
#[test]
fn w4_replace_unable_to_move_replacement_2_1177_all_recovery_cases() {
    // Case (a): expected_local moved to backup, target absent -> put back and defer cleanly.
    {
        let tmp = scratch();
        let t = tmp.path();
        let (mut tower, mut pi) = two_members(t);
        configure_simulated_windows_replace(&mut pi);

        fs::write(t.join("tower/doc.txt"), b"tower edit v2").unwrap();
        fs::write(t.join("tower/other.txt"), b"other arrives").unwrap();
        tower.scan(ts(10)).unwrap();

        let inject = Arc::new(AtomicBool::new(true));
        let inject_cb = Arc::clone(&inject);
        pi.fs()
            .set_replace_backup_override(move |f, staged, live, backup| {
                if live == "doc.txt" && inject_cb.swap(false, Ordering::Relaxed) {
                    f.move_live_to_tmp_no_replace(live, backup)?;
                    return Err(SyncError::Fs {
                        path: Some(live.to_string()),
                        detail: "simulated ERROR_UNABLE_TO_MOVE_REPLACEMENT_2 (1177)".to_string(),
                        kind: FsErrorKind::ReplaceUnableToMoveReplacement2,
                    });
                }
                f.move_live_to_tmp_no_replace(live, backup)?;
                f.commit_tmp_to_live_no_replace(staged, live)?;
                Ok(true)
            });

        let report = pi
            .pull_from_peer(&EnginePeer::new(&tower, "pi"), ts(20))
            .unwrap();
        assert!(report.deferred >= 1);
        assert_eq!(fs::read(t.join("pi/other.txt")).unwrap(), b"other arrives");
        assert_eq!(
            fs::read(t.join("pi/doc.txt")).unwrap(),
            b"v1 original",
            "1177 must restore v1 original to live path instead of leaving it missing"
        );
        assert!(pi.store().load_pending_op().unwrap().is_none());

        pi.pull_from_peer(&EnginePeer::new(&tower, "pi"), ts(30))
            .unwrap();
        assert_eq!(fs::read(t.join("pi/doc.txt")).unwrap(), b"tower edit v2");
        assert!(!where_is(&t.join("pi"), b"v1 original").is_empty());
    }

    // Case (b): unseen user edit right before 1177 -> restored to live_path, rescanned, and both
    // unseen edit and tower's v2 survive live as a conflict!
    {
        let tmp = scratch();
        let t = tmp.path();
        let (mut tower, mut pi) = two_members(t);
        configure_simulated_windows_replace(&mut pi);

        fs::write(t.join("tower/doc.txt"), b"tower edit v2").unwrap();
        tower.scan(ts(10)).unwrap();

        let p_edit = t.join("pi/doc.txt");
        let inject = Arc::new(AtomicBool::new(true));
        let inject_cb = Arc::clone(&inject);
        pi.fs()
            .set_replace_backup_override(move |f, staged, live, backup| {
                if live == "doc.txt" && inject_cb.swap(false, Ordering::Relaxed) {
                    fs::write(&p_edit, b"pi unseen save before 1177").unwrap();
                    f.move_live_to_tmp_no_replace(live, backup)?;
                    return Err(SyncError::Fs {
                        path: Some(live.to_string()),
                        detail: "simulated ERROR_UNABLE_TO_MOVE_REPLACEMENT_2 (1177)".to_string(),
                        kind: FsErrorKind::ReplaceUnableToMoveReplacement2,
                    });
                }
                f.move_live_to_tmp_no_replace(live, backup)?;
                f.commit_tmp_to_live_no_replace(staged, live)?;
                Ok(true)
            });

        pi.pull_from_peer(&EnginePeer::new(&tower, "pi"), ts(20))
            .unwrap();
        pi.scan(ts(30)).unwrap();
        let live_unseen: Vec<_> = where_is(&t.join("pi"), b"pi unseen save before 1177")
            .into_iter()
            .filter(|p| !p.starts_with(".legion-sync"))
            .collect();
        let live_remote: Vec<_> = where_is(&t.join("pi"), b"tower edit v2")
            .into_iter()
            .filter(|p| !p.starts_with(".legion-sync"))
            .collect();
        assert!(!live_unseen.is_empty(), "unseen user edit lost on 1177");
        assert!(!live_remote.is_empty(), "tower edit v2 lost after 1177");
    }

    // Case (c): unseen user edit moved to backup by 1177 AND a second concurrent save creates a
    // brand-new file at live_path before recovery put-back runs -> BOTH saves survive live!
    {
        let tmp = scratch();
        let t = tmp.path();
        let (mut tower, mut pi) = two_members(t);
        configure_simulated_windows_replace(&mut pi);

        fs::write(t.join("tower/doc.txt"), b"tower edit v2").unwrap();
        tower.scan(ts(10)).unwrap();

        let p_live = t.join("pi/doc.txt");
        let inject = Arc::new(AtomicBool::new(true));
        let inject_cb = Arc::clone(&inject);
        pi.fs()
            .set_replace_backup_override(move |f, staged, live, backup| {
                if live == "doc.txt" && inject_cb.swap(false, Ordering::Relaxed) {
                    fs::write(&p_live, b"pi unseen save 1").unwrap();
                    f.move_live_to_tmp_no_replace(live, backup)?;
                    fs::write(&p_live, b"pi brand new save 2").unwrap();
                    return Err(SyncError::Fs {
                        path: Some(live.to_string()),
                        detail: "simulated ERROR_UNABLE_TO_MOVE_REPLACEMENT_2 (1177)".to_string(),
                        kind: FsErrorKind::ReplaceUnableToMoveReplacement2,
                    });
                }
                f.move_live_to_tmp_no_replace(live, backup)?;
                f.commit_tmp_to_live_no_replace(staged, live)?;
                Ok(true)
            });

        pi.pull_from_peer(&EnginePeer::new(&tower, "pi"), ts(20))
            .unwrap();
        pi.scan(ts(30)).unwrap();
        assert!(
            !where_is(&t.join("pi"), b"pi unseen save 1")
                .into_iter()
                .filter(|p| !p.starts_with(".legion-sync"))
                .collect::<Vec<_>>()
                .is_empty(),
            "save 1 in 1177 backup lost when live_path was recreated"
        );
        assert!(
            !where_is(&t.join("pi"), b"pi brand new save 2")
                .into_iter()
                .filter(|p| !p.starts_with(".legion-sync"))
                .collect::<Vec<_>>()
                .is_empty(),
            "save 2 at live_path lost after 1177"
        );
    }

    // Case (d): crash immediately after 1177 moved target to backup -> restart recovery finishes
    // cleanly without losing any bytes!
    {
        let tmp = scratch();
        let t = tmp.path();
        let (mut tower, mut pi) = two_members(t);
        configure_simulated_windows_replace(&mut pi);

        fs::write(t.join("tower/doc.txt"), b"tower edit v2").unwrap();
        tower.scan(ts(10)).unwrap();

        let p_live = t.join("pi/doc.txt");
        pi.fs()
            .set_replace_backup_override(move |f, _staged, live, backup| {
                fs::write(&p_live, b"pi unseen edit before 1177 crash").unwrap();
                f.move_live_to_tmp_no_replace(live, backup)?;
                Err(SyncError::Fs {
                    path: Some(live.to_string()),
                    detail: "crash right after 1177 vacated live_path".to_string(),
                    kind: FsErrorKind::Crash,
                })
            });

        assert!(
            pi.pull_from_peer(&EnginePeer::new(&tower, "pi"), ts(20))
                .is_err()
        );
        drop(pi);

        let mut pi = open_hook(&t.join("pi"), &t.join("pi.redb"), "pi", &["tower", "pi"]);
        assert_eq!(
            fs::read(t.join("pi/doc.txt")).unwrap(),
            b"pi unseen edit before 1177 crash"
        );
        pi.scan(ts(30)).unwrap();
        pi.pull_from_peer(&EnginePeer::new(&tower, "pi"), ts(35))
            .unwrap();
        assert!(
            !where_is(&t.join("pi"), b"pi unseen edit before 1177 crash")
                .into_iter()
                .filter(|p| !p.starts_with(".legion-sync"))
                .collect::<Vec<_>>()
                .is_empty()
        );
        assert!(
            !where_is(&t.join("pi"), b"tower edit v2")
                .into_iter()
                .filter(|p| !p.starts_with(".legion-sync"))
                .collect::<Vec<_>>()
                .is_empty()
        );
    }
}

/// Simulates Windows sharing violations (`FsErrorKind::Busy`) and delete-pending errors across
/// delete, local rename, group-commit creation, and put-back of a claimed unseen save:
/// locked files defer cleanly with visible deferral records without blocking other files in the
/// same page, never lose bytes, and converge once unlocked.
#[test]
fn w5_sharing_violation_and_delete_pending_defer_without_loss() {
    // Subcase A: sharing violation on delete defers only the open file while sibling files sync.
    {
        let tmp = scratch();
        let t = tmp.path();
        let (mut tower, mut pi) = two_members(t);
        configure_simulated_windows_replace(&mut pi);

        fs::remove_file(t.join("tower/doc.txt")).unwrap();
        fs::write(t.join("tower/new.txt"), b"brand new sibling").unwrap();
        tower.scan(ts(10)).unwrap();

        pi.fs()
            .add("move_live_to_tmp_no_replace", When::Before, 1, || {
                Act::Err(FsErrorKind::Busy)
            });

        let report = pi
            .pull_from_peer(&EnginePeer::new(&tower, "pi"), ts(20))
            .unwrap();
        assert!(pi.fs().all_fired());
        assert!(report.deferred >= 1);
        assert_eq!(
            fs::read(t.join("pi/new.txt")).unwrap(),
            b"brand new sibling"
        );
        assert_eq!(fs::read(t.join("pi/doc.txt")).unwrap(), b"v1 original");
        assert!(pi.store().load_pending_op().unwrap().is_none());
        assert!(
            pi.store()
                .list_deferrals()
                .unwrap()
                .iter()
                .any(|d| d.path == "doc.txt")
        );

        pi.fs().clear();
        pi.pull_from_peer(&EnginePeer::new(&tower, "pi"), ts(30))
            .unwrap();
        assert!(!t.join("pi/doc.txt").exists());
        assert!(!where_is(&t.join("pi"), b"v1 original").is_empty());
        assert!(pi.store().list_deferrals().unwrap().is_empty());
    }

    // Subcase B: delete-pending (`PermissionDenied` on Windows) during group-commit creation of
    // one file defers only that file while the rest of the chunk commits cleanly.
    {
        let tmp = scratch();
        let t = tmp.path();
        let (mut tower, mut pi) = two_members(t);
        configure_simulated_windows_replace(&mut pi);

        fs::write(t.join("tower/a.txt"), b"file a").unwrap();
        fs::write(t.join("tower/b.txt"), b"file b").unwrap();
        tower.scan(ts(10)).unwrap();

        pi.fs().add(
            "commit_tmp_to_live_no_replace_deferred",
            When::Before,
            1,
            || Act::Err(FsErrorKind::PermissionDenied),
        );

        let report = pi
            .pull_from_peer(&EnginePeer::new(&tower, "pi"), ts(20))
            .unwrap();
        assert!(pi.fs().all_fired());
        assert!(report.deferred >= 1);
        let arrived_count =
            usize::from(t.join("pi/a.txt").exists()) + usize::from(t.join("pi/b.txt").exists());
        assert_eq!(arrived_count, 1, "one file deferred, sibling must commit");

        pi.fs().clear();
        pi.pull_from_peer(&EnginePeer::new(&tower, "pi"), ts(30))
            .unwrap();
        assert_eq!(fs::read(t.join("pi/a.txt")).unwrap(), b"file a");
        assert_eq!(fs::read(t.join("pi/b.txt")).unwrap(), b"file b");
        assert!(pi.store().list_deferrals().unwrap().is_empty());
    }

    // Subcase C: put-back of a claimed unseen edit hits `Busy` (Windows delete-pending) ->
    // `put_back_or_preserve_tmp` preserves the unseen save as a live conflict copy (zero loss!).
    {
        let tmp = scratch();
        let t = tmp.path();
        let (mut tower, mut pi) = two_members(t);
        pi.fs_mut().no_exchange = true;
        pi.fs_mut().no_replace_backup = true;
        pi.fs_mut().capabilities_override = Some(FsCapabilities::windows());

        fs::remove_file(t.join("tower/doc.txt")).unwrap();
        tower.scan(ts(10)).unwrap();

        let p = t.join("pi/doc.txt");
        pi.fs()
            .add("move_live_to_tmp_no_replace", When::Before, 1, move || {
                fs::write(&p, b"unseen save whose putback hits busy").unwrap();
                Act::Continue
            });
        pi.fs()
            .add("commit_tmp_to_live_no_replace", When::Before, 1, || {
                Act::Err(FsErrorKind::Busy)
            });

        pi.pull_from_peer(&EnginePeer::new(&tower, "pi"), ts(20))
            .unwrap();
        assert!(pi.fs().all_fired());
        pi.fs().clear();
        let live_saved: Vec<_> = where_is(&t.join("pi"), b"unseen save whose putback hits busy")
            .into_iter()
            .filter(|p| !p.starts_with(".legion-sync"))
            .collect();
        assert!(
            !live_saved.is_empty(),
            "claimed unseen edit must survive as a live conflict copy when put-back hits Busy"
        );
    }
}

/// Real Windows NTFS check: an exclusively locked file handle (`share_mode(0)`) causes replace
/// and delete to defer cleanly without blocking sibling files in the same pull page, and
/// converges as soon as the handle is closed.
#[cfg(windows)]
#[test]
fn w6_real_windows_exclusive_handle_defers_and_converges() {
    use std::os::windows::fs::OpenOptionsExt;

    let tmp = scratch();
    let t = tmp.path();
    let (mut tower, mut pi) = two_members(t);

    fs::write(t.join("tower/doc.txt"), b"tower edit v2").unwrap();
    fs::write(
        t.join("tower/sibling.txt"),
        b"sibling arrives while doc locked",
    )
    .unwrap();
    tower.scan(ts(10)).unwrap();

    // Hold doc.txt open exclusively (no read/write/delete sharing), matching Word/Excel locks.
    let lock = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .share_mode(0)
        .open(t.join("pi/doc.txt"))
        .unwrap();

    let report = pi
        .pull_from_peer(&EnginePeer::new(&tower, "pi"), ts(20))
        .unwrap();
    assert!(
        report.deferred >= 1,
        "locked doc.txt must defer: {report:?}"
    );
    assert_eq!(
        fs::read(t.join("pi/sibling.txt")).unwrap(),
        b"sibling arrives while doc locked"
    );
    assert!(pi.store().load_pending_op().unwrap().is_none());

    drop(lock);
    pi.pull_from_peer(&EnginePeer::new(&tower, "pi"), ts(30))
        .unwrap();
    assert_eq!(fs::read(t.join("pi/doc.txt")).unwrap(), b"tower edit v2");
    assert!(pi.store().list_deferrals().unwrap().is_empty());
    assert!(!where_is(&t.join("pi"), b"v1 original").is_empty());

    // Now lock doc.txt exclusively again during a remote deletion!
    fs::remove_file(t.join("tower/doc.txt")).unwrap();
    fs::write(
        t.join("tower/sibling2.txt"),
        b"sibling2 during locked delete",
    )
    .unwrap();
    tower.scan(ts(40)).unwrap();

    let lock2 = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .share_mode(0)
        .open(t.join("pi/doc.txt"))
        .unwrap();

    let report2 = pi
        .pull_from_peer(&EnginePeer::new(&tower, "pi"), ts(50))
        .unwrap();
    assert!(
        report2.deferred >= 1,
        "locked delete must defer: {report2:?}"
    );
    assert_eq!(
        fs::read(t.join("pi/sibling2.txt")).unwrap(),
        b"sibling2 during locked delete"
    );

    drop(lock2);
    pi.pull_from_peer(&EnginePeer::new(&tower, "pi"), ts(60))
        .unwrap();
    assert!(!t.join("pi/doc.txt").exists());
    assert!(pi.store().list_deferrals().unwrap().is_empty());
    assert!(!where_is(&t.join("pi"), b"tower edit v2").is_empty());
}
