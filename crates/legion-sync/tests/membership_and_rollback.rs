//! Real-disk safety regression tests: membership changes that reach members at
//! different times, on real directories.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod safety_common;

use std::fs;

use legion_sync::IndexStore;
use safety_common::*;

/// pi has not yet heard that nas joined; it pulls records nas touched from tower.
#[test]
fn m1_member_learned_late_never_receives_files() {
    let tmp = scratch();
    let t = tmp.path();
    let all = ["tower", "pi", "nas"];
    let mut tower = open_real(&t.join("tower"), &t.join("tower.redb"), "tower", &all);
    let mut nas = open_real(&t.join("nas"), &t.join("nas.redb"), "nas", &all);
    let mut pi = open_real(&t.join("pi"), &t.join("pi.redb"), "pi", &["tower", "pi"]);

    fs::create_dir_all(t.join("nas")).unwrap();
    fs::write(t.join("nas/photo.jpg"), b"nas photo").unwrap();
    nas.scan(ts(0)).unwrap();
    tower
        .pull_from_peer(&EnginePeer::new(&nas, "tower"), ts(1))
        .unwrap();
    assert!(t.join("tower/photo.jpg").exists());

    let r = pi
        .pull_from_peer(&EnginePeer::new(&tower, "pi"), ts(2))
        .unwrap();
    println!("pi pull before learning nas: {r:?}");
    pi.add_member("nas").unwrap();
    let r = pi
        .pull_from_peer(&EnginePeer::new(&tower, "pi"), ts(3))
        .unwrap();
    println!("pi pull after learning nas: {r:?}");
    let st = pi.status().unwrap();
    println!(
        "pi status problems: {:?}",
        (
            &st.state,
            st.members
                .iter()
                .map(|m| format!("{:?}", m))
                .collect::<Vec<_>>()
        )
    );
    drop(pi);
    let mut pi = open_real(
        &t.join("pi"),
        &t.join("pi.redb"),
        "pi",
        &["tower", "pi", "nas"],
    );
    let r = pi
        .pull_from_peer(&EnginePeer::new(&tower, "pi"), ts(4))
        .unwrap();
    println!("pi pull after restart: {r:?}");
    let st = pi.status().unwrap();
    println!(
        "pi status problems after restart: {:?}",
        (
            &st.state,
            st.members
                .iter()
                .map(|m| format!("{:?}", m))
                .collect::<Vec<_>>()
        )
    );
    assert!(
        t.join("pi/photo.jpg").exists(),
        "pi never receives nas's file from tower"
    );
}

/// A deletion pi skipped (version names a member pi did not know yet) is acknowledged anyway,
/// tower forgets it, and a later member brings the file back to everyone.
#[test]
fn m2_skipped_deletion_acknowledged_then_resurrected() {
    let tmp = scratch();
    let t = tmp.path();
    let all = ["tower", "pi", "nas"];
    let mut tower = open_real(&t.join("tower"), &t.join("tower.redb"), "tower", &all);
    let mut nas = open_real(&t.join("nas"), &t.join("nas.redb"), "nas", &all);
    let mut pi = open_real(&t.join("pi"), &t.join("pi.redb"), "pi", &["tower", "pi"]);

    fs::create_dir_all(t.join("tower")).unwrap();
    fs::write(t.join("tower/report.txt"), b"report v1").unwrap();
    tower.scan(ts(0)).unwrap();
    pi.pull_from_peer(&EnginePeer::new(&tower, "pi"), ts(1))
        .unwrap();
    nas.pull_from_peer(&EnginePeer::new(&tower, "nas"), ts(1))
        .unwrap();
    // nas edits; tower takes it; then tower deletes the file.
    fs::write(t.join("nas/report.txt"), b"report v2 by nas").unwrap();
    nas.scan(ts(10)).unwrap();
    tower
        .pull_from_peer(&EnginePeer::new(&nas, "tower"), ts(11))
        .unwrap();
    fs::remove_file(t.join("tower/report.txt")).unwrap();
    tower.scan(ts(20)).unwrap();
    let r = pi
        .pull_from_peer(&EnginePeer::new(&tower, "pi"), ts(21))
        .unwrap();
    println!("pi pull of the deletion: {r:?}");
    nas.pull_from_peer(&EnginePeer::new(&tower, "nas"), ts(21))
        .unwrap();
    // Exchange acknowledgements.
    tower
        .pull_from_peer(&EnginePeer::new(&pi, "tower"), ts(22))
        .unwrap();
    tower
        .pull_from_peer(&EnginePeer::new(&nas, "tower"), ts(22))
        .unwrap();
    pi.pull_from_peer(&EnginePeer::new(&tower, "pi"), ts(23))
        .unwrap();
    nas.pull_from_peer(&EnginePeer::new(&tower, "nas"), ts(23))
        .unwrap();
    let r = tower
        .pull_from_peer(&EnginePeer::new(&pi, "tower"), ts(24))
        .unwrap();
    let r2 = tower
        .pull_from_peer(&EnginePeer::new(&nas, "tower"), ts(24))
        .unwrap();
    println!("tower gc: {} {}", r.tombstones_gc, r2.tombstones_gc);
    let tomb = tower.store().get_record("report.txt").unwrap();
    println!("tower record for report.txt after acks: {tomb:?}");
    println!(
        "pi still has: {:?}",
        fs::read(t.join("pi/report.txt"))
            .ok()
            .map(|b| String::from_utf8_lossy(&b).into_owned())
    );

    // pi learns nas; later a new laptop joins everyone and first meets pi.
    pi.add_member("nas").unwrap();
    for e in [&mut tower, &mut nas] {
        e.add_member("laptop").unwrap();
    }
    pi.add_member("laptop").unwrap();
    let mut laptop = open_real(
        &t.join("laptop"),
        &t.join("laptop.redb"),
        "laptop",
        &["tower", "pi", "nas", "laptop"],
    );
    laptop
        .pull_from_peer(&EnginePeer::new(&pi, "laptop"), ts(30))
        .unwrap();
    tower
        .pull_from_peer(&EnginePeer::new(&laptop, "tower"), ts(31))
        .unwrap();
    nas.pull_from_peer(&EnginePeer::new(&tower, "nas"), ts(32))
        .unwrap();
    println!(
        "tower: {:?} nas: {:?} laptop: {:?}",
        fs::read(t.join("tower/report.txt"))
            .ok()
            .map(|b| String::from_utf8_lossy(&b).into_owned()),
        fs::read(t.join("nas/report.txt"))
            .ok()
            .map(|b| String::from_utf8_lossy(&b).into_owned()),
        fs::read(t.join("laptop/report.txt"))
            .ok()
            .map(|b| String::from_utf8_lossy(&b).into_owned()),
    );
    assert!(
        !t.join("tower/report.txt").exists(),
        "deleted report.txt came back on tower"
    );
}

fn copy_tree(src: &std::path::Path, dst: &std::path::Path) {
    let st = std::process::Command::new("cp")
        .arg("-a")
        .arg(src)
        .arg(dst)
        .status()
        .unwrap();
    assert!(st.success());
}

/// pi is restored from a backup (folder and index together), makes a few changes, and tower
/// pulls from pi before pi pulls from tower.
#[test]
fn m3_restored_backup_few_changes_silently_lost() {
    let tmp = scratch();
    let t = tmp.path();
    let all = ["tower", "pi"];
    let mut tower = open_real(&t.join("tower"), &t.join("tower.redb"), "tower", &all);
    let mut pi = open_real(&t.join("pi"), &t.join("pi.redb"), "pi", &all);
    fs::create_dir_all(t.join("pi")).unwrap();
    for i in 0..30 {
        fs::write(t.join(format!("pi/f{i}.txt")), format!("base {i}")).unwrap();
    }
    pi.scan(ts(0)).unwrap();
    tower
        .pull_from_peer(&EnginePeer::new(&pi, "tower"), ts(1))
        .unwrap();
    pi.pull_from_peer(&EnginePeer::new(&tower, "pi"), ts(1))
        .unwrap();
    // Backup of pi (folder + index) taken now.
    drop(pi);
    fs::create_dir_all(t.join("backup")).unwrap();
    copy_tree(&t.join("pi"), &t.join("backup/pi"));
    fs::copy(t.join("pi.redb"), t.join("backup/pi.redb")).unwrap();
    let mut pi = open_real(&t.join("pi"), &t.join("pi.redb"), "pi", &all);
    // pi keeps working; tower sees those changes.
    for i in 0..10 {
        fs::write(t.join(format!("pi/f{i}.txt")), format!("after backup {i}")).unwrap();
    }
    pi.scan(ts(10)).unwrap();
    tower
        .pull_from_peer(&EnginePeer::new(&pi, "tower"), ts(11))
        .unwrap();
    pi.pull_from_peer(&EnginePeer::new(&tower, "pi"), ts(11))
        .unwrap();
    // Disk dies; the person restores pi from the backup.
    drop(pi);
    fs::remove_dir_all(t.join("pi")).unwrap();
    copy_tree(&t.join("backup/pi"), &t.join("pi"));
    fs::copy(t.join("backup/pi.redb"), t.join("pi.redb")).unwrap();
    let mut pi = open_real(&t.join("pi"), &t.join("pi.redb"), "pi", &all);
    // The person writes 15 new documents before the machines meet.
    for i in 0..15 {
        fs::write(
            t.join(format!("pi/new{i}.txt")),
            format!("new after restore {i}"),
        )
        .unwrap();
    }
    pi.scan(ts(20)).unwrap();
    let r1 = tower.pull_from_peer(&EnginePeer::new(&pi, "tower"), ts(21));
    println!("tower pull: {r1:?}");
    let r2 = pi.pull_from_peer(&EnginePeer::new(&tower, "pi"), ts(22));
    println!(
        "pi pull: {r2:?} incarnation now {:x}",
        pi.meta().unwrap().incarnation
    );
    for k in 0..3 {
        let _ = tower.pull_from_peer(&EnginePeer::new(&pi, "tower"), ts(30 + k));
        let _ = pi.pull_from_peer(&EnginePeer::new(&tower, "pi"), ts(30 + k));
    }
    let missing: Vec<_> = (0..15)
        .filter(|i| !t.join(format!("tower/new{i}.txt")).exists())
        .collect();
    println!("new files missing on tower: {missing:?}");
    println!("tower status: {:?}", tower.status().unwrap().state);
    println!("pi status: {:?}", pi.status().unwrap().state);
    assert!(missing.is_empty(), "pi's new documents never reach tower");
}

/// Two machines. The laptop is off; tower deletes a file, then tower's Legion is reinstalled
/// (index lost, folder kept). The laptop comes back with its old copy.
#[test]
fn m4_reinstall_while_other_member_off_resurrects() {
    let tmp = scratch();
    let t = tmp.path();
    let all = ["tower", "laptop"];
    let mut tower = open_real(&t.join("tower"), &t.join("tower.redb"), "tower", &all);
    let mut laptop = open_real(&t.join("laptop"), &t.join("laptop.redb"), "laptop", &all);
    fs::create_dir_all(t.join("tower")).unwrap();
    fs::write(
        t.join("tower/old-tax-return.pdf"),
        b"private, deleted on purpose",
    )
    .unwrap();
    fs::write(t.join("tower/keep.txt"), b"keep").unwrap();
    tower.scan(ts(0)).unwrap();
    laptop
        .pull_from_peer(&EnginePeer::new(&tower, "laptop"), ts(1))
        .unwrap();
    tower
        .pull_from_peer(&EnginePeer::new(&laptop, "tower"), ts(1))
        .unwrap();
    // Laptop goes off for weeks. Tower deletes the file.
    fs::remove_file(t.join("tower/old-tax-return.pdf")).unwrap();
    tower.scan(ts(10)).unwrap();
    // Tower is reinstalled; its folder is kept, its index is gone.
    drop(tower);
    fs::remove_file(t.join("tower.redb")).unwrap();
    let mut tower = open_real(&t.join("tower"), &t.join("tower.redb"), "tower", &all);
    // Item 7: missing index beside existing marker and non-empty folder stops as damaged until
    // explicitly rebuilt from disk.
    assert!(tower.is_index_damaged());
    assert!(matches!(
        tower.scan(ts(20)),
        Err(legion_sync::SyncError::IndexDamaged { .. })
    ));
    tower.rebuild_from_disk(ts(20)).unwrap();
    assert!(!tower.is_index_damaged());
    // Laptop returns.
    for k in 0..3 {
        let _ = tower.pull_from_peer(&EnginePeer::new(&laptop, "tower"), ts(30 + k));
        let _ = laptop.pull_from_peer(&EnginePeer::new(&tower, "laptop"), ts(30 + k));
    }
    println!(
        "tower has it: {} laptop has it: {}",
        t.join("tower/old-tax-return.pdf").exists(),
        t.join("laptop/old-tax-return.pdf").exists()
    );
    assert!(
        !t.join("tower/old-tax-return.pdf").exists(),
        "deleted file came back on tower"
    );
    assert!(
        !t.join("laptop/old-tax-return.pdf").exists(),
        "deleted file was not deleted on laptop"
    );
}

/// After the restore, the person edits files that had also changed after the backup.
#[test]
fn m5_restored_backup_new_edits_replaced_by_old() {
    let tmp = scratch();
    let t = tmp.path();
    let all = ["tower", "pi"];
    let mut tower = open_real(&t.join("tower"), &t.join("tower.redb"), "tower", &all);
    let mut pi = open_real(&t.join("pi"), &t.join("pi.redb"), "pi", &all);
    fs::create_dir_all(t.join("pi")).unwrap();
    for i in 0..10 {
        fs::write(t.join(format!("pi/f{i}.txt")), format!("base {i}")).unwrap();
    }
    pi.scan(ts(0)).unwrap();
    tower
        .pull_from_peer(&EnginePeer::new(&pi, "tower"), ts(1))
        .unwrap();
    pi.pull_from_peer(&EnginePeer::new(&tower, "pi"), ts(1))
        .unwrap();
    drop(pi);
    fs::create_dir_all(t.join("backup")).unwrap();
    copy_tree(&t.join("pi"), &t.join("backup/pi"));
    fs::copy(t.join("pi.redb"), t.join("backup/pi.redb")).unwrap();
    let mut pi = open_real(&t.join("pi"), &t.join("pi.redb"), "pi", &all);
    for round in 0..3 {
        for i in 0..5 {
            fs::write(
                t.join(format!("pi/f{i}.txt")),
                format!("after backup {i} r{round}"),
            )
            .unwrap();
        }
        pi.scan(ts(10 + round)).unwrap();
    }
    tower
        .pull_from_peer(&EnginePeer::new(&pi, "tower"), ts(14))
        .unwrap();
    pi.pull_from_peer(&EnginePeer::new(&tower, "pi"), ts(14))
        .unwrap();
    drop(pi);
    fs::remove_dir_all(t.join("pi")).unwrap();
    copy_tree(&t.join("backup/pi"), &t.join("pi"));
    fs::copy(t.join("backup/pi.redb"), t.join("pi.redb")).unwrap();
    let mut pi = open_real(&t.join("pi"), &t.join("pi.redb"), "pi", &all);
    for i in 0..5 {
        fs::write(t.join(format!("pi/f{i}.txt")), format!("TODAY's work {i}")).unwrap();
    }
    pi.scan(ts(20)).unwrap();
    for k in 0..3 {
        let a = tower.pull_from_peer(&EnginePeer::new(&pi, "tower"), ts(30 + k));
        let b = pi.pull_from_peer(&EnginePeer::new(&tower, "pi"), ts(30 + k));
        println!(
            "round {k}: tower {:?} / pi {:?}",
            a.map(|r| (r.applied, r.kept)),
            b.map(|r| (r.applied, r.kept))
        );
    }
    for i in 0..5 {
        let want = format!("TODAY's work {i}");
        let live_pi = fs::read_dir(t.join("pi"))
            .unwrap()
            .flatten()
            .filter(|e| e.file_type().unwrap().is_file())
            .any(|e| fs::read(e.path()).unwrap() == want.as_bytes());
        let live_tower = fs::read_dir(t.join("tower"))
            .unwrap()
            .flatten()
            .filter(|e| e.file_type().unwrap().is_file())
            .any(|e| fs::read(e.path()).unwrap() == want.as_bytes());
        println!(
            "f{i}: today's work live on pi {live_pi}, on tower {live_tower}, anywhere {:?}; pi f{i} now {:?}",
            where_is(&t.join("pi"), want.as_bytes()),
            String::from_utf8_lossy(&fs::read(t.join(format!("pi/f{i}.txt"))).unwrap())
        );
        assert!(live_pi, "TODAY's work {i} must stay live on pi");
        assert!(live_tower, "TODAY's work {i} must reach tower as live");
    }
}

/// After the restore, the person edits files that had also changed after the backup.
#[test]
fn m6_restored_backup_more_changes_than_gap() {
    let tmp = scratch();
    let t = tmp.path();
    let all = ["tower", "pi"];
    let mut tower = open_real(&t.join("tower"), &t.join("tower.redb"), "tower", &all);
    let mut pi = open_real(&t.join("pi"), &t.join("pi.redb"), "pi", &all);
    fs::create_dir_all(t.join("pi")).unwrap();
    for i in 0..10 {
        fs::write(t.join(format!("pi/f{i}.txt")), format!("base {i}")).unwrap();
    }
    pi.scan(ts(0)).unwrap();
    tower
        .pull_from_peer(&EnginePeer::new(&pi, "tower"), ts(1))
        .unwrap();
    pi.pull_from_peer(&EnginePeer::new(&tower, "pi"), ts(1))
        .unwrap();
    drop(pi);
    fs::create_dir_all(t.join("backup")).unwrap();
    copy_tree(&t.join("pi"), &t.join("backup/pi"));
    fs::copy(t.join("pi.redb"), t.join("backup/pi.redb")).unwrap();
    let mut pi = open_real(&t.join("pi"), &t.join("pi.redb"), "pi", &all);
    for round in 0..3 {
        for i in 0..5 {
            fs::write(
                t.join(format!("pi/f{i}.txt")),
                format!("after backup {i} r{round}"),
            )
            .unwrap();
        }
        pi.scan(ts(10 + round)).unwrap();
    }
    tower
        .pull_from_peer(&EnginePeer::new(&pi, "tower"), ts(14))
        .unwrap();
    pi.pull_from_peer(&EnginePeer::new(&tower, "pi"), ts(14))
        .unwrap();
    drop(pi);
    fs::remove_dir_all(t.join("pi")).unwrap();
    copy_tree(&t.join("backup/pi"), &t.join("pi"));
    fs::copy(t.join("backup/pi.redb"), t.join("pi.redb")).unwrap();
    let mut pi = open_real(&t.join("pi"), &t.join("pi.redb"), "pi", &all);
    for i in 0..20 {
        fs::write(t.join(format!("pi/extra{i}.txt")), format!("extra {i}")).unwrap();
    }
    pi.scan(ts(19)).unwrap();
    for i in 0..5 {
        fs::write(t.join(format!("pi/f{i}.txt")), format!("TODAY's work {i}")).unwrap();
    }
    pi.scan(ts(20)).unwrap();
    for k in 0..3 {
        let a = tower.pull_from_peer(&EnginePeer::new(&pi, "tower"), ts(30 + k));
        let b = pi.pull_from_peer(&EnginePeer::new(&tower, "pi"), ts(30 + k));
        println!(
            "round {k}: tower {:?} / pi {:?}",
            a.map(|r| (r.applied, r.kept)),
            b.map(|r| (r.applied, r.kept))
        );
    }
    for i in 0..5 {
        let want = format!("TODAY's work {i}");
        let live_pi = fs::read_dir(t.join("pi"))
            .unwrap()
            .flatten()
            .filter(|e| e.file_type().unwrap().is_file())
            .any(|e| fs::read(e.path()).unwrap() == want.as_bytes());
        let live_tower = fs::read_dir(t.join("tower"))
            .unwrap()
            .flatten()
            .filter(|e| e.file_type().unwrap().is_file())
            .any(|e| fs::read(e.path()).unwrap() == want.as_bytes());
        println!(
            "f{i}: today's work live on pi {live_pi}, on tower {live_tower}, anywhere {:?}; pi f{i} now {:?}",
            where_is(&t.join("pi"), want.as_bytes()),
            String::from_utf8_lossy(&fs::read(t.join(format!("pi/f{i}.txt"))).unwrap())
        );
        assert!(live_pi, "TODAY's work {i} must stay live on pi");
        assert!(live_tower, "TODAY's work {i} must reach tower as live");
    }
}

/// After the restore, the person edits files that had also changed after the backup.
#[test]
fn m7_restored_backup_edit_first_then_more() {
    let tmp = scratch();
    let t = tmp.path();
    let all = ["tower", "pi"];
    let mut tower = open_real(&t.join("tower"), &t.join("tower.redb"), "tower", &all);
    let mut pi = open_real(&t.join("pi"), &t.join("pi.redb"), "pi", &all);
    fs::create_dir_all(t.join("pi")).unwrap();
    for i in 0..10 {
        fs::write(t.join(format!("pi/f{i}.txt")), format!("base {i}")).unwrap();
    }
    pi.scan(ts(0)).unwrap();
    tower
        .pull_from_peer(&EnginePeer::new(&pi, "tower"), ts(1))
        .unwrap();
    pi.pull_from_peer(&EnginePeer::new(&tower, "pi"), ts(1))
        .unwrap();
    drop(pi);
    fs::create_dir_all(t.join("backup")).unwrap();
    copy_tree(&t.join("pi"), &t.join("backup/pi"));
    fs::copy(t.join("pi.redb"), t.join("backup/pi.redb")).unwrap();
    let mut pi = open_real(&t.join("pi"), &t.join("pi.redb"), "pi", &all);
    for round in 0..3 {
        for i in 0..5 {
            fs::write(
                t.join(format!("pi/f{i}.txt")),
                format!("after backup {i} r{round}"),
            )
            .unwrap();
        }
        pi.scan(ts(10 + round)).unwrap();
    }
    tower
        .pull_from_peer(&EnginePeer::new(&pi, "tower"), ts(14))
        .unwrap();
    pi.pull_from_peer(&EnginePeer::new(&tower, "pi"), ts(14))
        .unwrap();
    drop(pi);
    fs::remove_dir_all(t.join("pi")).unwrap();
    copy_tree(&t.join("backup/pi"), &t.join("pi"));
    fs::copy(t.join("backup/pi.redb"), t.join("pi.redb")).unwrap();
    let mut pi = open_real(&t.join("pi"), &t.join("pi.redb"), "pi", &all);
    for i in 0..5 {
        fs::write(t.join(format!("pi/f{i}.txt")), format!("TODAY's work {i}")).unwrap();
    }
    pi.scan(ts(19)).unwrap();
    for i in 0..20 {
        fs::write(t.join(format!("pi/extra{i}.txt")), format!("extra {i}")).unwrap();
    }
    pi.scan(ts(20)).unwrap();
    for k in 0..3 {
        let a = tower.pull_from_peer(&EnginePeer::new(&pi, "tower"), ts(30 + k));
        let b = pi.pull_from_peer(&EnginePeer::new(&tower, "pi"), ts(30 + k));
        println!(
            "round {k}: tower {:?} / pi {:?}",
            a.map(|r| (r.applied, r.kept)),
            b.map(|r| (r.applied, r.kept))
        );
    }
    for i in 0..5 {
        let want = format!("TODAY's work {i}");
        let live_pi = fs::read_dir(t.join("pi"))
            .unwrap()
            .flatten()
            .filter(|e| e.file_type().unwrap().is_file())
            .any(|e| fs::read(e.path()).unwrap() == want.as_bytes());
        let live_tower = fs::read_dir(t.join("tower"))
            .unwrap()
            .flatten()
            .filter(|e| e.file_type().unwrap().is_file())
            .any(|e| fs::read(e.path()).unwrap() == want.as_bytes());
        println!(
            "f{i}: today's work live on pi {live_pi}, on tower {live_tower}, anywhere {:?}; pi f{i} now {:?}",
            where_is(&t.join("pi"), want.as_bytes()),
            String::from_utf8_lossy(&fs::read(t.join(format!("pi/f{i}.txt"))).unwrap())
        );
        assert!(live_pi, "TODAY's work {i} must stay live on pi");
        assert!(live_tower, "TODAY's work {i} must reach tower as live");
    }
}

/// A newly joining member stopping mid-page (due to a deferral or page limit) on a peer that
/// already ran tombstone/fingerprint pruning must still receive valid chained fingerprints and
/// never trip false duplicate-member detection or self-rotation.
#[test]
fn m8_joining_member_mid_page_deferral_after_pruning_preserves_fingerprints() {
    let tmp = scratch();
    let t = tmp.path();
    let two = ["tower", "pi"];
    let mut tower = open_real(&t.join("tower"), &t.join("tower.redb"), "tower", &two);
    let mut pi = open_real(&t.join("pi"), &t.join("pi.redb"), "pi", &two);

    fs::create_dir_all(t.join("tower")).unwrap();
    for i in 0..5 {
        fs::write(t.join(format!("tower/f{i}.txt")), format!("v1-{i}")).unwrap();
    }
    tower.scan(ts(0)).unwrap();
    pi.pull_from_peer(&EnginePeer::new(&tower, "pi"), ts(1))
        .unwrap();
    // Tower learns pi's direct ack and runs GC/pruning.
    tower
        .pull_from_peer(&EnginePeer::new(&pi, "tower"), ts(2))
        .unwrap();

    // Unscanned edit on tower's last file causes a deferral when laptop pulls.
    fs::write(t.join("tower/f4.txt"), "unscanned-newer-bytes").unwrap();

    // Laptop joins the existing two-member folder.
    let three = ["tower", "pi", "laptop"];
    tower.add_member("laptop").unwrap();
    pi.add_member("laptop").unwrap();
    let mut laptop = open_real(&t.join("laptop"), &t.join("laptop.redb"), "laptop", &three);

    let rep1 = laptop
        .pull_from_peer(&EnginePeer::new(&tower, "laptop"), ts(10))
        .unwrap();
    assert!(rep1.deferred >= 1, "unscanned f4.txt must defer on page 1");

    // Tower pulling from laptop while laptop is mid-page must NOT falsely rotate tower, and
    // laptop pulling from tower again must NOT falsely flag tower as a duplicate member!
    let tower_inc_before = tower.meta().unwrap().incarnation;
    tower
        .pull_from_peer(&EnginePeer::new(&laptop, "tower"), ts(11))
        .unwrap();
    assert_eq!(
        tower.meta().unwrap().incarnation,
        tower_inc_before,
        "tower must not falsely self-rotate when laptop stopped mid-page"
    );

    tower.scan(ts(12)).unwrap();
    laptop
        .pull_from_peer(&EnginePeer::new(&tower, "laptop"), ts(13))
        .unwrap();
    assert_eq!(
        fs::read_to_string(t.join("laptop/f4.txt")).unwrap(),
        "unscanned-newer-bytes"
    );
}
