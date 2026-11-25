//! End-to-end 3-node loopback sync folder tests, explicit refusal tests, watcher-disabled
//! convergence, mass-deletion brake decisions, `fleet.sync` CRDT merge, and SIGKILL mid-apply
//! recovery over real temporary directories and the `command` connector.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::collections::BTreeMap;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitCode, Stdio};
use std::time::{Duration, Instant};

use common::{Home, Run};
use serde_json::{Value, json};

const LEGION_BIN: &str = env!("CARGO_BIN_EXE_legion");
const HELPER_FLAG: &str = "--sync-test-helper";

type Test = (&'static str, fn());

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some(HELPER_FLAG) {
        return helper(&args[1..]);
    }
    let filter = args
        .iter()
        .find(|arg| !arg.starts_with('-'))
        .map(String::as_str);

    let tests: &[Test] = &[
        (
            "three_node_lifecycle_create_edit_offline_delete_restore_conflicts_remove",
            three_node_lifecycle_create_edit_offline_delete_restore_conflicts_remove,
        ),
        (
            "sigkill_mid_apply_of_2000_files_loses_nothing",
            sigkill_mid_apply_of_2000_files_loses_nothing,
        ),
        (
            "mass_deletion_brake_pauses_and_both_decisions_work_through_tools",
            mass_deletion_brake_pauses_and_both_decisions_work_through_tools,
        ),
        (
            "watcher_disabled_converges_via_periodic_scans_alone",
            watcher_disabled_converges_via_periodic_scans_alone,
        ),
        (
            "fleet_sync_merges_sync_toml_concurrent_adds_and_removals",
            fleet_sync_merges_sync_toml_concurrent_adds_and_removals,
        ),
        (
            "refusal_nested_and_containing_sync_folders",
            refusal_nested_and_containing_sync_folders,
        ),
        (
            "refusal_policy_denied_path",
            refusal_policy_denied_path,
        ),
        (
            "refusal_network_filesystem_and_unwritable_marker",
            refusal_network_filesystem_and_unwritable_marker,
        ),
        (
            "refusal_same_path_added_twice",
            refusal_same_path_added_twice,
        ),
        (
            "refusal_join_with_existing_files_does_not_trip_deletion_brake",
            refusal_join_with_existing_files_does_not_trip_deletion_brake,
        ),
        (
            "refusal_peer_too_old_to_know_sync_creates_nothing",
            refusal_peer_too_old_to_know_sync_creates_nothing,
        ),
        (
            "second_runner_instance_is_refused_by_lock",
            second_runner_instance_is_refused_by_lock,
        ),
    ];

    let mut failed = Vec::new();
    let mut ran = 0;
    for (name, test) in tests {
        if filter.is_some_and(|f| !name.contains(f)) {
            continue;
        }
        ran += 1;
        let started = Instant::now();
        let outcome = catch_unwind(AssertUnwindSafe(test));
        let verdict = if outcome.is_ok() { "ok" } else { "FAILED" };
        println!(
            "test {name} ... {verdict} ({:.2}s)",
            started.elapsed().as_secs_f64()
        );
        if outcome.is_err() {
            failed.push(*name);
        }
    }
    println!(
        "\ntest result: {} passed, {} failed",
        ran - failed.len(),
        failed.len()
    );
    if failed.is_empty() {
        ExitCode::SUCCESS
    } else {
        println!("failures: {}", failed.join(", "));
        ExitCode::FAILURE
    }
}

/// Connector helper:
/// - `node <legion_home> <user_home> <subcommand...>` launches the real `legion` binary inside
///   that isolated node's `LEGION_HOME` and `HOME`.
/// - `old-legion <name> <subcommand...>` simulates an older `legion` node that answers `hello`
///   over JSON-RPC but does not know any `sync.*` tools (returning `unknown_tool`).
fn helper(args: &[String]) -> ExitCode {
    match args.first().map(String::as_str) {
        Some("node") => {
            let legion_home = &args[1];
            let user_home = &args[2];
            let subcmd = &args[3..];
            let status = Command::new(LEGION_BIN)
                .args(subcmd)
                .env("LEGION_HOME", legion_home)
                .env("HOME", user_home)
                .env("USERPROFILE", user_home)
                .env("LEGION_SYNC_NO_SERVICE", "1")
                .env_remove("XDG_CONFIG_HOME")
                .env_remove("XDG_STATE_HOME")
                .stdin(Stdio::inherit())
                .stdout(Stdio::inherit())
                .stderr(Stdio::inherit())
                .status()
                .expect("spawn legion peer");
            ExitCode::from(status.code().unwrap_or(1) as u8)
        }
        Some("old-legion") => {
            let node_name = args.get(1).map(String::as_str).unwrap_or("oldbox");
            let subcmd = args.get(2).map(String::as_str).unwrap_or("rpc");
            if subcmd == "stream" {
                let mut out = std::io::stdout().lock();
                let _ = writeln!(out, "{{\"protocol\":1}}");
                let _ = writeln!(
                    out,
                    "{{\"error\":{{\"code\":\"unsupported\",\"message\":\"This node does not know sync stream operations.\"}}}}"
                );
                let _ = out.flush();
                return ExitCode::from(1);
            }
            let stdin = std::io::stdin();
            let mut stdout = std::io::stdout().lock();
            for line in BufReader::new(stdin.lock()).lines() {
                let Ok(line) = line else { break };
                let Ok(req) = serde_json::from_str::<Value>(&line) else {
                    continue;
                };
                let id = req.get("id").cloned().unwrap_or(Value::Null);
                let method = req.get("method").and_then(Value::as_str).unwrap_or("");
                let resp = match method {
                    "hello" => json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": {
                            "protocol": 1,
                            "node": node_name,
                            "version": "0.0.1-old",
                            "os": std::env::consts::OS,
                            "arch": std::env::consts::ARCH,
                        }
                    }),
                    "call" => {
                        let tool = req
                            .get("params")
                            .and_then(|p| p.get("tool"))
                            .and_then(Value::as_str)
                            .unwrap_or("");
                        json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "error": {
                                "code": -32000,
                                "message": format!("Unknown tool {tool}"),
                                "data": {
                                    "code": "unknown_tool",
                                    "message": format!("There is no tool called {tool} on {node_name}."),
                                    "hint": "Update legion on this machine."
                                }
                            }
                        })
                    }
                    _ => continue,
                };
                let _ = writeln!(stdout, "{resp}");
                let _ = stdout.flush();
            }
            ExitCode::SUCCESS
        }
        other => {
            eprintln!("unknown sync test helper mode: {other:?}");
            ExitCode::from(2)
        }
    }
}

struct NodeBox {
    name: String,
    home: Home,
    runner: Option<Child>,
}

impl NodeBox {
    fn new(name: &str) -> Self {
        let home = Home::new();
        home.write_config("node.toml", &format!("name = \"{name}\"\n"));
        Self {
            name: name.to_owned(),
            home,
            runner: None,
        }
    }

    fn legion_home(&self) -> PathBuf {
        self.home.state().parent().unwrap().to_path_buf()
    }

    fn sync_dir(&self, sub: &str) -> PathBuf {
        let p = self.home.home().join("sync").join(sub);
        fs::create_dir_all(&p).unwrap();
        p
    }

    fn connector_toml(&self) -> String {
        let exe = std::env::current_exe().unwrap();
        let argv = [
            exe.to_string_lossy().into_owned(),
            HELPER_FLAG.to_owned(),
            "node".to_owned(),
            self.legion_home().to_string_lossy().into_owned(),
            self.home.home().to_string_lossy().into_owned(),
            "rpc".to_owned(),
        ];
        format!(
            "[machines.{}]\ncommand = {}\n",
            self.name,
            serde_json::to_string(&argv).unwrap()
        )
    }

    fn start_runner(&mut self, extra_args: &[&str]) {
        assert!(self.runner.is_none(), "runner already running on {}", self.name);
        let mut args = vec![
            "sync",
            "run",
            "--scan-interval",
            "2",
            "--pull-interval",
            "1",
            "--config-sync-interval",
            "2",
            "--debounce-ms",
            "40",
            "--backoff-ms",
            "200",
        ];
        args.extend_from_slice(extra_args);
        let child = self
            .home
            .command(&args)
            .env("LEGION_SYNC_NO_SERVICE", "1")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("spawn legion sync run");
        self.runner = Some(child);
        // Wait until runner control token & lock appear.
        let token_path = self.home.state().join("sync").join("control.token");
        let deadline = Instant::now() + Duration::from_secs(10);
        while !token_path.exists() {
            assert!(
                Instant::now() < deadline,
                "runner on {} failed to start",
                self.name
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn stop_runner_graceful(&mut self) {
        if let Some(mut child) = self.runner.take() {
            #[cfg(unix)]
            {
                let pid = child.id().to_string();
                let _ = Command::new("kill").args(["-TERM", &pid]).status();
            }
            #[cfg(windows)]
            {
                let _ = child.kill();
            }
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                if let Ok(Some(_)) = child.try_wait() {
                    break;
                }
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    break;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        }
    }

    fn kill_runner_sigkill(&mut self) {
        if let Some(mut child) = self.runner.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }

    fn run_ok(&self, args: &[&str]) -> Value {
        let mut full = args.to_vec();
        full.push("--json");
        let res = self.home.run(&full);
        assert_eq!(
            res.code, 0,
            "node {} command {args:?} failed (code {}):\nstdout: {}\nstderr: {}",
            self.name, res.code, res.stdout, res.stderr
        );
        res.json()
    }

    fn run_raw(&self, args: &[&str]) -> Run {
        let mut full = args.to_vec();
        full.push("--json");
        self.home.run(&full)
    }
}

impl Drop for NodeBox {
    fn drop(&mut self) {
        self.stop_runner_graceful();
    }
}

fn wire_mesh(nodes: &[&NodeBox]) {
    for target in nodes {
        let mut fleet = String::new();
        for peer in nodes {
            if peer.name != target.name {
                fleet.push_str(&peer.connector_toml());
            }
        }
        target.home.write_config("fleet.toml", &fleet);
    }
}

fn wait_until(timeout: Duration, desc: &str, mut cond: impl FnMut() -> bool) {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if cond() {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("timed out after {timeout:?} waiting for: {desc}");
}

fn read_live_tree(root: &Path) -> BTreeMap<String, Vec<u8>> {
    let mut out = BTreeMap::new();
    collect_files(root, root, &mut out);
    out
}

fn collect_files(base: &Path, dir: &Path, out: &mut BTreeMap<String, Vec<u8>>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        if name.to_string_lossy() == ".legion-sync" {
            continue;
        }
        let path = entry.path();
        let meta = fs::symlink_metadata(&path).unwrap();
        if meta.is_dir() {
            collect_files(base, &path, out);
        } else if meta.is_file() {
            let rel = path
                .strip_prefix(base)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/");
            out.insert(rel, fs::read(&path).unwrap());
        }
    }
}

fn three_node_lifecycle_create_edit_offline_delete_restore_conflicts_remove() {
    let mut a = NodeBox::new("alpha");
    let mut b = NodeBox::new("beta");
    let mut c = NodeBox::new("gamma");
    wire_mesh(&[&a, &b, &c]);

    let dir_a = a.sync_dir("docs");
    let dir_b = b.sync_dir("docs");
    let dir_c = c.sync_dir("docs");

    a.start_runner(&[]);
    b.start_runner(&[]);
    c.start_runner(&[]);

    // 1. Create on A, add B and C.
    let created = a.run_ok(&[
        "sync.create",
        "name=Docs",
        &format!("path={}", dir_a.display()),
    ]);
    assert_eq!(created["id"], "docs");
    a.run_ok(&[
        "sync.add",
        "id=docs",
        "machine=beta",
        &format!("path={}", dir_b.display()),
    ]);
    a.run_ok(&[
        "sync.add",
        "id=docs",
        "machine=gamma",
        &format!("path={}", dir_c.display()),
    ]);

    // 2. File created on A appears on B and C.
    fs::write(dir_a.join("notes.txt"), b"v1 from alpha").unwrap();
    wait_until(Duration::from_secs(15), "notes.txt on beta & gamma", || {
        fs::read(dir_b.join("notes.txt")).ok().as_deref() == Some(b"v1 from alpha".as_slice())
            && fs::read(dir_c.join("notes.txt")).ok().as_deref()
                == Some(b"v1 from alpha".as_slice())
    });

    // Check sync.status reports clean up_to_date and --on routing works.
    let status_b = a.run_ok(&["sync.status", "id=docs", "--on", "beta"]);
    assert_eq!(status_b["node"], "beta");
    assert_eq!(status_b["runner_running"], true);
    assert_eq!(status_b["folders"][0]["state"]["state"], "up_to_date");

    // 3. Edit on B while C's runner is stopped -> reaches A immediately and C when C starts.
    c.stop_runner_graceful();
    fs::write(dir_b.join("notes.txt"), b"v2 from beta while gamma off").unwrap();
    wait_until(Duration::from_secs(15), "v2 on alpha", || {
        fs::read(dir_a.join("notes.txt")).ok().as_deref()
            == Some(b"v2 from beta while gamma off".as_slice())
    });
    assert_eq!(fs::read(dir_c.join("notes.txt")).unwrap(), b"v1 from alpha");

    c.start_runner(&[]);
    wait_until(Duration::from_secs(15), "v2 arrives on gamma after start", || {
        fs::read(dir_c.join("notes.txt")).ok().as_deref()
            == Some(b"v2 from beta while gamma off".as_slice())
    });

    // 4. Delete on A lands in B & C's bins; sync.restore brings it back everywhere.
    fs::remove_file(dir_a.join("notes.txt")).unwrap();
    wait_until(Duration::from_secs(15), "notes.txt deleted on beta & gamma", || {
        !dir_b.join("notes.txt").exists() && !dir_c.join("notes.txt").exists()
    });
    let bin_b = b.run_ok(&["sync.bin", "id=docs"]);
    let entries = bin_b["entries"].as_array().unwrap();
    let target_entry = entries
        .iter()
        .find(|e| e["original_path"] == "notes.txt" && e["reason"] == "deleted_by_peer")
        .expect("deleted notes.txt in beta bin");
    let entry_id = target_entry["id"].as_str().unwrap();

    b.run_ok(&[
        "sync.restore",
        "id=docs",
        &format!("entry_id={entry_id}"),
    ]);
    wait_until(Duration::from_secs(15), "restored notes.txt on all three", || {
        let want = b"v2 from beta while gamma off".as_slice();
        fs::read(dir_a.join("notes.txt")).ok().as_deref() == Some(want)
            && fs::read(dir_b.join("notes.txt")).ok().as_deref() == Some(want)
            && fs::read(dir_c.join("notes.txt")).ok().as_deref() == Some(want)
    });

    // 5. Concurrent edits on A and B while disconnected produce a two-version conflict file,
    //    listed by sync.conflicts and settled by sync.resolve across all three!
    b.stop_runner_graceful();
    c.stop_runner_graceful();
    fs::write(dir_a.join("notes.txt"), b"alpha concurrent edit").unwrap();
    a.run_ok(&["sync.now", "id=docs"]);
    a.stop_runner_graceful();

    b.start_runner(&[]);
    fs::write(dir_b.join("notes.txt"), b"beta concurrent edit").unwrap();
    b.run_ok(&["sync.now", "id=docs"]);

    a.start_runner(&[]);
    c.start_runner(&[]);
    wait_until(Duration::from_secs(20), "conflict detected on alpha", || {
        let res = a.run_ok(&["sync.conflicts", "id=docs"]);
        !res["conflicts"].as_array().unwrap().is_empty()
    });
    let conflicts = a.run_ok(&["sync.conflicts", "id=docs"]);
    let conflict_path = conflicts["conflicts"][0]["conflict_path"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(
        conflict_path.contains("changed on "),
        "unexpected conflict path: {conflict_path}"
    );

    a.run_ok(&[
        "sync.resolve",
        "id=docs",
        &format!("conflict_path={conflict_path}"),
        "choice=keep_original",
    ]);
    wait_until(
        Duration::from_secs(20),
        "conflict settled identically on all 3 nodes",
        || {
            let ta = read_live_tree(&dir_a);
            let tb = read_live_tree(&dir_b);
            let tc = read_live_tree(&dir_c);
            ta == tb
                && tb == tc
                && ta.len() == 1
                && ta.contains_key("notes.txt")
                && a.run_ok(&["sync.conflicts", "id=docs"])["conflicts"]
                    .as_array()
                    .unwrap()
                    .is_empty()
                && b.run_ok(&["sync.conflicts", "id=docs"])["conflicts"]
                    .as_array()
                    .unwrap()
                    .is_empty()
                && c.run_ok(&["sync.conflicts", "id=docs"])["conflicts"]
                    .as_array()
                    .unwrap()
                    .is_empty()
        },
    );

    // 6. Removing gamma stops syncing there and leaves its files untouched.
    let gamma_before = read_live_tree(&dir_c);
    a.run_ok(&["sync.remove", "id=docs", "machine=gamma"]);
    // Wait a moment for gamma to reload config, then modify on alpha.
    std::thread::sleep(Duration::from_millis(300));
    fs::write(dir_a.join("after-remove.txt"), b"only alpha and beta").unwrap();
    wait_until(Duration::from_secs(15), "after-remove.txt on beta", || {
        fs::read(dir_b.join("after-remove.txt")).ok().as_deref()
            == Some(b"only alpha and beta".as_slice())
    });
    assert_eq!(read_live_tree(&dir_c), gamma_before);
    assert!(!dir_c.join("after-remove.txt").exists());

    // 7. Pause, resume, and delete (leaves files untouched).
    a.run_ok(&["sync.pause", "id=docs"]);
    let st_paused = a.run_ok(&["sync.status", "id=docs"]);
    assert_eq!(st_paused["folders"][0]["state"]["state"], "paused");
    a.run_ok(&["sync.resume", "id=docs"]);
    let st_resumed = a.run_ok(&["sync.status", "id=docs"]);
    assert_ne!(st_resumed["folders"][0]["state"]["state"], "paused");

    let alpha_before_del = read_live_tree(&dir_a);
    a.run_ok(&["sync.delete", "id=docs"]);
    assert_eq!(read_live_tree(&dir_a), alpha_before_del);
}

fn sigkill_mid_apply_of_2000_files_loses_nothing() {
    let mut a = NodeBox::new("alpha");
    let mut b = NodeBox::new("beta");
    wire_mesh(&[&a, &b]);

    let dir_a = a.sync_dir("bulk");
    let dir_b = b.sync_dir("bulk");

    // Pre-populate 2,000 files on alpha across 20 subdirectories.
    for d in 0..20 {
        let sub = dir_a.join(format!("d{d:02}"));
        fs::create_dir_all(&sub).unwrap();
        for f in 0..100 {
            let idx = d * 100 + f;
            let body = format!("payload-{idx:04}-{}", "x".repeat(512));
            fs::write(sub.join(format!("f{f:03}.txt")), body.as_bytes()).unwrap();
        }
    }

    a.start_runner(&[]);
    a.run_ok(&[
        "sync.create",
        "name=Bulk",
        &format!("path={}", dir_a.display()),
    ]);
    a.run_ok(&[
        "sync.add",
        "id=bulk",
        "machine=beta",
        &format!("path={}", dir_b.display()),
    ]);
    a.run_ok(&["sync.now", "id=bulk"]);

    // Start beta's runner and SIGKILL it as soon as files begin landing mid-apply.
    b.start_runner(&[]);
    let kill_deadline = Instant::now() + Duration::from_secs(30);
    let mut killed_mid = false;
    while Instant::now() < kill_deadline {
        let count = read_live_tree(&dir_b).len();
        if count >= 20 {
            b.kill_runner_sigkill();
            killed_mid = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(15));
    }
    assert!(killed_mid, "beta never started receiving files before SIGKILL");

    // Restart beta's runner -> must recover cleanly and converge all 2,000 files byte-for-byte.
    b.start_runner(&[]);
    wait_until(Duration::from_secs(60), "all 2,000 files on beta after SIGKILL", || {
        read_live_tree(&dir_b).len() == 2_000
    });

    assert_eq!(read_live_tree(&dir_a), read_live_tree(&dir_b));
}

fn mass_deletion_brake_pauses_and_both_decisions_work_through_tools() {
    let mut a = NodeBox::new("alpha");
    let mut b = NodeBox::new("beta");
    wire_mesh(&[&a, &b]);

    let dir_a = a.sync_dir("brake");
    let dir_b = b.sync_dir("brake");

    for i in 0..70 {
        fs::write(dir_a.join(format!("item-{i:03}.txt")), format!("data-{i}")).unwrap();
    }

    a.start_runner(&[]);
    b.start_runner(&[]);
    a.run_ok(&[
        "sync.create",
        "name=Brake",
        &format!("path={}", dir_a.display()),
    ]);
    a.run_ok(&[
        "sync.add",
        "id=brake",
        "machine=beta",
        &format!("path={}", dir_b.display()),
    ]);
    wait_until(Duration::from_secs(20), "70 files on beta", || {
        read_live_tree(&dir_b).len() == 70
    });

    // Delete 60 of 70 files on alpha (> 50 files and > 25%) -> brake trips.
    for i in 0..60 {
        fs::remove_file(dir_a.join(format!("item-{i:03}.txt"))).unwrap();
    }
    a.run_ok(&["sync.now", "id=brake"]);

    wait_until(Duration::from_secs(10), "deletion brake pending on alpha", || {
        let st = a.run_ok(&["sync.status", "id=brake"]);
        st["folders"][0]["pending_brake"].is_object()
    });
    // Beta still has all 70 files untouched.
    assert_eq!(read_live_tree(&dir_b).len(), 70);

    // Resuming without brake_decision MUST be refused cleanly.
    let refused = a.run_raw(&["sync.resume", "id=brake"]);
    assert_ne!(refused.code, 0);
    assert_eq!(refused.json()["code"], "conflict");

    // Decision 1: restore files and `rescan` -> unpauses and preserves all 70 files everywhere.
    for i in 0..60 {
        fs::write(dir_a.join(format!("item-{i:03}.txt")), format!("data-{i}")).unwrap();
    }
    a.run_ok(&["sync.resume", "id=brake", "brake_decision=rescan"]);
    let st_after_rescan = a.run_ok(&["sync.status", "id=brake"]);
    assert!(st_after_rescan["folders"][0]["pending_brake"].is_null());
    assert_eq!(read_live_tree(&dir_b).len(), 70);

    // Decision 2: delete 60 files again, trip brake, then `confirm_deleted` -> deletions propagate
    // and land in beta's bin!
    for i in 0..60 {
        fs::remove_file(dir_a.join(format!("item-{i:03}.txt"))).unwrap();
    }
    a.run_ok(&["sync.now", "id=brake"]);
    wait_until(Duration::from_secs(10), "deletion brake tripped second time", || {
        let st = a.run_ok(&["sync.status", "id=brake"]);
        st["folders"][0]["pending_brake"].is_object()
    });

    a.run_ok(&["sync.resume", "id=brake", "brake_decision=confirm_deleted"]);
    wait_until(Duration::from_secs(20), "60 deletions applied on beta", || {
        read_live_tree(&dir_b).len() == 10
    });
    let bin_b = b.run_ok(&["sync.bin", "id=brake"]);
    assert!(
        bin_b["total"].as_u64().unwrap() >= 60,
        "expected >= 60 entries in beta bin, got {bin_b}"
    );
}

fn watcher_disabled_converges_via_periodic_scans_alone() {
    let mut a = NodeBox::new("alpha");
    let mut b = NodeBox::new("beta");
    wire_mesh(&[&a, &b]);

    let dir_a = a.sync_dir("nowatch");
    let dir_b = b.sync_dir("nowatch");

    a.start_runner(&["--no-watch"]);
    b.start_runner(&["--no-watch"]);

    a.run_ok(&[
        "sync.create",
        "name=NoWatch",
        &format!("path={}", dir_a.display()),
    ]);
    a.run_ok(&[
        "sync.add",
        "id=nowatch",
        "machine=beta",
        &format!("path={}", dir_b.display()),
    ]);

    fs::write(dir_a.join("periodic.txt"), b"arrived without watcher").unwrap();
    wait_until(Duration::from_secs(15), "periodic.txt on beta without watcher", || {
        fs::read(dir_b.join("periodic.txt")).ok().as_deref()
            == Some(b"arrived without watcher".as_slice())
    });

    fs::write(dir_b.join("periodic.txt"), b"edited without watcher").unwrap();
    wait_until(Duration::from_secs(15), "edited periodic.txt on alpha without watcher", || {
        fs::read(dir_a.join("periodic.txt")).ok().as_deref()
            == Some(b"edited without watcher".as_slice())
    });
}

fn fleet_sync_merges_sync_toml_concurrent_adds_and_removals() {
    let a = NodeBox::new("alpha");
    let b = NodeBox::new("beta");
    let c = NodeBox::new("gamma");
    wire_mesh(&[&a, &b, &c]);

    let dir_a = a.sync_dir("shared");
    let dir_b = b.sync_dir("shared");
    let dir_c = c.sync_dir("shared");

    // Create on A, push to B & C so all start with `{alpha}`.
    a.run_ok(&[
        "sync.create",
        "name=Shared",
        &format!("path={}", dir_a.display()),
    ]);
    a.run_ok(&[
        "fleet.sync",
        "direction=push",
        "documents=[\"sync.toml\"]",
    ]);

    // Disconnect A <-> C by only adding beta locally on B and gamma locally on C:
    b.run_ok(&[
        "sync.add",
        "id=shared",
        "machine=beta",
        &format!("path={}", dir_b.display()),
    ]);
    c.run_ok(&[
        "sync.add",
        "id=shared",
        "machine=gamma",
        &format!("path={}", dir_c.display()),
    ]);

    // Merge via fleet.sync from alpha: pull from beta, pull from gamma, push to all.
    a.run_ok(&[
        "fleet.sync",
        "direction=pull",
        "from=beta",
        "documents=[\"sync.toml\"]",
    ]);
    a.run_ok(&[
        "fleet.sync",
        "direction=pull",
        "from=gamma",
        "documents=[\"sync.toml\"]",
    ]);
    a.run_ok(&[
        "fleet.sync",
        "direction=push",
        "documents=[\"sync.toml\"]",
    ]);

    for node in [&a, &b, &c] {
        let listed = node.run_ok(&["sync.list"]);
        let members: Vec<&str> = listed["folders"][0]["members"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["machine"].as_str().unwrap())
            .collect();
        assert_eq!(members, vec!["alpha", "beta", "gamma"], "on {}", node.name);
    }
}

fn refusal_nested_and_containing_sync_folders() {
    let a = NodeBox::new("alpha");
    let parent = a.sync_dir("outer");
    let inner = parent.join("inner");
    let deep = inner.join("deep");
    fs::create_dir_all(&deep).unwrap();

    a.run_ok(&[
        "sync.create",
        "name=Inner",
        &format!("path={}", inner.display()),
    ]);

    // Inside existing sync folder -> refused.
    let nested = a.run_raw(&[
        "sync.create",
        "name=Deep",
        &format!("path={}", deep.display()),
    ]);
    assert_ne!(nested.code, 0);
    assert_eq!(nested.json()["code"], "conflict");

    // Containing existing sync folder -> refused.
    let containing = a.run_raw(&[
        "sync.create",
        "name=Outer",
        &format!("path={}", parent.display()),
    ]);
    assert_ne!(containing.code, 0);
    assert_eq!(containing.json()["code"], "conflict");

    // Only the original folder exists.
    let listed = a.run_ok(&["sync.list"]);
    assert_eq!(listed["folders"].as_array().unwrap().len(), 1);
}

fn refusal_policy_denied_path() {
    let a = NodeBox::new("alpha");
    let b = NodeBox::new("beta");
    wire_mesh(&[&a, &b]);

    let allowed_a = a.home.home().join("allowed").join("docs");
    let forbidden_a = a.home.home().join("forbidden").join("docs");
    let forbidden_b = b.home.home().join("forbidden").join("docs");
    fs::create_dir_all(&allowed_a).unwrap();
    fs::create_dir_all(&forbidden_a).unwrap();
    fs::create_dir_all(&forbidden_b).unwrap();

    let policy = "\
[principals.\"agent:scoped\"]
role = \"scoped\"

[roles.scoped]
allow = [\"effect:read\", \"effect:write\"]
paths = [\"~/allowed/**\"]
";
    a.home.write_config("policy.toml", policy);
    b.home.write_config("policy.toml", policy);

    // Local refusal on alpha: outside ~/allowed/**.
    let denied_local = a.run_raw(&[
        "sync.create",
        "name=Secret",
        &format!("path={}", forbidden_a.display()),
        "--as",
        "agent:scoped",
    ]);
    assert_ne!(denied_local.code, 0);
    assert_eq!(denied_local.json()["code"], "denied");

    // Allowed create on alpha succeeds.
    a.run_ok(&[
        "sync.create",
        "name=Scoped",
        &format!("path={}", allowed_a.display()),
        "--as",
        "agent:scoped",
    ]);

    // Remote refusal on beta checked from beta's own policy.toml: outside ~/allowed/** on beta!
    let denied_remote = a.run_raw(&[
        "sync.add",
        "id=scoped",
        "machine=beta",
        &format!("path={}", forbidden_b.display()),
        "--as",
        "agent:scoped",
    ]);
    assert_ne!(denied_remote.code, 0);
    assert_eq!(denied_remote.json()["code"], "denied");

    // Beta was NOT added to sync.toml.
    let listed = a.run_ok(&["sync.list"]);
    assert_eq!(listed["folders"][0]["members"].as_array().unwrap().len(), 1);
}

fn refusal_network_filesystem_and_unwritable_marker() {
    let a = NodeBox::new("alpha");

    // 1. Network filesystem refusal (via testing prefix hook in `legion-tools/testing`).
    let net_dir = a.sync_dir("nfs-mount");
    let output = a
        .home
        .command(&[
            "sync.create",
            "name=NfsMount",
            &format!("path={}", net_dir.display()),
            "--json",
        ])
        .env("LEGION_TEST_NETWORK_FS_PREFIX", &net_dir)
        .output()
        .unwrap();
    assert!(!output.status.success());
    let body: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(body["code"], "unsupported", "got: {body}");

    // 2. Unwritable directory unable to hold `.legion-sync/marker.json` (Unix chmod 0555).
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let ro_dir = a.sync_dir("readonly");
        fs::set_permissions(&ro_dir, fs::Permissions::from_mode(0o555)).unwrap();
        let res = a.run_raw(&[
            "sync.create",
            "name=ReadOnly",
            &format!("path={}", ro_dir.display()),
        ]);
        // Restore permissions before any assertion so TempDir cleanup always succeeds.
        fs::set_permissions(&ro_dir, fs::Permissions::from_mode(0o755)).unwrap();
        assert_ne!(res.code, 0);
        let err = res.json();
        assert!(
            err["code"] == "denied" || err["code"] == "failed",
            "unexpected error for unwritable marker: {err}"
        );
    }

    // Nothing was written to sync.toml.
    let listed = a.run_ok(&["sync.list"]);
    assert!(listed["folders"].as_array().unwrap().is_empty());
}

fn refusal_same_path_added_twice() {
    let a = NodeBox::new("alpha");
    let dir_a = a.sync_dir("dup");

    a.run_ok(&[
        "sync.create",
        "name=First",
        &format!("path={}", dir_a.display()),
    ]);

    // Same path in a second folder -> refused.
    let second = a.run_raw(&[
        "sync.create",
        "name=Second",
        &format!("path={}", dir_a.display()),
    ]);
    assert_ne!(second.code, 0);
    assert_eq!(second.json()["code"], "exists");

    // Adding alpha to the same folder twice -> refused.
    let dup_add = a.run_raw(&[
        "sync.add",
        "id=first",
        "machine=alpha",
        &format!("path={}", dir_a.display()),
    ]);
    assert_ne!(dup_add.code, 0);
    assert_eq!(dup_add.json()["code"], "exists");
}

fn refusal_join_with_existing_files_does_not_trip_deletion_brake() {
    let mut a = NodeBox::new("alpha");
    let mut b = NodeBox::new("beta");
    wire_mesh(&[&a, &b]);

    let dir_a = a.sync_dir("join-safe");
    let dir_b = b.sync_dir("join-safe");

    // Create 70 files on alpha, sync index, then delete 60 of them (below or confirmed?
    // Delete them in batches of 40 so alpha's own deletion brake never trips, recording 60
    // deleted hashes in alpha's tombstones/deleted history!).
    for i in 0..130 {
        fs::write(dir_a.join(format!("doc-{i:03}.txt")), format!("v1-{i}")).unwrap();
    }
    a.start_runner(&[]);
    a.run_ok(&[
        "sync.create",
        "name=JoinSafe",
        &format!("path={}", dir_a.display()),
    ]);
    a.run_ok(&["sync.now", "id=join-safe"]);

    // Delete 30 files, scan; then delete 30 more, scan (60 deleted total, 70 live remain).
    for i in 0..30 {
        fs::remove_file(dir_a.join(format!("doc-{i:03}.txt"))).unwrap();
    }
    a.run_ok(&["sync.now", "id=join-safe"]);
    for i in 30..60 {
        fs::remove_file(dir_a.join(format!("doc-{i:03}.txt"))).unwrap();
    }
    a.run_ok(&["sync.now", "id=join-safe"]);
    assert_eq!(read_live_tree(&dir_a).len(), 70);

    // Pre-populate beta with ALL 60 previously-deleted files (old stale copies that would look
    // like mass deletions if Rule D didn't handle join safely!).
    for i in 0..60 {
        fs::write(dir_b.join(format!("doc-{i:03}.txt")), format!("v1-{i}")).unwrap();
    }

    b.start_runner(&[]);
    a.run_ok(&[
        "sync.add",
        "id=join-safe",
        "machine=beta",
        &format!("path={}", dir_b.display()),
    ]);

    wait_until(
        Duration::from_secs(20),
        "beta converges to alpha's 70 live files without tripping deletion brake",
        || read_live_tree(&dir_b) == read_live_tree(&dir_a),
    );

    let st_a = a.run_ok(&["sync.status", "id=join-safe"]);
    let st_b = b.run_ok(&["sync.status", "id=join-safe"]);
    assert!(
        st_a["folders"][0]["pending_brake"].is_null(),
        "alpha wrongly tripped deletion brake: {st_a}"
    );
    assert!(
        st_b["folders"][0]["pending_brake"].is_null(),
        "beta wrongly tripped deletion brake: {st_b}"
    );
    // The 60 stale previously-deleted copies on beta went safely into beta's bin.
    let bin_b = b.run_ok(&["sync.bin", "id=join-safe"]);
    assert!(bin_b["total"].as_u64().unwrap() >= 60);
}

fn refusal_peer_too_old_to_know_sync_creates_nothing() {
    let a = NodeBox::new("alpha");
    let exe = std::env::current_exe().unwrap();
    let old_cmd = [
        exe.to_string_lossy().into_owned(),
        HELPER_FLAG.to_owned(),
        "old-legion".to_owned(),
        "oldbox".to_owned(),
        "rpc".to_owned(),
    ];
    a.home.write_config(
        "fleet.toml",
        &format!(
            "[machines.oldbox]\ncommand = {}\n",
            serde_json::to_string(&old_cmd).unwrap()
        ),
    );

    // 1. sync.create targeting oldbox -> refused clearly, nothing created!
    let create_res = a.run_raw(&[
        "sync.create",
        "name=OldTest",
        "machine=oldbox",
        "path=/tmp/old-test-unused",
    ]);
    assert_ne!(create_res.code, 0);
    let err = create_res.json();
    assert_eq!(err["code"], "unsupported", "got: {err}");
    assert!(
        err["message"]
            .as_str()
            .unwrap()
            .to_lowercase()
            .contains("older"),
        "expected clear older-legion message: {err}"
    );
    assert!(
        a.run_ok(&["sync.list"])["folders"]
            .as_array()
            .unwrap()
            .is_empty(),
        "nothing should be half-created on alpha"
    );

    // 2. sync.add targeting oldbox -> refused clearly, oldbox NOT added to sync.toml!
    let dir_a = a.sync_dir("existing");
    a.run_ok(&[
        "sync.create",
        "name=Existing",
        &format!("path={}", dir_a.display()),
    ]);
    let add_res = a.run_raw(&[
        "sync.add",
        "id=existing",
        "machine=oldbox",
        "path=/tmp/old-test-unused",
    ]);
    assert_ne!(add_res.code, 0);
    let err2 = add_res.json();
    assert_eq!(err2["code"], "unsupported", "got: {err2}");
    let members = a.run_ok(&["sync.list"])["folders"][0]["members"]
        .as_array()
        .unwrap()
        .clone();
    assert_eq!(members.len(), 1, "oldbox must not be half-added");
}

fn second_runner_instance_is_refused_by_lock() {
    let mut a = NodeBox::new("alpha");
    a.start_runner(&[]);

    let second = a.home.run(&["sync", "run"]);
    assert_ne!(second.code, 0, "second runner should be refused by lock");
    assert!(
        second.stderr.to_lowercase().contains("already running"),
        "unexpected stderr: {}",
        second.stderr
    );
}
