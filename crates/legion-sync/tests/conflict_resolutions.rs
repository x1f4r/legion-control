//! Real-disk safety regression tests: conflicts resolved differently on three members.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod safety_common;

use std::collections::BTreeSet;
use std::fs;
use std::path::Path;

use legion_sync::{ConflictChoice, RealFs, RedbIndexStore, SyncEngine};
use safety_common::*;

type E = SyncEngine<RealFs, RedbIndexStore>;

fn sync_all(es: &mut [E], t0: i64, rounds: i64) {
    for r in 0..rounds {
        for i in 0..es.len() {
            for j in 0..es.len() {
                if i == j {
                    continue;
                }
                let (a, b) = if i < j {
                    let (l, rr) = es.split_at_mut(j);
                    (&mut l[i], &rr[0])
                } else {
                    let (l, rr) = es.split_at_mut(i);
                    (&mut rr[0], &l[j])
                };
                let caller = a.meta().unwrap().local_member;
                let _ = a.scan(ts(t0 + r));
                let res = a.pull_from_peer(&EnginePeer::new(b, &caller), ts(t0 + r));
                if let Err(e) = res {
                    println!("pull {caller} <- {}: {e}", b.meta().unwrap().local_member);
                }
            }
        }
    }
}

fn live(dir: &Path) -> BTreeSet<(String, Vec<u8>)> {
    all_bytes(dir).into_iter().filter(|(p, _)| !p.starts_with(".legion-sync")).collect()
}

fn run(choices: [Option<ConflictChoice>; 3], edit_copy_on: Option<usize>) {
    let tmp = scratch();
    let t = tmp.path();
    let names = ["tower", "pi", "nas"];
    let mut es: Vec<E> = names
        .iter()
        .map(|n| open_real(&t.join(n), &t.join(format!("{n}.redb")), n, &names))
        .collect();
    fs::create_dir_all(t.join("tower")).unwrap();
    fs::write(t.join("tower/doc.txt"), b"base").unwrap();
    sync_all(&mut es, 0, 2);
    fs::write(t.join("tower/doc.txt"), b"tower's edit").unwrap();
    fs::write(t.join("pi/doc.txt"), b"pi's edit!!").unwrap();
    fs::write(t.join("nas/doc.txt"), b"nas's edit???").unwrap();
    sync_all(&mut es, 10, 3);
    let mut versions = vec![b"tower's edit".to_vec(), b"pi's edit!!".to_vec(), b"nas's edit???".to_vec()];
    for (i, c) in choices.iter().enumerate() {
        let conflicts = es[i].list_conflicts().unwrap();
        println!("{} sees {} conflicts", names[i], conflicts.len());
        if let Some(c) = c
            && let Some(first) = conflicts.first()
        {
            let r = es[i].resolve_conflict(&first.conflict_path, *c, ts(20));
            println!("{} resolves {:?} on {}: {r:?}", names[i], c, first.conflict_path);
        }
    }
    if let Some(i) = edit_copy_on {
        let cs = es[i].list_conflicts().unwrap();
        if let Some(c) = cs.last() {
            let p = t.join(names[i]).join(&c.conflict_path);
            if p.exists() {
                fs::write(&p, b"edited the copy").unwrap();
                versions.push(b"edited the copy".to_vec());
            }
        }
    }
    sync_all(&mut es, 30, 4);
    let l0 = live(&t.join("tower"));
    for (i, n) in names.iter().enumerate() {
        let l = live(&t.join(n));
        println!("{n} live: {:?}", l.iter().map(|(p, b)| format!("{p}={}", String::from_utf8_lossy(b))).collect::<Vec<_>>());
        assert_eq!(l, l0, "{n} differs from tower");
        let _ = i;
    }
    for v in versions {
        let found: Vec<_> = names.iter().flat_map(|n| where_is(&t.join(n), &v)).collect();
        assert!(!found.is_empty(), "{:?} lost", String::from_utf8_lossy(&v));
    }
}

#[test]
fn c1_three_way_resolved_differently() {
    run([Some(ConflictChoice::KeepOriginal), Some(ConflictChoice::KeepConflict), Some(ConflictChoice::KeepBoth)], None);
}

#[test]
fn c2_resolve_while_other_edits_copy() {
    run([Some(ConflictChoice::KeepConflict), None, None], Some(2));
}

#[test]
fn c3_both_promote_different_copies() {
    run([Some(ConflictChoice::KeepConflict), Some(ConflictChoice::KeepConflict), Some(ConflictChoice::KeepOriginal)], Some(1));
}
