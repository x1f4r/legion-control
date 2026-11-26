//! Asserting regression tests for every probe in the fifth independent review, plus coverage for
//! schema-bump rebuilds across all members, self-detected rollback confirmation, zero-growth
//! rebuilds, honest status, byte-bounded pages, `RealFs` directory-sync error propagation, and
//! `RealFs` durability tracing.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod safety_common;

use std::fs;
use std::io::Read;
use std::path::Path;
use std::sync::atomic::AtomicBool;

use legion_sync::{
    FetchOptions, FolderConfig, FolderMeta, HandshakeHello, IndexRecord, IndexStore,
    MAX_PAGE_BYTES, MIN_MIGRATABLE_SCHEMA_VERSION, MemberState, MemoryIndexStore,
    MemoryInstanceTokenStore, PageLimit, Peer, PeerAckView, PeerCursor, PeerHandshake,
    PeerRollbackClaimDecision, ProblemKind, RealFs, RecordProvenance, RecordsPage, RedbIndexStore,
    SYNC_PROTOCOL_VERSION, StoreBatch, SyncEngine, VersionKey, VersionVector,
    audit_real_fs_durability,
};
use legion_xfer::hash::hash_bytes;
use legion_xfer::{EntryKind, Hash};
use safety_common::*;

type E = SyncEngine<RealFs, RedbIndexStore>;

/// Models copying a sync folder while Legion is stopped (caller must drop any open `SyncEngine`
/// on `src` first because Windows holds byte-range locks on open `.legion-sync/index.redb`).
fn copy_folder_while_stopped(src: &Path, dst: &Path) {
    fs::create_dir_all(dst).unwrap();
    for entry in fs::read_dir(src).unwrap() {
        let entry = entry.unwrap();
        let ty = entry.file_type().unwrap();
        let to = dst.join(entry.file_name());
        if ty.is_dir() {
            copy_folder_while_stopped(&entry.path(), &to);
        } else if ty.is_file() {
            fs::copy(entry.path(), &to).unwrap();
        }
    }
}

/// Models copying only live user files (for example onto a USB stick), skipping `.legion-sync`
/// so an open `.legion-sync/index.redb` is never touched while Legion is running.
fn copy_user_files_excluding_internal(src: &Path, dst: &Path) {
    fs::create_dir_all(dst).unwrap();
    for entry in fs::read_dir(src).unwrap() {
        let entry = entry.unwrap();
        if entry
            .file_name()
            .to_string_lossy()
            .eq_ignore_ascii_case(".legion-sync")
        {
            continue;
        }
        let ty = entry.file_type().unwrap();
        let to = dst.join(entry.file_name());
        if ty.is_dir() {
            copy_user_files_excluding_internal(&entry.path(), &to);
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

/// Review 5 Probe A: every member upgrades schema in place or reinstalls (losing its machine
/// token) after deletions have settled and synced; a joiner carrying an old USB copy arrives. The
/// deleted file MUST NEVER resurrect on any member (whether none, one, both, or the sole surviving
/// member upgraded/reinstalled).
#[test]
fn review5_a_rebuild_every_member_after_deletions_never_resurrects() {
    for (tower_reinstalls, pi_upgrades, pi_gone) in [
        (false, false, false),
        (true, false, false),
        (true, true, false),
        (true, false, true),
    ] {
        let tmp = scratch();
        let t = tmp.path();
        let all = ["tower", "pi"];
        fs::create_dir_all(t.join("tower")).unwrap();
        fs::create_dir_all(t.join("pi")).unwrap();
        fs::write(t.join("pi/tax.pdf"), b"deleted on purpose").unwrap();
        fs::write(t.join("pi/keep.txt"), b"keep").unwrap();
        let mut tower = open_folder(
            &t.join("tower"),
            &token_store_dir(&t.join("tower"), "tower"),
            "tower",
            &all,
        );
        let mut pi = open_folder(
            &t.join("pi"),
            &token_store_dir(&t.join("pi"), "pi"),
            "pi",
            &all,
        );
        tower.scan(ts(0)).unwrap();
        pi.scan(ts(0)).unwrap();
        assert!(sync_all(&mut [&mut tower, &mut pi], 1).is_empty());

        copy_user_files_excluding_internal(&t.join("tower"), &t.join("usb"));

        fs::remove_file(t.join("pi/tax.pdf")).unwrap();
        pi.scan(ts(10)).unwrap();
        pi.flush_settled_deletions(ts(100)).unwrap();
        for k in 0..4 {
            assert!(sync_all(&mut [&mut tower, &mut pi], 101 + 10 * k).is_empty());
        }
        assert!(
            tower
                .store()
                .get_record("tax.pdf")
                .unwrap()
                .is_some_and(|r| r.deleted),
            "committed deletion tombstone must remain permanently in index"
        );
        assert!(!t.join("tower/tax.pdf").exists());

        // Simulate a reinstall (token lost) on tower and an in-place schema upgrade on pi.
        if tower_reinstalls {
            drop(tower);
            let _ = fs::remove_dir_all(token_store_dir(&t.join("tower"), "tower"));
            tower = open_folder(
                &t.join("tower"),
                &token_store_dir(&t.join("tower"), "tower"),
                "tower",
                &all,
            );
            assert!(
                tower
                    .store()
                    .get_record("tax.pdf")
                    .unwrap()
                    .is_some_and(|r| r.deleted),
                "reinstalled tower must keep tax.pdf tombstone in folder index"
            );
        }
        if pi_upgrades {
            pi.store_mut()
                .write_schema_version_for_test(MIN_MIGRATABLE_SCHEMA_VERSION)
                .unwrap();
            drop(pi);
            pi = open_folder(
                &t.join("pi"),
                &token_store_dir(&t.join("pi"), "pi"),
                "pi",
                &all,
            );
            assert!(
                pi.store()
                    .get_record("tax.pdf")
                    .unwrap()
                    .is_some_and(|r| r.deleted),
                "in-place schema upgrade on pi must keep tax.pdf tombstone"
            );
        }
        if !pi_gone {
            for k in 0..3 {
                assert!(sync_all(&mut [&mut tower, &mut pi], 300 + 10 * k).is_empty());
            }
        }

        let all3 = ["tower", "pi", "laptop"];
        tower.add_member("laptop").unwrap();
        if !pi_gone {
            pi.add_member("laptop").unwrap();
        }
        fs::rename(t.join("usb"), t.join("laptop")).unwrap();
        let mut laptop = open_folder(
            &t.join("laptop"),
            &token_store_dir(&t.join("laptop"), "laptop"),
            "laptop",
            &all3,
        );
        laptop.scan(ts(500)).unwrap();

        if pi_gone {
            tower.remove_member("pi").unwrap();
            laptop.remove_member("pi").unwrap();
            assert!(sync_all(&mut [&mut tower, &mut laptop], 600).is_empty());
        } else {
            assert!(sync_all(&mut [&mut tower, &mut pi, &mut laptop], 600).is_empty());
            assert!(sync_all(&mut [&mut tower, &mut pi, &mut laptop], 700).is_empty());
        }

        assert!(
            !t.join("tower/tax.pdf").exists(),
            "tower_reinstalls={tower_reinstalls} pi_upgrades={pi_upgrades} pi_gone={pi_gone}: tax.pdf resurrected on tower"
        );
        assert!(
            !t.join("laptop/tax.pdf").exists(),
            "tower_reinstalls={tower_reinstalls} pi_upgrades={pi_upgrades} pi_gone={pi_gone}: tax.pdf not binned on laptop"
        );
        assert!(
            laptop
                .list_bin()
                .unwrap()
                .iter()
                .any(|b| b.original_path == "tax.pdf"),
            "laptop's old USB copy of tax.pdf must be preserved in laptop's bin"
        );
        if !pi_gone {
            assert!(!t.join("pi/tax.pdf").exists());
        }
        assert_eq!(fs::read(t.join("tower/keep.txt")).unwrap(), b"keep");
        assert_eq!(fs::read(t.join("laptop/keep.txt")).unwrap(), b"keep");
    }
}

/// Review 5 Probe B: pi's index (and optionally folder) is restored from a backup taken BEFORE pi
/// applied tower's edits — including when pi made ZERO own edits between backup and restore (only
/// applied records were lost), and when pi makes more post-restore edits than the lost gap.
/// Recovery MUST succeed automatically on the first sync without stalling!
#[test]
fn review5_b_restore_with_only_applied_records_lost_recovers_cleanly() {
    for (restore_folder, pi_own_edit_after_backup, extra_edits) in [
        (false, false, false),
        (true, false, false),
        (false, true, false),
        (true, false, true),
    ] {
        let tmp = scratch();
        let t = tmp.path();
        let all = ["tower", "pi"];
        fs::create_dir_all(t.join("tower")).unwrap();
        fs::create_dir_all(t.join("pi")).unwrap();
        fs::write(t.join("tower/a.txt"), b"a1").unwrap();
        fs::write(t.join("pi/p.txt"), b"p1").unwrap();
        let mut tower = open_real(&t.join("tower"), &t.join("tower.redb"), "tower", &all);
        let mut pi = open_real(&t.join("pi"), &t.join("pi.redb"), "pi", &all);
        tower.scan(ts(0)).unwrap();
        pi.scan(ts(0)).unwrap();
        assert!(sync_all(&mut [&mut tower, &mut pi], 1).is_empty());

        drop(pi);
        fs::copy(t.join("pi.redb"), t.join("pi.redb.bak")).unwrap();
        copy_folder_while_stopped(&t.join("pi"), &t.join("pi.bak"));
        let mut pi = open_real(&t.join("pi"), &t.join("pi.redb"), "pi", &all);

        for i in 0..3 {
            fs::write(t.join(format!("tower/t{i}.txt")), format!("tower work {i}")).unwrap();
        }
        tower.scan(ts(10)).unwrap();
        if pi_own_edit_after_backup {
            fs::write(t.join("pi/p.txt"), b"p2").unwrap();
            pi.scan(ts(11)).unwrap();
        }
        assert!(sync_all(&mut [&mut tower, &mut pi], 20).is_empty());
        assert!(t.join("pi/t2.txt").exists());
        let inc_before = pi.meta().unwrap().incarnation;

        drop(pi);
        fs::copy(t.join("pi.redb.bak"), t.join("pi.redb")).unwrap();
        if restore_folder {
            fs::remove_dir_all(t.join("pi")).unwrap();
            fs::rename(t.join("pi.bak"), t.join("pi")).unwrap();
        }
        let mut pi = open_real(&t.join("pi"), &t.join("pi.redb"), "pi", &all);
        fs::write(t.join("pi/after.txt"), b"after restore").unwrap();
        pi.scan(ts(30)).unwrap();
        if extra_edits {
            for i in 0..4 {
                fs::write(t.join(format!("pi/extra{i}.txt")), format!("extra {i}")).unwrap();
                pi.scan(ts(31 + i)).unwrap();
            }
        }

        let mut last = Vec::new();
        for k in 0..4 {
            last = sync_all(&mut [&mut tower, &mut pi], 100 + 10 * k);
        }
        assert!(
            last.is_empty(),
            "restore_folder={restore_folder} own_edit={pi_own_edit_after_backup} extra={extra_edits}: sync stalled with {last:?}"
        );
        assert_ne!(
            pi.meta().unwrap().incarnation,
            inc_before,
            "restored pi must rotate to a fresh incarnation"
        );
        assert_eq!(
            fs::read(t.join("tower/after.txt")).unwrap(),
            b"after restore",
            "post-restore file must reach tower"
        );
        for i in 0..3 {
            assert_eq!(
                fs::read_to_string(t.join(format!("pi/t{i}.txt"))).unwrap(),
                format!("tower work {i}"),
                "tower's files must be restored onto pi"
            );
        }
        if extra_edits {
            for i in 0..4 {
                assert_eq!(
                    fs::read_to_string(t.join(format!("tower/extra{i}.txt"))).unwrap(),
                    format!("extra {i}")
                );
            }
        }
        assert_eq!(tower.status().unwrap().state, MemberState::UpToDate);
        let pi_probs = pi.store().list_problems().unwrap();
        assert!(
            pi_probs.iter().any(|(_, p)| {
                p.kind == ProblemKind::PeerForcedRotation
                    && p.sentence
                        == "This machine's copy of the folder is older than what tower has already received from it; it looks like it was restored from a backup. Legion is joining the folder again. Nothing is deleted."
            }),
            "expected exact backup-restore sentence on pi, got {pi_probs:?}"
        );
    }
}

struct RollbackClaimLiar<'a> {
    inner: EnginePeer<'a, RealFs, RedbIndexStore>,
}

impl Peer for RollbackClaimLiar<'_> {
    fn member_id(&self) -> &str {
        self.inner.member_id()
    }
    fn handshake(&self, c: &HandshakeHello) -> legion_sync::Result<PeerHandshake> {
        let mut h = self.inner.handshake(c)?;
        h.caller_pulled = Some(PeerAckView {
            incarnation: c.incarnation,
            seq: c.snapshot_seq.saturating_add(1),
            fingerprint: Some(hash_bytes(b"forged-fp")),
        });
        Ok(h)
    }
    fn records_since(
        &self,
        i: u64,
        s: u64,
        l: PageLimit,
        c: Option<&AtomicBool>,
    ) -> legion_sync::Result<RecordsPage> {
        self.inner.records_since(i, s, l, c)
    }
    fn fetch_file(
        &self,
        p: &str,
        h: Hash,
        o: &FetchOptions,
    ) -> legion_sync::Result<Box<dyn Read + Send>> {
        self.inner.fetch_file(p, h, o)
    }
}

/// Review 5 Probe C: a dishonest peer claiming rollback on every day across 26 weeks triggers at
/// most 1 rotation/day and STOPS completely after the 3rd rotation until the user explicitly
/// confirms in status (never driving the folder toward the 64-incarnation cap). Both confirmation
/// decisions (`KeepHonouring` and `IgnoreReports`) work as specified.
#[test]
fn review5_c_lying_peer_stops_after_three_until_confirmed() {
    const DAY: i64 = 86_400;
    let tmp = scratch();
    let t = tmp.path();
    let all = ["tower", "pi"];
    fs::create_dir_all(t.join("tower")).unwrap();
    fs::write(t.join("tower/a.txt"), b"a").unwrap();
    let mut tower = open_real(&t.join("tower"), &t.join("tower.redb"), "tower", &all);
    let mut pi = open_real(&t.join("pi"), &t.join("pi.redb"), "pi", &all);
    let base = 1_795_000_000i64;
    tower.scan(ts(base)).unwrap();
    assert!(sync_all(&mut [&mut tower, &mut pi], base + 1).is_empty());

    let mut rotations = 0usize;
    let mut cur = pi.meta().unwrap().incarnation;
    for week in 0..26i64 {
        for d in 0..3i64 {
            let now = base + DAY + week * 7 * DAY + d * DAY + 100;
            let res = pi.pull_from_peer(
                &RollbackClaimLiar {
                    inner: EnginePeer::new(&tower, "pi"),
                },
                ts(now),
            );
            let n = pi.meta().unwrap().incarnation;
            if n != cur {
                rotations += 1;
                cur = n;
            }
            if rotations >= 3 && (week > 0 || d >= 3) {
                assert!(
                    res.is_err(),
                    "week={week} day={d}: further claims after 3 rotations must be refused"
                );
            }
            fs::write(t.join("pi/a.txt"), format!("pi edit {week}-{d}")).unwrap();
            pi.scan(ts(now + 10)).unwrap();
            assert!(sync_all(&mut [&mut tower, &mut pi], now + 20).is_empty());
            assert_eq!(
                fs::read_to_string(t.join("tower/a.txt")).unwrap(),
                format!("pi edit {week}-{d}"),
                "pi's edits must continue syncing cleanly without hitting 64-incarnation cap"
            );
        }
    }

    assert_eq!(
        rotations, 3,
        "dishonest peer must trigger at most 3 rotations before stopping"
    );
    let rec = pi.store().get_record("a.txt").unwrap().unwrap();
    let pi_keys = rec
        .provenance
        .to_version_vector()
        .iter()
        .filter(|(k, _)| k.member == "pi")
        .count();
    assert!(
        pi_keys <= 4,
        "a.txt should have at most 4 pi incarnation keys, got {pi_keys}"
    );
    let st = pi.status().unwrap();
    assert_eq!(st.pending_rollback_confirmations, vec!["tower".to_string()]);
    match st.state {
        MemberState::Problem { problems, .. } => {
            assert!(
                problems.iter().any(|p| {
                    p.kind == ProblemKind::PeerRollbackRateLimited
                        && p.sentence
                            == "tower has reported three times that this machine's copy went back in time. If you restored this machine from a backup, that is expected. Otherwise tower may be faulty."
                }),
                "expected exact 3-rotation confirmation sentence, got {problems:?}"
            );
        }
        other => panic!("expected Problem with confirmation sentence, got {other:?}"),
    }

    // 1) User confirms KeepHonouring: resets counter and honours next day's claim.
    pi.confirm_peer_rollback_claims("tower", PeerRollbackClaimDecision::KeepHonouring)
        .unwrap();
    assert!(
        pi.status()
            .unwrap()
            .pending_rollback_confirmations
            .is_empty()
    );
    let after_keep = base + 300 * DAY;
    pi.pull_from_peer(
        &RollbackClaimLiar {
            inner: EnginePeer::new(&tower, "pi"),
        },
        ts(after_keep),
    )
    .unwrap();
    let inc_after_keep = pi.meta().unwrap().incarnation;
    assert_ne!(inc_after_keep, cur);

    // 2) User confirms IgnoreReports: future claims from tower change nothing (no rotation, no error).
    pi.confirm_peer_rollback_claims("tower", PeerRollbackClaimDecision::IgnoreReports)
        .unwrap();
    pi.pull_from_peer(
        &RollbackClaimLiar {
            inner: EnginePeer::new(&tower, "pi"),
        },
        ts(after_keep + 5 * DAY),
    )
    .unwrap();
    assert_eq!(
        pi.meta().unwrap().incarnation,
        inc_after_keep,
        "IgnoreReports must cause future claims from tower to change nothing"
    );
}

/// Review 5 Probes S & S7: honest `sync.status` for solo folders, 2-member local edits, 2-member
/// relays with `local_clock == 0` after removing a 3rd member (S5b), damaged indexes (S6), and
/// rebuilt members holding unsynced files after pulling from a peer (S7).
#[test]
fn review5_s_and_s7_honest_status_never_falsely_up_to_date() {
    let tmp = scratch();
    let t = tmp.path();

    // S1: one-member folder.
    fs::create_dir_all(t.join("solo")).unwrap();
    fs::write(t.join("solo/x.txt"), b"x").unwrap();
    let mut solo = open_real(&t.join("solo"), &t.join("solo.redb"), "solo", &["solo"]);
    solo.scan(ts(0)).unwrap();
    let s1 = solo.status_at(ts(10)).unwrap();
    assert_eq!(s1.state, MemberState::UpToDate);

    // S5: tower pulls from pi, then writes a local file -> must report WaitingForMember(pi).
    let all = ["tower", "pi"];
    fs::create_dir_all(t.join("tower")).unwrap();
    fs::create_dir_all(t.join("pi")).unwrap();
    fs::write(t.join("pi/p.txt"), b"p").unwrap();
    let mut tower = open_real(&t.join("tower"), &t.join("tower.redb"), "tower", &all);
    let mut pi = open_real(&t.join("pi"), &t.join("pi.redb"), "pi", &all);
    pi.scan(ts(0)).unwrap();
    assert!(pull(&mut tower, &pi, 1).is_none());
    assert_eq!(tower.status_at(ts(1)).unwrap().state, MemberState::UpToDate);
    fs::write(t.join("tower/t.txt"), b"t").unwrap();
    tower.scan(ts(2)).unwrap();
    assert_eq!(
        tower.status_at(ts(3)).unwrap().state,
        MemberState::WaitingForMember {
            members: vec!["pi".to_string()],
        }
    );

    // S5b: 3 members -> laptop2 creates doc.txt -> tower2 relays doc.txt (tower2.local_clock == 0!)
    // -> laptop2 removed (2 members left) -> tower2 pulls from pi2 -> pi2 STILL lacks doc.txt ->
    // tower2 MUST report WaitingForMember(pi2), NEVER UpToDate!
    let three = ["tower2", "pi2", "laptop2"];
    for n in three {
        fs::create_dir_all(t.join(n)).unwrap();
    }
    fs::write(t.join("pi2/p.txt"), b"p").unwrap();
    let mut tower2 = open_real(&t.join("tower2"), &t.join("tower2.redb"), "tower2", &three);
    let mut pi2 = open_real(&t.join("pi2"), &t.join("pi2.redb"), "pi2", &three);
    let mut laptop2 = open_real(
        &t.join("laptop2"),
        &t.join("laptop2.redb"),
        "laptop2",
        &three,
    );
    pi2.scan(ts(0)).unwrap();
    assert!(sync_all(&mut [&mut tower2, &mut pi2, &mut laptop2], 1).is_empty());
    fs::write(t.join("laptop2/doc.txt"), b"LAPTOP").unwrap();
    laptop2.scan(ts(5)).unwrap();
    assert!(pull(&mut tower2, &laptop2, 6).is_none());
    assert!(t.join("tower2/doc.txt").exists());
    drop(laptop2);
    tower2.remove_member("laptop2").unwrap();
    pi2.remove_member("laptop2").unwrap();
    assert!(pull(&mut tower2, &pi2, 7).is_none());
    assert_eq!(tower2.meta().unwrap().local_clock, 0);
    assert!(!t.join("pi2/doc.txt").exists());
    let s5b = tower2.status_at(ts(8)).unwrap();
    assert_eq!(
        s5b.state,
        MemberState::WaitingForMember {
            members: vec!["pi2".to_string()],
        },
        "S5b: relay with local_clock=0 holding unsent doc.txt must not report UpToDate (got {:?} | {})",
        s5b.state,
        s5b.summary
    );
    assert_eq!(
        s5b.members
            .iter()
            .find(|m| m.member == "pi2")
            .unwrap()
            .pending_outgoing,
        1
    );
    // Once pi2 pulls doc.txt and tower2 hears pi2's ack, both become UpToDate.
    assert!(sync_all(&mut [&mut tower2, &mut pi2], 9).is_empty());
    assert_eq!(fs::read(t.join("pi2/doc.txt")).unwrap(), b"LAPTOP");
    assert_eq!(
        tower2.status_at(ts(12)).unwrap().state,
        MemberState::UpToDate
    );

    // S6: corrupt redb database fails cleanly on open (never panics or deletes files).
    drop(tower2);
    fs::write(t.join("tower2.redb"), b"garbage that is not redb").unwrap();
    assert!(RedbIndexStore::open(t.join("tower2.redb")).is_err());

    // S7: tower has an unsynced file, rebuilds from disk, and pulls from pi -> pi still lacks
    // only-on-tower.txt -> tower MUST report WaitingForMember(pi), NEVER UpToDate!
    let all_s7 = ["tower3", "pi3"];
    fs::create_dir_all(t.join("tower3")).unwrap();
    fs::create_dir_all(t.join("pi3")).unwrap();
    fs::write(t.join("pi3/p.txt"), b"p").unwrap();
    let mut tower3 = open_real(&t.join("tower3"), &t.join("tower3.redb"), "tower3", &all_s7);
    let mut pi3 = open_real(&t.join("pi3"), &t.join("pi3.redb"), "pi3", &all_s7);
    pi3.scan(ts(0)).unwrap();
    assert!(sync_all(&mut [&mut tower3, &mut pi3], 1).is_empty());
    fs::write(t.join("tower3/only-on-tower.txt"), b"new").unwrap();
    drop(tower3);
    let (mut tower3, _) = SyncEngine::rebuild_redb_from_disk(
        cfg("tower3", &all_s7),
        RealFs::open(t.join("tower3")).unwrap(),
        t.join("tower3.redb"),
        token_store_for(&t.join("tower3"), "tower3"),
        ts(10),
    )
    .unwrap();
    assert!(pull(&mut tower3, &pi3, 12).is_none());
    assert!(!t.join("pi3/only-on-tower.txt").exists());
    let s7 = tower3.status_at(ts(13)).unwrap();
    assert_eq!(
        s7.state,
        MemberState::WaitingForMember {
            members: vec!["pi3".to_string()],
        },
        "S7: rebuilt tower holding only-on-tower.txt must report WaitingForMember, got {:?} | {}",
        s7.state,
        s7.summary
    );

    // S8 (>4,096 files): status paginates across pages instead of taking a seq_gap > 4096 shortcut.
    // Populate 4,200 records authored by `pi` + 1 relayed record authored by `laptop`: status must
    // count exactly 1 pending outgoing record (not 4,201 and not 0).
    let s8_dir = t.join("s8_tower");
    fs::create_dir_all(&s8_dir).unwrap();
    let s8_cfg = FolderConfig::new("s8", "tower", ["tower".to_string(), "pi".to_string()]);
    let mut mem = MemoryIndexStore::new();
    let mut meta = FolderMeta::new(&s8_cfg, 77, "tok-s8".to_string());
    meta.join_mode = false;
    let mut batch = StoreBatch::default();
    let pi_inc = 88u64;
    let lap_inc = 99u64;
    for i in 1..=4_200u64 {
        let seq = meta.next_seq();
        let mut rec = IndexRecord {
            path: format!("f{i:05}.txt"),
            kind: EntryKind::File,
            size: 1,
            mtime: ts(1),
            disk_mtime: None,
            disk_change_id: None,
            mode: Some(0o644),
            hash: Some(hash_bytes(b"x")),
            link_target: None,
            deleted: false,
            provenance: RecordProvenance::Versioned(VersionVector::singleton(
                VersionKey::new("pi", pi_inc),
                i,
            )),
            base_hash: None,
            modified_by: "pi".to_string(),
            seq,
            recent_hashes: Vec::new(),
            conflict_of: None,
            conflict_dismissed: false,
        };
        let chain = meta.advance_chain_fingerprint(seq, rec.head_fingerprint());
        batch.upsert_seq_fingerprints.push((seq, chain));
        rec.seq = seq;
        batch.upsert_records.push(rec);
    }
    // Add 1 relayed file authored by a removed member `laptop`.
    let relay_seq = meta.next_seq();
    let relay_rec = IndexRecord {
        path: "relayed.txt".to_string(),
        kind: EntryKind::File,
        size: 1,
        mtime: ts(2),
        disk_mtime: None,
        disk_change_id: None,
        mode: Some(0o644),
        hash: Some(hash_bytes(b"r")),
        link_target: None,
        deleted: false,
        provenance: RecordProvenance::Versioned(VersionVector::singleton(
            VersionKey::new("laptop", lap_inc),
            1,
        )),
        base_hash: None,
        modified_by: "laptop".to_string(),
        seq: relay_seq,
        recent_hashes: Vec::new(),
        conflict_of: None,
        conflict_dismissed: false,
    };
    let chain = meta.advance_chain_fingerprint(relay_seq, relay_rec.head_fingerprint());
    batch.upsert_seq_fingerprints.push((relay_seq, chain));
    batch.upsert_records.push(relay_rec);
    batch.folder_meta = Some(meta);
    batch.set_peer_cursors.insert(
        "pi".to_string(),
        PeerCursor {
            incarnation: pi_inc,
            seq: 4_200,
            applied_ack_seq: 4_200,
            last_seen_snapshot_seq: 4_200,
            last_synced_at: Some(ts(2)),
            ..PeerCursor::default()
        },
    );
    mem.apply_batch(batch).unwrap();
    let s8_fs = RealFs::open(&s8_dir).unwrap();
    legion_sync::SyncFs::write_marker(
        &s8_fs,
        &legion_sync::FolderMarker::new("s8".into(), "tower".into(), 77),
    )
    .unwrap();
    let s8_tokens = MemoryInstanceTokenStore::new();
    legion_sync::InstanceTokenStore::write_token(&s8_tokens, "s8", "tok-s8").unwrap();
    let s8_eng = SyncEngine::open(s8_cfg, s8_fs, mem, s8_tokens).unwrap();
    let s8_st = s8_eng.status_at(ts(3)).unwrap();
    assert_eq!(
        s8_st.state,
        MemberState::WaitingForMember {
            members: vec!["pi".to_string()],
        }
    );
    assert_eq!(
        s8_st
            .members
            .iter()
            .find(|m| m.member == "pi")
            .unwrap()
            .pending_outgoing,
        1,
        "paginated status over >4,096 records must count exactly the 1 relayed record"
    );
}

/// Review 5 Probe G: N=10 consecutive index rebuilds with NO edits leave EVERY untouched file's,
/// directory's, and committed deletion's version vector completely unchanged (zero key growth!),
/// both after normal sync AND even while a 3rd member is offline.
#[test]
fn review5_g_rebuild_without_edits_leaves_every_vector_unchanged() {
    let tmp = scratch();
    let t = tmp.path();
    let all = ["tower", "pi"];
    fs::create_dir_all(t.join("tower/sub")).unwrap();
    fs::create_dir_all(t.join("pi")).unwrap();
    // Pre-initial-scan file (born under `join_mode = true`).
    fs::write(t.join("tower/u.txt"), b"untouched").unwrap();
    fs::write(t.join("tower/sub/nested.txt"), b"nested").unwrap();
    fs::write(t.join("tower/deleted.txt"), b"bye").unwrap();
    let mut tower = open_real(&t.join("tower"), &t.join("tower.redb"), "tower", &all);
    let mut pi = open_real(&t.join("pi"), &t.join("pi.redb"), "pi", &all);
    tower.scan(ts(0)).unwrap();
    // Post-initial-scan file and deletion.
    fs::write(t.join("tower/post.txt"), b"post-join").unwrap();
    fs::remove_file(t.join("tower/deleted.txt")).unwrap();
    tower.scan(ts(1)).unwrap();
    tower.flush_settled_deletions(ts(60)).unwrap();
    assert!(sync_all(&mut [&mut tower, &mut pi], 61).is_empty());

    let expected_u = tower
        .store()
        .get_record("u.txt")
        .unwrap()
        .unwrap()
        .provenance;
    let expected_sub = tower.store().get_record("sub").unwrap().unwrap().provenance;
    let expected_nested = tower
        .store()
        .get_record("sub/nested.txt")
        .unwrap()
        .unwrap()
        .provenance;
    let expected_post = tower
        .store()
        .get_record("post.txt")
        .unwrap()
        .unwrap()
        .provenance;
    let expected_del = tower
        .store()
        .get_record("deleted.txt")
        .unwrap()
        .unwrap()
        .provenance;
    assert_eq!(expected_u, RecordProvenance::Unversioned);
    assert_eq!(expected_post.to_version_vector().len(), 1);
    assert_eq!(expected_del.to_version_vector().len(), 1);

    for r in 0..10i64 {
        drop(pi);
        let (e, _) = SyncEngine::rebuild_redb_from_disk(
            cfg("pi", &all),
            RealFs::open(t.join("pi")).unwrap(),
            t.join("pi.redb"),
            token_store_for(&t.join("pi"), "pi"),
            ts(100 + 10 * r),
        )
        .unwrap();
        pi = e;
        assert!(sync_all(&mut [&mut tower, &mut pi], 105 + 10 * r).is_empty());

        for (name, expected) in [
            ("u.txt", &expected_u),
            ("sub", &expected_sub),
            ("sub/nested.txt", &expected_nested),
            ("post.txt", &expected_post),
            ("deleted.txt", &expected_del),
        ] {
            let tv = tower.store().get_record(name).unwrap().unwrap().provenance;
            let pv = pi.store().get_record(name).unwrap().unwrap().provenance;
            assert_eq!(
                &tv, expected,
                "rebuild {r}: tower's provenance for {name} changed ({expected:?} -> {tv:?})"
            );
            assert_eq!(
                &pv, expected,
                "rebuild {r}: pi's provenance for {name} changed ({expected:?} -> {pv:?})"
            );
        }
    }

    // Also verify rebuilding while a 3rd member `nas` is offline (so both tower and pi hold
    // `pre.txt` as `Unversioned`): provenance stays `Unversioned` (0 keys) across 5 rebuilds!
    let three = ["tower3", "pi3", "nas3"];
    for n in three {
        fs::create_dir_all(t.join(n)).unwrap();
    }
    fs::write(t.join("tower3/pre.txt"), b"pre").unwrap();
    let mut tower3 = open_real(&t.join("tower3"), &t.join("tower3.redb"), "tower3", &three);
    let mut pi3 = open_real(&t.join("pi3"), &t.join("pi3.redb"), "pi3", &three);
    tower3.scan(ts(0)).unwrap();
    assert!(sync_all(&mut [&mut tower3, &mut pi3], 1).is_empty());
    for r in 0..5i64 {
        drop(pi3);
        let (e, _) = SyncEngine::rebuild_redb_from_disk(
            cfg("pi3", &three),
            RealFs::open(t.join("pi3")).unwrap(),
            t.join("pi3.redb"),
            token_store_for(&t.join("pi3"), "pi3"),
            ts(50 + 10 * r),
        )
        .unwrap();
        pi3 = e;
        assert!(sync_all(&mut [&mut tower3, &mut pi3], 55 + 10 * r).is_empty());
        let p = tower3
            .store()
            .get_record("pre.txt")
            .unwrap()
            .unwrap()
            .provenance;
        assert_eq!(
            p,
            RecordProvenance::Unversioned,
            "rebuild {r} with offline 3rd member grew pre.txt provenance to {p:?}"
        );
    }
}

/// Review 5 Item 5a: byte-bounded pages split cleanly by `max_bytes` even when `max_records` is
/// large, always admit at least one record so a single large record never stalls, and reject
/// oversized multi-record pages (> `MAX_PAGE_BYTES`) on the receiver.
#[test]
fn review5_byte_bounded_pages_split_and_never_deadlock() {
    let tmp = scratch();
    let t = tmp.path();
    let all = ["tower", "pi"];
    fs::create_dir_all(t.join("tower")).unwrap();
    fs::create_dir_all(t.join("pi")).unwrap();
    for i in 0..10 {
        let name = format!("long_directory_prefix_{i:02}_abcdefghijklmnopqrstuvwxyz.txt");
        fs::write(t.join("tower").join(name), format!("body {i}")).unwrap();
    }
    let mut tower = open_real(&t.join("tower"), &t.join("tower.redb"), "tower", &all);
    let mut pi = open_real(&t.join("pi"), &t.join("pi.redb"), "pi", &all);
    tower.scan(ts(0)).unwrap();

    // 1) Request max_records = 1000, max_bytes = 700 -> splits into multi-page chunks by bytes!
    let p1 = tower
        .serve_records_since("pi", 0, 0, PageLimit::new(1000, 700))
        .unwrap();
    assert!(p1.has_more, "small byte budget must split page");
    assert!(
        p1.records.len() < 10 && !p1.records.is_empty(),
        "expected byte-bounded partial page, got {} records",
        p1.records.len()
    );
    assert!(
        p1.estimated_wire_bytes() <= 700 || p1.records.len() == 1,
        "page exceeded byte budget: {} bytes for {} records",
        p1.estimated_wire_bytes(),
        p1.records.len()
    );

    // 2) Tiny byte budget (1 byte): must STILL admit 1 record per page (never deadlock!).
    let p_one = tower
        .serve_records_since("pi", 0, 0, PageLimit::new(1000, 1))
        .unwrap();
    assert_eq!(p_one.records.len(), 1);
    assert!(p_one.has_more);

    // 3) Full pull under a 1-byte page budget still converges all 10 files across 10 pages!
    pi.pull_from_peer_with_limit(
        &EnginePeer::new(&tower, "pi"),
        PageLimit::new(1000, 1),
        ts(1),
    )
    .unwrap();
    for i in 0..10 {
        let name = format!("long_directory_prefix_{i:02}_abcdefghijklmnopqrstuvwxyz.txt");
        assert_eq!(
            fs::read_to_string(t.join("pi").join(name)).unwrap(),
            format!("body {i}")
        );
    }

    // 4) Receiver rejects a malicious peer page whose estimated wire size exceeds MAX_PAGE_BYTES.
    struct OversizedPagePeer {
        records: Vec<IndexRecord>,
    }
    impl Peer for OversizedPagePeer {
        fn member_id(&self) -> &str {
            "tower"
        }
        fn handshake(&self, _c: &HandshakeHello) -> legion_sync::Result<PeerHandshake> {
            Ok(PeerHandshake {
                folder_id: "docs".to_string(),
                member_id: "tower".to_string(),
                incarnation: 99,
                snapshot_seq: self.records.len() as u64,
                head_fingerprint: None,
                protocol_version: SYNC_PROTOCOL_VERSION,
                caller_pulled: None,
                caller_acked: None,
                caller_pull_matches: None,
                caller_rejoining: false,
                refused_from_caller: None,
            })
        }
        fn records_since(
            &self,
            _inc: u64,
            _seq: u64,
            _limit: PageLimit,
            _cancel: Option<&AtomicBool>,
        ) -> legion_sync::Result<RecordsPage> {
            Ok(RecordsPage {
                incarnation: 99,
                records: self.records.clone(),
                snapshot_seq: self.records.len() as u64,
                has_more: false,
                caller_acked: None,
                caller_acked_fingerprint: None,
                head_fingerprint: None,
                chain_fingerprints: Default::default(),
            })
        }
        fn fetch_file(
            &self,
            _p: &str,
            _h: Hash,
            _o: &FetchOptions,
        ) -> legion_sync::Result<Box<dyn Read + Send>> {
            unreachable!()
        }
    }

    let template = tower
        .store()
        .get_record("long_directory_prefix_00_abcdefghijklmnopqrstuvwxyz.txt")
        .unwrap()
        .unwrap()
        .for_wire();
    // Build 1,500 records (~3 KiB each -> > 4 MiB total, while < MAX_PAGE_RECORDS = 4,096).
    let mut big_records = Vec::with_capacity(1_500);
    for i in 1..=1_500u64 {
        let mut r = template.clone();
        r.seq = i;
        r.path = format!("{}/f{i}.txt", "a".repeat(3_000));
        big_records.push(r);
    }
    let probe_page = RecordsPage {
        incarnation: 99,
        records: big_records.clone(),
        snapshot_seq: 1_500,
        has_more: false,
        caller_acked: None,
        caller_acked_fingerprint: None,
        head_fingerprint: None,
        chain_fingerprints: Default::default(),
    };
    assert!(probe_page.estimated_wire_bytes() > MAX_PAGE_BYTES);

    fs::create_dir_all(t.join("pi_fresh")).unwrap();
    let mut pi_fresh = open_real(&t.join("pi_fresh"), &t.join("pi_fresh.redb"), "pi", &all);
    pi_fresh
        .pull_from_peer(
            &OversizedPagePeer {
                records: big_records,
            },
            ts(5),
        )
        .unwrap();
    let probs = pi_fresh.store().list_problems().unwrap();
    assert!(
        probs.iter().any(|(_, p)| {
            p.kind == ProblemKind::InvalidPeerRecord
                && p.sentence.contains("exceeding limit")
                && p.sentence.contains("bytes")
        }),
        "receiver must refuse multi-record page exceeding MAX_PAGE_BYTES, got {probs:?}"
    );
}

/// Review 5 Items 5b & 5c: audit the REAL `RealFs` durability trace across every mutating
/// operation, and verify on Unix that `sync_dir` propagates errors instead of swallowing them.
#[test]
fn review5_real_fs_durability_trace_audit_and_sync_dir_error_propagation() {
    let tmp = scratch();
    let audit_dir = tmp.path().join("audit_root");
    let audit = audit_real_fs_durability(&audit_dir).unwrap();
    assert!(
        audit.sync_created_ancestors,
        "RealFs must sync every newly created ancestor directory's parent"
    );
    assert!(
        audit.sync_remove_tmp,
        "RealFs must sync staging directory after remove_tmp"
    );
    assert!(
        audit.unsynced_ops.is_empty(),
        "RealFs left unsynced directory mutations: {:?}",
        audit.unsynced_ops
    );

    #[cfg(unix)]
    {
        use legion_sync::SyncFs;
        // Calling `sync_parent_dir_live` on a missing directory MUST return Err (not swallow it).
        let fs = RealFs::open(&audit_dir).unwrap();
        let err = fs.sync_parent_dir_live("does-not-exist/child.txt");
        assert!(
            err.is_err(),
            "sync_parent_dir_live must propagate directory open/sync error on Unix, got {err:?}"
        );
    }
}
