//! Resilience and regression tests covering multi-member joiners, incarnation lifecycle and VV
//! pruning, held-handle bin promotion, local placement hold-back (`ENOSPC`, separate mounts),
//! rebuild during the deletion settle window, inbound deletion-brake batching, serving stat /
//! hash-mismatch rescans, oldest-first deleted-history eviction, and idle fetch timeouts.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, dead_code)]

mod safety_common;

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{self, Read};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[cfg(unix)]
use legion_sync::RestoreStrategy;
use legion_sync::model::MAX_DELETED_HISTORY_PATHS;
use legion_sync::{
    DeletionBrakeDecision, FetchClock, FetchOptions, FsCapabilities, FsErrorKind, FsMeta,
    FsWalkResult, HandshakeHello, IgnoreMatcher, InboundDeletionBrakeDecision, IndexRecord,
    IndexStore, MAX_HELD_BACK_ENTRIES, MemberState, MemoryIndexStore, Peer, PeerHandshake,
    ProblemKind, RealFs, RecordsPage, RedbIndexStore, SYNC_PROTOCOL_VERSION, ScanOptions,
    SyncEngine, SyncError, SyncFs, SyncWrite, VersionKey, VersionVector,
};
use legion_xfer::{EntryKind, Hash, Timestamp};
use safety_common::*;

type E = SyncEngine<RealFs, RedbIndexStore>;

fn worker_count(items: usize) -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        .min(8)
        .min(items.max(1))
}

fn parallel_map<T, R, F>(items: Vec<T>, f: F) -> Vec<R>
where
    T: Send,
    R: Send,
    F: Fn(T) -> R + Sync,
{
    let total = items.len();
    if total == 0 {
        return Vec::new();
    }
    let threads = worker_count(total);
    let queue = Mutex::new(items.into_iter().enumerate());
    let results: Mutex<Vec<(usize, R)>> = Mutex::new(Vec::with_capacity(total));
    std::thread::scope(|s| {
        for _ in 0..threads {
            s.spawn(|| {
                loop {
                    let next = {
                        let mut q = queue.lock().unwrap();
                        q.next()
                    };
                    let Some((idx, item)) = next else { break };
                    let out = f(item);
                    results.lock().unwrap().push((idx, out));
                }
            });
        }
    });
    let mut done = results.into_inner().unwrap();
    done.sort_by_key(|(idx, _)| *idx);
    done.into_iter().map(|(_, r)| r).collect()
}

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
            let meta = fs::metadata(&from).unwrap();
            fs::copy(&from, &to).unwrap();
            if let Ok(mtime) = meta.modified()
                && let Ok(file) = fs::OpenOptions::new().write(true).open(&to)
            {
                let _ = file.set_modified(mtime);
            }
        }
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
                let _ = a.pull_from_peer(&EnginePeer::new(&**b, &caller), ts(t0 + round));
            }
        }
    }
}

fn live_contents(root: &Path) -> BTreeMap<String, Vec<u8>> {
    let mut out = BTreeMap::new();
    for (p, b) in all_bytes(root) {
        if p.starts_with(".legion-sync") {
            continue;
        }
        out.insert(p, b);
    }
    out
}

fn pull(a: &mut E, b: &E, now: i64) -> Option<String> {
    let caller = a.meta().unwrap().local_member;
    match a.pull_from_peer(&EnginePeer::new(b, &caller), ts(now)) {
        Ok(_) => None,
        Err(e) => Some(format!("{caller}<-{}: {e}", b.meta().unwrap().local_member)),
    }
}

fn permutations(n: usize) -> Vec<Vec<usize>> {
    if n == 1 {
        return vec![vec![0]];
    }
    let mut out = Vec::new();
    for p in permutations(n - 1) {
        for i in 0..=p.len() {
            let mut q = p.clone();
            q.insert(i, n - 1);
            out.push(q);
        }
    }
    out
}

fn two<'a>(es: &'a mut [&mut E; 3], a: usize, b: usize) -> (&'a mut E, &'a E) {
    assert_ne!(a, b);
    if a < b {
        let (l, r) = es.split_at_mut(b);
        (&mut *l[a], &*r[0])
    } else {
        let (l, r) = es.split_at_mut(a);
        (&mut *r[0], &*l[b])
    }
}

fn shuffled_edges(n: usize, seed: &mut u64) -> Vec<(usize, usize)> {
    let mut v = Vec::new();
    for a in 0..n {
        for b in 0..n {
            if a != b {
                v.push((a, b));
            }
        }
    }
    for i in (1..v.len()).rev() {
        *seed = seed
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        let j = ((*seed >> 33) as usize) % (i + 1);
        v.swap(i, j);
    }
    v
}

fn body(tag: &str, size: usize) -> Vec<u8> {
    let mut v = Vec::with_capacity(size);
    while v.len() < size {
        v.extend_from_slice(tag.as_bytes());
    }
    v.truncate(size);
    v
}

/// tower deletes a file; pi and nas apply it; every member acknowledges, so the tombstone is
/// collected everywhere. A new laptop with an old copy of the folder then joins and first meets
/// pi (not tower, where the deletion was made).
#[test]
fn r1_collected_deletion_resurrected_via_non_originator() {
    let tmp = scratch();
    let t = tmp.path();
    let all = ["tower", "pi", "nas"];
    fs::create_dir_all(t.join("tower")).unwrap();
    fs::write(t.join("tower/tax-2019.pdf"), b"deleted on purpose").unwrap();
    fs::write(t.join("tower/keep.txt"), b"keep").unwrap();
    let mut tower = open_real(&t.join("tower"), &t.join("tower.redb"), "tower", &all);
    let mut pi = open_real(&t.join("pi"), &t.join("pi.redb"), "pi", &all);
    let mut nas = open_real(&t.join("nas"), &t.join("nas.redb"), "nas", &all);
    tower.scan(ts(0)).unwrap();
    sync_all(&mut [&mut tower, &mut pi, &mut nas], 1);
    assert!(t.join("pi/tax-2019.pdf").exists());

    copy_tree(&t.join("pi"), &t.join("usb"));
    fs::remove_dir_all(t.join("usb/.legion-sync")).unwrap();

    fs::remove_file(t.join("tower/tax-2019.pdf")).unwrap();
    tower.scan(ts(10)).unwrap();
    sync_all(&mut [&mut tower, &mut pi, &mut nas], 11);
    sync_all(&mut [&mut tower, &mut pi, &mut nas], 20);
    for e in [&mut tower, &mut pi, &mut nas] {
        let _ = e.gc_tombstones().unwrap();
        assert!(e.store().get_record("tax-2019.pdf").unwrap().is_none());
        assert_eq!(
            e.store().get_deleted_history("tax-2019.pdf").unwrap().len(),
            1
        );
    }
    assert!(!t.join("pi/tax-2019.pdf").exists());

    let all4 = ["tower", "pi", "nas", "laptop"];
    for e in [&mut tower, &mut pi, &mut nas] {
        e.add_member("laptop").unwrap();
    }
    fs::rename(t.join("usb"), t.join("laptop")).unwrap();
    let mut laptop = open_real(&t.join("laptop"), &t.join("laptop.redb"), "laptop", &all4);
    laptop.scan(ts(30)).unwrap();

    pi.pull_from_peer(&EnginePeer::new(&laptop, "pi"), ts(31))
        .unwrap();
    laptop
        .pull_from_peer(&EnginePeer::new(&pi, "laptop"), ts(32))
        .unwrap();
    sync_all(&mut [&mut tower, &mut pi, &mut nas, &mut laptop], 40);

    for n in ["tower", "pi", "nas", "laptop"] {
        assert!(
            !t.join(format!("{n}/tax-2019.pdf")).exists(),
            "deleted file came back on {n}"
        );
        assert_eq!(fs::read(t.join(format!("{n}/keep.txt"))).unwrap(), b"keep");
    }
}

/// One restore scenario. `seen_by`: which peers pulled pi's post-backup changes (0 none, 1 tower,
/// 2 both). `lost`/`new`: number of changes pi made after the backup / after the restore.
fn restore_case(
    seen_by: usize,
    lost: usize,
    new: usize,
    order: &[usize],
    edit_same: bool,
) -> Vec<String> {
    let tmp = scratch();
    let t = tmp.path();
    let all = ["tower", "pi", "nas"];
    let mut issues = Vec::new();
    fs::create_dir_all(t.join("tower")).unwrap();
    for i in 0..12 {
        fs::write(t.join(format!("tower/f{i}.txt")), format!("base {i}")).unwrap();
    }
    fs::write(t.join("tower/doomed.txt"), b"tower deletes this").unwrap();
    let mut tower = open_real(&t.join("tower"), &t.join("tower.redb"), "tower", &all);
    let mut pi = open_real(&t.join("pi"), &t.join("pi.redb"), "pi", &all);
    let mut nas = open_real(&t.join("nas"), &t.join("nas.redb"), "nas", &all);
    tower.scan(ts(0)).unwrap();
    for k in 0..2 {
        for (a, b) in [(0, 1), (0, 2), (1, 0), (1, 2), (2, 0), (2, 1)] {
            let mut es = [&mut tower, &mut pi, &mut nas];
            let (x, y) = two(&mut es, a, b);
            let _ = pull(x, y, 1 + k);
        }
    }
    drop(pi);
    copy_tree(&t.join("pi"), &t.join("backup_pi"));
    fs::copy(t.join("pi.redb"), t.join("backup_pi.redb")).unwrap();
    let mut pi = open_real(&t.join("pi"), &t.join("pi.redb"), "pi", &all);
    for i in 0..lost {
        fs::write(
            t.join(format!("pi/f{i}.txt")),
            format!("pi after backup {i}"),
        )
        .unwrap();
    }
    pi.scan(ts(10)).unwrap();
    fs::remove_file(t.join("tower/doomed.txt")).unwrap();
    tower.scan(ts(11)).unwrap();
    let _ = pull(&mut pi, &tower, 12);
    if seen_by >= 1 {
        let _ = pull(&mut tower, &pi, 13);
    }
    if seen_by >= 2 {
        let _ = pull(&mut nas, &pi, 13);
    }
    let _ = pull(&mut pi, &nas, 14);
    let _ = pull(&mut nas, &tower, 14);
    let _ = pull(&mut tower, &nas, 14);
    let tower_inc = tower.meta().unwrap().incarnation;
    let nas_inc = nas.meta().unwrap().incarnation;

    drop(pi);
    fs::remove_dir_all(t.join("pi")).unwrap();
    fs::remove_file(t.join("pi.redb")).unwrap();
    copy_tree(&t.join("backup_pi"), &t.join("pi"));
    fs::copy(t.join("backup_pi.redb"), t.join("pi.redb")).unwrap();
    let mut pi = open_real(&t.join("pi"), &t.join("pi.redb"), "pi", &all);
    let mut today = Vec::new();
    for i in 0..new {
        let (name, body) = if edit_same {
            (format!("f{i}.txt"), format!("TODAY {i}"))
        } else {
            (format!("new{i}.txt"), format!("TODAY new {i}"))
        };
        fs::write(t.join("pi").join(&name), &body).unwrap();
        today.push(body);
    }
    pi.scan(ts(20)).unwrap();

    let edges = [(0usize, 1usize), (1, 0), (2, 1), (1, 2)];
    for (k, &ix) in order.iter().enumerate() {
        let (a, b) = edges[ix];
        let mut es = [&mut tower, &mut pi, &mut nas];
        let (x, y) = two(&mut es, a, b);
        if let Some(e) = pull(x, y, 21 + k as i64) {
            issues.push(format!("first-meeting error {e}"));
        }
    }
    let mut last_errors = Vec::new();
    for round in 0..6 {
        last_errors.clear();
        for (a, b) in [(0, 1), (0, 2), (1, 0), (1, 2), (2, 0), (2, 1)] {
            let mut es = [&mut tower, &mut pi, &mut nas];
            let (x, y) = two(&mut es, a, b);
            if let Some(e) = pull(x, y, 100 + round * 4000) {
                last_errors.push(e);
            }
        }
    }
    for e in &last_errors {
        issues.push(format!("still failing after 6 rounds: {e}"));
    }
    if tower.meta().unwrap().incarnation != tower_inc {
        issues.push("healthy tower rotated".into());
    }
    if nas.meta().unwrap().incarnation != nas_inc {
        issues.push("healthy nas rotated".into());
    }
    for body in &today {
        for n in ["tower", "pi", "nas"] {
            let live = live_contents(&t.join(n));
            if !live.values().any(|b| b == body.as_bytes()) {
                let w = where_is(&t.join(n), body.as_bytes());
                issues.push(format!("{body:?} not live on {n} (found {w:?})"));
            }
        }
    }
    if seen_by >= 1 {
        for i in 0..lost {
            let body = format!("pi after backup {i}");
            let anywhere_live = ["tower", "pi", "nas"].iter().any(|n| {
                live_contents(&t.join(n))
                    .values()
                    .any(|b| b == body.as_bytes())
            });
            if !anywhere_live {
                let w: Vec<_> = ["tower", "pi", "nas"]
                    .iter()
                    .map(|n| (n, where_is(&t.join(n), body.as_bytes())))
                    .collect();
                issues.push(format!("{body:?} no longer live anywhere ({w:?})"));
            }
        }
    }
    for n in ["tower", "pi", "nas"] {
        if t.join(format!("{n}/doomed.txt")).exists() {
            issues.push(format!("deleted doomed.txt came back on {n}"));
        }
    }
    let lt = live_contents(&t.join("tower"));
    let lp = live_contents(&t.join("pi"));
    let ln = live_contents(&t.join("nas"));
    if lt != lp || lt != ln {
        let keys: BTreeSet<_> = lt
            .keys()
            .chain(lp.keys())
            .chain(ln.keys())
            .cloned()
            .collect();
        let diff: Vec<_> = keys
            .into_iter()
            .filter(|k| !(lt.get(k) == lp.get(k) && lt.get(k) == ln.get(k)))
            .collect();
        issues.push(format!("not converged: {diff:?}"));
    }
    for (n, e) in [("tower", &tower), ("pi", &pi), ("nas", &nas)] {
        for (_, p) in e.store().list_problems().unwrap() {
            if !matches!(p.kind, ProblemKind::PeerForcedRotation) {
                issues.push(format!("{n} problem {:?}: {}", p.kind, p.sentence));
            }
        }
    }
    issues
}

#[test]
fn r2_restore_matrix() {
    let mut specs = Vec::new();
    for seen_by in 0..3 {
        for (lost, new) in [(3, 1), (3, 3), (3, 6)] {
            for edit_same in [false, true] {
                for order in permutations(4) {
                    specs.push((seen_by, lost, new, edit_same, order));
                }
            }
        }
    }
    let cases = specs.len();
    assert_eq!(cases, 432);
    let results = parallel_map(specs, |(seen_by, lost, new, edit_same, order)| {
        let label = format!("seen{seen_by} l{lost} n{new} same{edit_same} {order:?}");
        (label, restore_case(seen_by, lost, new, &order, edit_same))
    });
    let mut summary = BTreeMap::<String, Vec<String>>::new();
    for (label, issues) in results {
        for i in issues {
            summary.entry(i).or_default().push(label.clone());
        }
    }
    println!("cases: {cases}");
    for (issue, where_) in &summary {
        println!("[{}x] {issue}\n     e.g. {}", where_.len(), where_[0]);
    }
    for (issue, where_) in &summary {
        let allowed_transient = issue.starts_with("first-meeting error ")
            && issue
                .contains("Two different machines appear to be using the same member name 'pi'");
        assert!(
            allowed_transient,
            "unexpected restore-matrix issue ({}x, e.g. {}): {issue}",
            where_.len(),
            where_[0]
        );
    }
}

#[test]
fn r2b_trace_one() {
    let issues = restore_case(1, 3, 1, &[1, 2, 3, 0], false);
    assert!(
        issues.is_empty(),
        "expected clean restore case, got {issues:#?}"
    );
}

/// Four machines. pi is restored from a backup. Every 20 minutes each machine pulls from every
/// other; the always-on machines happen to pull from pi before pi pulls from them. pi's person
/// edits one file between rounds. Count pi's rotations and the conflict copies they cause.
#[test]
fn r3_restored_member_rotation_cascade() {
    let tmp = scratch();
    let t = tmp.path();
    let names = ["tower", "nas", "laptop", "pi"];
    fs::create_dir_all(t.join("tower")).unwrap();
    for i in 0..10 {
        fs::write(t.join(format!("tower/f{i}.txt")), format!("base {i}")).unwrap();
    }
    let mut es: Vec<E> = names
        .iter()
        .map(|n| open_real(&t.join(n), &t.join(format!("{n}.redb")), n, &names))
        .collect();
    es[0].scan(ts(0)).unwrap();
    let round = |es: &mut Vec<E>, now: i64| -> Vec<String> {
        let mut errs = Vec::new();
        let mut order = Vec::new();
        for a in 0..3 {
            order.push((a, 3));
        }
        for b in 0..3 {
            order.push((3, b));
        }
        for a in 0..3 {
            for b in 0..3 {
                if a != b {
                    order.push((a, b));
                }
            }
        }
        for (a, b) in order {
            let (x, y) = if a < b {
                let (l, r) = es.split_at_mut(b);
                (&mut l[a], &r[0])
            } else {
                let (l, r) = es.split_at_mut(a);
                (&mut r[0], &l[b])
            };
            if let Some(e) = pull(x, y, now) {
                errs.push(e);
            }
        }
        errs
    };
    for k in 0..3 {
        round(&mut es, 1 + k);
    }
    let pi = es.pop().unwrap();
    drop(pi);
    copy_tree(&t.join("pi"), &t.join("backup_pi"));
    fs::copy(t.join("pi.redb"), t.join("backup_pi.redb")).unwrap();
    es.push(open_real(&t.join("pi"), &t.join("pi.redb"), "pi", &names));
    fs::write(t.join("pi/f0.txt"), b"pi after backup").unwrap();
    fs::write(t.join("tower/f9.txt"), b"tower edit seen by old pi").unwrap();
    es[3].scan(ts(10)).unwrap();
    es[0].scan(ts(10)).unwrap();
    for k in 0..2 {
        round(&mut es, 11 + k);
    }

    let pi = es.pop().unwrap();
    drop(pi);
    fs::remove_dir_all(t.join("pi")).unwrap();
    fs::remove_file(t.join("pi.redb")).unwrap();
    copy_tree(&t.join("backup_pi"), &t.join("pi"));
    fs::copy(t.join("backup_pi.redb"), t.join("pi.redb")).unwrap();
    es.push(open_real(&t.join("pi"), &t.join("pi.redb"), "pi", &names));

    let mut incs = vec![es[3].meta().unwrap().incarnation];
    for r in 0..36 {
        let now = 100 + r * 1200;
        let body = format!("pi work round {r}");
        fs::write(t.join("pi/f5.txt"), &body).unwrap();
        let _ = es[3].scan(ts(now - 1));
        let _ = round(&mut es, now);
        let inc = es[3].meta().unwrap().incarnation;
        if *incs.last().unwrap() != inc {
            incs.push(inc);
        }
    }
    let conflicts: Vec<_> = fs::read_dir(t.join("pi"))
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.contains("changed on"))
        .collect();
    assert_eq!(incs.len() - 1, 1, "pi should rotate exactly once");
    assert!(
        conflicts.len() <= 1,
        "unexpected conflict cascade: {conflicts:?}"
    );
    assert_eq!(
        fs::read_to_string(t.join("tower/f5.txt")).unwrap(),
        "pi work round 35"
    );
    for (n, e) in names[..3].iter().zip(es[..3].iter()) {
        assert!(
            e.store().list_problems().unwrap().is_empty(),
            "healthy peer {n} should have no stale problems"
        );
    }
}

fn cascade_case(seed0: u64) -> (usize, usize, usize, usize) {
    let tmp = scratch();
    let t = tmp.path();
    let names = ["tower", "nas", "laptop", "pi"];
    fs::create_dir_all(t.join("tower")).unwrap();
    for i in 0..10 {
        fs::write(t.join(format!("tower/f{i}.txt")), format!("base {i}")).unwrap();
    }
    let mut es: Vec<E> = names
        .iter()
        .map(|n| open_real(&t.join(n), &t.join(format!("{n}.redb")), n, &names))
        .collect();
    es[0].scan(ts(0)).unwrap();
    let mut seed = seed0;
    let round = |es: &mut Vec<E>, now: i64, seed: &mut u64| -> Vec<String> {
        let mut errs = Vec::new();
        for (a, b) in shuffled_edges(4, seed) {
            let (x, y) = if a < b {
                let (l, r) = es.split_at_mut(b);
                (&mut l[a], &r[0])
            } else {
                let (l, r) = es.split_at_mut(a);
                (&mut r[0], &l[b])
            };
            if let Some(e) = pull(x, y, now) {
                errs.push(e);
            }
        }
        errs
    };
    for k in 0..3 {
        round(&mut es, 1 + k, &mut seed);
    }
    let pi = es.pop().unwrap();
    drop(pi);
    copy_tree(&t.join("pi"), &t.join("backup_pi"));
    fs::copy(t.join("pi.redb"), t.join("backup_pi.redb")).unwrap();
    es.push(open_real(&t.join("pi"), &t.join("pi.redb"), "pi", &names));
    fs::write(t.join("pi/f0.txt"), b"pi after backup").unwrap();
    fs::write(t.join("tower/f9.txt"), b"tower edit seen by old pi").unwrap();
    es[3].scan(ts(10)).unwrap();
    es[0].scan(ts(10)).unwrap();
    for k in 0..2 {
        round(&mut es, 11 + k, &mut seed);
    }
    let pi = es.pop().unwrap();
    drop(pi);
    fs::remove_dir_all(t.join("pi")).unwrap();
    fs::remove_file(t.join("pi.redb")).unwrap();
    copy_tree(&t.join("backup_pi"), &t.join("pi"));
    fs::copy(t.join("backup_pi.redb"), t.join("pi.redb")).unwrap();
    es.push(open_real(&t.join("pi"), &t.join("pi.redb"), "pi", &names));
    let mut incs = vec![es[3].meta().unwrap().incarnation];
    let mut failed_rounds = 0;
    let mut last_rot_round = 0;
    for r in 0..36 {
        let now = 100 + r * 1200;
        fs::write(t.join("pi/f5.txt"), format!("pi work round {r}")).unwrap();
        let _ = es[3].scan(ts(now - 1));
        let errs = round(&mut es, now, &mut seed);
        let inc = es[3].meta().unwrap().incarnation;
        if *incs.last().unwrap() != inc {
            incs.push(inc);
            last_rot_round = r;
        }
        if !errs.is_empty() {
            failed_rounds += 1;
        }
    }
    let conflicts = fs::read_dir(t.join("pi"))
        .unwrap()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().contains("changed on"))
        .count();
    (
        incs.len() - 1,
        conflicts,
        failed_rounds,
        last_rot_round as usize,
    )
}

fn run_r3b_seeds(seed_count: u64) {
    let seeds: Vec<u64> = (1..=seed_count).collect();
    let results = parallel_map(seeds, |seed| (seed, cascade_case(seed)));
    let mut hist = BTreeMap::<usize, usize>::new();
    let mut worst = (0, 0, 0, 0, 0u64);
    for (seed, (rot, conf, failed, last)) in results {
        *hist.entry(rot).or_default() += 1;
        if rot > worst.0 || (rot == worst.0 && failed > worst.2) {
            worst = (rot, conf, failed, last, seed);
        }
        assert_eq!(
            rot, 1,
            "seed {seed}: expected exactly 1 rotation, got {rot}"
        );
        assert_eq!(
            last, 0,
            "seed {seed}: unexpected rotation after round 0 (round {last})"
        );
        assert!(
            failed <= 1,
            "seed {seed}: unexpected failed rounds {failed}"
        );
    }
    println!("rotations histogram over {seed_count} seeds: {hist:?}");
    println!(
        "worst: seed {} rotations {} conflict copies {} rounds with failed pulls {} last rotation in round {}",
        worst.4, worst.0, worst.1, worst.2, worst.3
    );
}

#[test]
fn r3b_cascade_random_orders() {
    let seeds: u64 = std::env::var("REVIEW_SEEDS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(24);
    run_r3b_seeds(seeds);
}

#[test]
#[ignore]
fn r3b_cascade_random_orders_200() {
    run_r3b_seeds(200);
}

/// pi is the slow machine: each 20-minute round, the three always-on machines pull from pi (and
/// each other), and pi pulls from one of them in turn.
#[test]
fn r3c_cascade_slow_member() {
    let tmp = scratch();
    let t = tmp.path();
    let names = ["tower", "nas", "laptop", "pi"];
    fs::create_dir_all(t.join("tower")).unwrap();
    for i in 0..10 {
        fs::write(t.join(format!("tower/f{i}.txt")), format!("base {i}")).unwrap();
    }
    let mut es: Vec<E> = names
        .iter()
        .map(|n| open_real(&t.join(n), &t.join(format!("{n}.redb")), n, &names))
        .collect();
    es[0].scan(ts(0)).unwrap();
    fn do_pull(es: &mut [E], a: usize, b: usize, now: i64) -> Option<String> {
        let (x, y) = if a < b {
            let (l, r) = es.split_at_mut(b);
            (&mut l[a], &r[0])
        } else {
            let (l, r) = es.split_at_mut(a);
            (&mut r[0], &l[b])
        };
        pull(x, y, now)
    }
    for k in 0..3 {
        for a in 0..4 {
            for b in 0..4 {
                if a != b {
                    do_pull(&mut es, a, b, 1 + k);
                }
            }
        }
    }
    let pi = es.pop().unwrap();
    drop(pi);
    copy_tree(&t.join("pi"), &t.join("backup_pi"));
    fs::copy(t.join("pi.redb"), t.join("backup_pi.redb")).unwrap();
    es.push(open_real(&t.join("pi"), &t.join("pi.redb"), "pi", &names));
    fs::write(t.join("tower/f9.txt"), b"tower edit seen by old pi").unwrap();
    es[0].scan(ts(10)).unwrap();
    for b in 0..3 {
        do_pull(&mut es, 3, b, 11);
    }
    for a in 0..3 {
        do_pull(&mut es, a, 3, 12);
    }
    let pi = es.pop().unwrap();
    drop(pi);
    fs::remove_dir_all(t.join("pi")).unwrap();
    fs::remove_file(t.join("pi.redb")).unwrap();
    copy_tree(&t.join("backup_pi"), &t.join("pi"));
    fs::copy(t.join("backup_pi.redb"), t.join("pi.redb")).unwrap();
    es.push(open_real(&t.join("pi"), &t.join("pi.redb"), "pi", &names));
    let mut incs = vec![es[3].meta().unwrap().incarnation];
    for r in 0..36i64 {
        let now = 100 + r * 1200;
        fs::write(t.join("pi/f5.txt"), format!("pi work round {r}")).unwrap();
        let _ = es[3].scan(ts(now - 1));
        for a in 0..3 {
            for b in 0..4 {
                if a != b {
                    let _ = do_pull(&mut es, a, b, now);
                }
            }
        }
        let _ = do_pull(&mut es, 3, (r % 3) as usize, now);
        let inc = es[3].meta().unwrap().incarnation;
        if *incs.last().unwrap() != inc {
            incs.push(inc);
        }
    }
    let conflicts = fs::read_dir(t.join("pi"))
        .unwrap()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().contains("changed on"))
        .count();
    let tower_conflicts = fs::read_dir(t.join("tower"))
        .unwrap()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().contains("changed on"))
        .count();
    println!(
        "12 hours: pi rotations {}, conflict copies on pi {conflicts}, on tower {tower_conflicts}",
        incs.len() - 1
    );
    assert_eq!(incs.len() - 1, 1, "slow member pi should rotate once");
    assert!(conflicts <= 1);
    assert!(tower_conflicts <= 1);
    assert_eq!(
        fs::read_to_string(t.join("tower/f5.txt")).unwrap(),
        "pi work round 35"
    );
    for (n, e) in names.iter().zip(es.iter()) {
        let mut over = 0;
        e.store()
            .scan_records_in_order(&mut |rec| {
                if rec.version.iter().filter(|(k, _)| k.member == "pi").count() >= 8 {
                    over += 1;
                }
                Ok(true)
            })
            .unwrap();
        assert_eq!(over, 0, "{n}: records carried >= 8 pi incarnations");
        assert_eq!(
            e.store().held_back_count().unwrap(),
            0,
            "{n}: unexpected held back"
        );
        if *n != "pi" {
            assert_eq!(
                e.status().unwrap().state,
                MemberState::UpToDate,
                "{n}: not UpToDate"
            );
        }
    }
}

/// Over a folder's life, pi's Legion is reinstalled (index lost, folder kept) several times; each
/// time the person runs the explicit rebuild. Afterwards pi edits a file.
#[test]
fn r4_lifetime_incarnations_cut_member_off() {
    let tmp = scratch();
    let t = tmp.path();
    let all = ["tower", "pi"];
    fs::create_dir_all(t.join("tower")).unwrap();
    for i in 0..5 {
        fs::write(t.join(format!("tower/f{i}.txt")), format!("base {i}")).unwrap();
    }
    let mut tower = open_real(&t.join("tower"), &t.join("tower.redb"), "tower", &all);
    let mut pi = open_real(&t.join("pi"), &t.join("pi.redb"), "pi", &all);
    tower.scan(ts(0)).unwrap();
    for k in 0..2 {
        assert!(pull(&mut pi, &tower, 1 + k).is_none());
        assert!(pull(&mut tower, &pi, 1 + k).is_none());
    }
    for n in 1..=10 {
        drop(pi);
        fs::remove_file(t.join("pi.redb")).unwrap();
        let mut p = open_real(&t.join("pi"), &t.join("pi.redb"), "pi", &all);
        assert!(p.is_index_damaged());
        p.rebuild_from_disk(ts(100 * n)).unwrap();
        pi = p;
        for k in 0..2 {
            assert!(pull(&mut pi, &tower, 100 * n + 1 + k).is_none());
            assert!(pull(&mut tower, &pi, 100 * n + 1 + k).is_none());
        }
        let body = format!("edit on pi after rebuild {n}");
        fs::write(t.join("pi/f1.txt"), &body).unwrap();
        pi.scan(ts(100 * n + 10)).unwrap();
        assert!(pull(&mut tower, &pi, 100 * n + 11).is_none());
        assert!(pull(&mut pi, &tower, 100 * n + 12).is_none());
        let got = fs::read_to_string(t.join("tower/f1.txt")).unwrap();
        assert_eq!(got, body);
        let keys = tower
            .store()
            .get_record("f1.txt")
            .unwrap()
            .map(|r| r.version.iter().filter(|(k, _)| k.member == "pi").count())
            .unwrap();
        assert!(
            keys <= 2,
            "rebuild {n}: expected <= 2 pi keys after VV pruning, got {keys}"
        );
        assert_eq!(tower.store().held_back_count().unwrap(), 0);
    }
    assert_eq!(pi.status().unwrap().state, MemberState::UpToDate);
    assert_eq!(tower.status().unwrap().state, MemberState::UpToDate);
}

struct ChangingPeer<'a> {
    inner: EnginePeer<'a, RealFs, RedbIndexStore>,
}
impl Peer for ChangingPeer<'_> {
    fn member_id(&self) -> &str {
        self.inner.member_id()
    }
    fn handshake(&self, c: &HandshakeHello) -> legion_sync::Result<PeerHandshake> {
        self.inner.handshake(c)
    }
    fn records_since(
        &self,
        a: u64,
        b: u64,
        l: usize,
        c: Option<&AtomicBool>,
    ) -> legion_sync::Result<RecordsPage> {
        self.inner.records_since(a, b, l, c)
    }
    fn fetch_file(
        &self,
        path: &str,
        _h: Hash,
        _o: &FetchOptions,
    ) -> legion_sync::Result<Box<dyn Read + Send>> {
        Err(SyncError::NotFound {
            what: path.to_string(),
        })
    }
}

#[test]
#[ignore]
fn r5_deferral_cost() {
    for n in [2_000usize, 4_000, 8_000] {
        let tmp = scratch();
        let t = tmp.path();
        let all = ["tower", "pi"];
        fs::create_dir_all(t.join("tower")).unwrap();
        for i in 0..n {
            fs::write(t.join(format!("tower/f{i:06}.txt")), format!("x{i}")).unwrap();
        }
        let mut tower = open_real(&t.join("tower"), &t.join("tower.redb"), "tower", &all);
        let mut pi = open_real(&t.join("pi"), &t.join("pi.redb"), "pi", &all);
        tower.scan(ts(0)).unwrap();
        let start = std::time::Instant::now();
        let r = pi.pull_from_peer(
            &ChangingPeer {
                inner: EnginePeer::new(&tower, "pi"),
            },
            ts(1),
        );
        let first = start.elapsed();
        let start = std::time::Instant::now();
        let r2 = pi.pull_from_peer(
            &ChangingPeer {
                inner: EnginePeer::new(&tower, "pi"),
            },
            ts(2),
        );
        let second = start.elapsed();
        println!(
            "n={n}: first pull {first:?} ({:?}), second pull {second:?} ({:?}), deferrals {}",
            r.as_ref()
                .map(|r| (r.deferred, r.held_back))
                .map_err(|e| e.to_string()),
            r2.as_ref()
                .map(|r| (r.deferred, r.held_back))
                .map_err(|e| e.to_string()),
            pi.store().list_deferrals().unwrap().len()
        );
        assert!(r.is_ok(), "first pull failed: {r:?}");
        assert!(r2.is_ok(), "second pull failed: {r2:?}");
    }
}

fn in_place(p: &Path, b: &[u8]) -> io::Result<()> {
    use std::io::Write;
    let mut f = fs::OpenOptions::new().write(true).truncate(true).open(p)?;
    f.write_all(b)
}

fn in_place_slow(p: &Path, b: &[u8], pause: Duration) -> io::Result<()> {
    use std::io::Write;
    let mut f = fs::OpenOptions::new().write(true).truncate(true).open(p)?;
    let half = b.len() / 2;
    f.write_all(&b[..half])?;
    std::thread::sleep(pause);
    f.write_all(&b[half..])
}

/// Runs `iters` real-timing races of `shape` against pi applying tower's new version of
/// `dir/big.bin`. Returns (newest save lost, index hides a disk change after a scan, newest still
/// only in bin after scan).
fn race_shape(shape: &str, iters: usize, no_exchange: bool) -> (Vec<String>, usize, Vec<String>) {
    let size = 4 << 20;
    let all = ["tower", "pi"];
    let mut lost = Vec::new();
    let mut bin_only_after_scan = Vec::new();
    let mut hidden = 0;
    for chunk_start in (0..iters).step_by(50) {
        let tmp = scratch();
        let t = tmp.path();
        let mut tower = open_real(&t.join("tower"), &t.join("tower.redb"), "tower", &all);
        let mut pi = open_hook(&t.join("pi"), &t.join("pi.redb"), "pi", &all);
        pi.fs_mut().no_exchange = no_exchange;
        fs::create_dir_all(t.join("tower/dir")).unwrap();
        fs::write(t.join("tower/dir/big.bin"), body("init;", size)).unwrap();
        tower.scan(ts(0)).unwrap();
        pi.pull_from_peer(&EnginePeer::new(&tower, "pi"), ts(1))
            .unwrap();
        for it in chunk_start..(chunk_start + 50).min(iters) {
            let base = 100 * it as i64 + 10;
            if !t.join("pi/dir").exists() {
                fs::create_dir_all(t.join("pi/dir")).unwrap();
            }
            for k in 0..3 {
                let _ = pi.scan(ts(base + k));
                let _ = tower.scan(ts(base + k));
                let _ = tower.pull_from_peer(&EnginePeer::new(&pi, "tower"), ts(base + k));
                let _ = pi.pull_from_peer(&EnginePeer::new(&tower, "pi"), ts(base + k));
            }
            if !t.join("tower/dir/big.bin").exists() {
                fs::create_dir_all(t.join("tower/dir")).unwrap();
            }
            fs::write(
                t.join("tower/dir/big.bin"),
                body(&format!("tower-{it};"), size),
            )
            .unwrap();
            tower.scan(ts(base + 5)).unwrap();
            let pdir = t.join("pi/dir");
            let path = pdir.join("big.bin");
            let d1 = Duration::from_micros((it as u64 * 7919) % 5000);
            let d2 = Duration::from_micros((it as u64 * 104_729) % 3000);
            let s1 = body(&format!("pi-first-{it};"), size);
            let s2 = body(&format!("pi-second-{it};"), size);
            let s3 = body(&format!("pi-NEWEST-{it};"), size);
            let (a, b, c) = (s1.clone(), s2.clone(), s3.clone());
            let shape_s = shape.to_string();
            let (p, pd, root) = (path.clone(), pdir.clone(), t.join("pi"));
            let h = std::thread::spawn(move || -> bool {
                std::thread::sleep(d1);
                match shape_s.as_str() {
                    "three-saves" => {
                        let _ = in_place(&p, &a);
                        std::thread::sleep(d2);
                        write_via_rename(&p, &b);
                        std::thread::sleep(d2 / 2);
                        in_place(&p, &c).is_ok()
                    }
                    "save-then-rename-parent" => {
                        let _ = in_place(&p, &a);
                        std::thread::sleep(d2);
                        let _ = fs::rename(&pd, root.join("dir-renamed"));
                        fs::write(root.join("dir-renamed/big.bin"), &c).is_ok()
                    }
                    "rename-parent-then-save" => {
                        let _ = fs::rename(&pd, root.join("dir-renamed"));
                        std::thread::sleep(d2);
                        in_place(&root.join("dir-renamed/big.bin"), &c).is_ok()
                    }
                    "slow-truncating-writer" => in_place_slow(&p, &c, d2).is_ok(),
                    "held-handle" => {
                        use std::io::{Seek, Write};
                        if let Ok(mut f) = fs::OpenOptions::new().write(true).open(&p) {
                            std::thread::sleep(d2 * 2);
                            f.seek(io::SeekFrom::Start(0)).is_ok() && f.write_all(&c).is_ok()
                        } else {
                            false
                        }
                    }
                    _ => unreachable!(),
                }
            });
            let _ = pi.pull_from_peer(&EnginePeer::new(&tower, "pi"), ts(base + 6));
            let saved = h.join().unwrap();
            let _ = pi.scan(ts(base + 7));
            let w = where_is(&t.join("pi"), &s3);
            if saved {
                if w.is_empty() {
                    lost.push(format!("it {it}: {}", dump(&t.join("pi")).lines().count()));
                } else if w.iter().all(|x| x.contains(".legion-sync/bin")) {
                    bin_only_after_scan.push(format!("it {it}: newest only in bin {w:?}"));
                }
            }
            for rel in ["dir/big.bin", "dir-renamed/big.bin"] {
                let rec = pi.store().get_record(rel).unwrap();
                if let (Some(rec), Ok(bytes)) = (rec, fs::read(t.join("pi").join(rel)))
                    && !rec.deleted
                    && rec.hash != Some(legion_xfer::hash::hash_bytes(&bytes))
                {
                    hidden += 1;
                    let _ = pi.scan_with_options(
                        ts(base + 8),
                        ScanOptions {
                            verify_checksums: true,
                            ..Default::default()
                        },
                    );
                }
            }
            if t.join("pi/dir-renamed").exists() {
                let _ = fs::remove_dir_all(t.join("pi/dir"));
                fs::rename(t.join("pi/dir-renamed"), t.join("pi/dir")).unwrap();
            }
        }
    }
    (lost, hidden, bin_only_after_scan)
}

#[test]
#[ignore]
fn r6_real_timing_races() {
    let iters: usize = std::env::var("REVIEW_ITERS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(200);
    let only: Option<String> = std::env::var("SHAPE").ok();
    for no_exchange in [false, true] {
        for shape in [
            "three-saves",
            "save-then-rename-parent",
            "rename-parent-then-save",
            "slow-truncating-writer",
            "held-handle",
        ] {
            if only.as_deref().is_some_and(|o| o != shape) {
                continue;
            }
            let (lost, hidden, bin_only) = race_shape(shape, iters, no_exchange);
            println!(
                "{shape} (no_exchange={no_exchange}): {iters} races, newest save lost {} {:?}, newest only in bin {} {:?}, index hid a disk change {hidden}",
                lost.len(),
                &lost[..lost.len().min(3)],
                bin_only.len(),
                &bin_only[..bin_only.len().min(2)]
            );
            assert!(
                lost.is_empty(),
                "{shape} (no_exchange={no_exchange}): lost saves {lost:?}"
            );
            assert!(
                bin_only.is_empty(),
                "{shape} (no_exchange={no_exchange}): saves stuck in bin after scan {bin_only:?}"
            );
            assert_eq!(
                hidden, 0,
                "{shape} (no_exchange={no_exchange}): index hid disk changes"
            );
        }
    }
}

/// Deterministic held-handle races across all four timings:
/// 1) late write immediately after `move_tmp_to_bin_no_replace` during pull,
/// 2) late write after pull completes, promoted on `scan()`,
/// 3) late write after pull completes, promoted directly on `restore_from_bin()` without scan,
/// 4) late write after pull completes, promoted directly on `expire_bin()` without scan.
#[test]
fn r7_held_handle_save_in_bin_unrestorable_then_expired() {
    use std::cell::RefCell;
    use std::io::{Seek, Write};
    use std::rc::Rc;

    // Timing 1: late write right after move_tmp_to_bin_no_replace during pull.
    {
        let tmp = scratch();
        let t = tmp.path();
        let all = ["tower", "pi"];
        fs::create_dir_all(t.join("tower")).unwrap();
        fs::write(t.join("tower/mail.mbox"), b"old mailbox contents........").unwrap();
        let mut tower = open_real(&t.join("tower"), &t.join("tower.redb"), "tower", &all);
        let mut pi = open_hook(&t.join("pi"), &t.join("pi.redb"), "pi", &all);
        tower.scan(ts(0)).unwrap();
        pi.pull_from_peer(&EnginePeer::new(&tower, "pi"), ts(1))
            .unwrap();

        let handle = Rc::new(RefCell::new(
            fs::OpenOptions::new()
                .write(true)
                .open(t.join("pi/mail.mbox"))
                .unwrap(),
        ));
        fs::write(t.join("tower/mail.mbox"), b"tower's newer mailbox").unwrap();
        tower.scan(ts(10)).unwrap();
        let h2 = handle.clone();
        pi.fs()
            .add("move_tmp_to_bin_no_replace", When::After, 1, move || {
                let mut f = h2.borrow_mut();
                f.seek(io::SeekFrom::End(0)).unwrap();
                f.write_all(b"NEW MESSAGE the person just received")
                    .unwrap();
                f.sync_all().unwrap();
                Act::Continue
            });
        pi.pull_from_peer(&EnginePeer::new(&tower, "pi"), ts(11))
            .unwrap();
        assert!(pi.fs().all_fired());
        let saved = b"old mailbox contents........NEW MESSAGE the person just received";
        let locs = where_is(&t.join("pi"), saved);
        assert!(
            locs.iter().any(|p| !p.starts_with(".legion-sync")),
            "expected immediate promotion out of bin, got {locs:?}"
        );
        pi.scan(ts(12)).unwrap();
        assert_eq!(pi.status().unwrap().state, MemberState::UpToDate);
        let gone = pi.expire_bin(ts(13 + 31 * 86_400)).unwrap();
        assert!(gone.is_empty());
        assert!(
            where_is(&t.join("pi"), saved)
                .iter()
                .any(|p| !p.starts_with(".legion-sync"))
        );
    }

    // Timings 2, 3, and 4: write through held handle AFTER pull returns.
    // On Windows an open handle cannot be replaced/renamed into the bin by the OS, so the
    // post-pull held-handle write cases apply on Unix where open descriptors follow renames.
    #[cfg(unix)]
    for mode in ["scan", "restore", "expire"] {
        let tmp = scratch();
        let t = tmp.path();
        let all = ["tower", "pi"];
        fs::create_dir_all(t.join("tower")).unwrap();
        fs::write(t.join("tower/mail.mbox"), b"old mailbox contents........").unwrap();
        let mut tower = open_real(&t.join("tower"), &t.join("tower.redb"), "tower", &all);
        let mut pi = open_real(&t.join("pi"), &t.join("pi.redb"), "pi", &all);
        tower.scan(ts(0)).unwrap();
        pi.pull_from_peer(&EnginePeer::new(&tower, "pi"), ts(1))
            .unwrap();

        let mut handle = fs::OpenOptions::new()
            .write(true)
            .open(t.join("pi/mail.mbox"))
            .unwrap();
        fs::write(t.join("tower/mail.mbox"), b"tower's newer mailbox").unwrap();
        tower.scan(ts(10)).unwrap();
        pi.pull_from_peer(&EnginePeer::new(&tower, "pi"), ts(11))
            .unwrap();
        let bin = pi.list_bin().unwrap();
        assert_eq!(bin.len(), 1, "mode {mode}: displaced file should be in bin");

        // Program appends through its still-open fd AFTER pull finished.
        handle.seek(io::SeekFrom::End(0)).unwrap();
        handle.write_all(b"LATE WRITE AFTER PULL").unwrap();
        handle.sync_all().unwrap();
        drop(handle);
        let saved = b"old mailbox contents........LATE WRITE AFTER PULL";

        match mode {
            "scan" => {
                pi.scan(ts(12)).unwrap();
                assert!(
                    where_is(&t.join("pi"), saved)
                        .iter()
                        .any(|p| !p.starts_with(".legion-sync")),
                    "scan should promote modified bin file to live"
                );
                assert!(pi.list_bin().unwrap().is_empty());
            }
            "restore" => {
                let res = pi.restore_from_bin(
                    &bin[0].id,
                    Some("restored.mbox"),
                    RestoreStrategy::FailIfExists,
                    ts(12),
                );
                assert!(
                    res.is_ok(),
                    "restore_from_bin should accept modified bin file: {res:?}"
                );
                assert!(
                    where_is(&t.join("pi"), saved)
                        .iter()
                        .any(|p| !p.starts_with(".legion-sync")),
                    "restore should preserve late-written bytes live"
                );
            }
            "expire" => {
                let gone = pi.expire_bin(ts(12 + 31 * 86_400)).unwrap();
                assert!(
                    gone.is_empty(),
                    "expire_bin must promote modified bin file instead of deleting it"
                );
                assert!(
                    where_is(&t.join("pi"), saved)
                        .iter()
                        .any(|p| !p.starts_with(".legion-sync")),
                    "expire_bin should rescue late-written bytes live"
                );
                assert!(pi.list_bin().unwrap().is_empty());
            }
            _ => unreachable!(),
        }
    }
}

fn live_snapshot(root: &Path) -> BTreeMap<String, Vec<u8>> {
    let mut out = BTreeMap::new();
    for (p, b) in all_bytes(root) {
        if p.starts_with(".legion-sync/") && !p.starts_with(".legion-sync/bin/") {
            continue;
        }
        out.insert(p, b);
    }
    out
}

#[test]
#[ignore]
fn r8_index_corruption_fuzz() {
    let tmp = scratch();
    let t = tmp.path();
    let all = ["tower", "pi"];
    fs::create_dir_all(t.join("tower")).unwrap();
    for i in 0..300 {
        let p = t.join(format!("tower/d{}", i % 7));
        fs::create_dir_all(&p).unwrap();
        fs::write(p.join(format!("f{i}.txt")), format!("file {i}")).unwrap();
    }
    let mut tower = open_real(&t.join("tower"), &t.join("tower.redb"), "tower", &all);
    let mut pi = open_real(&t.join("pi"), &t.join("pi.redb"), "pi", &all);
    tower.scan(ts(0)).unwrap();
    pi.pull_from_peer(&EnginePeer::new(&tower, "pi"), ts(1))
        .unwrap();
    tower
        .pull_from_peer(&EnginePeer::new(&pi, "tower"), ts(1))
        .unwrap();
    for i in 0..20 {
        fs::remove_file(t.join(format!("pi/d{}/f{i}.txt", i % 7))).unwrap();
    }
    pi.scan(ts(5)).unwrap();
    tower
        .pull_from_peer(&EnginePeer::new(&pi, "tower"), ts(6))
        .unwrap();
    fs::remove_file(t.join("pi/d3/f290.txt")).unwrap();
    fs::write(t.join("pi/d1/f100.txt"), b"pi edit not yet pulled").unwrap();
    pi.scan(ts(7)).unwrap();
    drop(pi);
    let pristine = fs::read(t.join("pi.redb")).unwrap();
    let pi_disk = live_snapshot(&t.join("pi"));
    let mut seed: u64 = 0x9e3779b97f4a7c15;
    let mut rnd = move || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };
    let mut outcomes = BTreeMap::<String, usize>::new();
    let mut bad = Vec::new();
    let iters: usize = std::env::var("REVIEW_ITERS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(500);
    for it in 0..iters {
        let mut bytes = pristine.clone();
        let kind = it % 5;
        let page = 4096usize;
        let pages = bytes.len() / page;
        let what = match kind {
            0 => {
                for _ in 0..(1 + rnd() % 4) {
                    let off = (rnd() as usize) % bytes.len();
                    bytes[off] ^= 1 << (rnd() % 8);
                }
                "bit flips"
            }
            1 => {
                let a = 1 + (rnd() as usize) % (pages - 1);
                let b = 1 + (rnd() as usize) % (pages - 1);
                let pa = bytes[a * page..(a + 1) * page].to_vec();
                let pb = bytes[b * page..(b + 1) * page].to_vec();
                bytes[a * page..(a + 1) * page].copy_from_slice(&pb);
                bytes[b * page..(b + 1) * page].copy_from_slice(&pa);
                "page swap"
            }
            2 => {
                let off = (rnd() as usize) % bytes.len();
                let len = (1 + rnd() as usize % 64).min(bytes.len() - off);
                for x in &mut bytes[off..off + len] {
                    *x = rnd() as u8;
                }
                "random run"
            }
            3 => {
                let cut = page + (rnd() as usize) % (bytes.len() - page);
                bytes.truncate(cut);
                "truncate"
            }
            _ => {
                bytes.extend(std::iter::repeat_n(0u8, 4096 * (1 + rnd() as usize % 4)));
                "extended with zeros"
            }
        };
        fs::write(t.join("pi.redb"), &bytes).unwrap();
        let res = std::panic::catch_unwind(|| RedbIndexStore::open(t.join("pi.redb")));
        let outcome = match res {
            Err(_) => {
                bad.push(format!("{it} {what}: PANIC escaped RedbIndexStore::open"));
                "panic".to_string()
            }
            Ok(Err(e)) => format!(
                "store refused ({})",
                e.to_string().chars().take(40).collect::<String>()
            ),
            Ok(Ok(store)) => {
                let eng = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    SyncEngine::open(cfg("pi", &all), RealFs::open(t.join("pi")).unwrap(), store)
                }));
                match eng {
                    Err(_) => {
                        bad.push(format!("{it} {what}: PANIC escaped SyncEngine::open"));
                        "engine panic".into()
                    }
                    Ok(Err(e)) => format!(
                        "engine refused ({})",
                        e.to_string().chars().take(30).collect::<String>()
                    ),
                    Ok(Ok(mut e)) => {
                        let damaged = e.is_index_damaged();
                        let s = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                            let a = e
                                .scan(ts(100 + it as i64))
                                .map(|_| ())
                                .map_err(|x| x.to_string());
                            let b = e
                                .pull_from_peer(&EnginePeer::new(&tower, "pi"), ts(101 + it as i64))
                                .map(|_| ())
                                .map_err(|x| x.to_string());
                            let c = e.status().map(|_| ());
                            (a, b, c.is_ok())
                        }));
                        match s {
                            Err(_) => {
                                bad.push(format!("{it} {what}: PANIC escaped scan/pull"));
                                "panic in use".into()
                            }
                            Ok((a, b, _)) => {
                                let now_disk = live_snapshot(&t.join("pi"));
                                let changed = now_disk != pi_disk;
                                if a.is_ok() && b.is_ok() && !damaged {
                                    let mut mismatch = 0;
                                    e.store()
                                        .scan_records_in_order(&mut |rec| {
                                            if !rec.deleted && rec.kind == EntryKind::File {
                                                if let Ok(bytes) =
                                                    fs::read(t.join("pi").join(&rec.path))
                                                {
                                                    if rec.hash
                                                        != Some(legion_xfer::hash::hash_bytes(
                                                            &bytes,
                                                        ))
                                                    {
                                                        mismatch += 1;
                                                    }
                                                } else {
                                                    mismatch += 1;
                                                }
                                            }
                                            Ok(true)
                                        })
                                        .ok();
                                    if mismatch > 0 {
                                        bad.push(format!(
                                            "{it} {what}: opened healthy but index disagrees with disk on {mismatch} files"
                                        ));
                                    }
                                }
                                if changed && damaged {
                                    bad.push(format!(
                                        "{it} {what}: damaged index but disk changed"
                                    ));
                                }
                                format!(
                                    "opened damaged={damaged} scan_ok={} pull_ok={} disk_changed={changed}",
                                    a.is_ok(),
                                    b.is_ok()
                                )
                            }
                        }
                    }
                }
            }
        };
        *outcomes.entry(format!("{what}: {outcome}")).or_default() += 1;
        if live_snapshot(&t.join("pi")) != pi_disk {
            for (p, _) in live_snapshot(&t.join("pi")) {
                if !pi_disk.contains_key(&p) {
                    let _ = fs::remove_file(t.join("pi").join(&p));
                }
            }
            for (p, b) in &pi_disk {
                let full = t.join("pi").join(p);
                fs::create_dir_all(full.parent().unwrap()).unwrap();
                fs::write(full, b).unwrap();
            }
        }
    }
    for (k, v) in &outcomes {
        println!("{v:>4}  {k}");
    }
    assert!(bad.is_empty(), "index corruption fuzz failures: {bad:#?}");
}

#[test]
fn r9_rebuild_after_damage_carries_unsynced_work() {
    let settles: Vec<i64> = match std::env::var("SETTLE").ok().and_then(|s| s.parse().ok()) {
        Some(v) => vec![v],
        None => vec![0, 30],
    };
    for settle in settles {
        for variant in [
            "truncate",
            "deletion-log-missing",
            "deletion-log-truncated",
            "deletion-log-from-other-member",
        ] {
            let tmp = scratch();
            let t = tmp.path();
            let all = ["tower", "pi"];
            for i in 0..60 {
                let p = t.join(format!("tower/d{}", i % 7));
                fs::create_dir_all(&p).unwrap();
                fs::write(p.join(format!("f{i}.txt")), format!("file {i}")).unwrap();
            }
            let mut tower = open_real(&t.join("tower"), &t.join("tower.redb"), "tower", &all);
            let mut pi = open_real(&t.join("pi"), &t.join("pi.redb"), "pi", &all);
            tower.scan(ts(0)).unwrap();
            pi.pull_from_peer(&EnginePeer::new(&tower, "pi"), ts(1))
                .unwrap();
            tower
                .pull_from_peer(&EnginePeer::new(&pi, "tower"), ts(1))
                .unwrap();

            for i in 0..5 {
                fs::remove_file(t.join(format!("pi/d{}/f{i}.txt", i % 7))).unwrap();
            }
            pi.scan(ts(5)).unwrap();
            pi.scan(ts(5 + settle)).unwrap();
            tower
                .pull_from_peer(&EnginePeer::new(&pi, "tower"), ts(6 + settle))
                .unwrap();
            pi.pull_from_peer(&EnginePeer::new(&tower, "pi"), ts(6 + settle))
                .unwrap();

            fs::remove_file(t.join("tower/d2/f9.txt")).unwrap();
            tower.scan(ts(7 + settle)).unwrap();
            tower.scan(ts(7 + 2 * settle)).unwrap();
            pi.pull_from_peer(&EnginePeer::new(&tower, "pi"), ts(8 + 2 * settle))
                .unwrap();

            fs::remove_file(t.join("pi/d3/f52.txt")).unwrap();
            fs::write(t.join("pi/d1/f50.txt"), b"pi edit not yet pulled").unwrap();
            pi.scan(ts(9 + 2 * settle)).unwrap();
            pi.scan(ts(9 + 3 * settle)).unwrap();
            drop(pi);
            let log = t.join("pi/.legion-sync/deletions.log");
            match variant {
                "truncate" => {
                    let b = fs::read(t.join("pi.redb")).unwrap();
                    fs::write(t.join("pi.redb"), &b[..b.len() / 2]).unwrap();
                }
                "deletion-log-missing" => {
                    fs::write(t.join("pi.redb"), b"garbage").unwrap();
                    let _ = fs::remove_file(&log);
                }
                "deletion-log-truncated" => {
                    fs::write(t.join("pi.redb"), b"garbage").unwrap();
                    let b = fs::read(&log).unwrap_or_default();
                    fs::write(&log, &b[..b.len().saturating_sub(7)]).unwrap();
                }
                _ => {
                    fs::write(t.join("pi.redb"), b"garbage").unwrap();
                    let other =
                        fs::read(t.join("tower/.legion-sync/deletions.log")).unwrap_or_default();
                    fs::write(&log, other).unwrap();
                }
            }
            let (mut pi, _) = SyncEngine::rebuild_redb_from_disk(
                cfg("pi", &all),
                RealFs::open(t.join("pi")).unwrap(),
                t.join("pi.redb"),
                ts(20 + 3 * settle),
            )
            .unwrap();
            for k in 0..3 {
                tower
                    .pull_from_peer(&EnginePeer::new(&pi, "tower"), ts(30 + 3 * settle + k))
                    .unwrap();
                pi.pull_from_peer(&EnginePeer::new(&tower, "pi"), ts(30 + 3 * settle + k))
                    .unwrap();
            }
            let mut issues = Vec::new();
            for (n, root) in [("tower", t.join("tower")), ("pi", t.join("pi"))] {
                for i in 0..5 {
                    if root.join(format!("d{}/f{i}.txt", i % 7)).exists() {
                        issues.push(format!("{n}: acknowledged deletion f{i} came back"));
                    }
                }
                if root.join("d2/f9.txt").exists() {
                    issues.push(format!("{n}: tower's deletion d2/f9 came back"));
                }
                if root.join("d3/f52.txt").exists() {
                    issues.push(format!(
                        "{n}: pi's unsynced deletion d3/f52 not applied / came back"
                    ));
                }
                if where_is(&root, b"pi edit not yet pulled")
                    .iter()
                    .all(|p| p.contains(".legion-sync"))
                {
                    issues.push(format!("{n}: pi's unsynced edit not live"));
                }
                for i in 5..60 {
                    if i == 9 || i == 52 || i == 50 {
                        continue;
                    }
                    if !root.join(format!("d{}/f{i}.txt", i % 7)).exists() {
                        issues.push(format!("{n}: lost f{i}"));
                    }
                }
            }
            if variant == "truncate" || (variant == "deletion-log-truncated" && settle >= 30) {
                assert!(
                    issues.is_empty(),
                    "settle={settle} {variant}: unexpected issues {issues:?}"
                );
            } else {
                // Even when both index and deletion log are destroyed, no live edits or other files
                // may be lost and tower's own deletion d2/f9 stays deleted.
                assert!(
                    !issues.iter().any(|s| s.contains("unsynced edit")
                        || s.contains("lost f")
                        || s.contains("d2/f9")),
                    "settle={settle} {variant}: unexpected data loss {issues:?}"
                );
            }
        }
    }
}

fn run_r10_inbound_brake(archive_files: usize, keep_files: usize, page_limit: usize) {
    let fetch_opts = FetchOptions::default();
    for decision in [
        InboundDeletionBrakeDecision::ApplyDeletions,
        InboundDeletionBrakeDecision::KeepLocalFiles,
    ] {
        let tmp = scratch();
        let t = tmp.path();
        let all = ["tower", "pi"];
        fs::create_dir_all(t.join("tower/archive")).unwrap();
        fs::create_dir_all(t.join("tower/keep")).unwrap();
        for i in 0..archive_files {
            fs::write(t.join(format!("tower/archive/a{i}.txt")), format!("a{i}")).unwrap();
        }
        for i in 0..keep_files {
            fs::write(t.join(format!("tower/keep/k{i}.txt")), format!("k{i}")).unwrap();
        }
        let mut tower = open_real(&t.join("tower"), &t.join("tower.redb"), "tower", &all);
        let mut pi = open_real(&t.join("pi"), &t.join("pi.redb"), "pi", &all);
        tower.scan(ts(0)).unwrap();
        while pi
            .pull_from_peer_with_options(
                &EnginePeer::new(&tower, "pi"),
                page_limit,
                &fetch_opts,
                ts(1),
            )
            .unwrap()
            .records_seen
            > 0
        {}
        fs::remove_dir_all(t.join("tower/archive")).unwrap();
        let r = tower.scan(ts(100)).unwrap();
        assert!(r.brake_tripped);
        tower
            .resolve_deletion_brake(DeletionBrakeDecision::ConfirmDeleted, ts(101))
            .unwrap();
        let _ = tower.scan(ts(200));
        let _ = tower.flush_settled_deletions(ts(200));

        let mut confirmations = 0;
        let mut now = 300;
        loop {
            now += 1;
            match pi.pull_from_peer_with_options(
                &EnginePeer::new(&tower, "pi"),
                page_limit,
                &fetch_opts,
                ts(now),
            ) {
                Err(SyncError::Paused { .. }) => {
                    confirmations += 1;
                    pi.resolve_inbound_deletion_brake("tower", decision, ts(now))
                        .unwrap();
                    assert!(confirmations <= 5, "inbound brake asked too many times");
                }
                Err(e) => panic!("unexpected pull error: {e}"),
                Ok(r) if r.records_seen == 0 => break,
                Ok(_) => {}
            }
        }
        assert_eq!(
            confirmations, 1,
            "{decision:?}: expected exactly 1 confirmation"
        );
        let left = fs::read_dir(t.join("pi/archive"))
            .map(|d| d.count())
            .unwrap_or(0);
        match decision {
            InboundDeletionBrakeDecision::ApplyDeletions => {
                assert_eq!(left, 0);
            }
            InboundDeletionBrakeDecision::KeepLocalFiles => {
                assert_eq!(left, archive_files);
                for k in 0..4 {
                    tower
                        .pull_from_peer_with_options(
                            &EnginePeer::new(&pi, "tower"),
                            page_limit,
                            &fetch_opts,
                            ts(now + 10 + k),
                        )
                        .unwrap();
                }
                let back = fs::read_dir(t.join("tower/archive"))
                    .map(|d| d.count())
                    .unwrap_or(0);
                assert_eq!(back, archive_files);
            }
        }
    }
}

/// The person deliberately deletes an archive spanning multiple sync pages on tower and confirms
/// tower's brake. They must confirm at most once per batch on pi for both ApplyDeletions and
/// KeepLocalFiles (default CI runs 300 files across 100-record pages; the 3,000-file default-page
/// stress test is `r10_inbound_brake_3000_files_default_page`).
#[test]
fn r10_inbound_brake_confirmations_per_page() {
    run_r10_inbound_brake(300, 20, 100);
}

#[test]
#[ignore]
fn r10_inbound_brake_3000_files_default_page() {
    run_r10_inbound_brake(3_000, 200, legion_sync::DEFAULT_PAGE_LIMIT);
}

struct LyingPeer<'a> {
    inner: EnginePeer<'a, RealFs, RedbIndexStore>,
}
impl Peer for LyingPeer<'_> {
    fn member_id(&self) -> &str {
        self.inner.member_id()
    }
    fn handshake(&self, c: &HandshakeHello) -> legion_sync::Result<PeerHandshake> {
        let mut h = self.inner.handshake(c)?;
        h.caller_regressed = true;
        Ok(h)
    }
    fn records_since(
        &self,
        a: u64,
        b: u64,
        l: usize,
        c: Option<&AtomicBool>,
    ) -> legion_sync::Result<RecordsPage> {
        self.inner.records_since(a, b, l, c)
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

/// A paired but dishonest member ("nas") tells pi once an hour that pi's history went back.
#[test]
fn r11_liar_cuts_honest_member_off() {
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
    tower.scan(ts(0)).unwrap();
    for k in 0..2 {
        pull(&mut pi, &tower, 1 + k);
        pull(&mut nas, &tower, 1 + k);
        pull(&mut tower, &pi, 1 + k);
        pull(&mut tower, &nas, 1 + k);
        pull(&mut pi, &nas, 1 + k);
        pull(&mut nas, &pi, 1 + k);
    }
    let mut incs = vec![pi.meta().unwrap().incarnation];
    for h in 1..=12i64 {
        let now = h * 3700;
        let r = pi.pull_from_peer(
            &LyingPeer {
                inner: EnginePeer::new(&nas, "pi"),
            },
            ts(now),
        );
        if h >= 2 {
            assert!(r.is_err(), "hour {h}: liar should be rate-limited");
        }
        let inc = pi.meta().unwrap().incarnation;
        if *incs.last().unwrap() != inc {
            incs.push(inc);
        }
        for k in 0..2 {
            pull(&mut tower, &pi, now + 10 + k);
            pull(&mut pi, &tower, now + 10 + k);
            pull(&mut nas, &pi, now + 10 + k);
        }
        let body = format!("pi's work in hour {h}");
        fs::write(t.join("pi/f2.txt"), &body).unwrap();
        pi.scan(ts(now + 100)).unwrap();
        assert!(pull(&mut tower, &pi, now + 101).is_none());
        let got = fs::read_to_string(t.join("tower/f2.txt")).unwrap_or_default();
        assert_eq!(got, body, "hour {h}: tower must keep receiving pi's work");
        assert_eq!(tower.store().held_back_count().unwrap(), 0);
    }
    assert_eq!(
        incs.len() - 1,
        1,
        "pi should rotate at most once per day for nas"
    );
    let probs = pi.store().list_problems().unwrap();
    assert!(
        probs.iter().any(|(_, p)| {
            p.kind == ProblemKind::PeerRollbackRateLimited
                && p.sentence == "nas keeps reporting that this machine's history went back."
        }),
        "expected PeerRollbackRateLimited problem naming nas, got {probs:?}"
    );
}

/// Free space shared by all writes into `.legion-sync/tmp` (bytes); `ENOSPC` once exhausted.
struct FullFs {
    inner: RealFs,
    budget: Arc<AtomicU64>,
    report_available_space: bool,
    tmp_written: Arc<Mutex<BTreeMap<String, u64>>>,
}
struct BudgetWriter {
    inner: Option<Box<dyn SyncWrite>>,
    budget: Arc<AtomicU64>,
    tmp_name: String,
    tmp_written: Arc<Mutex<BTreeMap<String, u64>>>,
}
impl io::Write for BudgetWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let left = self.budget.load(Ordering::SeqCst);
        if left == 0 {
            return Err(io::Error::from_raw_os_error(28));
        }
        let n = (buf.len() as u64).min(left) as usize;
        let w = self.inner.as_mut().unwrap().write(&buf[..n])?;
        self.budget.fetch_sub(w as u64, Ordering::SeqCst);
        *self
            .tmp_written
            .lock()
            .unwrap()
            .entry(self.tmp_name.clone())
            .or_default() += w as u64;
        Ok(w)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.inner.as_mut().unwrap().flush()
    }
}
impl SyncWrite for BudgetWriter {
    fn finish_sync(mut self: Box<Self>) -> legion_sync::Result<()> {
        self.inner.take().unwrap().finish_sync()
    }
}
impl SyncFs for FullFs {
    fn capabilities(&self) -> FsCapabilities {
        self.inner.capabilities()
    }
    fn read_marker(&self) -> legion_sync::Result<Option<legion_sync::FolderMarker>> {
        self.inner.read_marker()
    }
    fn write_marker(&self, marker: &legion_sync::FolderMarker) -> legion_sync::Result<()> {
        self.inner.write_marker(marker)
    }
    fn append_deletion_log(&self, bytes: &[u8]) -> legion_sync::Result<()> {
        self.inner.append_deletion_log(bytes)
    }
    fn read_deletion_log(&self) -> legion_sync::Result<Vec<u8>> {
        self.inner.read_deletion_log()
    }
    fn compact_deletion_log(&self, bytes: &[u8]) -> legion_sync::Result<()> {
        self.inner.compact_deletion_log(bytes)
    }
    fn ensure_internal_dirs(&self) -> legion_sync::Result<()> {
        self.inner.ensure_internal_dirs()
    }
    fn check_not_nested(&self) -> legion_sync::Result<()> {
        self.inner.check_not_nested()
    }
    fn walk(&self, ignore: &IgnoreMatcher) -> legion_sync::Result<FsWalkResult> {
        self.inner.walk(ignore)
    }
    fn walk_subtree(
        &self,
        rel_root: &str,
        ignore: &IgnoreMatcher,
    ) -> legion_sync::Result<FsWalkResult> {
        self.inner.walk_subtree(rel_root, ignore)
    }
    fn stat_live(&self, p: &str) -> legion_sync::Result<Option<FsMeta>> {
        self.inner.stat_live(p)
    }
    fn hash_live(&self, p: &str) -> legion_sync::Result<(Hash, u64)> {
        self.inner.hash_live(p)
    }
    fn open_live_read_at(&self, p: &str, offset: u64) -> legion_sync::Result<Box<dyn Read + Send>> {
        self.inner.open_live_read_at(p, offset)
    }
    fn create_dir_all_live(&self, p: &str) -> legion_sync::Result<()> {
        self.inner.create_dir_all_live(p)
    }
    fn list_dir_live(&self, p: &str) -> legion_sync::Result<Vec<String>> {
        self.inner.list_dir_live(p)
    }
    fn remove_empty_dir_live(&self, p: &str) -> legion_sync::Result<()> {
        self.inner.remove_empty_dir_live(p)
    }
    fn create_symlink_live(&self, p: &str, target: &str) -> legion_sync::Result<()> {
        self.inner.create_symlink_live(p, target)
    }
    fn remove_symlink_live(&self, p: &str) -> legion_sync::Result<()> {
        self.inner.remove_symlink_live(p)
    }
    fn set_live_metadata(
        &self,
        p: &str,
        mtime: Timestamp,
        mode: Option<u32>,
    ) -> legion_sync::Result<()> {
        self.inner.set_live_metadata(p, mtime, mode)
    }
    fn rename_live_to_live_no_replace(&self, a: &str, b: &str) -> legion_sync::Result<()> {
        self.inner.rename_live_to_live_no_replace(a, b)
    }
    fn sync_parent_dir_live(&self, p: &str) {
        self.inner.sync_parent_dir_live(p)
    }
    fn sync_tmp_dir(&self) {
        self.inner.sync_tmp_dir()
    }
    fn create_tmp(&self, t: &str) -> legion_sync::Result<Box<dyn SyncWrite>> {
        let w = self.inner.create_tmp(t)?;
        Ok(Box::new(BudgetWriter {
            inner: Some(w),
            budget: self.budget.clone(),
            tmp_name: t.to_string(),
            tmp_written: self.tmp_written.clone(),
        }))
    }
    fn open_tmp_append(&self, t: &str) -> legion_sync::Result<Box<dyn SyncWrite>> {
        let w = self.inner.open_tmp_append(t)?;
        Ok(Box::new(BudgetWriter {
            inner: Some(w),
            budget: self.budget.clone(),
            tmp_name: t.to_string(),
            tmp_written: self.tmp_written.clone(),
        }))
    }
    fn stat_tmp(&self, t: &str) -> legion_sync::Result<Option<FsMeta>> {
        self.inner.stat_tmp(t)
    }
    fn set_tmp_metadata(
        &self,
        t: &str,
        mtime: Timestamp,
        mode: Option<u32>,
    ) -> legion_sync::Result<()> {
        self.inner.set_tmp_metadata(t, mtime, mode)
    }
    fn hash_tmp(&self, t: &str) -> legion_sync::Result<(Hash, u64)> {
        self.inner.hash_tmp(t)
    }
    fn remove_tmp(&self, t: &str) -> legion_sync::Result<()> {
        let res = self.inner.remove_tmp(t);
        if let Some(freed) = self.tmp_written.lock().unwrap().remove(t) {
            self.budget.fetch_add(freed, Ordering::SeqCst);
        }
        res
    }
    fn list_tmp(&self) -> legion_sync::Result<Vec<String>> {
        self.inner.list_tmp()
    }
    fn commit_tmp_to_live_no_replace(&self, t: &str, p: &str) -> legion_sync::Result<()> {
        self.tmp_written.lock().unwrap().remove(t);
        self.inner.commit_tmp_to_live_no_replace(t, p)
    }
    fn commit_tmp_to_live_no_replace_deferred_dir_sync(
        &self,
        t: &str,
        p: &str,
    ) -> legion_sync::Result<()> {
        self.tmp_written.lock().unwrap().remove(t);
        self.inner
            .commit_tmp_to_live_no_replace_deferred_dir_sync(t, p)
    }
    fn exchange_tmp_with_live(&self, t: &str, p: &str) -> legion_sync::Result<bool> {
        self.inner.exchange_tmp_with_live(t, p)
    }
    fn replace_live_with_tmp_backup(&self, t: &str, p: &str, b: &str) -> legion_sync::Result<bool> {
        self.inner.replace_live_with_tmp_backup(t, p, b)
    }
    fn move_live_to_tmp_no_replace(&self, p: &str, t: &str) -> legion_sync::Result<()> {
        self.inner.move_live_to_tmp_no_replace(p, t)
    }
    fn move_tmp_to_bin_no_replace(&self, t: &str, b: &str) -> legion_sync::Result<()> {
        self.tmp_written.lock().unwrap().remove(t);
        self.inner.move_tmp_to_bin_no_replace(t, b)
    }
    fn move_live_to_bin_no_replace(&self, p: &str, b: &str) -> legion_sync::Result<()> {
        self.inner.move_live_to_bin_no_replace(p, b)
    }
    fn write_bin_meta(&self, m: &str, bytes: &[u8]) -> legion_sync::Result<()> {
        self.inner.write_bin_meta(m, bytes)
    }
    fn read_bin_meta(&self, m: &str) -> legion_sync::Result<Option<Vec<u8>>> {
        self.inner.read_bin_meta(m)
    }
    fn stat_bin(&self, b: &str) -> legion_sync::Result<Option<FsMeta>> {
        self.inner.stat_bin(b)
    }
    fn hash_bin(&self, b: &str) -> legion_sync::Result<(Hash, u64)> {
        self.inner.hash_bin(b)
    }
    fn copy_bin_to_tmp(&self, b: &str, t: &str) -> legion_sync::Result<()> {
        self.inner.copy_bin_to_tmp(b, t)
    }
    fn move_bin_to_live_no_replace(&self, b: &str, p: &str) -> legion_sync::Result<()> {
        self.inner.move_bin_to_live_no_replace(b, p)
    }
    fn remove_bin_file(&self, b: &str) -> legion_sync::Result<()> {
        self.inner.remove_bin_file(b)
    }
    fn list_bin(&self) -> legion_sync::Result<Vec<String>> {
        self.inner.list_bin()
    }
    fn available_space_for(&self, _rel_path: &str) -> legion_sync::Result<Option<u64>> {
        if self.report_available_space {
            Ok(Some(self.budget.load(Ordering::SeqCst)))
        } else {
            Ok(None)
        }
    }
}

/// tower holds a 6 MB video; pi has 4 MB free. tower also adds small documents after it.
/// With free-space pre-check, video.mkv writes 0 bytes to tmp, all 19 small docs + doc0 edit land
/// on the first pull, pi reports NoSpace, and tower sees PeerRefusedRecords via handshake.
#[test]
fn r12_file_larger_than_free_space_blocks_peer() {
    let tmp = scratch();
    let t = tmp.path();
    let all = ["tower", "pi"];
    fs::create_dir_all(t.join("tower")).unwrap();
    fs::write(t.join("tower/doc0.txt"), b"doc 0").unwrap();
    let mut tower = open_real(&t.join("tower"), &t.join("tower.redb"), "tower", &all);
    tower.scan(ts(0)).unwrap();
    let budget = Arc::new(AtomicU64::new(4 << 20));
    let mut pi = SyncEngine::open(
        cfg("pi", &all),
        FullFs {
            inner: RealFs::open(t.join("pi")).unwrap(),
            budget: budget.clone(),
            report_available_space: true,
            tmp_written: Arc::new(Mutex::new(BTreeMap::new())),
        },
        RedbIndexStore::open(t.join("pi.redb")).unwrap(),
    )
    .unwrap();
    pi.pull_from_peer(&EnginePeer::new(&tower, "pi"), ts(1))
        .unwrap();

    fs::write(t.join("tower/video.mkv"), body("video;", 6 << 20)).unwrap();
    tower.scan(ts(10)).unwrap();
    for i in 1..20 {
        fs::write(t.join(format!("tower/doc{i}.txt")), format!("doc {i}")).unwrap();
        tower.scan(ts(10 + i)).unwrap();
    }
    fs::write(t.join("tower/doc0.txt"), b"doc 0 edited on tower").unwrap();
    fs::remove_file(t.join("tower/doc1.txt")).unwrap();
    tower.scan(ts(40)).unwrap();

    for k in 0..5 {
        budget.store(4 << 20, Ordering::SeqCst);
        let rep = pi
            .pull_from_peer(&EnginePeer::new(&tower, "pi"), ts(50 + k))
            .unwrap();
        let docs = (0..20)
            .filter(|i| t.join(format!("pi/doc{i}.txt")).exists())
            .count();
        assert_eq!(docs, 19, "pull {k}: all 19 remaining docs must arrive");
        assert!(
            fs::read_to_string(t.join("pi/doc0.txt"))
                .unwrap()
                .contains("edited")
        );
        assert!(!t.join("pi/video.mkv").exists());
        let used = (4u64 << 20) - budget.load(Ordering::SeqCst);
        if k == 0 {
            assert_eq!(rep.held_back, 1);
            // Only the tiny docs (~200 bytes total) should have touched tmp; 0 bytes for video.mkv!
            assert!(
                used < 4096,
                "video.mkv should have written 0 bytes (used={used})"
            );
        } else {
            assert_eq!(
                used, 0,
                "pull {k}: pre-check should reject video.mkv with 0 bytes written"
            );
        }
    }
    let pi_st = pi.status().unwrap();
    match &pi_st.state {
        MemberState::Problem { problems, .. } => {
            assert!(
                problems
                    .iter()
                    .any(|p| p.kind == ProblemKind::NoSpace && p.path == "video.mkv")
            );
        }
        other => panic!("expected NoSpace problem on pi, got {other:?}"),
    }
    // Handshake carries pi's refusal summary back to tower.
    tower
        .pull_from_peer(&EnginePeer::new(&pi, "tower"), ts(60))
        .unwrap();
    match &tower.status().unwrap().state {
        MemberState::Problem { problems, .. } => {
            assert!(
                problems
                    .iter()
                    .any(|p| p.kind == ProblemKind::PeerRefusedRecords
                        && p.peer.as_deref() == Some("pi")),
                "tower should surface PeerRefusedRecords for pi, got {problems:?}"
            );
        }
        other => panic!("expected PeerRefusedRecords problem on tower, got {other:?}"),
    }
}

/// Same full-disk scenario when `available_space_for` is unavailable (`None`): reactive `ENOSPC`
/// cleans up the partial temp file, holds back only `video.mkv`, and delivers all 19 small docs.
#[test]
fn r12b_reactive_enospc_without_space_query_continues_sync() {
    let tmp = scratch();
    let t = tmp.path();
    let all = ["tower", "pi"];
    fs::create_dir_all(t.join("tower")).unwrap();
    fs::write(t.join("tower/doc0.txt"), b"doc 0").unwrap();
    let mut tower = open_real(&t.join("tower"), &t.join("tower.redb"), "tower", &all);
    tower.scan(ts(0)).unwrap();
    let budget = Arc::new(AtomicU64::new(4 << 20));
    let mut pi = SyncEngine::open(
        cfg("pi", &all),
        FullFs {
            inner: RealFs::open(t.join("pi")).unwrap(),
            budget: budget.clone(),
            report_available_space: false,
            tmp_written: Arc::new(Mutex::new(BTreeMap::new())),
        },
        RedbIndexStore::open(t.join("pi.redb")).unwrap(),
    )
    .unwrap();
    pi.pull_from_peer(&EnginePeer::new(&tower, "pi"), ts(1))
        .unwrap();

    fs::write(t.join("tower/video.mkv"), body("video;", 6 << 20)).unwrap();
    tower.scan(ts(10)).unwrap();
    for i in 1..20 {
        fs::write(t.join(format!("tower/doc{i}.txt")), format!("doc {i}")).unwrap();
        tower.scan(ts(10 + i)).unwrap();
    }
    fs::write(t.join("tower/doc0.txt"), b"doc 0 edited on tower").unwrap();
    fs::remove_file(t.join("tower/doc1.txt")).unwrap();
    tower.scan(ts(40)).unwrap();

    let rep = pi
        .pull_from_peer(&EnginePeer::new(&tower, "pi"), ts(50))
        .unwrap();
    assert_eq!(rep.held_back, 1);
    let docs = (0..20)
        .filter(|i| t.join(format!("pi/doc{i}.txt")).exists())
        .count();
    assert_eq!(docs, 19);
    assert!(
        fs::read_to_string(t.join("pi/doc0.txt"))
            .unwrap()
            .contains("edited")
    );
    assert!(!t.join("pi/video.mkv").exists());
    match &pi.status().unwrap().state {
        MemberState::Problem { problems, .. } => {
            assert!(
                problems
                    .iter()
                    .any(|p| p.kind == ProblemKind::NoSpace && p.path == "video.mkv")
            );
        }
        other => panic!("expected NoSpace problem on pi, got {other:?}"),
    }
}

/// The person removes the laptop from the folder on tower by mistake while the laptop is off,
/// and adds it back the same evening. Neither healthy member rotates, and no conflict copies form.
#[test]
fn r13_remove_and_readd_forces_healthy_rotation() {
    let tmp = scratch();
    let t = tmp.path();
    let all = ["tower", "laptop"];
    fs::create_dir_all(t.join("tower")).unwrap();
    for i in 0..5 {
        fs::write(t.join(format!("tower/f{i}.txt")), format!("base {i}")).unwrap();
    }
    let mut tower = open_real(&t.join("tower"), &t.join("tower.redb"), "tower", &all);
    let mut laptop = open_real(&t.join("laptop"), &t.join("laptop.redb"), "laptop", &all);
    tower.scan(ts(0)).unwrap();
    for k in 0..2 {
        assert!(pull(&mut laptop, &tower, 1 + k).is_none());
        assert!(pull(&mut tower, &laptop, 1 + k).is_none());
    }
    let tower_inc = tower.meta().unwrap().incarnation;
    let laptop_inc = laptop.meta().unwrap().incarnation;
    tower.remove_member("laptop").unwrap();
    fs::write(t.join("tower/f3.txt"), b"tower edit after removal").unwrap();
    tower.scan(ts(10)).unwrap();
    tower.add_member("laptop").unwrap();
    for k in 0..4 {
        assert!(pull(&mut laptop, &tower, 20 + k * 4000).is_none());
        assert!(pull(&mut tower, &laptop, 21 + k * 4000).is_none());
    }
    assert_eq!(
        tower.meta().unwrap().incarnation,
        tower_inc,
        "tower should not rotate"
    );
    assert_eq!(
        laptop.meta().unwrap().incarnation,
        laptop_inc,
        "laptop should not rotate"
    );
    assert_eq!(
        fs::read_to_string(t.join("laptop/f3.txt")).unwrap(),
        "tower edit after removal"
    );
    let conflicts: Vec<_> = fs::read_dir(t.join("tower"))
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.contains("changed on"))
        .collect();
    assert!(conflicts.is_empty(), "unexpected conflicts: {conflicts:?}");
    assert_eq!(tower.status().unwrap().state, MemberState::UpToDate);
    assert_eq!(laptop.status().unwrap().state, MemberState::UpToDate);
}

/// Portable `SyncFs` wrapper simulating a subdirectory (`VMs/`) mounted on a different device:
/// `needs_local_staging("VMs/...")` returns `Ok(true)`, and any rename from `.legion-sync/tmp`
/// (flat token without `/`) into `VMs/` fails with `FsErrorKind::CrossDevice`.
struct CrossDeviceSubdirFs {
    inner: RealFs,
    prefix: &'static str,
}
impl CrossDeviceSubdirFs {
    fn is_other_dev(&self, rel: &str) -> bool {
        rel == self.prefix || rel.starts_with(&format!("{}/", self.prefix))
    }
    fn check_same_dev(&self, tmp: &str, live: &str) -> legion_sync::Result<()> {
        if self.is_other_dev(live) && !tmp.starts_with(&format!("{}/", self.prefix)) {
            return Err(SyncError::Fs {
                path: Some(live.to_string()),
                detail: "simulated cross-device link".to_string(),
                kind: FsErrorKind::CrossDevice,
            });
        }
        Ok(())
    }
}
impl SyncFs for CrossDeviceSubdirFs {
    fn capabilities(&self) -> FsCapabilities {
        self.inner.capabilities()
    }
    fn read_marker(&self) -> legion_sync::Result<Option<legion_sync::FolderMarker>> {
        self.inner.read_marker()
    }
    fn write_marker(&self, marker: &legion_sync::FolderMarker) -> legion_sync::Result<()> {
        self.inner.write_marker(marker)
    }
    fn append_deletion_log(&self, bytes: &[u8]) -> legion_sync::Result<()> {
        self.inner.append_deletion_log(bytes)
    }
    fn read_deletion_log(&self) -> legion_sync::Result<Vec<u8>> {
        self.inner.read_deletion_log()
    }
    fn compact_deletion_log(&self, bytes: &[u8]) -> legion_sync::Result<()> {
        self.inner.compact_deletion_log(bytes)
    }
    fn ensure_internal_dirs(&self) -> legion_sync::Result<()> {
        self.inner.ensure_internal_dirs()
    }
    fn check_not_nested(&self) -> legion_sync::Result<()> {
        self.inner.check_not_nested()
    }
    fn walk(&self, ignore: &IgnoreMatcher) -> legion_sync::Result<FsWalkResult> {
        self.inner.walk(ignore)
    }
    fn walk_subtree(
        &self,
        rel_root: &str,
        ignore: &IgnoreMatcher,
    ) -> legion_sync::Result<FsWalkResult> {
        self.inner.walk_subtree(rel_root, ignore)
    }
    fn stat_live(&self, p: &str) -> legion_sync::Result<Option<FsMeta>> {
        self.inner.stat_live(p)
    }
    fn hash_live(&self, p: &str) -> legion_sync::Result<(Hash, u64)> {
        self.inner.hash_live(p)
    }
    fn open_live_read_at(&self, p: &str, offset: u64) -> legion_sync::Result<Box<dyn Read + Send>> {
        self.inner.open_live_read_at(p, offset)
    }
    fn create_dir_all_live(&self, p: &str) -> legion_sync::Result<()> {
        self.inner.create_dir_all_live(p)
    }
    fn list_dir_live(&self, p: &str) -> legion_sync::Result<Vec<String>> {
        self.inner.list_dir_live(p)
    }
    fn remove_empty_dir_live(&self, p: &str) -> legion_sync::Result<()> {
        self.inner.remove_empty_dir_live(p)
    }
    fn create_symlink_live(&self, p: &str, target: &str) -> legion_sync::Result<()> {
        self.inner.create_symlink_live(p, target)
    }
    fn remove_symlink_live(&self, p: &str) -> legion_sync::Result<()> {
        self.inner.remove_symlink_live(p)
    }
    fn set_live_metadata(
        &self,
        p: &str,
        mtime: Timestamp,
        mode: Option<u32>,
    ) -> legion_sync::Result<()> {
        self.inner.set_live_metadata(p, mtime, mode)
    }
    fn rename_live_to_live_no_replace(&self, a: &str, b: &str) -> legion_sync::Result<()> {
        self.inner.rename_live_to_live_no_replace(a, b)
    }
    fn sync_parent_dir_live(&self, p: &str) {
        self.inner.sync_parent_dir_live(p)
    }
    fn sync_tmp_dir(&self) {
        self.inner.sync_tmp_dir()
    }
    fn create_tmp(&self, t: &str) -> legion_sync::Result<Box<dyn SyncWrite>> {
        self.inner.create_tmp(t)
    }
    fn open_tmp_append(&self, t: &str) -> legion_sync::Result<Box<dyn SyncWrite>> {
        self.inner.open_tmp_append(t)
    }
    fn stat_tmp(&self, t: &str) -> legion_sync::Result<Option<FsMeta>> {
        self.inner.stat_tmp(t)
    }
    fn set_tmp_metadata(
        &self,
        t: &str,
        mtime: Timestamp,
        mode: Option<u32>,
    ) -> legion_sync::Result<()> {
        self.inner.set_tmp_metadata(t, mtime, mode)
    }
    fn hash_tmp(&self, t: &str) -> legion_sync::Result<(Hash, u64)> {
        self.inner.hash_tmp(t)
    }
    fn remove_tmp(&self, t: &str) -> legion_sync::Result<()> {
        self.inner.remove_tmp(t)
    }
    fn list_tmp(&self) -> legion_sync::Result<Vec<String>> {
        self.inner.list_tmp()
    }
    fn commit_tmp_to_live_no_replace(&self, t: &str, p: &str) -> legion_sync::Result<()> {
        self.check_same_dev(t, p)?;
        self.inner.commit_tmp_to_live_no_replace(t, p)
    }
    fn commit_tmp_to_live_no_replace_deferred_dir_sync(
        &self,
        t: &str,
        p: &str,
    ) -> legion_sync::Result<()> {
        self.check_same_dev(t, p)?;
        self.inner
            .commit_tmp_to_live_no_replace_deferred_dir_sync(t, p)
    }
    fn exchange_tmp_with_live(&self, t: &str, p: &str) -> legion_sync::Result<bool> {
        self.check_same_dev(t, p)?;
        self.inner.exchange_tmp_with_live(t, p)
    }
    fn replace_live_with_tmp_backup(&self, t: &str, p: &str, b: &str) -> legion_sync::Result<bool> {
        self.check_same_dev(t, p)?;
        self.check_same_dev(b, p)?;
        self.inner.replace_live_with_tmp_backup(t, p, b)
    }
    fn move_live_to_tmp_no_replace(&self, p: &str, t: &str) -> legion_sync::Result<()> {
        self.check_same_dev(t, p)?;
        self.inner.move_live_to_tmp_no_replace(p, t)
    }
    fn move_tmp_to_bin_no_replace(&self, t: &str, b: &str) -> legion_sync::Result<()> {
        self.inner.move_tmp_to_bin_no_replace(t, b)
    }
    fn move_live_to_bin_no_replace(&self, p: &str, b: &str) -> legion_sync::Result<()> {
        self.inner.move_live_to_bin_no_replace(p, b)
    }
    fn write_bin_meta(&self, m: &str, bytes: &[u8]) -> legion_sync::Result<()> {
        self.inner.write_bin_meta(m, bytes)
    }
    fn read_bin_meta(&self, m: &str) -> legion_sync::Result<Option<Vec<u8>>> {
        self.inner.read_bin_meta(m)
    }
    fn stat_bin(&self, b: &str) -> legion_sync::Result<Option<FsMeta>> {
        self.inner.stat_bin(b)
    }
    fn hash_bin(&self, b: &str) -> legion_sync::Result<(Hash, u64)> {
        self.inner.hash_bin(b)
    }
    fn copy_bin_to_tmp(&self, b: &str, t: &str) -> legion_sync::Result<()> {
        self.inner.copy_bin_to_tmp(b, t)
    }
    fn move_bin_to_live_no_replace(&self, b: &str, p: &str) -> legion_sync::Result<()> {
        self.inner.move_bin_to_live_no_replace(b, p)
    }
    fn remove_bin_file(&self, b: &str) -> legion_sync::Result<()> {
        self.inner.remove_bin_file(b)
    }
    fn list_bin(&self) -> legion_sync::Result<Vec<String>> {
        self.inner.list_bin()
    }
    fn needs_local_staging(&self, rel_path: &str) -> legion_sync::Result<bool> {
        Ok(self.is_other_dev(rel_path))
    }
}

/// Portable separate-mount test: `VMs/` sits on another device, so staging beside destination
/// handles create, replace, and delete-to-bin cleanly across all OSes.
#[test]
fn r14_separate_mount_inside_folder_syncs_cleanly() {
    let tmp = scratch();
    let t = tmp.path();
    let all = ["tower", "pi"];
    fs::create_dir_all(t.join("tower/VMs")).unwrap();
    fs::write(t.join("tower/VMs/notes.txt"), b"vm notes v1").unwrap();
    fs::write(t.join("tower/doc0.txt"), b"doc 0").unwrap();
    let mut tower = open_real(&t.join("tower"), &t.join("tower.redb"), "tower", &all);
    tower.scan(ts(0)).unwrap();

    fs::create_dir_all(t.join("pi/VMs")).unwrap();
    let mut pi = SyncEngine::open(
        cfg("pi", &all),
        CrossDeviceSubdirFs {
            inner: RealFs::open(t.join("pi")).unwrap(),
            prefix: "VMs",
        },
        RedbIndexStore::open(t.join("pi.redb")).unwrap(),
    )
    .unwrap();

    pi.pull_from_peer(&EnginePeer::new(&tower, "pi"), ts(1))
        .unwrap();
    assert_eq!(
        fs::read(t.join("pi/VMs/notes.txt")).unwrap(),
        b"vm notes v1"
    );
    assert_eq!(fs::read(t.join("pi/doc0.txt")).unwrap(), b"doc 0");

    // Replace VMs/notes.txt and add docs 1..10 + edit doc0.
    fs::write(t.join("tower/VMs/notes.txt"), b"vm notes v2").unwrap();
    for i in 1..10 {
        fs::write(t.join(format!("tower/doc{i}.txt")), format!("doc {i}")).unwrap();
    }
    fs::write(t.join("tower/doc0.txt"), b"doc 0 edited").unwrap();
    tower.scan(ts(20)).unwrap();
    pi.pull_from_peer(&EnginePeer::new(&tower, "pi"), ts(30))
        .unwrap();
    assert_eq!(
        fs::read(t.join("pi/VMs/notes.txt")).unwrap(),
        b"vm notes v2"
    );
    for i in 1..10 {
        assert!(t.join(format!("pi/doc{i}.txt")).exists());
    }
    assert_eq!(fs::read(t.join("pi/doc0.txt")).unwrap(), b"doc 0 edited");
    assert_eq!(pi.status().unwrap().state, MemberState::UpToDate);
}

/// Real btrfs subvolume test on Linux btrfs filesystems (skips cleanly on non-btrfs hosts).
#[test]
fn r14b_real_btrfs_subvolume_inside_folder_syncs_cleanly() {
    #[cfg(target_os = "linux")]
    {
        let tmp = scratch();
        let t = tmp.path();
        let all = ["tower", "pi"];
        fs::create_dir_all(t.join("tower/VMs")).unwrap();
        fs::write(t.join("tower/VMs/notes.txt"), b"vm notes v1").unwrap();
        fs::write(t.join("tower/doc0.txt"), b"doc 0").unwrap();
        let mut tower = open_real(&t.join("tower"), &t.join("tower.redb"), "tower", &all);
        tower.scan(ts(0)).unwrap();
        fs::create_dir_all(t.join("pi")).unwrap();
        let subvol = t.join("pi/VMs");
        let Ok(st) = std::process::Command::new("btrfs")
            .args(["subvolume", "create"])
            .arg(&subvol)
            .output()
        else {
            return;
        };
        if !st.status.success() {
            return;
        }
        let mut pi = open_real(&t.join("pi"), &t.join("pi.redb"), "pi", &all);
        pi.pull_from_peer(&EnginePeer::new(&tower, "pi"), ts(1))
            .unwrap();
        assert_eq!(
            fs::read(t.join("pi/VMs/notes.txt")).unwrap(),
            b"vm notes v1"
        );
        assert_eq!(fs::read(t.join("pi/doc0.txt")).unwrap(), b"doc 0");

        fs::write(t.join("tower/VMs/notes.txt"), b"vm notes v2").unwrap();
        for i in 1..10 {
            fs::write(t.join(format!("tower/doc{i}.txt")), format!("doc {i}")).unwrap();
        }
        fs::write(t.join("tower/doc0.txt"), b"doc 0 edited").unwrap();
        tower.scan(ts(20)).unwrap();
        pi.pull_from_peer(&EnginePeer::new(&tower, "pi"), ts(30))
            .unwrap();
        assert_eq!(
            fs::read(t.join("pi/VMs/notes.txt")).unwrap(),
            b"vm notes v2"
        );
        assert_eq!(fs::read(t.join("pi/doc0.txt")).unwrap(), b"doc 0 edited");
        for i in 1..10 {
            assert!(t.join(format!("pi/doc{i}.txt")).exists());
        }
        assert_eq!(pi.status().unwrap().state, MemberState::UpToDate);
        assert_eq!(pi.store().held_back_count().unwrap(), 0);
        drop(pi);
        let _ = std::process::Command::new("btrfs")
            .args(["subvolume", "delete"])
            .arg(&subvol)
            .output();
        let _ = fs::remove_dir_all(&subvol);
    }
}

#[test]
#[ignore]
fn r15_pending_deletions_slow_every_pull() {
    for (files, deleted) in [(4_000usize, 0usize), (4_000, 900), (20_000, 4_500)] {
        let tmp = scratch();
        let t = tmp.path();
        let all = ["tower", "pi"];
        fs::create_dir_all(t.join("pi/old")).unwrap();
        for i in 0..files {
            fs::write(t.join(format!("pi/old/o{i}.txt")), format!("o{i}")).unwrap();
        }
        let mut pi = open_real(&t.join("pi"), &t.join("pi.redb"), "pi", &all);
        pi.scan(ts(0)).unwrap();
        for i in 0..deleted {
            fs::remove_file(t.join(format!("pi/old/o{i}.txt"))).unwrap();
        }
        let _ = pi.scan(ts(10)).unwrap();
        fs::create_dir_all(t.join("tower/new")).unwrap();
        for i in 0..1000 {
            fs::write(t.join(format!("tower/new/n{i}.txt")), format!("n{i}")).unwrap();
        }
        let mut tower = open_real(&t.join("tower"), &t.join("tower.redb"), "tower", &all);
        tower.scan(ts(11)).unwrap();
        let start = std::time::Instant::now();
        let rep = pi
            .pull_from_peer(&EnginePeer::new(&tower, "pi"), ts(12))
            .unwrap();
        println!(
            "{files} files, {deleted} held deletions: pull of 1000 records took {:?} (applied={})",
            start.elapsed(),
            rep.applied
        );
        assert_eq!(rep.applied, 1001);
    }
}

fn two_restored_case(seed0: u64) -> Vec<String> {
    let tmp = scratch();
    let t = tmp.path();
    let names = ["tower", "pi", "nas"];
    fs::create_dir_all(t.join("tower")).unwrap();
    for i in 0..10 {
        fs::write(t.join(format!("tower/f{i}.txt")), format!("base {i}")).unwrap();
    }
    fs::write(t.join("tower/doomed.txt"), b"doomed").unwrap();
    let mut es: Vec<E> = names
        .iter()
        .map(|n| open_real(&t.join(n), &t.join(format!("{n}.redb")), n, &names))
        .collect();
    es[0].scan(ts(0)).unwrap();
    let mut seed = seed0;
    let round = |es: &mut Vec<E>, now: i64, seed: &mut u64| {
        for (a, b) in shuffled_edges(3, seed) {
            let (x, y) = if a < b {
                let (l, r) = es.split_at_mut(b);
                (&mut l[a], &r[0])
            } else {
                let (l, r) = es.split_at_mut(a);
                (&mut r[0], &l[b])
            };
            let _ = pull(x, y, now);
        }
    };
    for k in 0..3 {
        round(&mut es, 1 + k, &mut seed);
    }
    let nas = es.pop().unwrap();
    let pi = es.pop().unwrap();
    drop(nas);
    drop(pi);
    for n in ["pi", "nas"] {
        copy_tree(&t.join(n), &t.join(format!("backup_{n}")));
        fs::copy(
            t.join(format!("{n}.redb")),
            t.join(format!("backup_{n}.redb")),
        )
        .unwrap();
    }
    es.push(open_real(&t.join("pi"), &t.join("pi.redb"), "pi", &names));
    es.push(open_real(
        &t.join("nas"),
        &t.join("nas.redb"),
        "nas",
        &names,
    ));
    fs::write(t.join("pi/f1.txt"), b"pi after backup").unwrap();
    fs::write(t.join("nas/f2.txt"), b"nas after backup").unwrap();
    fs::remove_file(t.join("tower/doomed.txt")).unwrap();
    for engine in &mut es {
        engine.scan(ts(10)).unwrap();
    }
    for k in 0..2 {
        round(&mut es, 11 + k, &mut seed);
    }

    let nas = es.pop().unwrap();
    let pi = es.pop().unwrap();
    drop(nas);
    drop(pi);
    for n in ["pi", "nas"] {
        fs::remove_dir_all(t.join(n)).unwrap();
        fs::remove_file(t.join(format!("{n}.redb"))).unwrap();
        copy_tree(&t.join(format!("backup_{n}")), &t.join(n));
        fs::copy(
            t.join(format!("backup_{n}.redb")),
            t.join(format!("{n}.redb")),
        )
        .unwrap();
    }
    es.push(open_real(&t.join("pi"), &t.join("pi.redb"), "pi", &names));
    es.push(open_real(
        &t.join("nas"),
        &t.join("nas.redb"),
        "nas",
        &names,
    ));
    fs::write(t.join("pi/new_pi.txt"), b"TODAY pi").unwrap();
    fs::write(t.join("nas/new_nas.txt"), b"TODAY nas").unwrap();
    es[1].scan(ts(20)).unwrap();
    es[2].scan(ts(20)).unwrap();
    let tower_inc = es[0].meta().unwrap().incarnation;
    for r in 0..12 {
        round(&mut es, 100 + r * 1200, &mut seed);
    }
    let mut issues = Vec::new();
    for n in names {
        let live = live_contents(&t.join(n));
        for body in [
            &b"TODAY pi"[..],
            b"TODAY nas",
            b"pi after backup",
            b"nas after backup",
        ] {
            if !live.values().any(|b| b == body) {
                issues.push(format!(
                    "{:?} not live on {n}",
                    String::from_utf8_lossy(body)
                ));
            }
        }
        if t.join(format!("{n}/doomed.txt")).exists() {
            issues.push(format!("doomed back on {n}"));
        }
    }
    if es[0].meta().unwrap().incarnation != tower_inc {
        issues.push("healthy tower rotated".into());
    }
    let (a, b, c) = (
        live_contents(&t.join("tower")),
        live_contents(&t.join("pi")),
        live_contents(&t.join("nas")),
    );
    if a != b || a != c {
        issues.push("not converged".into());
    }
    issues
}

#[test]
#[ignore]
fn r16_two_members_restored() {
    let seeds: Vec<u64> = (1..=60).collect();
    let results = parallel_map(seeds, two_restored_case);
    let mut summary = BTreeMap::<String, usize>::new();
    for issues in results {
        for i in issues {
            *summary.entry(i).or_default() += 1;
        }
    }
    println!("60 seeds: {summary:#?}");
    assert!(summary.is_empty(), "two-restored failures: {summary:#?}");
}

#[test]
fn r16b_trace() {
    let seed: u64 = std::env::var("SEED")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(1);
    let issues = two_restored_case(seed);
    assert!(issues.is_empty(), "seed {seed}: {issues:?}");
}

/// tower deletes an old file; pi acknowledges; the tombstone is collected. Later two new laptops
/// are set up from the same old USB copy of the folder; they meet each other before tower.
#[test]
fn r17_two_joiners_with_same_old_copy_resurrect() {
    for order in ["joiners-first", "tower-first"] {
        let tmp = scratch();
        let t = tmp.path();
        let all = ["tower", "pi"];
        fs::create_dir_all(t.join("tower")).unwrap();
        fs::write(t.join("tower/old-tax-return.pdf"), b"deleted on purpose").unwrap();
        fs::write(t.join("tower/keep.txt"), b"keep").unwrap();
        let mut tower = open_real(&t.join("tower"), &t.join("tower.redb"), "tower", &all);
        let mut pi = open_real(&t.join("pi"), &t.join("pi.redb"), "pi", &all);
        tower.scan(ts(0)).unwrap();
        for k in 0..2 {
            pull(&mut pi, &tower, 1 + k);
            pull(&mut tower, &pi, 1 + k);
        }
        copy_tree(&t.join("tower"), &t.join("usb"));
        fs::remove_dir_all(t.join("usb/.legion-sync")).unwrap();
        fs::remove_file(t.join("tower/old-tax-return.pdf")).unwrap();
        tower.scan(ts(10)).unwrap();
        for k in 0..3 {
            pull(&mut pi, &tower, 11 + k);
            pull(&mut tower, &pi, 11 + k);
        }
        assert!(
            tower
                .store()
                .get_record("old-tax-return.pdf")
                .unwrap()
                .is_none()
        );
        let all4 = ["tower", "pi", "laptop1", "laptop2"];
        for e in [&mut tower, &mut pi] {
            e.add_member("laptop1").unwrap();
            e.add_member("laptop2").unwrap();
        }
        copy_tree(&t.join("usb"), &t.join("laptop1"));
        copy_tree(&t.join("usb"), &t.join("laptop2"));
        let mut l1 = open_real(&t.join("laptop1"), &t.join("l1.redb"), "laptop1", &all4);
        let mut l2 = open_real(&t.join("laptop2"), &t.join("l2.redb"), "laptop2", &all4);
        l1.scan(ts(20)).unwrap();
        l2.scan(ts(20)).unwrap();
        if order == "joiners-first" {
            pull(&mut l1, &l2, 21);
            pull(&mut l2, &l1, 22);
        }
        for r in 0..4 {
            let now = 30 + r * 10;
            for (a, b) in [
                (0usize, 2usize),
                (2, 0),
                (0, 3),
                (3, 0),
                (2, 3),
                (3, 2),
                (1, 0),
                (0, 1),
                (1, 2),
                (2, 1),
            ] {
                let mut es: [&mut E; 4] = [&mut tower, &mut pi, &mut l1, &mut l2];
                let (x, y): (&mut E, &E) = if a < b {
                    let (l, rr) = es.split_at_mut(b);
                    (&mut *l[a], &*rr[0])
                } else {
                    let (l, rr) = es.split_at_mut(a);
                    (&mut *rr[0], &*l[b])
                };
                let _ = pull(x, y, now);
            }
        }
        for n in ["tower", "pi", "laptop1", "laptop2"] {
            assert!(
                !t.join(format!("{n}/old-tax-return.pdf")).exists(),
                "{order}: old-tax-return.pdf resurrected on {n}"
            );
            assert_eq!(fs::read(t.join(format!("{n}/keep.txt"))).unwrap(), b"keep");
        }
    }
}

/// Replacing or deleting an unmodified live file must read the displaced file at most twice
/// (once after displacing to classify it, zero times beforehand when stat matches the index).
#[test]
fn displaced_file_reads_bounded_at_two() {
    for no_exchange in [false, true] {
        let tmp = scratch();
        let t = tmp.path();
        let all = ["tower", "pi"];
        fs::create_dir_all(t.join("tower")).unwrap();
        fs::write(t.join("tower/big.bin"), body("v1;", 64 * 1024)).unwrap();
        let mut tower = open_real(&t.join("tower"), &t.join("tower.redb"), "tower", &all);
        let mut pi = open_hook(&t.join("pi"), &t.join("pi.redb"), "pi", &all);
        pi.fs_mut().no_exchange = no_exchange;
        tower.scan(ts(0)).unwrap();
        pi.pull_from_peer(&EnginePeer::new(&tower, "pi"), ts(1))
            .unwrap();

        // Replace big.bin from tower.
        fs::write(t.join("tower/big.bin"), body("v2;", 64 * 1024)).unwrap();
        tower.scan(ts(10)).unwrap();
        pi.fs().clear();
        pi.pull_from_peer(&EnginePeer::new(&tower, "pi"), ts(11))
            .unwrap();
        let replace_reads = *pi.fs().counts.borrow().get("hash_live").unwrap_or(&0)
            + *pi.fs().counts.borrow().get("hash_tmp").unwrap_or(&0);
        assert!(
            replace_reads <= 2,
            "no_exchange={no_exchange}: replace hashed displaced file {replace_reads} times (limit 2)"
        );

        // Delete big.bin from tower.
        fs::remove_file(t.join("tower/big.bin")).unwrap();
        tower.scan(ts(20)).unwrap();
        pi.fs().clear();
        pi.pull_from_peer(&EnginePeer::new(&tower, "pi"), ts(21))
            .unwrap();
        let delete_reads = *pi.fs().counts.borrow().get("hash_live").unwrap_or(&0)
            + *pi.fs().counts.borrow().get("hash_tmp").unwrap_or(&0);
        assert!(
            delete_reads <= 2,
            "no_exchange={no_exchange}: delete hashed displaced file {delete_reads} times (limit 2)"
        );
    }
}

/// Serving checks file stat against the index before streaming; if a file changes underneath the
/// index (whether stat changes OR same-size/same-mtime bytes change), serving refuses or the
/// fetcher reports a hash mismatch, and the next normal scan re-hashes that path so peers converge.
#[test]
fn serving_stat_mismatch_and_hash_mismatch_trigger_rescan() {
    // Case 1: stat-changing edit on disk without scanning.
    {
        let tmp = scratch();
        let t = tmp.path();
        let all = ["tower", "pi"];
        fs::create_dir_all(t.join("tower")).unwrap();
        fs::write(t.join("tower/doc.txt"), b"initial version").unwrap();
        let mut tower = open_real(&t.join("tower"), &t.join("tower.redb"), "tower", &all);
        let mut pi = open_real(&t.join("pi"), &t.join("pi.redb"), "pi", &all);
        tower.scan(ts(0)).unwrap();

        // Mutate file on disk (different size) BEFORE pi fetches it.
        fs::write(
            t.join("tower/doc.txt"),
            b"edited on disk with a different length",
        )
        .unwrap();
        let rep = pi
            .pull_from_peer(&EnginePeer::new(&tower, "pi"), ts(1))
            .unwrap();
        assert_eq!(
            rep.deferred, 1,
            "stat mismatch before serve should defer cleanly"
        );
        assert!(!t.join("pi/doc.txt").exists());

        tower.scan(ts(2)).unwrap();
        pi.pull_from_peer(&EnginePeer::new(&tower, "pi"), ts(3))
            .unwrap();
        assert_eq!(
            fs::read(t.join("pi/doc.txt")).unwrap(),
            b"edited on disk with a different length"
        );
    }

    // Case 2: same-size, same-mtime rewrite on disk without scanning -> fetcher detects hash
    // mismatch, notifies peer via `report_hash_mismatch`, and the next normal scan re-hashes the
    // path despite identical size and mtime.
    {
        let tmp = scratch();
        let t = tmp.path();
        let all = ["tower", "pi"];
        fs::create_dir_all(t.join("tower")).unwrap();
        let p = t.join("tower/same_stat.txt");
        fs::write(&p, b"AAAA1111").unwrap();
        let orig_mtime = fs::metadata(&p).unwrap().modified().unwrap();
        let mut tower = open_real(&t.join("tower"), &t.join("tower.redb"), "tower", &all);
        let mut pi = open_real(&t.join("pi"), &t.join("pi.redb"), "pi", &all);
        tower.scan(ts(0)).unwrap();

        // Overwrite with same length AND restore exact original mtime.
        fs::write(&p, b"BBBB2222").unwrap();
        fs::OpenOptions::new()
            .write(true)
            .open(&p)
            .unwrap()
            .set_modified(orig_mtime)
            .unwrap();

        let rep = pi
            .pull_from_peer(&EnginePeer::new(&tower, "pi"), ts(1))
            .unwrap();
        assert_eq!(rep.deferred, 1, "hash mismatch should defer cleanly");
        assert!(!t.join("pi/same_stat.txt").exists());

        // Normal scan (verify_checksums = false) MUST still re-hash `same_stat.txt` due to the hint.
        let scan_rep = tower.scan(ts(2)).unwrap();
        assert!(scan_rep.modified >= 1, "scan must re-hash hinted path");
        pi.pull_from_peer(&EnginePeer::new(&tower, "pi"), ts(3))
            .unwrap();
        assert_eq!(fs::read(t.join("pi/same_stat.txt")).unwrap(), b"BBBB2222");
    }
}

/// Deleted history evicts oldest entries first (not lexicographically) when past the cap, and
/// recreating a deleted file intentionally after tombstone GC succeeds even with identical bytes.
#[test]
fn deleted_history_evicts_oldest_first_and_allows_intentional_restore() {
    // Part 1: Oldest-first eviction past MAX_DELETED_HISTORY_PATHS.
    let mut mem = MemoryIndexStore::new();
    let h = legion_xfer::hash::hash_bytes(b"payload");
    let mut batch = legion_sync::StoreBatch::default();
    // Insert paths with reverse-alphabetical names first ("z_oldest") and "a_newest" last.
    batch
        .upsert_deleted_history
        .push(("z_oldest.txt".to_string(), vec![h]));
    for i in 1..MAX_DELETED_HISTORY_PATHS {
        batch
            .upsert_deleted_history
            .push((format!("m_{i:06}.txt"), vec![h]));
    }
    batch
        .upsert_deleted_history
        .push(("a_newest.txt".to_string(), vec![h]));
    mem.apply_batch(batch).unwrap();
    assert!(
        mem.get_deleted_history("z_oldest.txt").unwrap().is_empty(),
        "oldest entry must be evicted first regardless of alphabetical order"
    );
    assert_eq!(mem.get_deleted_history("a_newest.txt").unwrap().len(), 1);

    // Part 2: Intentional user restore with the exact same bytes after tombstone GC.
    let tmp = scratch();
    let t = tmp.path();
    let all = ["tower", "pi"];
    fs::create_dir_all(t.join("tower")).unwrap();
    fs::write(t.join("tower/reborn.txt"), b"identical content").unwrap();
    let mut tower = open_real(&t.join("tower"), &t.join("tower.redb"), "tower", &all);
    let mut pi = open_real(&t.join("pi"), &t.join("pi.redb"), "pi", &all);
    tower.scan(ts(0)).unwrap();
    sync_all(&mut [&mut tower, &mut pi], 1);

    fs::remove_file(t.join("tower/reborn.txt")).unwrap();
    tower.scan(ts(10)).unwrap();
    sync_all(&mut [&mut tower, &mut pi], 11);
    let _ = tower.gc_tombstones().unwrap();
    let _ = pi.gc_tombstones().unwrap();
    assert!(tower.store().get_record("reborn.txt").unwrap().is_none());
    assert!(pi.store().get_record("reborn.txt").unwrap().is_none());

    // The person intentionally puts the file back with the exact same bytes.
    fs::write(t.join("tower/reborn.txt"), b"identical content").unwrap();
    tower.scan(ts(20)).unwrap();
    sync_all(&mut [&mut tower, &mut pi], 21);
    assert_eq!(
        fs::read(t.join("tower/reborn.txt")).unwrap(),
        b"identical content"
    );
    assert_eq!(
        fs::read(t.join("pi/reborn.txt")).unwrap(),
        b"identical content"
    );
}

struct VirtualTickReader {
    data: Vec<u8>,
    pos: usize,
    chunk: usize,
    tick_ms: u64,
    eof_tick_ms: u64,
    clock_ms: Arc<AtomicU64>,
}
impl Read for VirtualTickReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.pos >= self.data.len() {
            self.clock_ms.fetch_add(self.eof_tick_ms, Ordering::SeqCst);
            return Ok(0);
        }
        self.clock_ms.fetch_add(self.tick_ms, Ordering::SeqCst);
        let n = (self.data.len() - self.pos).min(self.chunk).min(buf.len());
        buf[..n].copy_from_slice(&self.data[self.pos..self.pos + n]);
        self.pos += n;
        Ok(n)
    }
}

struct VirtualTimedPeer<'a> {
    inner: EnginePeer<'a, RealFs, RedbIndexStore>,
    payload: Vec<u8>,
    chunk: usize,
    tick_ms: u64,
    eof_tick_ms: u64,
    clock_ms: Arc<AtomicU64>,
}
impl Peer for VirtualTimedPeer<'_> {
    fn member_id(&self) -> &str {
        self.inner.member_id()
    }
    fn handshake(&self, c: &HandshakeHello) -> legion_sync::Result<PeerHandshake> {
        self.inner.handshake(c)
    }
    fn records_since(
        &self,
        a: u64,
        b: u64,
        l: usize,
        c: Option<&AtomicBool>,
    ) -> legion_sync::Result<RecordsPage> {
        self.inner.records_since(a, b, l, c)
    }
    fn fetch_file(
        &self,
        _p: &str,
        _h: Hash,
        o: &FetchOptions,
    ) -> legion_sync::Result<Box<dyn Read + Send>> {
        Ok(Box::new(VirtualTickReader {
            data: self.payload.clone(),
            pos: (o.offset as usize).min(self.payload.len()),
            chunk: self.chunk,
            tick_ms: self.tick_ms,
            eof_tick_ms: self.eof_tick_ms,
            clock_ms: Arc::clone(&self.clock_ms),
        }))
    }
}

struct TrickleReader {
    data: Vec<u8>,
    pos: usize,
    chunk: usize,
    delay: Duration,
}
impl Read for TrickleReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.pos >= self.data.len() {
            return Ok(0);
        }
        if !self.delay.is_zero() {
            std::thread::sleep(self.delay);
        }
        let n = (self.data.len() - self.pos).min(self.chunk).min(buf.len());
        buf[..n].copy_from_slice(&self.data[self.pos..self.pos + n]);
        self.pos += n;
        Ok(n)
    }
}

struct TimedPeer<'a> {
    inner: EnginePeer<'a, RealFs, RedbIndexStore>,
    payload: Vec<u8>,
    chunk: usize,
    delay: Duration,
}
impl Peer for TimedPeer<'_> {
    fn member_id(&self) -> &str {
        self.inner.member_id()
    }
    fn handshake(&self, c: &HandshakeHello) -> legion_sync::Result<PeerHandshake> {
        self.inner.handshake(c)
    }
    fn records_since(
        &self,
        a: u64,
        b: u64,
        l: usize,
        c: Option<&AtomicBool>,
    ) -> legion_sync::Result<RecordsPage> {
        self.inner.records_since(a, b, l, c)
    }
    fn fetch_file(
        &self,
        _p: &str,
        _h: Hash,
        o: &FetchOptions,
    ) -> legion_sync::Result<Box<dyn Read + Send>> {
        Ok(Box::new(TrickleReader {
            data: self.payload.clone(),
            pos: (o.offset as usize).min(self.payload.len()),
            chunk: self.chunk,
            delay: self.delay,
        }))
    }
}

/// Fetch timeout is an IDLE timeout: verified deterministically on a virtual clock (20 chunks
/// each just inside a 30s limit = 600s total succeeds; a single chunk or EOF exceeding 30s aborts
/// cleanly with no leftover tmp), plus a wide-margin real-time smoke test proving `Instant::now()`
/// is wired when no virtual clock is supplied.
#[test]
fn idle_fetch_timeout_allows_slow_progress_and_aborts_stall() {
    let tmp = scratch();
    let t = tmp.path();
    let all = ["tower", "pi"];
    let payload = vec![b'x'; 2_000];
    fs::create_dir_all(t.join("tower")).unwrap();
    fs::write(t.join("tower/stream.bin"), &payload).unwrap();
    let mut tower = open_real(&t.join("tower"), &t.join("tower.redb"), "tower", &all);
    let mut pi = open_real(&t.join("pi"), &t.join("pi.redb"), "pi", &all);
    tower.scan(ts(0)).unwrap();

    // Part 1 (deterministic virtual clock): 20 chunks * 29,999ms = 599.98s total >> 30s idle
    // limit, yet every read arrives 1ms inside the limit -> succeeds with zero wall-clock sleep.
    let clock_ms = Arc::new(AtomicU64::new(0));
    let vclock = {
        let c = Arc::clone(&clock_ms);
        FetchClock::new(move || Duration::from_millis(c.load(Ordering::SeqCst)))
    };
    let slow_ok = VirtualTimedPeer {
        inner: EnginePeer::new(&tower, "pi"),
        payload: payload.clone(),
        chunk: 100,
        tick_ms: 29_999,
        eof_tick_ms: 1_000,
        clock_ms: Arc::clone(&clock_ms),
    };
    let opts = FetchOptions::at_offset(0, Duration::from_secs(30), None).with_clock(vclock.clone());
    pi.pull_from_peer_with_options(&slow_ok, 100, &opts, ts(1))
        .unwrap();
    assert_eq!(fs::read(t.join("pi/stream.bin")).unwrap(), payload);
    assert!(
        clock_ms.load(Ordering::SeqCst) >= 599_000,
        "virtual clock must have crossed well past the 30s idle limit"
    );

    // Part 2 (deterministic virtual clock): a single chunk taking 30,001ms > 30s limit aborts
    // immediately and cleans `.legion-sync/tmp`.
    let payload2 = vec![b'y'; 400];
    fs::write(t.join("tower/stream.bin"), &payload2).unwrap();
    tower.scan(ts(10)).unwrap();
    let stalled_chunk = VirtualTimedPeer {
        inner: EnginePeer::new(&tower, "pi"),
        payload: payload2.clone(),
        chunk: 100,
        tick_ms: 30_001,
        eof_tick_ms: 0,
        clock_ms: Arc::clone(&clock_ms),
    };
    let err = pi
        .pull_from_peer_with_options(&stalled_chunk, 100, &opts, ts(11))
        .unwrap_err();
    assert!(
        err.to_string().to_lowercase().contains("timed out"),
        "expected idle timeout error on stalled chunk, got: {err}"
    );
    assert!(
        pi.fs().list_tmp().unwrap().is_empty(),
        "aborted fetch must clean up tmp files"
    );

    // Part 3 (deterministic virtual clock): stall right before EOF also aborts.
    let stalled_eof = VirtualTimedPeer {
        inner: EnginePeer::new(&tower, "pi"),
        payload: payload2.clone(),
        chunk: 400,
        tick_ms: 1_000,
        eof_tick_ms: 30_001,
        clock_ms: Arc::clone(&clock_ms),
    };
    let err_eof = pi
        .pull_from_peer_with_options(&stalled_eof, 100, &opts, ts(12))
        .unwrap_err();
    assert!(
        err_eof.to_string().to_lowercase().contains("timed out"),
        "expected idle timeout error on stalled EOF, got: {err_eof}"
    );

    // Part 4 (real wall-clock smoke test with order-of-magnitude separated limits):
    // stall (250ms sleep vs 20ms limit) must abort; unstalled (0ms sleep vs 10s limit) succeeds
    // even if a CI runner pauses for seconds.
    let real_stalled = TimedPeer {
        inner: EnginePeer::new(&tower, "pi"),
        payload: payload2.clone(),
        chunk: 100,
        delay: Duration::from_millis(250),
    };
    let real_stall_opts = FetchOptions::at_offset(0, Duration::from_millis(20), None);
    let real_err = pi
        .pull_from_peer_with_options(&real_stalled, 100, &real_stall_opts, ts(13))
        .unwrap_err();
    assert!(
        real_err.to_string().to_lowercase().contains("timed out"),
        "expected real-time idle timeout error, got: {real_err}"
    );

    let real_ok = TimedPeer {
        inner: EnginePeer::new(&tower, "pi"),
        payload: payload2.clone(),
        chunk: 100,
        delay: Duration::ZERO,
    };
    let real_ok_opts = FetchOptions::at_offset(0, Duration::from_secs(10), None);
    pi.pull_from_peer_with_options(&real_ok, 100, &real_ok_opts, ts(14))
        .unwrap();
    assert_eq!(fs::read(t.join("pi/stream.bin")).unwrap(), payload2);
}

struct InflatedVvPeer<'a> {
    inner: EnginePeer<'a, RealFs, RedbIndexStore>,
    extra_incarnations: usize,
}
impl Peer for InflatedVvPeer<'_> {
    fn member_id(&self) -> &str {
        self.inner.member_id()
    }
    fn handshake(&self, c: &HandshakeHello) -> legion_sync::Result<PeerHandshake> {
        self.inner.handshake(c)
    }
    fn records_since(
        &self,
        a: u64,
        b: u64,
        l: usize,
        c: Option<&AtomicBool>,
    ) -> legion_sync::Result<RecordsPage> {
        let mut page = self.inner.records_since(a, b, l, c)?;
        for rec in &mut page.records {
            for i in 1..=self.extra_incarnations {
                rec.version
                    .set(VersionKey::new("pi", 900_000 + i as u64), 1);
            }
        }
        Ok(page)
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

/// 1) 70 consecutive rebuilds stay bounded past the 64-incarnation cap via deterministic VV
///    pruning;
/// 2) When a peer actually presents >64 live incarnations in one VV, the receiver rejects it with
///    an actionable message mentioning remove & re-add, and the sender learns of the refusal via
///    handshake (`PeerRefusedRecords`).
#[test]
fn over_64_incarnations_pruning_and_actionable_visibility() {
    // Part 1: 70 consecutive rebuilds stay healthy via VV pruning.
    {
        let tmp = scratch();
        let t = tmp.path();
        let all = ["tower", "pi"];
        fs::create_dir_all(t.join("tower")).unwrap();
        fs::write(t.join("tower/f.txt"), b"init").unwrap();
        let mut tower = open_real(&t.join("tower"), &t.join("tower.redb"), "tower", &all);
        let mut pi = open_real(&t.join("pi"), &t.join("pi.redb"), "pi", &all);
        tower.scan(ts(0)).unwrap();
        sync_all(&mut [&mut tower, &mut pi], 1);

        for n in 1..=70i64 {
            drop(pi);
            fs::remove_file(t.join("pi.redb")).unwrap();
            let mut p = open_real(&t.join("pi"), &t.join("pi.redb"), "pi", &all);
            p.rebuild_from_disk(ts(100 * n)).unwrap();
            pi = p;
            assert!(pull(&mut pi, &tower, 100 * n + 1).is_none());
            assert!(pull(&mut tower, &pi, 100 * n + 2).is_none());
            assert!(pull(&mut pi, &tower, 100 * n + 3).is_none());
            assert!(pull(&mut tower, &pi, 100 * n + 4).is_none());
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
        assert!(
            pi_keys <= 2,
            "expected <= 2 pi keys after 70 rebuilds, got {pi_keys}"
        );
        assert_eq!(tower.status().unwrap().state, MemberState::UpToDate);
        assert_eq!(pi.status().unwrap().state, MemberState::UpToDate);
    }

    // Part 2: 65 live non-prunable incarnations rejected with actionable guidance & visible on both
    // sides.
    {
        let tmp = scratch();
        let t = tmp.path();
        let all = ["tower", "pi"];
        fs::create_dir_all(t.join("pi")).unwrap();
        fs::write(t.join("pi/over.txt"), b"too many incarnations").unwrap();
        let mut tower = open_real(&t.join("tower"), &t.join("tower.redb"), "tower", &all);
        let mut pi = open_real(&t.join("pi"), &t.join("pi.redb"), "pi", &all);
        pi.scan(ts(0)).unwrap();

        let bad_peer = InflatedVvPeer {
            inner: EnginePeer::new(&pi, "tower"),
            extra_incarnations: 65,
        };
        let rep = tower.pull_from_peer(&bad_peer, ts(1)).unwrap();
        assert_eq!(rep.invalid_skipped, 1);
        match &tower.status().unwrap().state {
            MemberState::Problem { problems, .. } => {
                assert!(
                    problems.iter().any(|p| {
                        p.kind == ProblemKind::InvalidPeerRecord
                            && p.sentence.contains("remove")
                            && p.sentence.contains("add it again")
                    }),
                    "expected actionable remove/re-add message on tower, got {problems:?}"
                );
            }
            other => panic!("expected InvalidPeerRecord problem on tower, got {other:?}"),
        }

        // pi pulls from tower and receives the refusal summary in tower's handshake.
        pi.pull_from_peer(&EnginePeer::new(&tower, "pi"), ts(2))
            .unwrap();
        match &pi.status().unwrap().state {
            MemberState::Problem { problems, .. } => {
                assert!(
                    problems.iter().any(|p| {
                        p.kind == ProblemKind::PeerRefusedRecords
                            && p.peer.as_deref() == Some("tower")
                    }),
                    "expected PeerRefusedRecords on pi naming tower, got {problems:?}"
                );
            }
            other => panic!("expected PeerRefusedRecords problem on pi, got {other:?}"),
        }
    }
}

struct ManyOversizedPeer {
    member: String,
    incarnation: u64,
    total_records: usize,
    file_size: u64,
}
impl Peer for ManyOversizedPeer {
    fn member_id(&self) -> &str {
        &self.member
    }
    fn handshake(&self, _caller: &HandshakeHello) -> legion_sync::Result<PeerHandshake> {
        let head_fp = legion_xfer::hash::hash_bytes(&self.total_records.to_le_bytes());
        Ok(PeerHandshake {
            folder_id: "docs".to_string(),
            member_id: self.member.clone(),
            incarnation: self.incarnation,
            snapshot_seq: self.total_records as u64,
            head_fingerprint: Some(head_fp),
            protocol_version: SYNC_PROTOCOL_VERSION,
            caller_pulled: None,
            caller_acked: None,
            caller_pull_matches: None,
            caller_regressed: false,
            caller_rejoining: false,
            refused_from_caller: None,
            pruned_incarnations: Vec::new(),
            known_incarnations: BTreeMap::new(),
            known_members: BTreeSet::new(),
            unsettled_vv_keys: Vec::new(),
        })
    }
    fn records_since(
        &self,
        _since_incarnation: u64,
        since_seq: u64,
        limit: usize,
        _cancel: Option<&AtomicBool>,
    ) -> legion_sync::Result<RecordsPage> {
        let start = (since_seq as usize) + 1;
        let end = (start + limit - 1).min(self.total_records);
        let mut records = Vec::new();
        let mut chain_fingerprints = BTreeMap::new();
        for seq in start..=end {
            let mut vv = VersionVector::new();
            vv.set(VersionKey::new(&self.member, self.incarnation), seq as u64);
            let rec = IndexRecord {
                path: format!("big_{seq:05}.bin"),
                kind: EntryKind::File,
                size: self.file_size,
                mtime: ts(seq as i64),
                disk_mtime: None,
                disk_change_id: None,
                mode: None,
                hash: Some(legion_xfer::hash::hash_bytes(&seq.to_le_bytes())),
                link_target: None,
                deleted: false,
                version: vv,
                modified_by: self.member.clone(),
                seq: seq as u64,
                recent_hashes: Vec::new(),
                from_join: false,
                conflict_of: None,
                conflict_dismissed: false,
            };
            let fp = rec.head_fingerprint();
            records.push(rec);
            chain_fingerprints.insert(seq as u64, fp);
        }
        let head_fp = legion_xfer::hash::hash_bytes(&self.total_records.to_le_bytes());
        Ok(RecordsPage {
            incarnation: self.incarnation,
            snapshot_seq: self.total_records as u64,
            head_fingerprint: Some(head_fp),
            chain_fingerprints,
            caller_acked: None,
            caller_acked_fingerprint: None,
            records,
            has_more: end < self.total_records,
        })
    }
    fn fetch_file(
        &self,
        path: &str,
        _h: Hash,
        _o: &FetchOptions,
    ) -> legion_sync::Result<Box<dyn Read + Send>> {
        Err(SyncError::NotFound {
            what: path.to_string(),
        })
    }
}

/// Holding back past `MAX_HELD_BACK_ENTRIES` (4,096) for one peer stops only that peer and names
/// it, while other peers keep syncing into the same folder without interference.
#[test]
fn per_peer_held_back_limit_isolates_peers() {
    let tmp = scratch();
    let t = tmp.path();
    let all = ["pi", "bad_peer", "good_peer"];
    fs::create_dir_all(t.join("good_peer")).unwrap();
    fs::write(t.join("good_peer/hello.txt"), b"from good peer").unwrap();
    let mut good_peer = open_real(
        &t.join("good_peer"),
        &t.join("good.redb"),
        "good_peer",
        &all,
    );
    good_peer.scan(ts(0)).unwrap();

    let budget = Arc::new(AtomicU64::new(1 << 20)); // 1 MiB available
    let mut pi = SyncEngine::open(
        cfg("pi", &all),
        FullFs {
            inner: RealFs::open(t.join("pi")).unwrap(),
            budget,
            report_available_space: true,
            tmp_written: Arc::new(Mutex::new(BTreeMap::new())),
        },
        RedbIndexStore::open(t.join("pi.redb")).unwrap(),
    )
    .unwrap();

    let bad_peer = ManyOversizedPeer {
        member: "bad_peer".to_string(),
        incarnation: 42,
        total_records: MAX_HELD_BACK_ENTRIES + 20,
        file_size: 4 << 20, // 4 MiB > 1 MiB available -> rejected by free-space pre-check
    };
    let rep = pi.pull_from_peer(&bad_peer, ts(1)).unwrap();
    assert_eq!(rep.held_back, MAX_HELD_BACK_ENTRIES);
    assert_eq!(pi.store().held_back_count().unwrap(), MAX_HELD_BACK_ENTRIES);

    let probs = pi.store().list_problems().unwrap();
    assert!(
        probs.iter().any(|(_, p)| {
            p.kind == ProblemKind::HeldBackLimitReached
                && p.peer.as_deref() == Some("bad_peer")
                && p.sentence.contains("bad_peer")
        }),
        "expected HeldBackLimitReached naming bad_peer, got {probs:?}"
    );

    // good_peer MUST still sync cleanly despite bad_peer being capped at MAX_HELD_BACK_ENTRIES!
    pi.pull_from_peer(&EnginePeer::new(&good_peer, "pi"), ts(2))
        .unwrap();
    assert_eq!(fs::read(t.join("pi/hello.txt")).unwrap(), b"from good peer");

    // Even if good_peer also has one oversized file of its own, good_peer has its own per-peer
    // held-back quota and is not blocked by bad_peer's 4,096 entries.
    fs::write(t.join("good_peer/tiny_ok.txt"), b"still syncing").unwrap();
    good_peer.scan(ts(3)).unwrap();
    pi.pull_from_peer(&EnginePeer::new(&good_peer, "pi"), ts(4))
        .unwrap();
    assert_eq!(
        fs::read(t.join("pi/tiny_ok.txt")).unwrap(),
        b"still syncing"
    );
}
