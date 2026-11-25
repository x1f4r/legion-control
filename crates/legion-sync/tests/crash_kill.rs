//! Real-disk safety stress test: SIGKILL a real process at random moments while
//! two members scan and apply on real directories with real redb indexes, then restart and
//! account for every version a person wrote.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod safety_common;

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use safety_common::*;

const MEMBERS: &[&str] = &["tower", "pi"];

fn big(tag: &str, n: usize) -> Vec<u8> {
    let mut v = tag.as_bytes().to_vec();
    v.resize(n, b'.');
    v.extend_from_slice(tag.as_bytes());
    v
}

/// Builds the starting point and returns the ledger of versions that must survive.
fn setup(root: &Path, seed: u64) -> Vec<Vec<u8>> {
    let mut rng = seed;
    let mut next = || {
        rng ^= rng << 13;
        rng ^= rng >> 7;
        rng ^= rng << 17;
        rng
    };
    let tw = root.join("tower");
    let pw = root.join("pi");
    let mut tower = open_real(&tw, &root.join("tower.redb"), "tower", MEMBERS);
    let mut pi = open_real(&pw, &root.join("pi.redb"), "pi", MEMBERS);
    let n = 120;
    for i in 0..n {
        let d = tw.join(format!("d{}", i % 8));
        fs::create_dir_all(&d).unwrap();
        let body = if i % 17 == 0 {
            big(&format!("tower-v1-{i}"), 600_000)
        } else {
            format!("tower-v1-{i}").into_bytes()
        };
        fs::write(d.join(format!("f{i}.txt")), body).unwrap();
    }
    tower.scan(ts(0)).unwrap();
    pi.pull_from_peer(&EnginePeer::new(&tower, "pi"), ts(1))
        .unwrap();

    let mut ledger: Vec<Vec<u8>> = Vec::new();
    let mut tower_touched = BTreeSet::new();
    let mut pi_touched = BTreeSet::new();
    let mut tower_deleted = BTreeSet::new();
    // Tower: modify, delete, new, dir rename, file->dir.
    for i in 0..n {
        let p = tw.join(format!("d{}/f{i}.txt", i % 8));
        match next() % 6 {
            0 | 1 => {
                let body = if i % 5 == 0 {
                    big(&format!("tower-v2-{i}"), 800_000)
                } else {
                    format!("tower-v2-{i}").into_bytes()
                };
                fs::write(&p, &body).unwrap();
                ledger.push(body);
                tower_touched.insert(i);
            }
            2 => {
                fs::remove_file(&p).unwrap();
                tower_touched.insert(i);
                tower_deleted.insert(i);
            }
            _ => {}
        }
    }
    for i in 0..30 {
        let body = format!("tower-new-{i}").into_bytes();
        fs::create_dir_all(tw.join("new")).unwrap();
        fs::write(tw.join(format!("new/n{i}.txt")), &body).unwrap();
        ledger.push(body);
    }
    // Rename d7 (files not otherwise touched keep their bytes).
    if tw.join("d7").exists() {
        fs::rename(tw.join("d7"), tw.join("d7-renamed")).unwrap();
    }
    tower.scan(ts(100)).unwrap();
    // Pi: edits, some scanned, some not.
    for i in 0..n {
        if next() % 5 == 0 {
            let p = pw.join(format!("d{}/f{i}.txt", i % 8));
            if p.exists() {
                let body = format!("pi-edit-{i}").into_bytes();
                fs::write(&p, &body).unwrap();
                ledger.push(body);
                pi_touched.insert(i);
            }
        }
    }
    if next() % 2 == 0 {
        pi.scan(ts(110)).unwrap();
    }
    // v1 must survive wherever at least one side never overwrote it and tower did not delete it
    // while pi left it alone (that v1 legitimately goes to pi's bin, so it still counts).
    for i in 0..n {
        if !(tower_touched.contains(&i) && pi_touched.contains(&i)) {
            let body = if i % 17 == 0 {
                big(&format!("tower-v1-{i}"), 600_000)
            } else {
                format!("tower-v1-{i}").into_bytes()
            };
            ledger.push(body);
        }
    }
    let _ = &tower_deleted;
    drop(tower);
    drop(pi);
    ledger
}

fn sync_round(root: &Path, t: i64) -> Result<(), String> {
    let mut tower = SyncEngineHolder::open(root, "tower")?;
    let mut pi = SyncEngineHolder::open(root, "pi")?;
    tower.0.scan(ts(t)).map_err(|e| format!("tower scan {e}"))?;
    pi.0.scan(ts(t)).map_err(|e| format!("pi scan {e}"))?;
    pi.0.pull_from_peer(&EnginePeer::new(&tower.0, "pi"), ts(t + 1))
        .map_err(|e| format!("pi pull {e}"))?;
    tower
        .0
        .pull_from_peer(&EnginePeer::new(&pi.0, "tower"), ts(t + 2))
        .map_err(|e| format!("tower pull {e}"))?;
    pi.0.pull_from_peer(&EnginePeer::new(&tower.0, "pi"), ts(t + 3))
        .map_err(|e| format!("pi pull2 {e}"))?;
    Ok(())
}

struct SyncEngineHolder(legion_sync::SyncEngine<legion_sync::RealFs, legion_sync::RedbIndexStore>);
impl SyncEngineHolder {
    fn open(root: &Path, m: &str) -> Result<Self, String> {
        let e = legion_sync::SyncEngine::open(
            cfg(m, MEMBERS),
            legion_sync::RealFs::open(root.join(m)).map_err(|e| e.to_string())?,
            legion_sync::RedbIndexStore::open(root.join(format!("{m}.redb")))
                .map_err(|e| e.to_string())?,
        )
        .map_err(|e| format!("open {m}: {e}"))?;
        Ok(Self(e))
    }
}

#[test]
#[ignore]
fn kill_child_entry() {
    let Ok(root) = std::env::var("REVIEW_ROOT") else {
        return;
    };
    sync_round(Path::new(&root), 200).unwrap();
    println!("CHILD-DONE");
}

fn live_tree(dir: &Path) -> BTreeSet<(String, Vec<u8>)> {
    all_bytes(dir)
        .into_iter()
        .filter(|(p, _)| !p.starts_with(".legion-sync"))
        .collect()
}

#[test]
#[ignore]
fn kill_at_random_moments() {
    let iters: u64 = std::env::var("REVIEW_ITERS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(40);
    let exe = std::env::current_exe().unwrap();
    // Measure an uncrashed child run.
    let mut est = Duration::from_millis(500);
    {
        let tmp = scratch();
        setup(tmp.path(), 1);
        let st = Instant::now();
        let out = Command::new(&exe)
            .args(["kill_child_entry", "--ignored", "--exact", "--nocapture"])
            .env("REVIEW_ROOT", tmp.path())
            .output()
            .unwrap();
        assert!(
            String::from_utf8_lossy(&out.stdout).contains("CHILD-DONE"),
            "{}",
            String::from_utf8_lossy(&out.stdout)
        );
        est = est.max(st.elapsed());
        println!("uncrashed child run: {:?}", st.elapsed());
    }
    let mut failures = Vec::new();
    for it in 0..iters {
        let tmp = scratch();
        let root: PathBuf = tmp.path().to_path_buf();
        let ledger = setup(&root, 0x9e37_79b9 ^ (it * 7919 + 13));
        let mut kills = 0;
        // Up to three kills per iteration, each at a random point.
        for k in 0..3 {
            let delay =
                Duration::from_micros((it * 104_729 + k * 7_727) % (est.as_micros() as u64).max(1));
            let mut child = Command::new(&exe)
                .args(["kill_child_entry", "--ignored", "--exact", "--nocapture"])
                .env("REVIEW_ROOT", &root)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap();
            std::thread::sleep(delay);
            let _ = child.kill();
            let _ = child.wait();
            kills += 1;
        }
        // Restart and finish.
        let mut last_err = None;
        for r in 0..4 {
            if let Err(e) = sync_round(&root, 300 + r * 10) {
                last_err = Some(e);
            } else {
                last_err = None;
            }
        }
        let mut missing = Vec::new();
        for v in &ledger {
            let a = where_is(&root.join("tower"), v);
            let b = where_is(&root.join("pi"), v);
            if a.iter()
                .chain(b.iter())
                .all(|p| p.starts_with(".legion-sync/tmp"))
            {
                missing.push(String::from_utf8_lossy(&v[..v.len().min(20)]).into_owned());
            }
        }
        let lt = live_tree(&root.join("tower"));
        let lp = live_tree(&root.join("pi"));
        let diverged = lt != lp;
        if !missing.is_empty() || diverged || last_err.is_some() {
            let diff: Vec<_> = lt
                .symmetric_difference(&lp)
                .map(|(p, b)| format!("{p}={}", String::from_utf8_lossy(&b[..b.len().min(20)])))
                .take(8)
                .collect();
            println!(
                "iter {it} (kills {kills}): missing {missing:?} diverged {diverged} {diff:?} err {last_err:?}"
            );
            failures.push((it, missing, diverged, last_err));
            std::mem::forget(tmp);
        }
    }
    println!("{} iterations, {} with problems", iters, failures.len());
    assert!(failures.is_empty());
}
