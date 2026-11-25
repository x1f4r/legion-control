//! Real-disk safety regression tests: a person's programs interfering with apply on a
//! real btrfs directory, at exact operation boundaries.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod safety_common;

use std::fs;

use legion_sync::IndexStore;

use safety_common::*;

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
    pi.fs()
        .add("exchange_tmp_with_live", When::Before, 1, move || {
            fs::write(&p1, b"pi save 1 (in place)").unwrap();
            Act::Continue
        });
    let p2 = t.join("pi/doc.txt");
    pi.fs()
        .add("exchange_tmp_with_live", When::After, 1, move || {
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
    pi.fs()
        .add("exchange_tmp_with_live", When::Before, 1, move || {
            fs::write(&p1, b"pi save 1 (in place)").unwrap();
            Act::Continue
        });
    let p2 = t.join("pi/doc.txt");
    let p2b = t.join("pi/moved.txt");
    pi.fs()
        .add("exchange_tmp_with_live", When::After, 1, move || {
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
    pi.fs()
        .add("exchange_tmp_with_live", When::After, 1, move || {
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
        .add("exchange_tmp_with_live", When::After, 1, || Act::Crash);
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
