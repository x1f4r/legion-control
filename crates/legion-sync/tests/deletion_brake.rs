//! Real-disk safety regression tests: move-aware, windowed outbound and inbound deletion brakes.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod safety_common;

use std::fs;

use legion_sync::{
    DeletionBrakeDecision, InboundDeletionBrakeDecision, MemberState, ProblemKind, ScanOptions,
    SyncError,
};
use safety_common::*;

#[test]
fn b1_renaming_a_large_folder_trips_the_brake() {
    let tmp = scratch();
    let t = tmp.path();
    let mut tower = open_real(
        &t.join("tower"),
        &t.join("tower.redb"),
        "tower",
        &["tower", "pi"],
    );
    let mut pi = open_real(&t.join("pi"), &t.join("pi.redb"), "pi", &["tower", "pi"]);
    fs::create_dir_all(t.join("tower/Photos")).unwrap();
    fs::create_dir_all(t.join("tower/Docs")).unwrap();
    for i in 0..300 {
        fs::write(
            t.join(format!("tower/Photos/p{i}.jpg")),
            format!("photo {i}"),
        )
        .unwrap();
    }
    for i in 0..100 {
        fs::write(t.join(format!("tower/Docs/d{i}.txt")), format!("doc {i}")).unwrap();
    }
    tower.scan(ts(0)).unwrap();
    pi.pull_from_peer(&EnginePeer::new(&tower, "pi"), ts(1))
        .unwrap();

    fs::rename(t.join("tower/Photos"), t.join("tower/Pictures")).unwrap();
    let r = tower.scan(ts(10)).unwrap();
    println!(
        "after renaming a folder: brake_tripped={} deleted={} created={}",
        r.brake_tripped, r.deleted, r.created
    );
    assert!(
        !r.brake_tripped,
        "a folder rename must not pause the folder as a mass deletion"
    );
    assert_eq!(r.created, 301); // Pictures dir + 300 files
    assert_eq!(r.deleted, 301); // Photos dir + 300 moved files

    // Peer must also apply the folder rename without tripping the inbound brake!
    let pr = pi
        .pull_from_peer(&EnginePeer::new(&tower, "pi"), ts(11))
        .unwrap();
    assert_eq!(pr.local_renames, 300);
    assert!(t.join("pi/Pictures/p0.jpg").exists());
    assert!(!t.join("pi/Photos").exists());
}

#[test]
fn b2_subtree_scans_split_a_wipe_under_the_brake() {
    let tmp = scratch();
    let t = tmp.path();
    let mut tower = open_real(
        &t.join("tower"),
        &t.join("tower.redb"),
        "tower",
        &["tower", "pi"],
    );
    for d in 0..5 {
        fs::create_dir_all(t.join(format!("tower/d{d}"))).unwrap();
        for i in 0..100 {
            fs::write(t.join(format!("tower/d{d}/f{i}.txt")), format!("{d}/{i}")).unwrap();
        }
    }
    tower.scan(ts(0)).unwrap();
    let mut total_deleted = 0;
    let mut tripped = false;
    for d in 0..5 {
        fs::remove_dir_all(t.join(format!("tower/d{d}"))).unwrap();
        let r = tower
            .scan_subtree(&format!("d{d}"), ts(10 + d), ScanOptions::default())
            .unwrap();
        total_deleted += r.deleted;
        tripped |= r.brake_tripped;
    }
    println!(
        "whole folder wiped dir by dir: deleted {total_deleted} records, brake tripped: {tripped}"
    );
    assert!(
        tripped,
        "split wipe across subtree scans must trip the brake"
    );
    assert_eq!(
        total_deleted, 0,
        "no deletions may leak to peers before the settle period and rolling window check"
    );
    assert!(tower.is_paused().unwrap());
    let st = tower.status().unwrap();
    assert!(st.pending_brake.is_some());
}

#[test]
fn b3_settle_period_holds_deletions_for_30s_and_cross_scan_move_cancels() {
    let tmp = scratch();
    let t = tmp.path();
    let all = ["tower", "pi"];
    let mut tower = open_real(&t.join("tower"), &t.join("tower.redb"), "tower", &all);
    let mut pi = open_real(&t.join("pi"), &t.join("pi.redb"), "pi", &all);
    fs::create_dir_all(t.join("tower/src")).unwrap();
    fs::create_dir_all(t.join("tower/dst")).unwrap();
    for i in 0..100 {
        fs::write(t.join(format!("tower/src/f{i}.txt")), format!("body-{i}")).unwrap();
    }
    tower.scan(ts(0)).unwrap();
    pi.pull_from_peer(&EnginePeer::new(&tower, "pi"), ts(1))
        .unwrap();

    // Move 40 files from src -> dst, scanning src FIRST (deletes seen) and dst 10s later
    // (creates seen within the 30s settle window).
    for i in 0..40 {
        fs::rename(
            t.join(format!("tower/src/f{i}.txt")),
            t.join(format!("tower/dst/f{i}.txt")),
        )
        .unwrap();
    }
    let r1 = tower
        .scan_subtree("src", ts(10), ScanOptions::default())
        .unwrap();
    assert!(!r1.brake_tripped);
    assert_eq!(
        r1.deleted, 0,
        "unmatched deletions must sit pending during the 30s settle period"
    );
    // Peer pulling before settle sees zero deletions.
    pi.pull_from_peer(&EnginePeer::new(&tower, "pi"), ts(15))
        .unwrap();
    assert!(t.join("pi/src/f0.txt").exists());

    // Scanning dst at t=20 (< 10 + 30) matches all 40 pending deletions as moves!
    let r2 = tower
        .scan_subtree("dst", ts(20), ScanOptions::default())
        .unwrap();
    assert!(!r2.brake_tripped);
    assert_eq!(r2.created, 40);
    assert_eq!(r2.deleted, 40);

    // Now genuinely delete 15 files at t=25: because the 40 moves did NOT count toward the
    // rolling 10-minute window, 15/100 does not trip the brake and settles after 30s (at t=56).
    for i in 40..55 {
        fs::remove_file(t.join(format!("tower/src/f{i}.txt"))).unwrap();
    }
    let r3 = tower.scan(ts(25)).unwrap();
    assert!(!r3.brake_tripped);
    assert_eq!(r3.deleted, 0);
    // Still pending at t=50 (25s < 30s).
    assert_eq!(tower.flush_settled_deletions(ts(50)).unwrap(), 0);
    // Settles at t=56 (31s >= 30s).
    assert_eq!(tower.flush_settled_deletions(ts(56)).unwrap(), 15);
    pi.pull_from_peer(&EnginePeer::new(&tower, "pi"), ts(57))
        .unwrap();
    assert!(!t.join("pi/src/f40.txt").exists());
    assert!(t.join("pi/dst/f0.txt").exists());
}

#[test]
fn b4_inbound_deletion_brake_pauses_peer_and_supports_keep_local_or_apply() {
    let tmp = scratch();
    let t = tmp.path();
    let all = ["tower", "pi", "laptop"];
    let mut tower = open_real(&t.join("tower"), &t.join("tower.redb"), "tower", &all);
    let mut pi = open_real(&t.join("pi"), &t.join("pi.redb"), "pi", &all);
    let mut laptop = open_real(&t.join("laptop"), &t.join("laptop.redb"), "laptop", &all);
    fs::create_dir_all(t.join("tower")).unwrap();
    for i in 0..80 {
        fs::write(t.join(format!("tower/f{i}.txt")), format!("payload {i}")).unwrap();
    }
    tower.scan(ts(0)).unwrap();
    pi.pull_from_peer(&EnginePeer::new(&tower, "pi"), ts(1))
        .unwrap();
    laptop
        .pull_from_peer(&EnginePeer::new(&tower, "laptop"), ts(1))
        .unwrap();

    // Tower deletes 60/80 files and explicitly confirms its outbound brake.
    for i in 0..60 {
        fs::remove_file(t.join(format!("tower/f{i}.txt"))).unwrap();
    }
    assert!(tower.scan(ts(10)).unwrap().brake_tripped);
    tower
        .resolve_deletion_brake(DeletionBrakeDecision::ConfirmDeleted, ts(11))
        .unwrap();

    // Pi pulls from tower -> inbound brake pauses ONLY tower on pi without deleting any file!
    assert!(matches!(
        pi.pull_from_peer(&EnginePeer::new(&tower, "pi"), ts(20)),
        Err(SyncError::Paused { .. })
    ));
    assert!(t.join("pi/f0.txt").exists());
    let pi_st = pi.status().unwrap();
    assert_eq!(pi_st.inbound_brakes.len(), 1);
    assert_eq!(pi_st.inbound_brakes[0].peer, "tower");
    assert_eq!(pi_st.inbound_brakes[0].deleted_count, 60);
    match &pi_st.state {
        MemberState::Problem { problems, .. } => {
            assert!(
                problems
                    .iter()
                    .any(|p| p.kind == ProblemKind::InboundDeletionBrakeTripped)
            );
        }
        other => panic!("expected InboundDeletionBrakeTripped problem, got {other:?}"),
    }

    // Pi can STILL sync with laptop while tower is paused!
    fs::write(t.join("laptop/from-laptop.txt"), b"ok").unwrap();
    laptop.scan(ts(21)).unwrap();
    pi.pull_from_peer(&EnginePeer::new(&laptop, "pi"), ts(22))
        .unwrap();
    assert!(t.join("pi/from-laptop.txt").exists());

    // Branch A: pi chooses KeepLocalFiles -> re-asserts all 60 files as newer so tower gets them
    // back!
    let reasserted = pi
        .resolve_inbound_deletion_brake(
            "tower",
            InboundDeletionBrakeDecision::KeepLocalFiles,
            ts(25),
        )
        .unwrap();
    assert_eq!(reasserted, 60);
    pi.pull_from_peer(&EnginePeer::new(&tower, "pi"), ts(26))
        .unwrap();
    tower
        .pull_from_peer(&EnginePeer::new(&pi, "tower"), ts(27))
        .unwrap();
    for i in 0..60 {
        assert!(t.join(format!("pi/f{i}.txt")).exists());
        assert!(
            t.join(format!("tower/f{i}.txt")).exists(),
            "tower must receive pi's kept file f{i}.txt"
        );
    }

    // Branch B: laptop pulls from tower (before tower pulled pi? No, test fresh ApplyDeletions on
    // laptop with a second confirmed wipe on tower!).
    for i in 0..60 {
        fs::remove_file(t.join(format!("tower/f{i}.txt"))).unwrap();
    }
    assert!(tower.scan(ts(40)).unwrap().brake_tripped);
    tower
        .resolve_deletion_brake(DeletionBrakeDecision::ConfirmDeleted, ts(41))
        .unwrap();
    assert!(matches!(
        laptop.pull_from_peer(&EnginePeer::new(&tower, "laptop"), ts(42)),
        Err(SyncError::Paused { .. })
    ));
    laptop
        .resolve_inbound_deletion_brake(
            "tower",
            InboundDeletionBrakeDecision::ApplyDeletions,
            ts(43),
        )
        .unwrap();
    laptop
        .pull_from_peer(&EnginePeer::new(&tower, "laptop"), ts(44))
        .unwrap();
    for i in 0..60 {
        assert!(!t.join(format!("laptop/f{i}.txt")).exists());
    }
    for i in 60..80 {
        assert!(t.join(format!("laptop/f{i}.txt")).exists());
    }
}
