//! Real-disk safety tests for lying-peer defenses, symlink containment, durable problems,
//! and forced-rotation rate limiting (Items 4 & 9).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod safety_common;

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::fs;
use std::io::{Cursor, Read};
use std::sync::atomic::AtomicBool;

use legion_sync::{
    FORCED_ROTATION_COOLDOWN_SECS, FetchOptions, HandshakeHello, IndexRecord, IndexStore,
    MAX_VERSION_COUNTER, MemberState, Peer, PeerAckView, PeerHandshake, ProblemKind, RecordsPage,
    SYNC_PROTOCOL_VERSION, SyncError, VersionKey, VersionVector,
};
use legion_xfer::hash::hash_bytes;
use legion_xfer::{EntryKind, Hash, Timestamp};
use safety_common::*;

struct LyingPeer {
    id: String,
    incarnation: u64,
    snapshot_seq: u64,
    caller_pulled: Option<PeerAckView>,
    caller_acked: Option<PeerAckView>,
    pages: RefCell<Vec<RecordsPage>>,
    payloads: BTreeMap<String, Vec<u8>>,
}

impl Peer for LyingPeer {
    fn member_id(&self) -> &str {
        &self.id
    }

    fn handshake(&self, _caller: &HandshakeHello) -> legion_sync::Result<PeerHandshake> {
        Ok(PeerHandshake {
            folder_id: "docs".to_string(),
            member_id: self.id.clone(),
            incarnation: self.incarnation,
            snapshot_seq: self.snapshot_seq,
            head_fingerprint: None,
            protocol_version: SYNC_PROTOCOL_VERSION,
            caller_pulled: self.caller_pulled,
            caller_acked: self.caller_acked,
            caller_pull_matches: Some(true),
            caller_regressed: false,
            caller_rejoining: false,
            refused_from_caller: None,
            caller_evidence_record: None,
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
                records: Vec::new(),
                snapshot_seq: self.snapshot_seq,
                has_more: false,
                caller_acked: self.caller_acked.map(|v| (v.incarnation, v.seq)),
                caller_acked_fingerprint: self.caller_acked.and_then(|v| v.fingerprint),
                head_fingerprint: None,
                chain_fingerprints: BTreeMap::new(),
            })
        } else {
            Ok(p.remove(0))
        }
    }

    fn fetch_file(
        &self,
        path: &str,
        _expected_hash: Hash,
        options: &FetchOptions,
    ) -> legion_sync::Result<Box<dyn Read + Send>> {
        let bytes = self
            .payloads
            .get(path)
            .cloned()
            .ok_or_else(|| SyncError::NotFound {
                what: path.to_string(),
            })?;
        let start = (options.offset as usize).min(bytes.len());
        Ok(Box::new(Cursor::new(bytes[start..].to_vec())))
    }
}

fn file_record(
    path: &str,
    data: &[u8],
    version: VersionVector,
    modified_by: &str,
    seq: u64,
    mtime: Timestamp,
) -> IndexRecord {
    IndexRecord {
        path: path.to_string(),
        kind: EntryKind::File,
        size: data.len() as u64,
        mtime,
        disk_mtime: None,
        disk_change_id: None,
        mode: Some(0o644),
        hash: Some(hash_bytes(data)),
        link_target: None,
        deleted: false,
        version,
        modified_by: modified_by.to_string(),
        seq,
        recent_hashes: Vec::new(),
        from_join: false,
        conflict_of: None,
        conflict_dismissed: false,
    }
}

#[test]
fn forged_own_counter_and_huge_counter_are_refused_unacked_and_durable() {
    let tmp = scratch();
    let t = tmp.path();
    let all = ["tower", "evil"];
    let mut tower = open_real(&t.join("tower"), &t.join("tower.redb"), "tower", &all);

    fs::create_dir_all(t.join("tower")).unwrap();
    fs::write(t.join("tower/doc.txt"), b"tower original").unwrap();
    tower.scan(ts(0)).unwrap();

    let tower_meta = tower.meta().unwrap();
    let tower_inc = tower_meta.incarnation;
    let tower_clock = tower_meta.local_clock;
    assert!(tower_clock >= 1);

    // 1. Lying peer tries to raise tower's own `(tower, tower_inc)` counter above `tower_clock`
    //    (e.g. `tower_clock + 1` and `u64::MAX`).
    let mut forged_own_vv = VersionVector::new();
    forged_own_vv.set(VersionKey::new("tower", tower_inc), tower_clock + 50);
    forged_own_vv.set(VersionKey::new("evil", 7), 1);
    let rec1 = file_record(
        "doc.txt",
        b"poisoned own clock",
        forged_own_vv,
        "evil",
        1,
        ts(5),
    );

    // 2. Lying peer sends a counter above `2^62` for its own key.
    let mut huge_vv = VersionVector::new();
    huge_vv.set(VersionKey::new("evil", 7), MAX_VERSION_COUNTER + 1);
    let rec2 = file_record("huge.txt", b"huge counter", huge_vv, "evil", 2, ts(5));

    // 3. Honest file after the two forged records must still apply!
    let mut good_vv = VersionVector::new();
    good_vv.set(VersionKey::new("evil", 7), 3);
    let rec3 = file_record("good.txt", b"valid bytes", good_vv, "evil", 3, ts(5));

    let mut payloads = BTreeMap::new();
    payloads.insert("doc.txt".into(), b"poisoned own clock".to_vec());
    payloads.insert("huge.txt".into(), b"huge counter".to_vec());
    payloads.insert("good.txt".into(), b"valid bytes".to_vec());

    let peer = LyingPeer {
        id: "evil".into(),
        incarnation: 7,
        snapshot_seq: 3,
        caller_pulled: None,
        caller_acked: None,
        pages: RefCell::new(vec![RecordsPage {
            incarnation: 7,
            records: vec![rec1, rec2, rec3],
            snapshot_seq: 3,
            has_more: false,
            caller_acked: None,
            caller_acked_fingerprint: None,
            head_fingerprint: None,
            chain_fingerprints: BTreeMap::new(),
        }]),
        payloads,
    };

    let rep = tower.pull_from_peer(&peer, ts(10)).unwrap();
    assert_eq!(rep.invalid_skipped, 2);
    assert_eq!(
        fs::read(t.join("tower/doc.txt")).unwrap(),
        b"tower original"
    );
    assert!(!t.join("tower/huge.txt").exists());
    assert_eq!(fs::read(t.join("tower/good.txt")).unwrap(), b"valid bytes");

    // Pull position advanced to 3, but applied acknowledgement MUST remain 0 (before seq 1)!
    let cursor = tower
        .store()
        .load_peer_cursors()
        .unwrap()
        .remove("evil")
        .unwrap();
    assert_eq!(cursor.seq, 3);
    assert_eq!(cursor.applied_ack_seq, 0);

    // Problems persist across restart!
    drop(tower);
    let tower = open_real(&t.join("tower"), &t.join("tower.redb"), "tower", &all);
    match tower.status().unwrap().state {
        MemberState::Problem { problems, .. } => {
            let invalid: Vec<_> = problems
                .iter()
                .filter(|p| p.kind == ProblemKind::InvalidPeerRecord)
                .collect();
            assert_eq!(
                invalid.len(),
                2,
                "both forged records must stay reported after restart"
            );
        }
        other => panic!("expected durable InvalidPeerRecord problems after restart, got {other:?}"),
    }
}

#[test]
fn peer_forced_rotation_requires_evidence_and_is_budgeted_per_claimant() {
    let tmp = scratch();
    let t = tmp.path();
    let all = ["tower", "evil1", "honest"];
    let mut tower = open_real(&t.join("tower"), &t.join("tower.redb"), "tower", &all);

    fs::create_dir_all(t.join("tower")).unwrap();
    fs::write(t.join("tower/keep.txt"), b"keep").unwrap();
    tower.scan(ts(0)).unwrap();
    let inc1 = tower.meta().unwrap().incarnation;

    // 1. A peer citing a sequence BEYOND tower's local_seq with NO record evidence rotates nothing.
    let no_evidence = LyingPeer {
        id: "evil1".into(),
        incarnation: 10,
        snapshot_seq: 0,
        caller_pulled: Some(PeerAckView {
            incarnation: inc1,
            seq: 999,
            fingerprint: Some(hash_bytes(b"fake-fp")),
        }),
        caller_acked: None,
        pages: RefCell::new(Vec::new()),
        payloads: BTreeMap::new(),
    };
    tower.pull_from_peer(&no_evidence, ts(50)).unwrap();
    assert_eq!(
        tower.meta().unwrap().incarnation,
        inc1,
        "claim citing seq beyond local_seq without record evidence must not rotate"
    );

    // 2. A peer presenting checkable evidence (mismatched fingerprint at seq=1 which tower HAS)
    //    forces rotation once per day and reports the honest backup-restore sentence.
    let liar_at_seq1 = |id: &str, peer_inc: u64, target_inc: u64, tag: &[u8]| LyingPeer {
        id: id.into(),
        incarnation: peer_inc,
        snapshot_seq: 0,
        caller_pulled: Some(PeerAckView {
            incarnation: target_inc,
            seq: 1,
            fingerprint: Some(hash_bytes(tag)),
        }),
        caller_acked: None,
        pages: RefCell::new(Vec::new()),
        payloads: BTreeMap::new(),
    };
    tower
        .pull_from_peer(&liar_at_seq1("evil1", 10, inc1, b"fake-fp-1"), ts(100))
        .unwrap();
    let inc2 = tower.meta().unwrap().incarnation;
    assert_ne!(
        inc2, inc1,
        "first evidence-backed rollback claim should rotate incarnation"
    );
    match tower.status().unwrap().state {
        MemberState::Problem { problems, .. } => {
            assert!(
                problems.iter().any(|p| {
                    p.kind == ProblemKind::PeerForcedRotation
                        && p.sentence.contains("restored from a backup")
                        && p.sentence.contains("Nothing is deleted.")
                }),
                "forced rotation must be reported with backup-restore sentence, got {problems:?}"
            );
        }
        other => panic!("expected PeerForcedRotation problem, got {other:?}"),
    }

    // 3. Second claim 2 hours later (< 1 day) from evil1: refused and rate-limited (no rotation!).
    let err = tower.pull_from_peer(
        &liar_at_seq1("evil1", 10, inc2, b"fake-fp-2"),
        ts(100 + 7200),
    );
    assert!(matches!(err, Err(SyncError::Conflict { .. })));
    assert_eq!(
        tower.meta().unwrap().incarnation,
        inc2,
        "second forced rotation within 1 day must be refused"
    );
    match tower.status().unwrap().state {
        MemberState::Problem { problems, .. } => {
            let rl = problems
                .iter()
                .find(|p| p.kind == ProblemKind::PeerRollbackRateLimited)
                .expect("rate-limited rotation attempt must be reported");
            assert!(
                rl.sentence
                    .contains("evil1 keeps reporting that this machine's history went back"),
                "unexpected rate-limit sentence: {}",
                rl.sentence
            );
        }
        other => panic!("expected PeerRollbackRateLimited problem, got {other:?}"),
    }

    // 4. evil1 uses its remaining 2 per-peer weekly slots on days 2 and 3 (total 3 for evil1).
    let day = FORCED_ROTATION_COOLDOWN_SECS as i64;
    tower
        .pull_from_peer(
            &liar_at_seq1("evil1", 10, inc2, b"fake-fp-d2"),
            ts(100 + day + 1),
        )
        .unwrap();
    let inc3 = tower.meta().unwrap().incarnation;
    assert_ne!(inc3, inc2);

    tower
        .pull_from_peer(
            &liar_at_seq1("evil1", 10, inc3, b"fake-fp-d3"),
            ts(100 + 2 * day + 2),
        )
        .unwrap();
    let inc4 = tower.meta().unwrap().incarnation;
    assert_ne!(inc4, inc3);

    // 5. Fourth claim from evil1 inside 7 days is blocked by evil1's per-peer weekly budget!
    assert!(matches!(
        tower.pull_from_peer(
            &liar_at_seq1("evil1", 10, inc4, b"fake-fp-d4"),
            ts(100 + 3 * day + 3)
        ),
        Err(SyncError::Conflict { .. })
    ));
    assert_eq!(
        tower.meta().unwrap().incarnation,
        inc4,
        "4th rotation from evil1 within 7 days must be refused"
    );

    // 6. Budgets are strictly per claimant: honest peer can STILL recover immediately on day 4!
    tower
        .pull_from_peer(
            &liar_at_seq1("honest", 20, inc4, b"honest-fp-d4"),
            ts(100 + 3 * day + 10),
        )
        .unwrap();
    assert_ne!(
        tower.meta().unwrap().incarnation,
        inc4,
        "evil1 exhausting its own budget must never block honest peer recovery"
    );
}

#[cfg(unix)]
#[test]
fn inbound_escaping_and_absolute_symlinks_are_held_back_unacked() {
    let tmp = scratch();
    let t = tmp.path();
    let all = ["tower", "evil"];
    let mut tower = open_real(&t.join("tower"), &t.join("tower.redb"), "tower", &all);

    let mk_sym = |path: &str, target: &str, seq: u64| IndexRecord {
        path: path.to_string(),
        kind: EntryKind::Symlink,
        size: 0,
        mtime: ts(5),
        disk_mtime: None,
        disk_change_id: None,
        mode: None,
        hash: None,
        link_target: Some(target.to_string()),
        deleted: false,
        version: VersionVector::singleton(VersionKey::new("evil", 42), seq),
        modified_by: "evil".to_string(),
        seq,
        recent_hashes: Vec::new(),
        from_join: false,
        conflict_of: None,
        conflict_dismissed: false,
    };

    let peer = LyingPeer {
        id: "evil".into(),
        incarnation: 42,
        snapshot_seq: 4,
        caller_pulled: None,
        caller_acked: None,
        pages: RefCell::new(vec![RecordsPage {
            incarnation: 42,
            records: vec![
                mk_sym("abs_link", "/etc/passwd", 1),
                mk_sym("sub/esc_link", "../../outside.txt", 2),
                mk_sym("meta_link", ".legion-sync/marker.json", 3),
                mk_sym("sub/safe_link", "../inside.txt", 4),
            ],
            snapshot_seq: 4,
            has_more: false,
            caller_acked: None,
            caller_acked_fingerprint: None,
            head_fingerprint: None,
            chain_fingerprints: BTreeMap::new(),
        }]),
        payloads: BTreeMap::new(),
    };

    let rep = tower.pull_from_peer(&peer, ts(10)).unwrap();
    assert_eq!(rep.held_back, 3);
    assert_eq!(rep.applied, 1);

    // Only the safe relative in-folder symlink was created on disk!
    assert!(fs::symlink_metadata(t.join("tower/abs_link")).is_err());
    assert!(fs::symlink_metadata(t.join("tower/sub/esc_link")).is_err());
    assert!(fs::symlink_metadata(t.join("tower/meta_link")).is_err());
    assert_eq!(
        fs::read_link(t.join("tower/sub/safe_link")).unwrap(),
        std::path::PathBuf::from("../inside.txt")
    );

    // Applied ack must stay at 0 because seq 1 is held back!
    let cursor = tower
        .store()
        .load_peer_cursors()
        .unwrap()
        .remove("evil")
        .unwrap();
    assert_eq!(cursor.seq, 4);
    assert_eq!(cursor.applied_ack_seq, 0);

    // Problems persist durably across restart.
    drop(tower);
    let tower = open_real(&t.join("tower"), &t.join("tower.redb"), "tower", &all);
    match tower.status().unwrap().state {
        MemberState::Problem { problems, .. } => {
            let sym_probs: Vec<_> = problems
                .iter()
                .filter(|p| p.kind == ProblemKind::UnsupportedSymlink)
                .collect();
            assert_eq!(sym_probs.len(), 3);
        }
        other => panic!("expected 3 durable UnsupportedSymlink problems, got {other:?}"),
    }
}
