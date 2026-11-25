//! Three-role relayed `legion stream` test bed:
//! - Role 1: device (only talks to `tower`, has no direct entry for `studio`)
//! - Role 2: `tower` (paired relay node with `studio` in its `fleet.toml`)
//! - Role 3: `studio` (target node holding the files and path grants)
//!
//! Uses the built `legion` binary over the `command` connector (and self-executes as a helper for
//! slow streams, heartbeats, old relays without `relay`/`stream.relay`, and mid-transfer kills).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::PathBuf;
use std::process::{Child, Command, ExitCode, Stdio};
use std::time::{Duration, Instant};

use common::Home;
use serde_json::{Value, json};

const LEGION_BIN: &str = env!("CARGO_BIN_EXE_legion");
const HELPER_FLAG: &str = "--relay-helper";

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
            "relayed_get_ranges_keep_open_and_dual_audit",
            relayed_get_ranges_keep_open_and_dual_audit,
        ),
        (
            "relayed_put_cancel_leaves_same_partial_as_direct_and_resumes",
            relayed_put_cancel_leaves_same_partial_as_direct_and_resumes,
        ),
        (
            "relayed_stat_manifest_pack_unpack_delete_and_local_self",
            relayed_stat_manifest_pack_unpack_delete_and_local_self,
        ),
        (
            "refusals_relay_denies_target_denies_path_outside_grant_and_no_escalation",
            refusals_relay_denies_target_denies_path_outside_grant_and_no_escalation,
        ),
        (
            "hops_limit_chained_via_and_old_relay_refusal",
            hops_limit_chained_via_and_old_relay_refusal,
        ),
        (
            "heartbeats_pass_through_and_both_ends_dying_close_in_bounded_time",
            heartbeats_pass_through_and_both_ends_dying_close_in_bounded_time,
        ),
        (
            "stuck_target_50mb_get_sigstop_client_gone_and_timeout",
            stuck_target_50mb_get_sigstop_client_gone_and_timeout,
        ),
        (
            "stuck_target_checksum_stat_sigstop_timeout_client_gone_and_deadline_resets",
            stuck_target_checksum_stat_sigstop_timeout_client_gone_and_deadline_resets,
        ),
    ];

    let mut passed = 0usize;
    let mut failed = 0usize;
    for &(name, body) in tests {
        if let Some(f) = filter
            && !name.contains(f)
        {
            continue;
        }
        let started = Instant::now();
        match catch_unwind(AssertUnwindSafe(body)) {
            Ok(()) => {
                println!(
                    "test {name} ... ok ({:.2}s)",
                    started.elapsed().as_secs_f64()
                );
                passed += 1;
            }
            Err(_) => {
                println!("test {name} ... FAILED");
                failed += 1;
            }
        }
    }
    println!("\ntest result: {passed} passed, {failed} failed");
    if failed == 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    }
}

fn helper(args: &[String]) -> ExitCode {
    match args.first().map(String::as_str) {
        Some("node") => {
            // Args: node <legion_home> <user_home> <subcommand>
            let legion_home = &args[1];
            let user_home = &args[2];
            let sub = &args[3];
            let mut cmd = Command::new(LEGION_BIN);
            cmd.arg(sub)
                .env("LEGION_HOME", legion_home)
                .env("HOME", user_home)
                .stdin(Stdio::inherit())
                .stdout(Stdio::inherit())
                .stderr(Stdio::inherit());
            #[cfg(unix)]
            {
                use std::os::unix::process::CommandExt;
                let err = cmd.exec();
                panic!("exec legion node failed: {err}");
            }
            #[cfg(not(unix))]
            {
                let status = cmd.status().expect("spawn legion node");
                ExitCode::from(status.code().unwrap_or(1).clamp(0, 255) as u8)
            }
        }
        Some("node-pid") => {
            // Args: node-pid <pid_file> <legion_home> <user_home> <subcommand>
            let pid_file = PathBuf::from(&args[1]);
            let legion_home = &args[2];
            let user_home = &args[3];
            let sub = &args[4];
            fs::write(&pid_file, std::process::id().to_string()).unwrap();
            let mut cmd = Command::new(LEGION_BIN);
            cmd.arg(sub)
                .env("LEGION_HOME", legion_home)
                .env("HOME", user_home)
                .stdin(Stdio::inherit())
                .stdout(Stdio::inherit())
                .stderr(Stdio::inherit());
            #[cfg(unix)]
            {
                use std::os::unix::process::CommandExt;
                let err = cmd.exec();
                panic!("exec legion node-pid failed: {err}");
            }
            #[cfg(not(unix))]
            {
                let status = cmd.status().expect("spawn legion node-pid");
                ExitCode::from(status.code().unwrap_or(1).clamp(0, 255) as u8)
            }
        }
        Some("stalling-stat") => {
            // Args: stalling-stat <pid_file> <seen_file> <subcommand>
            let pid_file = PathBuf::from(&args[1]);
            let seen_file = PathBuf::from(&args[2]);
            fs::write(&pid_file, std::process::id().to_string()).unwrap();
            let mut out = std::io::stdout().lock();
            writeln!(
                out,
                "{}",
                json!({ "protocol": 1, "capabilities": ["stream.relay"] })
            )
            .unwrap();
            out.flush().unwrap();
            let mut line = String::new();
            let mut stdin = BufReader::new(std::io::stdin());
            if stdin.read_line(&mut line).unwrap_or(0) > 0 {
                fs::write(&seen_file, &line).unwrap();
                loop {
                    std::thread::sleep(Duration::from_secs(60));
                }
            }
            ExitCode::SUCCESS
        }
        Some("paced-heartbeat-stat") => {
            let mut out = std::io::stdout().lock();
            writeln!(
                out,
                "{}",
                json!({ "protocol": 1, "capabilities": ["stream.relay"] })
            )
            .unwrap();
            out.flush().unwrap();
            let mut line = String::new();
            let mut stdin = BufReader::new(std::io::stdin());
            let _ = stdin.read_line(&mut line);
            for _ in 0..3 {
                std::thread::sleep(Duration::from_millis(200));
                writeln!(out, "{}", json!({ "busy": true })).unwrap();
                out.flush().unwrap();
            }
            std::thread::sleep(Duration::from_millis(200));
            writeln!(out, "{}", json!({ "kind": "file", "size": 456 })).unwrap();
            out.flush().unwrap();
            ExitCode::SUCCESS
        }
        Some("paced-get") => {
            let mut out = std::io::stdout().lock();
            writeln!(
                out,
                "{}",
                json!({ "protocol": 1, "capabilities": ["stream.relay"] })
            )
            .unwrap();
            out.flush().unwrap();
            let mut line = String::new();
            let mut stdin = BufReader::new(std::io::stdin());
            let _ = stdin.read_line(&mut line);
            let chunk = vec![0x42u8; 16 * 1024];
            let total = (chunk.len() * 3) as u64;
            writeln!(
                out,
                "{}",
                json!({
                    "size": total,
                    "offset": 0,
                    "length": total,
                    "mtime": { "secs": 1_700_000_000i64, "nanos": 0u32 }
                })
            )
            .unwrap();
            out.flush().unwrap();
            for _ in 0..3 {
                std::thread::sleep(Duration::from_millis(200));
                if out.write_all(&chunk).is_err() || out.flush().is_err() {
                    return ExitCode::from(1);
                }
            }
            ExitCode::SUCCESS
        }
        Some("old-relay") => {
            // Simulates an older `legion stream` that emits `{"protocol": 1}` without
            // `capabilities` and answers `unsupported` for `op: "relay"`.
            let mut out = std::io::stdout().lock();
            writeln!(out, "{}", json!({ "protocol": 1 })).unwrap();
            out.flush().unwrap();
            let mut line = String::new();
            let mut stdin = BufReader::new(std::io::stdin());
            if stdin.read_line(&mut line).unwrap_or(0) > 0 {
                let err = json!({
                    "error": {
                        "code": "unsupported",
                        "message": "This node does not know the stream operation relay; it knows manifest, stat, get, put, pack, unpack, delete.",
                        "machine": "oldrelay",
                        "details": { "protocol": [1], "ops": ["manifest", "stat", "get", "put", "pack", "unpack", "delete"] }
                    }
                });
                writeln!(out, "{err}").unwrap();
                out.flush().unwrap();
            }
            ExitCode::from(1)
        }
        Some("heartbeat-stat") => {
            // Emits greeting, reads header, emits two `{"busy": true}` lines, then replies.
            let mut out = std::io::stdout().lock();
            writeln!(
                out,
                "{}",
                json!({ "protocol": 1, "capabilities": ["stream.relay"] })
            )
            .unwrap();
            out.flush().unwrap();
            let mut line = String::new();
            let mut stdin = BufReader::new(std::io::stdin());
            let _ = stdin.read_line(&mut line);
            writeln!(out, "{}", json!({ "busy": true })).unwrap();
            out.flush().unwrap();
            std::thread::sleep(Duration::from_millis(40));
            writeln!(out, "{}", json!({ "busy": true })).unwrap();
            out.flush().unwrap();
            writeln!(out, "{}", json!({ "kind": "file", "size": 123 })).unwrap();
            out.flush().unwrap();
            ExitCode::SUCCESS
        }
        Some("slow-get") => {
            // Writes a marker file while alive and removes it on exit, emitting a 32 MiB `get`
            // reply in 64 KiB chunks so the client can die mid-get while the test observes both
            // relay and target teardown time.
            struct RemoveOnDrop(PathBuf);
            impl Drop for RemoveOnDrop {
                fn drop(&mut self) {
                    let _ = fs::remove_file(&self.0);
                }
            }
            let pid_file = PathBuf::from(&args[1]);
            fs::write(&pid_file, std::process::id().to_string()).unwrap();
            let _guard = RemoveOnDrop(pid_file);
            let mut out = std::io::stdout().lock();
            writeln!(
                out,
                "{}",
                json!({ "protocol": 1, "capabilities": ["stream.relay"] })
            )
            .unwrap();
            out.flush().unwrap();
            let mut line = String::new();
            let mut stdin = BufReader::new(std::io::stdin());
            if stdin.read_line(&mut line).unwrap_or(0) == 0 {
                return ExitCode::SUCCESS;
            }
            let total: u64 = 32 * 1024 * 1024;
            writeln!(
                out,
                "{}",
                json!({
                    "size": total,
                    "offset": 0,
                    "length": total,
                    "mtime": { "secs": 1_700_000_000i64, "nanos": 0u32 }
                })
            )
            .unwrap();
            out.flush().unwrap();
            let chunk = vec![0x5Au8; 64 * 1024];
            let mut sent = 0u64;
            while sent < total {
                if out.write_all(&chunk).is_err() || out.flush().is_err() {
                    return ExitCode::from(1);
                }
                sent += chunk.len() as u64;
                std::thread::sleep(Duration::from_millis(10));
            }
            ExitCode::SUCCESS
        }
        Some("dying-put") => {
            // Accepts a `put` header, sends `{"offset": 0}`, reads 64 KiB, then aborts immediately
            // (simulating target process killed mid-put).
            let mut out = std::io::stdout().lock();
            writeln!(
                out,
                "{}",
                json!({ "protocol": 1, "capabilities": ["stream.relay"] })
            )
            .unwrap();
            out.flush().unwrap();
            let mut line = String::new();
            let mut stdin = BufReader::new(std::io::stdin());
            if stdin.read_line(&mut line).unwrap_or(0) == 0 {
                return ExitCode::from(1);
            }
            writeln!(out, "{}", json!({ "offset": 0 })).unwrap();
            out.flush().unwrap();
            let mut buf = vec![0u8; 64 * 1024];
            let _ = stdin.read_exact(&mut buf);
            std::process::exit(9);
        }
        other => {
            eprintln!("unknown relay helper mode: {other:?}");
            ExitCode::from(2)
        }
    }
}

fn self_exe() -> String {
    std::env::current_exe()
        .unwrap()
        .to_string_lossy()
        .into_owned()
}

fn node(name: &str) -> Home {
    let home = Home::new();
    home.write_config("node.toml", &format!("name = \"{name}\"\n"));
    home
}

fn legion_home_of(home: &Home) -> PathBuf {
    home.state().parent().unwrap().to_path_buf()
}

fn write_home_file(home: &Home, rel: &str, bytes: &[u8]) {
    let path = home.home().join(rel);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(&path, bytes).unwrap();
}

fn toml_string(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

fn fleet_entry_for_node(alias: &str, target: &Home) -> String {
    let exe = toml_string(&self_exe());
    let flag = toml_string(HELPER_FLAG);
    let mode = toml_string("node");
    let lh = toml_string(&legion_home_of(target).to_string_lossy());
    let uh = toml_string(&target.home().to_string_lossy());
    format!("[machines.{alias}]\ncommand = [{exe}, {flag}, {mode}, {lh}, {uh}, \"rpc\"]\n")
}

fn fleet_entry_for_helper(alias: &str, mode: &str, extra: &[&str]) -> String {
    let mut argv = vec![
        toml_string(&self_exe()),
        toml_string(HELPER_FLAG),
        toml_string(mode),
    ];
    for item in extra {
        argv.push(toml_string(item));
    }
    argv.push(toml_string("rpc"));
    format!("[machines.{alias}]\ncommand = [{}]\n", argv.join(", "))
}

fn spawn_stream(node_home: &Home, pinned: Option<&str>) -> Child {
    spawn_stream_with_timeout(node_home, pinned, None)
}

fn spawn_stream_with_timeout(
    node_home: &Home,
    pinned: Option<&str>,
    relay_timeout_ms: Option<u64>,
) -> Child {
    let mut cmd = Command::new(LEGION_BIN);
    cmd.arg("stream");
    if let Some(p) = pinned {
        cmd.arg("--as").arg(p);
    }
    cmd.env("LEGION_HOME", legion_home_of(node_home))
        .env("HOME", node_home.home());
    if let Some(ms) = relay_timeout_ms {
        cmd.env("LEGION_TEST_RELAY_TIMEOUT_MS", ms.to_string());
    }
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn legion stream")
}

fn fleet_entry_for_node_with_pid(alias: &str, pid_file: &std::path::Path, target: &Home) -> String {
    let exe = toml_string(&self_exe());
    let flag = toml_string(HELPER_FLAG);
    let mode = toml_string("node-pid");
    let pf = toml_string(&pid_file.to_string_lossy());
    let lh = toml_string(&legion_home_of(target).to_string_lossy());
    let uh = toml_string(&target.home().to_string_lossy());
    format!("[machines.{alias}]\ncommand = [{exe}, {flag}, {mode}, {pf}, {lh}, {uh}, \"rpc\"]\n")
}

fn read_json_line<R: BufRead>(reader: &mut R) -> Value {
    let mut line = String::new();
    let n = reader.read_line(&mut line).expect("read json line");
    assert!(n > 0, "unexpected EOF waiting for JSON line");
    serde_json::from_str(&line).unwrap_or_else(|e| panic!("invalid JSON line {line:?}: {e}"))
}

fn read_greeting<R: BufRead>(reader: &mut R) -> Value {
    let g = read_json_line(reader);
    assert_eq!(g["protocol"], json!(1), "bad greeting: {g}");
    assert!(
        g["capabilities"]
            .as_array()
            .is_some_and(|c| c.iter().any(|v| v == "stream.relay")),
        "greeting missing stream.relay: {g}"
    );
    g
}

fn write_json_line<W: Write>(writer: &mut W, value: &Value) {
    writeln!(writer, "{value}").unwrap();
    writer.flush().unwrap();
}

fn audit_entries(home: &Home) -> Vec<Value> {
    let path = home.state().join("audit.jsonl");
    if !path.is_file() {
        return Vec::new();
    }
    fs::read_to_string(&path)
        .unwrap()
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

fn write_device_policy(home: &Home, role_body: &str) {
    home.write_config(
        "policy.toml",
        &format!(
            r#"
[principals.device]
role = "device_role"

[roles.device_role]
{role_body}
"#
        ),
    );
}

fn relayed_get_ranges_keep_open_and_dual_audit() {
    let tower = node("tower");
    let studio = node("studio");
    tower.write_config("fleet.toml", &fleet_entry_for_node("studio", &studio));
    write_device_policy(&tower, "allow = [\"fs.read\"]\n");
    write_device_policy(&studio, "allow = [\"fs.read\"]\n");

    let media: Vec<u8> = (0..200_000usize).map(|i| (i % 251) as u8).collect();
    write_home_file(&studio, "media/clip.bin", &media);

    // 1. Single ranged get through relay `tower` targeting `studio`.
    let mut child = spawn_stream(&tower, Some("device"));
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());
    let _ = read_greeting(&mut stdout);

    write_json_line(
        &mut stdin,
        &json!({
            "protocol": 1,
            "op": "relay",
            "on": "studio",
            "request": {
                "protocol": 1,
                "op": "get",
                "path": "~/media/clip.bin",
                "offset": 1234,
                "length": 5000
            }
        }),
    );
    drop(stdin);

    let ready = read_json_line(&mut stdout);
    assert_eq!(ready["size"], json!(media.len()));
    assert_eq!(ready["offset"], json!(1234));
    assert_eq!(ready["length"], json!(5000));
    let mut got = vec![0u8; 5000];
    stdout.read_exact(&mut got).unwrap();
    assert_eq!(got, &media[1234..6234]);
    assert!(child.wait().unwrap().success());

    // 2. Persistent `keep_open: true` multi-window get through relay `tower` -> `studio`.
    let mut child = spawn_stream(&tower, Some("device"));
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());
    let _ = read_greeting(&mut stdout);

    for (offset, len) in [(0u64, 4096u64), (10_000, 8192), (190_000, 20_000)] {
        write_json_line(
            &mut stdin,
            &json!({
                "protocol": 1,
                "op": "relay",
                "on": "studio",
                "request": {
                    "protocol": 1,
                    "op": "get",
                    "path": "~/media/clip.bin",
                    "offset": offset,
                    "length": len,
                    "keep_open": true
                }
            }),
        );
        let ready = read_json_line(&mut stdout);
        assert_eq!(ready["keep_open"], json!(true));
        let expected_len = ready["length"].as_u64().unwrap() as usize;
        let mut chunk = vec![0u8; expected_len];
        stdout.read_exact(&mut chunk).unwrap();
        assert_eq!(
            chunk,
            &media[offset as usize..offset as usize + expected_len]
        );
        let done = read_json_line(&mut stdout);
        assert_eq!(done, json!({ "done": true }));
    }
    drop(stdin);
    assert!(child.wait().unwrap().success());

    // Verify audit logs on BOTH nodes (relay records `target: "studio"`, target records `studio`).
    let tower_audit = audit_entries(&tower);
    assert!(
        tower_audit.iter().any(|e| {
            e["target"] == "studio"
                && e["tool"] == "stream.get"
                && e["principal"] == "device"
                && e["outcome"] == "ok"
        }),
        "tower audit missing relayed stream.get targeting studio: {tower_audit:?}"
    );
    let studio_audit = audit_entries(&studio);
    assert!(
        studio_audit.iter().any(|e| {
            e["target"] == "studio"
                && e["tool"] == "stream.get"
                && e["principal"] == "device"
                && e["outcome"] == "ok"
        }),
        "studio audit missing stream.get for device: {studio_audit:?}"
    );
}

fn relayed_put_cancel_leaves_same_partial_as_direct_and_resumes() {
    let tower = node("tower");
    let studio = node("studio");
    tower.write_config("fleet.toml", &fleet_entry_for_node("studio", &studio));
    write_device_policy(&tower, "allow = [\"fs.read\", \"fs.write\"]\n");
    write_device_policy(&studio, "allow = [\"fs.read\", \"fs.write\"]\n");

    let payload: Vec<u8> = (0..256 * 1024usize)
        .map(|i| b'a' + ((i * 7 + 13) % 26) as u8)
        .collect();
    let digest = legion_core::tokens::hash(std::str::from_utf8(&payload).unwrap());
    let cut = 96 * 1024usize;

    // A) Direct cancelled put on `studio` for baseline comparison.
    {
        let mut child = spawn_stream(&studio, Some("device"));
        let mut stdin = child.stdin.take().unwrap();
        let mut stdout = BufReader::new(child.stdout.take().unwrap());
        let _ = read_greeting(&mut stdout);
        write_json_line(
            &mut stdin,
            &json!({
                "protocol": 1,
                "op": "put",
                "path": "~/uploads/direct.bin",
                "size": payload.len(),
                "hash": digest,
            }),
        );
        let ready = read_json_line(&mut stdout);
        assert_eq!(ready["offset"], json!(0));
        stdin.write_all(&payload[..cut]).unwrap();
        stdin.flush().unwrap();
        drop(stdin);
        let _ = child.wait().unwrap();
    }

    // B) Relayed cancelled put through `tower` targeting `studio`.
    {
        let mut child = spawn_stream(&tower, Some("device"));
        let mut stdin = child.stdin.take().unwrap();
        let mut stdout = BufReader::new(child.stdout.take().unwrap());
        let _ = read_greeting(&mut stdout);
        write_json_line(
            &mut stdin,
            &json!({
                "protocol": 1,
                "op": "relay",
                "on": "studio",
                "request": {
                    "protocol": 1,
                    "op": "put",
                    "path": "~/uploads/relayed.bin",
                    "size": payload.len(),
                    "hash": digest,
                }
            }),
        );
        let ready = read_json_line(&mut stdout);
        assert_eq!(ready["offset"], json!(0));
        stdin.write_all(&payload[..cut]).unwrap();
        stdin.flush().unwrap();
        drop(stdin);
        let _ = child.wait().unwrap();
    }

    let direct_target = studio.home().join("uploads/direct.bin");
    let relayed_target = studio.home().join("uploads/relayed.bin");
    assert!(!direct_target.exists());
    assert!(!relayed_target.exists());
    let direct_part = studio.home().join("uploads/direct.bin.legion-part");
    let relayed_part = studio.home().join("uploads/relayed.bin.legion-part");
    assert!(
        direct_part.is_file(),
        "direct cancel should leave .legion-part"
    );
    assert!(
        relayed_part.is_file(),
        "relayed cancel should leave .legion-part"
    );
    assert_eq!(
        fs::read(&relayed_part).unwrap(),
        fs::read(&direct_part).unwrap(),
        "cancelled relayed put must leave the exact same partial file bytes as cancelled direct put"
    );

    // C) Resume the relayed put from the partial offset and verify final file integrity.
    {
        let mut child = spawn_stream(&tower, Some("device"));
        let mut stdin = child.stdin.take().unwrap();
        let mut stdout = BufReader::new(child.stdout.take().unwrap());
        let _ = read_greeting(&mut stdout);
        write_json_line(
            &mut stdin,
            &json!({
                "protocol": 1,
                "op": "relay",
                "on": "studio",
                "request": {
                    "protocol": 1,
                    "op": "put",
                    "path": "~/uploads/relayed.bin",
                    "size": payload.len(),
                    "hash": digest,
                }
            }),
        );
        let ready = read_json_line(&mut stdout);
        let offset = ready["offset"].as_u64().unwrap() as usize;
        assert_eq!(offset, cut);
        stdin.write_all(&payload[offset..]).unwrap();
        stdin.flush().unwrap();
        drop(stdin);
        let done = read_json_line(&mut stdout);
        assert_eq!(done["ok"], json!(true));
        assert_eq!(done["size"], json!(payload.len()));
        assert_eq!(done["hash"], json!(digest));
        assert!(child.wait().unwrap().success());
    }
    assert_eq!(fs::read(&relayed_target).unwrap(), payload);
    assert!(!relayed_part.exists());
}

fn relayed_stat_manifest_pack_unpack_delete_and_local_self() {
    let tower = node("tower");
    let studio = node("studio");
    tower.write_config("fleet.toml", &fleet_entry_for_node("studio", &studio));
    write_device_policy(
        &tower,
        "allow = [\"fs.read\", \"fs.write\", \"fs.remove\"]\n",
    );
    write_device_policy(
        &studio,
        "allow = [\"fs.read\", \"fs.write\", \"fs.remove\"]\n",
    );

    write_home_file(&studio, "tree/a.txt", b"alpha");
    write_home_file(&studio, "tree/sub/b.txt", b"beta");
    write_home_file(&tower, "local.txt", b"hello-from-tower");

    // 1. stat
    {
        let mut child = spawn_stream(&tower, Some("device"));
        let mut stdin = child.stdin.take().unwrap();
        let mut stdout = BufReader::new(child.stdout.take().unwrap());
        let _ = read_greeting(&mut stdout);
        write_json_line(
            &mut stdin,
            &json!({
                "protocol": 1,
                "op": "relay",
                "on": "studio",
                "request": { "protocol": 1, "op": "stat", "path": "~/tree/a.txt", "checksum": true }
            }),
        );
        drop(stdin);
        let reply = read_json_line(&mut stdout);
        assert_eq!(reply["kind"], json!("file"));
        assert_eq!(reply["size"], json!(5));
        assert!(reply["hash"].as_str().unwrap().starts_with("blake3:"));
        assert!(child.wait().unwrap().success());
    }

    // 2. manifest
    {
        let mut child = spawn_stream(&tower, Some("device"));
        let mut stdin = child.stdin.take().unwrap();
        let mut stdout = BufReader::new(child.stdout.take().unwrap());
        let _ = read_greeting(&mut stdout);
        write_json_line(
            &mut stdin,
            &json!({
                "protocol": 1,
                "op": "relay",
                "on": "studio",
                "request": { "protocol": 1, "op": "manifest", "root": "~/tree" }
            }),
        );
        drop(stdin);
        let mut paths = Vec::new();
        loop {
            let line = read_json_line(&mut stdout);
            if line.get("done") == Some(&json!(true)) {
                break;
            }
            if let Some(p) = line.get("path").and_then(Value::as_str) {
                paths.push(p.to_owned());
            }
        }
        assert!(paths.iter().any(|p| p == "a.txt"));
        assert!(paths.iter().any(|p| p == "sub/b.txt"));
        assert!(child.wait().unwrap().success());
    }

    // 3. pack -> unpack through relay
    let archive = {
        let mut child = spawn_stream(&tower, Some("device"));
        let mut stdin = child.stdin.take().unwrap();
        let mut stdout = BufReader::new(child.stdout.take().unwrap());
        let _ = read_greeting(&mut stdout);
        write_json_line(
            &mut stdin,
            &json!({
                "protocol": 1,
                "op": "relay",
                "on": "studio",
                "request": { "protocol": 1, "op": "pack", "root": "~/tree", "names": ["a.txt"] }
            }),
        );
        drop(stdin);
        let ready = read_json_line(&mut stdout);
        assert_eq!(ready["ready"], json!(true));
        let mut bytes = Vec::new();
        stdout.read_to_end(&mut bytes).unwrap();
        assert!(child.wait().unwrap().success());
        bytes
    };
    {
        let mut child = spawn_stream(&tower, Some("device"));
        let mut stdin = child.stdin.take().unwrap();
        let mut stdout = BufReader::new(child.stdout.take().unwrap());
        let _ = read_greeting(&mut stdout);
        write_json_line(
            &mut stdin,
            &json!({
                "protocol": 1,
                "op": "relay",
                "on": "studio",
                "request": {
                    "protocol": 1,
                    "op": "unpack",
                    "root": "~/unpacked",
                    "entries": [{ "path": "a.txt", "kind": "file", "size": 5 }]
                }
            }),
        );
        let ready = read_json_line(&mut stdout);
        assert_eq!(ready["ready"], json!(true));
        stdin.write_all(&archive).unwrap();
        stdin.flush().unwrap();
        drop(stdin);
        let done = read_json_line(&mut stdout);
        assert_eq!(done["ok"], json!(true));
        assert_eq!(done["files"], json!(1));
        assert!(child.wait().unwrap().success());
    }
    assert_eq!(
        fs::read(studio.home().join("unpacked/a.txt")).unwrap(),
        b"alpha"
    );

    // 4. delete
    {
        let mut child = spawn_stream(&tower, Some("device"));
        let mut stdin = child.stdin.take().unwrap();
        let mut stdout = BufReader::new(child.stdout.take().unwrap());
        let _ = read_greeting(&mut stdout);
        write_json_line(
            &mut stdin,
            &json!({
                "protocol": 1,
                "op": "relay",
                "on": "studio",
                "request": { "protocol": 1, "op": "delete", "root": "~/unpacked", "names": ["a.txt"] }
            }),
        );
        drop(stdin);
        let done = read_json_line(&mut stdout);
        assert_eq!(done["removed"], json!(1));
        assert!(child.wait().unwrap().success());
    }
    assert!(!studio.home().join("unpacked/a.txt").exists());

    // 5. `on` naming the relaying node itself (`tower`) is served locally (including keep_open).
    {
        let mut child = spawn_stream(&tower, Some("device"));
        let mut stdin = child.stdin.take().unwrap();
        let mut stdout = BufReader::new(child.stdout.take().unwrap());
        let _ = read_greeting(&mut stdout);
        for (offset, len) in [(0u64, 5u64), (6u64, 4u64)] {
            write_json_line(
                &mut stdin,
                &json!({
                    "protocol": 1,
                    "op": "relay",
                    "on": "tower",
                    "request": {
                        "protocol": 1,
                        "op": "get",
                        "path": "~/local.txt",
                        "offset": offset,
                        "length": len,
                        "keep_open": true
                    }
                }),
            );
            let ready = read_json_line(&mut stdout);
            assert_eq!(ready["keep_open"], json!(true));
            let mut got = vec![0u8; len as usize];
            stdout.read_exact(&mut got).unwrap();
            let expected = &b"hello-from-tower"[offset as usize..offset as usize + len as usize];
            assert_eq!(got, expected);
            assert_eq!(read_json_line(&mut stdout), json!({ "done": true }));
        }
        drop(stdin);
        assert!(child.wait().unwrap().success());
    }
}

fn refusals_relay_denies_target_denies_path_outside_grant_and_no_escalation() {
    let tower = node("tower");
    let studio = node("studio");
    tower.write_config("fleet.toml", &fleet_entry_for_node("studio", &studio));
    write_home_file(&studio, "allowed/ok.txt", b"ok");
    write_home_file(&studio, "secret/private.txt", b"secret");

    // 1. Relay denies (`tower` only allows `fs.read` for `device`; `device` attempts relayed `put`).
    write_device_policy(&tower, "allow = [\"fs.read\"]\n");
    write_device_policy(&studio, "allow = [\"fs.read\", \"fs.write\"]\n");
    {
        let mut child = spawn_stream(&tower, Some("device"));
        let mut stdin = child.stdin.take().unwrap();
        let mut stdout = BufReader::new(child.stdout.take().unwrap());
        let _ = read_greeting(&mut stdout);
        write_json_line(
            &mut stdin,
            &json!({
                "protocol": 1,
                "op": "relay",
                "on": "studio",
                "request": {
                    "protocol": 1,
                    "op": "put",
                    "path": "~/allowed/new.txt",
                    "size": 2,
                    "hash": legion_core::tokens::hash("hi")
                }
            }),
        );
        drop(stdin);
        let reply = read_json_line(&mut stdout);
        assert_eq!(reply["error"]["code"], json!("denied"));
        assert_eq!(reply["error"]["machine"], json!("tower"));
        let _ = child.wait().unwrap();
    }
    assert!(
        audit_entries(&studio).is_empty(),
        "target should never be contacted when relay denies"
    );

    // 2. Target denies (`tower` allows `fs.write`, `studio` denies `fs.write` for `device`).
    write_device_policy(&tower, "allow = [\"fs.read\", \"fs.write\"]\n");
    write_device_policy(&studio, "allow = [\"fs.read\"]\n");
    {
        let mut child = spawn_stream(&tower, Some("device"));
        let mut stdin = child.stdin.take().unwrap();
        let mut stdout = BufReader::new(child.stdout.take().unwrap());
        let _ = read_greeting(&mut stdout);
        write_json_line(
            &mut stdin,
            &json!({
                "protocol": 1,
                "op": "relay",
                "on": "studio",
                "request": {
                    "protocol": 1,
                    "op": "put",
                    "path": "~/allowed/new.txt",
                    "size": 2,
                    "hash": legion_core::tokens::hash("hi")
                }
            }),
        );
        drop(stdin);
        let reply = read_json_line(&mut stdout);
        assert_eq!(reply["error"]["code"], json!("denied"));
        assert_eq!(reply["error"]["machine"], json!("studio"));
        let _ = child.wait().unwrap();
    }

    // 3. Path outside target's grant (and path confinement is the target's, NOT the relay's!).
    // Give `tower` a DIFFERENT allowed path (`~/tower-only`) while `studio` grants `~/allowed`.
    write_device_policy(
        &tower,
        "allow = [\"fs.read\"]\npaths = [\"~/tower-only\"]\n",
    );
    write_device_policy(&studio, "allow = [\"fs.read\"]\npaths = [\"~/allowed\"]\n");
    // Reading ~/allowed/ok.txt on studio MUST SUCCEED even though ~/allowed is outside tower's paths!
    {
        let mut child = spawn_stream(&tower, Some("device"));
        let mut stdin = child.stdin.take().unwrap();
        let mut stdout = BufReader::new(child.stdout.take().unwrap());
        let _ = read_greeting(&mut stdout);
        write_json_line(
            &mut stdin,
            &json!({
                "protocol": 1,
                "op": "relay",
                "on": "studio",
                "request": { "protocol": 1, "op": "get", "path": "~/allowed/ok.txt" }
            }),
        );
        drop(stdin);
        let ready = read_json_line(&mut stdout);
        assert_eq!(ready["length"], json!(2));
        let mut got = vec![0u8; 2];
        stdout.read_exact(&mut got).unwrap();
        assert_eq!(got, b"ok");
        assert!(child.wait().unwrap().success());
    }
    // Reading ~/secret/private.txt on studio MUST BE DENIED by studio!
    {
        let mut child = spawn_stream(&tower, Some("device"));
        let mut stdin = child.stdin.take().unwrap();
        let mut stdout = BufReader::new(child.stdout.take().unwrap());
        let _ = read_greeting(&mut stdout);
        write_json_line(
            &mut stdin,
            &json!({
                "protocol": 1,
                "op": "relay",
                "on": "studio",
                "request": { "protocol": 1, "op": "get", "path": "~/secret/private.txt" }
            }),
        );
        drop(stdin);
        let reply = read_json_line(&mut stdout);
        assert_eq!(reply["error"]["code"], json!("denied"));
        assert_eq!(reply["error"]["machine"], json!("studio"));
        let _ = child.wait().unwrap();
    }

    // 4. Spoofing `"as": "owner"` inside `request` (or outer `as`) while pinned as `device` cannot
    //    bypass studio's path restriction!
    {
        let mut child = spawn_stream(&tower, Some("device"));
        let mut stdin = child.stdin.take().unwrap();
        let mut stdout = BufReader::new(child.stdout.take().unwrap());
        let _ = read_greeting(&mut stdout);
        write_json_line(
            &mut stdin,
            &json!({
                "protocol": 1,
                "op": "relay",
                "on": "studio",
                "as": "owner",
                "request": { "protocol": 1, "op": "get", "path": "~/secret/private.txt", "as": "owner" }
            }),
        );
        drop(stdin);
        let reply = read_json_line(&mut stdout);
        assert_eq!(reply["error"]["code"], json!("denied"));
        assert_eq!(reply["error"]["machine"], json!("studio"));
        let _ = child.wait().unwrap();
    }
}

fn hops_limit_chained_via_and_old_relay_refusal() {
    let tower = node("tower");
    let mid = node("mid");
    let studio = node("studio");
    write_home_file(&studio, "hello.txt", b"via-chain");

    tower.write_config(
        "fleet.toml",
        &format!(
            "{}[machines.studio]\nvia = \"mid\"\n{}[machines.behind_old]\nvia = \"oldrelay\"\n",
            fleet_entry_for_node("mid", &mid),
            fleet_entry_for_helper("oldrelay", "old-relay", &[])
        ),
    );
    mid.write_config("fleet.toml", &fleet_entry_for_node("studio", &studio));
    write_device_policy(&tower, "allow = [\"fs.read\"]\n");
    write_device_policy(&mid, "allow = [\"fs.read\"]\n");
    write_device_policy(&studio, "allow = [\"fs.read\"]\n");

    // 1. Chained `via` (`device -> tower -> mid -> studio`) succeeds within MAX_HOPS.
    {
        let mut child = spawn_stream(&tower, Some("device"));
        let mut stdin = child.stdin.take().unwrap();
        let mut stdout = BufReader::new(child.stdout.take().unwrap());
        let _ = read_greeting(&mut stdout);
        write_json_line(
            &mut stdin,
            &json!({
                "protocol": 1,
                "op": "relay",
                "on": "studio",
                "request": { "protocol": 1, "op": "get", "path": "~/hello.txt" }
            }),
        );
        drop(stdin);
        let ready = read_json_line(&mut stdout);
        assert_eq!(ready["length"], json!(9));
        let mut got = vec![0u8; 9];
        stdout.read_exact(&mut got).unwrap();
        assert_eq!(got, b"via-chain");
        assert!(child.wait().unwrap().success());
    }

    // 2. Exceeding MAX_HOPS (3) is refused cleanly.
    {
        let mut child = spawn_stream(&tower, Some("device"));
        let mut stdin = child.stdin.take().unwrap();
        let mut stdout = BufReader::new(child.stdout.take().unwrap());
        let _ = read_greeting(&mut stdout);
        write_json_line(
            &mut stdin,
            &json!({
                "protocol": 1,
                "op": "relay",
                "on": "studio",
                "hops": 3,
                "request": { "protocol": 1, "op": "stat", "path": "~/hello.txt" }
            }),
        );
        drop(stdin);
        let reply = read_json_line(&mut stdout);
        assert_eq!(reply["error"]["code"], json!("failed"));
        assert!(
            reply["error"]["message"]
                .as_str()
                .unwrap()
                .contains("maximum of 3 hops"),
            "unexpected hop error: {reply}"
        );
        let _ = child.wait().unwrap();
    }

    // 3. Old relay omitting `stream.relay` in its greeting is refused before forwarding.
    {
        let mut child = spawn_stream(&tower, Some("device"));
        let mut stdin = child.stdin.take().unwrap();
        let mut stdout = BufReader::new(child.stdout.take().unwrap());
        let _ = read_greeting(&mut stdout);
        write_json_line(
            &mut stdin,
            &json!({
                "protocol": 1,
                "op": "relay",
                "on": "behind_old",
                "request": { "protocol": 1, "op": "stat", "path": "~/hello.txt" }
            }),
        );
        drop(stdin);
        let reply = read_json_line(&mut stdout);
        assert_eq!(reply["error"]["code"], json!("unsupported"));
        assert_eq!(reply["error"]["machine"], json!("oldrelay"));
        assert_eq!(reply["error"]["details"]["reason"], json!("relay_too_old"));
        assert_eq!(
            reply["error"]["message"],
            json!("oldrelay needs an update before it can pass files along from behind_old.")
        );
        let _ = child.wait().unwrap();
    }

    // 4. Directly asking an old relay for `op: "relay"` answers `unsupported` (never misreads as
    //    local op).
    {
        let mut child = Command::new(self_exe())
            .args([HELPER_FLAG, "old-relay"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let mut stdin = child.stdin.take().unwrap();
        let mut stdout = BufReader::new(child.stdout.take().unwrap());
        let greeting = read_json_line(&mut stdout);
        assert_eq!(greeting, json!({ "protocol": 1 }));
        write_json_line(
            &mut stdin,
            &json!({
                "protocol": 1,
                "op": "relay",
                "on": "studio",
                "request": { "protocol": 1, "op": "stat", "path": "~/hello.txt" }
            }),
        );
        drop(stdin);
        let reply = read_json_line(&mut stdout);
        assert_eq!(reply["error"]["code"], json!("unsupported"));
        let _ = child.wait().unwrap();
    }
}

fn heartbeats_pass_through_and_both_ends_dying_close_in_bounded_time() {
    let tower = node("tower");
    let pid_file = tower.home().join("slow-get.pid");
    tower.write_config(
        "fleet.toml",
        &format!(
            "{}{}{}",
            fleet_entry_for_helper("busybox", "heartbeat-stat", &[]),
            fleet_entry_for_helper("slowtarget", "slow-get", &[&pid_file.to_string_lossy()]),
            fleet_entry_for_helper("dyingtarget", "dying-put", &[]),
        ),
    );
    write_device_policy(&tower, "allow = [\"fs.read\", \"fs.write\"]\n");

    // 1. Heartbeat `{"busy": true}` lines pass straight through the relay.
    {
        let mut child = spawn_stream(&tower, Some("device"));
        let mut stdin = child.stdin.take().unwrap();
        let mut stdout = BufReader::new(child.stdout.take().unwrap());
        let _ = read_greeting(&mut stdout);
        write_json_line(
            &mut stdin,
            &json!({
                "protocol": 1,
                "op": "relay",
                "on": "busybox",
                "request": { "protocol": 1, "op": "stat", "path": "~/big.iso" }
            }),
        );
        drop(stdin);
        assert_eq!(read_json_line(&mut stdout), json!({ "busy": true }));
        assert_eq!(read_json_line(&mut stdout), json!({ "busy": true }));
        let reply = read_json_line(&mut stdout);
        assert_eq!(reply["kind"], json!("file"));
        assert_eq!(reply["size"], json!(123));
        assert!(child.wait().unwrap().success());
    }

    // 2. Client killed mid-get: dropping client's stdout/stdin closes both relay and downstream
    //    target within bounded time (< 3s).
    {
        let mut child = spawn_stream(&tower, Some("device"));
        let mut stdin = child.stdin.take().unwrap();
        let mut stdout = BufReader::new(child.stdout.take().unwrap());
        let _ = read_greeting(&mut stdout);
        write_json_line(
            &mut stdin,
            &json!({
                "protocol": 1,
                "op": "relay",
                "on": "slowtarget",
                "request": { "protocol": 1, "op": "get", "path": "~/huge.bin" }
            }),
        );
        let ready = read_json_line(&mut stdout);
        assert_eq!(ready["length"], json!(32 * 1024 * 1024));
        let mut first = vec![0u8; 64 * 1024];
        stdout.read_exact(&mut first).unwrap();

        // Simulate client going away mid-get.
        let started = Instant::now();
        drop(stdout);
        drop(stdin);

        let deadline = Instant::now() + Duration::from_secs(3);
        let exited = loop {
            if let Some(status) = child.try_wait().unwrap() {
                break Some(status);
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                break None;
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        assert!(
            exited.is_some(),
            "relay did not exit within 3s after client disappeared mid-get"
        );
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "relay teardown took too long: {:?}",
            started.elapsed()
        );
        // Downstream target should also notice the broken pipe / shutdown and exit promptly.
        let target_deadline = Instant::now() + Duration::from_secs(3);
        while pid_file.exists() && Instant::now() < target_deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(
            !pid_file.exists(),
            "downstream target did not exit within bounded time after client disappeared"
        );
    }

    // 3. Target killed mid-put: relay reports error and closes client side within bounded time
    //    even while client keeps stdin open.
    {
        let mut child = spawn_stream(&tower, Some("device"));
        let mut stdin = child.stdin.take().unwrap();
        let mut stdout = BufReader::new(child.stdout.take().unwrap());
        let _ = read_greeting(&mut stdout);
        let total = 4 * 1024 * 1024usize;
        write_json_line(
            &mut stdin,
            &json!({
                "protocol": 1,
                "op": "relay",
                "on": "dyingtarget",
                "request": {
                    "protocol": 1,
                    "op": "put",
                    "path": "~/upload.bin",
                    "size": total,
                    "hash": format!("blake3:{}", "0".repeat(64)),
                }
            }),
        );
        let ready = read_json_line(&mut stdout);
        assert_eq!(ready["offset"], json!(0));
        // Write 64 KiB (enough for dying-put to exit), intentionally leaving stdin open!
        let chunk = vec![0xABu8; 64 * 1024];
        stdin.write_all(&chunk).unwrap();
        stdin.flush().unwrap();

        let started = Instant::now();
        let reply = read_json_line(&mut stdout);
        assert_eq!(reply["error"]["code"], json!("failed"));
        assert_eq!(reply["error"]["machine"], json!("dyingtarget"));
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "relay did not report target death mid-put in bounded time: {:?}",
            started.elapsed()
        );
        drop(stdin);
        let _ = child.wait().unwrap();
    }
}

fn wait_for_file(path: &std::path::Path, limit: Duration) -> String {
    let deadline = Instant::now() + limit;
    loop {
        if let Ok(text) = fs::read_to_string(path)
            && !text.trim().is_empty()
        {
            return text;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {}",
            path.display()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn wait_child_exit(child: &mut Child, limit: Duration) -> Option<std::process::ExitStatus> {
    let deadline = Instant::now() + limit;
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            return Some(status);
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return None;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[cfg(unix)]
fn send_signal(pid: u32, sig: &str) {
    let status = Command::new("kill")
        .args([sig, &pid.to_string()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("run kill");
    assert!(status.success(), "kill {sig} {pid} failed");
}

#[cfg(unix)]
fn pid_is_alive(pid: u32) -> bool {
    let proc_stat = PathBuf::from(format!("/proc/{pid}/stat"));
    if let Ok(stat) = fs::read_to_string(&proc_stat)
        && let Some(rparen) = stat.rfind(')')
        && let Some(state) = stat[rparen + 1..].trim_start().chars().next()
    {
        return state != 'Z';
    }
    Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

#[cfg(unix)]
fn count_alive_pids(pids: &[u32]) -> usize {
    pids.iter()
        .copied()
        .filter(|&pid| pid_is_alive(pid))
        .count()
}

#[cfg(unix)]
fn wait_pids_gone(pids: &[u32], limit: Duration) -> usize {
    let deadline = Instant::now() + limit;
    loop {
        let alive = count_alive_pids(pids);
        if alive == 0 || Instant::now() >= deadline {
            return alive;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn stuck_target_50mb_get_sigstop_client_gone_and_timeout() {
    #[cfg(not(unix))]
    {
        // SIGSTOP and stdout peer hangup detection are Unix-only.
        return;
    }

    #[cfg(unix)]
    {
        let tower = node("tower");
        let studio = node("studio");
        let target_pid_file = tower.home().join("studio-get.pid");
        tower.write_config(
            "fleet.toml",
            &fleet_entry_for_node_with_pid("studio", &target_pid_file, &studio),
        );
        write_device_policy(&tower, "allow = [\"fs.read\"]\n");
        write_device_policy(&studio, "allow = [\"fs.read\"]\n");

        // Create a real 50 MiB file on `studio` (sparse-friendly on ext4/btrfs/tmpfs via set_len
        // + end byte so reads return 50 MiB cleanly without slowing the test down).
        let huge_path = studio.home().join("huge50.bin");
        let total: u64 = 50 * 1024 * 1024;
        {
            let mut file = fs::File::create(&huge_path).unwrap();
            file.set_len(total).unwrap();
            use std::io::Seek;
            file.seek(std::io::SeekFrom::Start(total - 4)).unwrap();
            file.write_all(b"END!").unwrap();
        }

        // Scenario 1A: 50 MiB relayed `get`, target's `legion stream` SIGSTOPped after 1 MiB,
        // client killed while timeout is long (60s) -> relay notices client gone and tears down
        // both relay and target within a few seconds without SIGCONT.
        {
            let _ = fs::remove_file(&target_pid_file);
            let mut child = spawn_stream_with_timeout(&tower, Some("device"), Some(60_000));
            let relay_pid = child.id();
            let mut stdin = child.stdin.take().unwrap();
            let mut stdout = BufReader::new(child.stdout.take().unwrap());
            let _ = read_greeting(&mut stdout);
            write_json_line(
                &mut stdin,
                &json!({
                    "protocol": 1,
                    "op": "relay",
                    "on": "studio",
                    "request": { "protocol": 1, "op": "get", "path": "~/huge50.bin" }
                }),
            );
            drop(stdin);

            let ready = read_json_line(&mut stdout);
            assert_eq!(ready["length"], json!(total));
            let mut first_mb = vec![0u8; 1024 * 1024];
            stdout.read_exact(&mut first_mb).unwrap();

            let target_pid: u32 = wait_for_file(&target_pid_file, Duration::from_secs(3))
                .trim()
                .parse()
                .unwrap();
            assert_eq!(count_alive_pids(&[relay_pid]), 1);
            assert_eq!(count_alive_pids(&[target_pid]), 1);

            send_signal(target_pid, "-STOP");

            // Kill the client side by dropping its stdout reader.
            let started = Instant::now();
            drop(stdout);

            let status = wait_child_exit(&mut child, Duration::from_secs(4));
            assert!(
                status.is_some(),
                "relay stayed stuck instead of noticing client gone on SIGSTOPped target"
            );
            assert!(
                started.elapsed() < Duration::from_secs(4),
                "client-gone teardown took too long: {:?}",
                started.elapsed()
            );
            let remaining_relay = wait_pids_gone(&[relay_pid], Duration::from_secs(2));
            let remaining_target = wait_pids_gone(&[target_pid], Duration::from_secs(2));
            if remaining_target > 0 {
                let _ = Command::new("kill")
                    .args(["-KILL", &target_pid.to_string()])
                    .status();
            }
            assert_eq!(
                remaining_relay, 0,
                "relay process lingered after client gone"
            );
            assert_eq!(
                remaining_target, 0,
                "SIGSTOPped target process lingered after client gone"
            );
        }

        // Scenario 1B: 50 MiB relayed `get`, target's `legion stream` SIGSTOPped after 1 MiB,
        // client stays alive -> shortened between-messages timeout (500ms) ends both sides with
        // `timeout` labelled with `studio` and leaves 0 processes on relay and target.
        {
            let _ = fs::remove_file(&target_pid_file);
            let mut child = spawn_stream_with_timeout(&tower, Some("device"), Some(500));
            let relay_pid = child.id();
            let mut stdin = child.stdin.take().unwrap();
            let mut stdout = BufReader::new(child.stdout.take().unwrap());
            let mut stderr = child.stderr.take().unwrap();
            let _ = read_greeting(&mut stdout);
            write_json_line(
                &mut stdin,
                &json!({
                    "protocol": 1,
                    "op": "relay",
                    "on": "studio",
                    "request": { "protocol": 1, "op": "get", "path": "~/huge50.bin" }
                }),
            );
            drop(stdin);

            let ready = read_json_line(&mut stdout);
            assert_eq!(ready["length"], json!(total));
            let mut first_mb = vec![0u8; 1024 * 1024];
            stdout.read_exact(&mut first_mb).unwrap();

            let target_pid: u32 = wait_for_file(&target_pid_file, Duration::from_secs(3))
                .trim()
                .parse()
                .unwrap();
            send_signal(target_pid, "-STOP");

            // Client keeps reading; relay must time out and close stdout instead of hanging.
            let started = Instant::now();
            let mut rest = Vec::new();
            let _ = stdout.read_to_end(&mut rest);
            assert!(
                started.elapsed() < Duration::from_secs(4),
                "timeout took too long on SIGSTOPped get: {:?}",
                started.elapsed()
            );
            assert!(
                (first_mb.len() + rest.len()) < total as usize,
                "stream should terminate short on timeout"
            );
            let status = wait_child_exit(&mut child, Duration::from_secs(3)).expect("relay exit");
            assert!(!status.success());
            let mut err_text = String::new();
            let _ = stderr.read_to_string(&mut err_text);
            assert!(
                err_text.contains("studio") && err_text.contains("stopped responding"),
                "expected timeout stderr naming studio, got: {err_text:?}"
            );

            let audits = audit_entries(&tower);
            let last = audits.last().expect("relay audit entry");
            assert_eq!(last["target"], json!("studio"));
            assert_eq!(last["outcome"], json!("timeout"));

            let remaining_relay = wait_pids_gone(&[relay_pid], Duration::from_secs(2));
            let remaining_target = wait_pids_gone(&[target_pid], Duration::from_secs(2));
            if remaining_target > 0 {
                let _ = Command::new("kill")
                    .args(["-KILL", &target_pid.to_string()])
                    .status();
            }
            assert_eq!(remaining_relay, 0, "relay process lingered after timeout");
            assert_eq!(
                remaining_target, 0,
                "SIGSTOPped target process lingered after timeout"
            );
        }
    }
}

fn stuck_target_checksum_stat_sigstop_timeout_client_gone_and_deadline_resets() {
    let tower = node("tower");
    let pid_file = tower.home().join("stat-target.pid");
    let seen_file = tower.home().join("stat-seen.txt");
    tower.write_config(
        "fleet.toml",
        &format!(
            "{}{}{}",
            fleet_entry_for_helper(
                "studio",
                "stalling-stat",
                &[&pid_file.to_string_lossy(), &seen_file.to_string_lossy()],
            ),
            fleet_entry_for_helper("busy_stat", "paced-heartbeat-stat", &[]),
            fleet_entry_for_helper("paced_get", "paced-get", &[]),
        ),
    );
    write_device_policy(&tower, "allow = [\"fs.read\"]\n");

    // Scenario 2A: checksum `stat` stopped before first reply -> shortened 500ms timeout replies
    // with `timeout` labelled with `studio` and leaves 0 processes on relay and target.
    {
        let _ = fs::remove_file(&pid_file);
        let _ = fs::remove_file(&seen_file);
        let mut child = spawn_stream_with_timeout(&tower, Some("device"), Some(500));
        #[cfg(unix)]
        let relay_pid = child.id();
        let mut stdin = child.stdin.take().unwrap();
        let mut stdout = BufReader::new(child.stdout.take().unwrap());
        let _ = read_greeting(&mut stdout);
        write_json_line(
            &mut stdin,
            &json!({
                "protocol": 1,
                "op": "relay",
                "on": "studio",
                "request": { "protocol": 1, "op": "stat", "path": "~/big.iso", "checksum": true }
            }),
        );
        drop(stdin);

        let _ = wait_for_file(&seen_file, Duration::from_secs(3));
        let target_pid: u32 = wait_for_file(&pid_file, Duration::from_secs(3))
            .trim()
            .parse()
            .unwrap();
        #[cfg(unix)]
        send_signal(target_pid, "-STOP");

        let started = Instant::now();
        let reply = read_json_line(&mut stdout);
        assert!(
            started.elapsed() < Duration::from_secs(4),
            "checksum stat timeout took too long: {:?}",
            started.elapsed()
        );
        assert_eq!(reply["error"]["code"], json!("timeout"), "reply={reply}");
        assert_eq!(reply["error"]["machine"], json!("studio"), "reply={reply}");
        let status = wait_child_exit(&mut child, Duration::from_secs(3)).expect("relay exit");
        assert!(!status.success());

        #[cfg(unix)]
        {
            let remaining_relay = wait_pids_gone(&[relay_pid], Duration::from_secs(2));
            let remaining_target = wait_pids_gone(&[target_pid], Duration::from_secs(2));
            if remaining_target > 0 {
                let _ = Command::new("kill")
                    .args(["-KILL", &target_pid.to_string()])
                    .status();
            }
            assert_eq!(
                remaining_relay, 0,
                "relay process lingered after stat timeout"
            );
            assert_eq!(
                remaining_target, 0,
                "target process lingered after stat timeout"
            );
        }
        #[cfg(not(unix))]
        let _ = target_pid;
    }

    // Scenario 2B (Unix): checksum `stat` SIGSTOPped before first reply with a 60s timeout, client
    // killed -> relay notices client gone and tears down both sides within a few seconds.
    #[cfg(unix)]
    {
        let _ = fs::remove_file(&pid_file);
        let _ = fs::remove_file(&seen_file);
        let mut child = spawn_stream_with_timeout(&tower, Some("device"), Some(60_000));
        let relay_pid = child.id();
        let mut stdin = child.stdin.take().unwrap();
        let mut stdout = BufReader::new(child.stdout.take().unwrap());
        let _ = read_greeting(&mut stdout);
        write_json_line(
            &mut stdin,
            &json!({
                "protocol": 1,
                "op": "relay",
                "on": "studio",
                "request": { "protocol": 1, "op": "stat", "path": "~/big.iso", "checksum": true }
            }),
        );
        drop(stdin);

        let _ = wait_for_file(&seen_file, Duration::from_secs(3));
        let target_pid: u32 = wait_for_file(&pid_file, Duration::from_secs(3))
            .trim()
            .parse()
            .unwrap();
        send_signal(target_pid, "-STOP");

        let started = Instant::now();
        drop(stdout);

        let status = wait_child_exit(&mut child, Duration::from_secs(4));
        assert!(
            status.is_some(),
            "relay stayed stuck on SIGSTOPped checksum stat after client disappeared"
        );
        assert!(
            started.elapsed() < Duration::from_secs(4),
            "client-gone teardown on stat took too long: {:?}",
            started.elapsed()
        );
        let remaining_relay = wait_pids_gone(&[relay_pid], Duration::from_secs(2));
        let remaining_target = wait_pids_gone(&[target_pid], Duration::from_secs(2));
        if remaining_target > 0 {
            let _ = Command::new("kill")
                .args(["-KILL", &target_pid.to_string()])
                .status();
        }
        assert_eq!(
            remaining_relay, 0,
            "relay process lingered after stat client gone"
        );
        assert_eq!(
            remaining_target, 0,
            "SIGSTOPped target lingered after stat client gone"
        );
    }

    // Scenario 2C: heartbeats reset the between-messages deadline (3 heartbeats spaced 200ms apart
    // = 800ms total > 450ms timeout).
    {
        let mut child = spawn_stream_with_timeout(&tower, Some("device"), Some(450));
        let mut stdin = child.stdin.take().unwrap();
        let mut stdout = BufReader::new(child.stdout.take().unwrap());
        let _ = read_greeting(&mut stdout);
        write_json_line(
            &mut stdin,
            &json!({
                "protocol": 1,
                "op": "relay",
                "on": "busy_stat",
                "request": { "protocol": 1, "op": "stat", "path": "~/big.iso", "checksum": true }
            }),
        );
        drop(stdin);
        for _ in 0..3 {
            assert_eq!(read_json_line(&mut stdout), json!({ "busy": true }));
        }
        let reply = read_json_line(&mut stdout);
        assert_eq!(reply["kind"], json!("file"));
        assert_eq!(reply["size"], json!(456));
        assert!(child.wait().unwrap().success());
    }

    // Scenario 2D: data chunks reset the between-messages deadline (3 chunks spaced 200ms apart =
    // 600ms total > 450ms timeout).
    {
        let mut child = spawn_stream_with_timeout(&tower, Some("device"), Some(450));
        let mut stdin = child.stdin.take().unwrap();
        let mut stdout = BufReader::new(child.stdout.take().unwrap());
        let _ = read_greeting(&mut stdout);
        write_json_line(
            &mut stdin,
            &json!({
                "protocol": 1,
                "op": "relay",
                "on": "paced_get",
                "request": { "protocol": 1, "op": "get", "path": "~/paced.bin" }
            }),
        );
        drop(stdin);
        let ready = read_json_line(&mut stdout);
        let expected_len = 3 * 16 * 1024;
        assert_eq!(ready["length"], json!(expected_len));
        let mut got = vec![0u8; expected_len];
        stdout.read_exact(&mut got).unwrap();
        assert!(got.iter().all(|&b| b == 0x42));
        assert!(child.wait().unwrap().success());
    }
}
