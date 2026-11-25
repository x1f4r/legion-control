//! Property-based tests for `VersionVector` causal ordering, semilattice merge laws, and
//! deterministic conflict winner ordering.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::cmp::Ordering;

use legion_sync::{IndexRecord, VersionKey, VersionOrder, VersionVector, compare_live_winner};
use legion_xfer::hash::hash_bytes;
use legion_xfer::{EntryKind, Timestamp};
use proptest::prelude::*;

fn member_name() -> impl Strategy<Value = String> {
    prop::sample::select(vec![
        "alpha".to_string(),
        "beta".to_string(),
        "gamma".to_string(),
        "delta".to_string(),
        "epsilon".to_string(),
        "zeta".to_string(),
    ])
}

fn version_key() -> impl Strategy<Value = VersionKey> {
    (member_name(), 1u64..=3).prop_map(|(m, inc)| VersionKey::new(m, inc))
}

fn version_vector() -> impl Strategy<Value = VersionVector> {
    prop::collection::btree_map(version_key(), 0u64..8, 0..=6).prop_map(VersionVector::from_map)
}

fn live_record() -> impl Strategy<Value = IndexRecord> {
    (
        -1_000_000i64..2_000_000_000i64,
        0u32..1_000_000_000u32,
        member_name(),
        prop::collection::vec(any::<u8>(), 0..32),
    )
        .prop_map(|(secs, nanos, member, bytes)| IndexRecord {
            path: "notes.txt".to_string(),
            kind: EntryKind::File,
            size: bytes.len() as u64,
            mtime: Timestamp::new(secs, nanos),
            disk_mtime: None,
            disk_change_id: None,
            mode: Some(0o644),
            hash: Some(hash_bytes(&bytes)),
            link_target: None,
            deleted: false,
            version: VersionVector::singleton(VersionKey::new(member.clone(), 1), 1),
            modified_by: member,
            seq: 1,
            recent_hashes: Vec::new(),
            from_join: false,
            conflict_of: None,
            conflict_dismissed: false,
        })
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 1000,
        failure_persistence: None,
        ..ProptestConfig::default()
    })]

    #[test]
    fn compare_reflexive_and_antisymmetric(a in version_vector(), b in version_vector()) {
        prop_assert_eq!(a.compare(&a), VersionOrder::Equal);

        match (a.compare(&b), b.compare(&a)) {
            (VersionOrder::Equal, VersionOrder::Equal) => prop_assert_eq!(a, b),
            (VersionOrder::Newer, VersionOrder::Older) => prop_assert_ne!(a, b),
            (VersionOrder::Older, VersionOrder::Newer) => prop_assert_ne!(a, b),
            (VersionOrder::Concurrent, VersionOrder::Concurrent) => prop_assert_ne!(a, b),
            (ab, ba) => prop_assert!(false, "antisymmetry violated: {ab:?} vs {ba:?}"),
        }
    }

    #[test]
    fn compare_transitive(
        a in version_vector(),
        b in version_vector(),
        c in version_vector(),
    ) {
        if a.dominates_or_equal(&b) && b.dominates_or_equal(&c) {
            prop_assert!(a.dominates_or_equal(&c));
        }
        if a.compare(&b) == VersionOrder::Newer && b.dominates_or_equal(&c) {
            prop_assert_eq!(a.compare(&c), VersionOrder::Newer);
        }
        if a.dominates_or_equal(&b) && b.compare(&c) == VersionOrder::Newer {
            prop_assert_eq!(a.compare(&c), VersionOrder::Newer);
        }
    }

    #[test]
    fn merge_forms_join_semilattice(
        a in version_vector(),
        b in version_vector(),
        c in version_vector(),
    ) {
        prop_assert_eq!(a.merge(&a), a.clone());

        let ab = a.merge(&b);
        let ba = b.merge(&a);
        prop_assert_eq!(&ab, &ba);

        prop_assert_eq!(ab.merge(&c), a.merge(&b.merge(&c)));

        prop_assert!(ab.dominates_or_equal(&a));
        prop_assert!(ab.dominates_or_equal(&b));

        if c.dominates_or_equal(&a) && c.dominates_or_equal(&b) {
            prop_assert!(c.dominates_or_equal(&ab));
        }

        prop_assert_eq!(a.dominates_or_equal(&b), ab == a);
    }

    #[test]
    fn canonical_representation_never_contains_zero(a in version_vector(), b in version_vector()) {
        for (_, v) in a.iter() {
            prop_assert!(v > 0);
        }
        let merged = a.merge(&b);
        for (_, v) in merged.iter() {
            prop_assert!(v > 0);
        }
        let json = serde_json::to_string(&merged).unwrap();
        let decoded: VersionVector = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(decoded, merged);
    }

    #[test]
    fn bump_above_strictly_dominates(
        mut a in version_vector(),
        key in version_key(),
        floor in 0u64..100,
    ) {
        let before = a.clone();
        let next = a.bump_above(key.clone(), floor);
        prop_assert!(next > floor);
        prop_assert!(next > before.get(&key));
        prop_assert_eq!(a.compare(&before), VersionOrder::Newer);
    }

    #[test]
    fn live_winner_order_is_total_and_transitive(
        a in live_record(),
        b in live_record(),
        c in live_record(),
    ) {
        prop_assert_eq!(compare_live_winner(&a, &a), Ordering::Equal);
        prop_assert_eq!(compare_live_winner(&a, &b), compare_live_winner(&b, &a).reverse());

        let ab = compare_live_winner(&a, &b);
        let bc = compare_live_winner(&b, &c);
        let ac = compare_live_winner(&a, &c);
        if ab != Ordering::Greater && bc != Ordering::Greater {
            prop_assert_ne!(ac, Ordering::Greater);
        }
        if ab == Ordering::Less && bc != Ordering::Greater {
            prop_assert_eq!(ac, Ordering::Less);
        }
    }
}
