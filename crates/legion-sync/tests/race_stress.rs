//! Real-disk safety stress test: a person's program saving a large file on its own
//! thread while the engine applies a remote version of it. No hooks: real timing only.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod safety_common;

use std::fs;
use std::io::Write;
use std::time::{Duration, Instant};

use legion_sync::{IndexStore, ScanOptions};
use safety_common::*;

fn body(tag: &str, size: usize) -> Vec<u8> {
    let mut v = Vec::with_capacity(size);
    while v.len() < size {
        v.extend_from_slice(tag.as_bytes());
    }
    v.truncate(size);
    v
}

#[test]
#[ignore]
fn s1_saves_racing_remote_replacements() {
    let iters: usize = std::env::var("REVIEW_ITERS").ok().and_then(|s| s.parse().ok()).unwrap_or(200);
    let size = 8 << 20;
    let tmp = scratch();
    let t = tmp.path();
    let all = ["tower", "pi"];
    let mut tower = open_real(&t.join("tower"), &t.join("tower.redb"), "tower", &all);
    let mut pi = open_real(&t.join("pi"), &t.join("pi.redb"), "pi", &all);
    fs::create_dir_all(t.join("tower")).unwrap();
    fs::write(t.join("tower/big.bin"), body("tower-0;", size)).unwrap();
    tower.scan(ts(0)).unwrap();
    pi.pull_from_peer(&EnginePeer::new(&tower, "pi"), ts(1)).unwrap();
    let mut lost = Vec::new();
    let mut hidden = 0;
    for it in 0..iters {
        // Make pi converge first.
        for k in 0..3 {
            let _ = pi.scan(ts(10 * it as i64 + k));
            let _ = tower.scan(ts(10 * it as i64 + k));
            let _ = tower.pull_from_peer(&EnginePeer::new(&pi, "tower"), ts(10 * it as i64 + k));
            let _ = pi.pull_from_peer(&EnginePeer::new(&tower, "pi"), ts(10 * it as i64 + k));
        }
        fs::write(t.join("tower/big.bin"), body(&format!("tower-{it};"), size)).unwrap();
        tower.scan(ts(10 * it as i64 + 5)).unwrap();
        let path = t.join("pi/big.bin");
        let d1 = Duration::from_micros((it as u64 * 7919) % 6000);
        let d2 = Duration::from_micros((it as u64 * 104_729) % 4000);
        let save1 = body(&format!("pi-in-place-{it};"), size);
        let save2 = body(&format!("pi-NEWEST-{it};"), size);
        let (s1, s2) = (save1.clone(), save2.clone());
        let p = path.clone();
        let h = std::thread::spawn(move || {
            std::thread::sleep(d1);
            let mut f = fs::OpenOptions::new().write(true).truncate(true).open(&p).unwrap();
            f.write_all(&s1).unwrap();
            drop(f);
            std::thread::sleep(d2);
            write_via_rename(&p, &s2);
        });
        let st = Instant::now();
        let _ = pi.pull_from_peer(&EnginePeer::new(&tower, "pi"), ts(10 * it as i64 + 6));
        let pull_time = st.elapsed();
        h.join().unwrap();
        if where_is(&t.join("pi"), &save2).is_empty() {
            let w1 = where_is(&t.join("pi"), &save1);
            lost.push((it, pull_time, w1));
        }
        // Invisible change: index says one hash, disk holds another, and a normal scan agrees.
        let _ = pi.scan(ts(10 * it as i64 + 7));
        let rec = pi.store().get_record("big.bin").unwrap();
        if let (Some(rec), Ok(bytes)) = (rec, fs::read(&path)) {
            if rec.hash != Some(legion_xfer::hash::hash_bytes(&bytes)) {
                hidden += 1;
                let _ = pi.scan_with_options(ts(10 * it as i64 + 8), ScanOptions { verify_checksums: true, ..ScanOptions::default() });
            }
        }
    }
    println!("{iters} races: newest save lost {} times {:?}; index/disk mismatch after a scan {hidden} times", lost.len(), &lost[..lost.len().min(5)]);
    assert!(lost.is_empty() && hidden == 0);
}
