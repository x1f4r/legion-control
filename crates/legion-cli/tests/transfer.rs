//! `fs.transfer`, `legion cp`, `legion stream` and `legion gate` end to end: the built `legion`
//! binary as the node a test talks to and as the second and third nodes it reaches over the
//! `command` connector, each with its own `LEGION_HOME`. The test binary also runs itself as the
//! connector, which lets it cut a stream, slow it down or print a banner before it.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::time::{Duration, Instant};

use common::{Home, PATIENCE, Run};
use legion_net::{RemoteOs, Subcommand, remote_command};
use serde_json::{Value, json};

const LEGION_BIN: &str = env!("CARGO_BIN_EXE_legion");
const HELPER_FLAG: &str = "--transfer-helper";

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
            "files_go_every_way_and_land_inside_a_directory",
            files_go_every_way_and_land_inside_a_directory,
        ),
        (
            "a_tree_arrives_whole_and_a_second_run_sends_nothing",
            a_tree_arrives_whole_and_a_second_run_sends_nothing,
        ),
        (
            "delete_checksum_and_exclude_decide_what_changes",
            delete_checksum_and_exclude_decide_what_changes,
        ),
        ("a_dry_run_changes_nothing", a_dry_run_changes_nothing),
        (
            "an_interrupted_file_resumes_where_it_stopped",
            an_interrupted_file_resumes_where_it_stopped,
        ),
        (
            "a_cancelled_transfer_leaves_nothing_half_written_under_a_final_name",
            a_cancelled_transfer_leaves_nothing_half_written_under_a_final_name,
        ),
        (
            "each_end_applies_its_own_policy_and_says_which_machine_refused",
            each_end_applies_its_own_policy_and_says_which_machine_refused,
        ),
        (
            "a_stream_that_fails_early_never_leaves_the_other_side_waiting",
            a_stream_that_fails_early_never_leaves_the_other_side_waiting,
        ),
        (
            "ends_are_parsed_and_unknown_machines_named",
            ends_are_parsed_and_unknown_machines_named,
        ),
        (
            "a_source_cannot_send_what_was_not_planned",
            a_source_cannot_send_what_was_not_planned,
        ),
        (
            "extra_entries_go_only_once_everything_new_has_arrived",
            extra_entries_go_only_once_everything_new_has_arrived,
        ),
        (
            "names_that_differ_only_in_case_are_held_back_where_they_would_collide",
            names_that_differ_only_in_case_are_held_back_where_they_would_collide,
        ),
        (
            "an_end_that_speaks_another_protocol_is_told_apart_from_a_banner",
            an_end_that_speaks_another_protocol_is_told_apart_from_a_banner,
        ),
        (
            "what_arrives_is_checked_and_a_file_keeps_its_time",
            what_arrives_is_checked_and_a_file_keeps_its_time,
        ),
        (
            "an_end_that_is_stopped_is_given_time_to_clean_up",
            an_end_that_is_stopped_is_given_time_to_clean_up,
        ),
        (
            "gate_serves_only_the_connectors_commands_pinned_to_its_principal",
            gate_serves_only_the_connectors_commands_pinned_to_its_principal,
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

/// The connector the nodes' `fleet.toml` names: `<mode> <legion> <legion_home> <home> [extra…]
/// <subcommand>`, the last word being `rpc` or, for a stream pipe, `stream`.
fn helper(args: &[String]) -> ExitCode {
    let (mode, rest) = args.split_first().unwrap();
    let (legion, legion_home, home) = (&rest[0], &rest[1], &rest[2]);
    let subcommand = rest.last().unwrap();
    let extra = &rest[3..rest.len() - 1];
    let mut command = Command::new(legion);
    command
        .arg(subcommand)
        .env("LEGION_HOME", legion_home)
        .env("HOME", home)
        .env("USERPROFILE", home)
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("XDG_STATE_HOME");
    match mode.as_str() {
        "node" => exit_of(command.status().unwrap()),
        // Prints what a login script might before the real thing starts.
        "noise" => {
            let mut stdout = std::io::stdout().lock();
            writeln!(stdout, "Welcome to pi!").unwrap();
            writeln!(stdout, "{{\"motd\": \"not a reply\"").unwrap();
            writeln!(stdout, "{{\"banner\": true}}").unwrap();
            stdout.flush().unwrap();
            drop(stdout);
            exit_of(command.status().unwrap())
        }
        // The first `put` stream is cut after `limit` bytes of the file, as a dropped link would.
        "cut" if subcommand == "stream" => {
            let (marker, limit) = (Path::new(&extra[0]), extra[1].parse::<u64>().unwrap());
            let mut input = BufReader::new(std::io::stdin().lock());
            let mut header = Vec::new();
            input.read_until(b'\n', &mut header).unwrap();
            let cut = header.starts_with(b"{\"op\":\"put\"") && !marker.exists();
            let mut child = command.stdin(Stdio::piped()).spawn().unwrap();
            let mut child_in = child.stdin.take().unwrap();
            child_in.write_all(&header).unwrap();
            if cut {
                fs::write(marker, b"cut").unwrap();
                let _ = std::io::copy(&mut input.take(limit), &mut child_in);
                drop(child_in);
                let _ = child.wait();
                return ExitCode::from(1);
            }
            let _ = std::io::copy(&mut input, &mut child_in);
            drop(child_in);
            exit_of(child.wait().unwrap())
        }
        // Passes a stream's input on slowly, so a test can act while it is under way.
        "slow" if subcommand == "stream" => {
            let mut input = std::io::stdin().lock();
            let mut child = command.stdin(Stdio::piped()).spawn().unwrap();
            let mut child_in = child.stdin.take().unwrap();
            let mut buf = [0u8; 16 * 1024];
            while let Ok(read) = input.read(&mut buf) {
                if read == 0 || child_in.write_all(&buf[..read]).is_err() {
                    break;
                }
                let _ = child_in.flush();
                std::thread::sleep(Duration::from_millis(20));
            }
            drop(child_in);
            exit_of(child.wait().unwrap())
        }
        // A source that does not keep to the protocol, in the way `extra[0]` names.
        "evil" if subcommand == "stream" => evil(&extra[0], Path::new(home), command),
        "cut" | "slow" | "evil" => exit_of(command.status().unwrap()),
        other => {
            eprintln!("unknown helper mode {other}");
            ExitCode::from(2)
        }
    }
}

/// Runs `legion stream` behind a source that misbehaves: it rewrites the header it passes on and
/// the replies it passes back.
fn evil(behaviour: &str, home: &Path, mut command: Command) -> ExitCode {
    let mut input = BufReader::new(std::io::stdin());
    let mut line = String::new();
    input.read_line(&mut line).unwrap();
    let mut header: Value = serde_json::from_str(&line).unwrap();
    let op = header["op"].as_str().unwrap().to_owned();
    match (behaviour, op.as_str()) {
        // Packs more than it was asked for: files it never listed.
        ("extra-entries", "pack") => header["names"] = json!(["readme.txt", ".env", "other.txt"]),
        ("oversized", "pack") => header["names"] = json!(["readme.txt", "big.bin"]),
        // Its file is edited in place after it was listed and before it is packed.
        ("mutate", "pack") => fs::write(home.join("export/readme.txt"), b"READ ME").unwrap(),
        // An unpack that, like a real one, has a temporary file to remove once its input ends,
        // and takes a moment to do it: killed at once, it leaves the file behind.
        ("slow-cleanup", "unpack") => {
            let root = home.join(header["root"].as_str().unwrap().trim_start_matches("~/"));
            fs::create_dir_all(&root).unwrap();
            let temporary = root.join(".legion-unpack-1-0.tmp");
            fs::write(&temporary, b"half").unwrap();
            println!("{}", json!({"ready": true}));
            std::io::stdout().flush().unwrap();
            let _ = std::io::copy(&mut input, &mut std::io::sink());
            std::thread::sleep(Duration::from_millis(300));
            fs::remove_file(&temporary).unwrap();
            return ExitCode::from(1);
        }
        // Fails as soon as it is asked for an archive.
        ("fail-pack", "pack") => {
            let error =
                json!({"code": "failed", "message": "The disk went away.", "machine": "broken"});
            println!("{}", json!({ "error": error }));
            return ExitCode::from(1);
        }
        _ => {}
    }
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut child_in = child.stdin.take().unwrap();
    writeln!(child_in, "{header}").unwrap();
    std::thread::spawn(move || {
        let _ = std::io::copy(&mut input, &mut child_in);
    });
    let mut replies = BufReader::new(child.stdout.take().unwrap());
    let mut out = std::io::stdout().lock();
    match (behaviour, op.as_str()) {
        // Lists only readme.txt, whatever is there.
        ("extra-entries" | "oversized", "manifest") => {
            let mut kept = 0;
            for line in replies.lines() {
                let mut reply: Value = serde_json::from_str(&line.unwrap()).unwrap();
                if reply.get("done").is_some() {
                    reply["entries"] = json!(kept);
                } else if reply["path"] != "readme.txt" {
                    continue;
                } else {
                    kept += 1;
                }
                writeln!(out, "{reply}").unwrap();
            }
        }
        // Speaks an older protocol, whose archive came without a line before it.
        ("old", "pack") => {
            let mut line = String::new();
            replies.read_line(&mut line).unwrap();
            std::io::copy(&mut replies, &mut out).unwrap();
        }
        // Says its file system ignores letter case, as macOS and Windows usually do.
        ("caseless", "stat") => {
            let mut line = String::new();
            replies.read_line(&mut line).unwrap();
            let mut reply: Value = serde_json::from_str(&line).unwrap();
            if reply.get("kind").is_some() {
                reply["case_insensitive"] = json!(true);
            }
            writeln!(out, "{reply}").unwrap();
            std::io::copy(&mut replies, &mut out).unwrap();
        }
        // Stops half way through its archive and fails.
        ("truncate", "pack") => {
            let mut taken = Vec::new();
            replies.read_until(b'\n', &mut taken).unwrap();
            out.write_all(&taken).unwrap();
            let mut archive = vec![0u8; 300 * 1024];
            replies.read_exact(&mut archive).unwrap();
            out.write_all(&archive).unwrap();
            out.flush().unwrap();
            let _ = child.kill();
            let _ = child.wait();
            return ExitCode::from(1);
        }
        // Sends more than the size it announced.
        ("extra-bytes", "get") => {
            std::io::copy(&mut replies, &mut out).unwrap();
            out.write_all(b"and then some").unwrap();
        }
        _ => {
            std::io::copy(&mut replies, &mut out).unwrap();
        }
    }
    out.flush().unwrap();
    exit_of(child.wait().unwrap())
}

fn exit_of(status: std::process::ExitStatus) -> ExitCode {
    ExitCode::from(status.code().unwrap_or(1).clamp(0, 255) as u8)
}

/// A node with a name, in its own temporary directory.
fn node(name: &str) -> Home {
    let home = Home::new();
    home.write_config("node.toml", &format!("name = \"{name}\"\n"));
    home
}

fn legion_home_of(home: &Home) -> PathBuf {
    home.state().parent().unwrap().to_path_buf()
}

fn config_of(home: &Home) -> PathBuf {
    legion_home_of(home).join("config")
}

/// `fleet.toml` on `from`, reaching each of `machines` through the helper in `mode`.
fn fleet(from: &Home, machines: &[(&str, &Home, &str, Vec<String>)]) {
    let mut text = String::new();
    for (name, target, mode, extra) in machines {
        let mut argv = vec![
            std::env::current_exe()
                .unwrap()
                .to_string_lossy()
                .into_owned(),
            HELPER_FLAG.to_owned(),
            (*mode).to_owned(),
            LEGION_BIN.to_owned(),
            legion_home_of(target).to_string_lossy().into_owned(),
            target.home().to_string_lossy().into_owned(),
        ];
        argv.extend(extra.iter().cloned());
        argv.push("rpc".to_owned());
        let quoted: Vec<String> = argv
            .iter()
            .map(|arg| serde_json::to_string(arg).unwrap())
            .collect();
        text.push_str(&format!(
            "[machines.{name}]\ncommand = [{}]\n\n",
            quoted.join(", ")
        ));
    }
    from.write_config("fleet.toml", &text);
}

/// Runs the binary on `home` and gives up, killing it, after `PATIENCE`.
fn run_bounded(home: &Home, args: &[&str]) -> Run {
    let mut child = home
        .command(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdout = child.stdout.take().unwrap();
    let mut stderr = child.stderr.take().unwrap();
    let out = std::thread::spawn(move || {
        let mut text = String::new();
        let _ = stdout.read_to_string(&mut text);
        text
    });
    let err = std::thread::spawn(move || {
        let mut text = String::new();
        let _ = stderr.read_to_string(&mut text);
        text
    });
    let deadline = Instant::now() + PATIENCE;
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            panic!("legion {args:?} did not finish within {PATIENCE:?}");
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    Run {
        code: status.code().unwrap_or(-1),
        stdout: out.join().unwrap(),
        stderr: err.join().unwrap(),
    }
}

/// `fs.transfer` with these arguments, on `home`.
fn transfer(home: &Home, args: Value, extra: &[&str]) -> Run {
    let text = args.to_string();
    let mut line = vec!["fs.transfer", "--args", text.as_str()];
    line.extend_from_slice(extra);
    run_bounded(home, &line)
}

fn ok(run: &Run) -> Value {
    assert_eq!(
        run.code, 0,
        "stdout: {}\nstderr: {}",
        run.stdout, run.stderr
    );
    run.json()
}

fn refused(run: &Run) -> Value {
    assert_eq!(
        run.code, 1,
        "stdout: {}\nstderr: {}",
        run.stdout, run.stderr
    );
    run.json()
}

/// Bytes that do not repeat in any short pattern, the same every run.
fn data(len: usize, seed: u64) -> Vec<u8> {
    let mut state = seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
    (0..len)
        .map(|_| {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            (state >> 33) as u8
        })
        .collect()
}

/// Every file under `root`, by its relative path, with its bytes; directories with `None`.
fn tree_of(root: &Path) -> Vec<(String, Option<Vec<u8>>)> {
    let mut out = Vec::new();
    fn walk(root: &Path, dir: &Path, out: &mut Vec<(String, Option<Vec<u8>>)>) {
        let mut entries: Vec<_> = fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect();
        entries.sort();
        for path in entries {
            let rel = path
                .strip_prefix(root)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/");
            if path.is_dir() {
                out.push((rel, None));
                walk(root, &path, out);
            } else {
                out.push((rel, Some(fs::read(&path).unwrap())));
            }
        }
    }
    walk(root, root, &mut out);
    out
}

fn stream_lines(home: &Home) -> Vec<Value> {
    home.audit()
        .into_iter()
        .filter(|entry| entry["door"] == "stream")
        .collect()
}

fn files_go_every_way_and_land_inside_a_directory() {
    let laptop = node("laptop");
    let pi = node("pi");
    let tower = node("tower");
    // pi's pipe prints a banner first, which the stream reader skips.
    fleet(
        &laptop,
        &[
            ("pi", &pi, "noise", vec![]),
            ("tower", &tower, "node", vec![]),
        ],
    );
    let report = data(300_000, 1);
    fs::write(laptop.home().join("report.bin"), &report).unwrap();

    // local -> remote, into an existing directory under its own name
    let sent = ok(&transfer(
        &laptop,
        json!({"from": "~/report.bin", "to": "pi:~/projects"}),
        &[],
    ));
    assert_eq!(sent["kind"], "file");
    assert_eq!(sent["files_sent"], 1);
    assert_eq!(sent["bytes_sent"], 300_000);
    assert_eq!(sent["to"], "pi:~/projects/report.bin");
    assert!(sent.get("resumed_from").is_none(), "{sent}");
    assert_eq!(
        fs::read(pi.home().join("projects/report.bin")).unwrap(),
        report
    );

    // The same again is already there.
    let again = ok(&transfer(
        &laptop,
        json!({"from": "~/report.bin", "to": "pi:~/projects"}),
        &[],
    ));
    assert_eq!(again["files_sent"], 0);
    assert_eq!(again["files_unchanged"], 1);

    // remote -> local, naming the destination itself, with parents made on the way
    ok(&transfer(
        &laptop,
        json!({"from": "pi:~/projects/report.bin", "to": "~/back/again.bin"}),
        &[],
    ));
    assert_eq!(
        fs::read(laptop.home().join("back/again.bin")).unwrap(),
        report
    );

    // remote -> remote, relayed through this node; a path ending in / is a directory
    let relayed = ok(&transfer(
        &laptop,
        json!({"from": "pi:~/projects/report.bin", "to": "tower:~/inbox/"}),
        &[],
    ));
    assert_eq!(relayed["to"], "tower:~/inbox/report.bin");
    assert_eq!(
        fs::read(tower.home().join("inbox/report.bin")).unwrap(),
        report
    );

    // Each end authorised and recorded its own part.
    let pi_ops: Vec<String> = stream_lines(&pi)
        .iter()
        .map(|e| e["tool"].as_str().unwrap().to_owned())
        .collect();
    for op in ["stream.stat", "stream.put", "stream.get"] {
        assert!(
            pi_ops.iter().any(|tool| tool == op),
            "{op} missing from {pi_ops:?}"
        );
    }
    assert!(
        stream_lines(&tower)
            .iter()
            .any(|e| e["tool"] == "stream.put" && e["principal"] == "owner")
    );
    // The local end is served through the same endpoint and audited the same way.
    let local = stream_lines(&laptop);
    assert!(
        local
            .iter()
            .any(|e| e["tool"] == "stream.get" && e["target"] == "laptop"),
        "{local:?}"
    );
    assert!(
        laptop
            .audit()
            .iter()
            .any(|e| e["tool"] == "fs.transfer" && e["door"] == "cli")
    );

    // legion cp is the same call.
    let cp = run_bounded(
        &laptop,
        &["cp", "~/report.bin", "tower:~/inbox/cp.bin", "--json"],
    );
    assert_eq!(ok(&cp)["files_sent"], 1);
    assert_eq!(fs::read(tower.home().join("inbox/cp.bin")).unwrap(), report);
}

fn a_tree_arrives_whole_and_a_second_run_sends_nothing() {
    let laptop = node("laptop");
    let pi = node("pi");
    fleet(&laptop, &[("pi", &pi, "node", vec![])]);
    let src = laptop.home().join("projects/app");
    let names = [
        "with space.txt",
        "ünï cödé.txt",
        "-leading-dash",
        "semi;colon & amp",
        "it's.txt",
        "#hash",
        "brackets [1].txt",
        "emoji 🦀.txt",
        "a b/c d/e.txt",
        "deep/er/est/leaf.txt",
    ];
    for (at, name) in names.iter().enumerate() {
        let path = src.join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, format!("file {at}")).unwrap();
    }
    fs::create_dir_all(src.join("empty/inner-empty")).unwrap();
    fs::write(src.join("zero.bin"), b"").unwrap();
    let big = data(9 * 1024 * 1024 + 123, 7);
    fs::write(src.join("deep/big.bin"), &big).unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink("zero.bin", src.join("link")).unwrap();

    let first = ok(&transfer(
        &laptop,
        json!({"from": "~/projects/app", "to": "pi:~/copy"}),
        &[],
    ));
    assert_eq!(first["kind"], "tree");
    assert_eq!(first["files_sent"], names.len() + 2, "{first}");
    assert_eq!(
        first["bytes_sent"],
        big.len()
            + names
                .iter()
                .enumerate()
                .map(|(at, _)| format!("file {at}").len())
                .sum::<usize>()
    );
    assert_eq!(first["skipped"], json!([]), "{first}");
    let dst = pi.home().join("copy");
    assert_eq!(tree_of(&dst), tree_of(&src));
    #[cfg(unix)]
    assert_eq!(
        fs::read_link(dst.join("link")).unwrap(),
        Path::new("zero.bin")
    );

    let second = ok(&transfer(
        &laptop,
        json!({"from": "~/projects/app", "to": "pi:~/copy"}),
        &[],
    ));
    assert_eq!(second["files_sent"], 0, "{second}");
    assert_eq!(second["bytes_sent"], 0);
    assert_eq!(second["files_unchanged"], names.len() + 2);
    // Nothing was packed: only listings happened.
    let packs = stream_lines(&laptop)
        .iter()
        .filter(|e| e["tool"] == "stream.pack")
        .count();
    assert_eq!(packs, 1);

    // Back the other way, into a tree that is already partly there.
    fs::write(dst.join("new on pi.txt"), b"fresh").unwrap();
    let back = ok(&transfer(
        &laptop,
        json!({"from": "pi:~/copy", "to": "~/projects/app"}),
        &[],
    ));
    assert_eq!(back["files_sent"], 1, "{back}");
    assert_eq!(fs::read(src.join("new on pi.txt")).unwrap(), b"fresh");
}

fn delete_checksum_and_exclude_decide_what_changes() {
    let laptop = node("laptop");
    let pi = node("pi");
    fleet(&laptop, &[("pi", &pi, "node", vec![])]);
    let src = laptop.home().join("site");
    fs::create_dir_all(src.join("old/sub")).unwrap();
    fs::create_dir_all(src.join("target/debug")).unwrap();
    fs::write(src.join("index.html"), b"<h1>one</h1>").unwrap();
    fs::write(src.join("old/sub/stale.txt"), b"stale").unwrap();
    fs::write(src.join("build.log"), b"log").unwrap();
    fs::write(src.join("target/debug/app"), b"binary").unwrap();
    let exclude = json!(["*.log", "target/"]);

    let first = ok(&transfer(
        &laptop,
        json!({"from": "~/site", "to": "pi:~/site", "exclude": exclude}),
        &[],
    ));
    assert_eq!(first["files_sent"], 2, "{first}");
    let dst = pi.home().join("site");
    assert!(!dst.join("build.log").exists());
    assert!(!dst.join("target").exists());
    // What the destination has that matches an exclude is its own, and is never deleted.
    fs::write(dst.join("keep.log"), b"theirs").unwrap();

    // Without delete, what the source dropped stays.
    fs::remove_dir_all(src.join("old")).unwrap();
    let kept = ok(&transfer(
        &laptop,
        json!({"from": "~/site", "to": "pi:~/site", "exclude": exclude}),
        &[],
    ));
    assert_eq!(kept["deleted"], 0);
    assert!(dst.join("old/sub/stale.txt").exists());
    let removed = ok(&transfer(
        &laptop,
        json!({"from": "~/site", "to": "pi:~/site", "exclude": exclude, "delete": true}),
        &[],
    ));
    assert_eq!(removed["deleted"], 3, "{removed}");
    assert!(!dst.join("old").exists());
    assert!(dst.join("keep.log").exists());
    assert!(
        stream_lines(&pi)
            .iter()
            .any(|e| e["tool"] == "stream.delete" && e["outcome"] == "ok")
    );

    // Same size and time, different bytes: only checksum notices.
    let index = src.join("index.html");
    let time = fs::metadata(&index).unwrap().modified().unwrap();
    fs::write(&index, b"<h1>two</h1>").unwrap();
    fs::File::options()
        .write(true)
        .open(&index)
        .unwrap()
        .set_modified(time)
        .unwrap();
    let by_time = ok(&transfer(
        &laptop,
        json!({"from": "~/site", "to": "pi:~/site", "exclude": exclude}),
        &[],
    ));
    assert_eq!(by_time["files_sent"], 0, "{by_time}");
    assert_eq!(fs::read(dst.join("index.html")).unwrap(), b"<h1>one</h1>");
    let by_hash = ok(&transfer(
        &laptop,
        json!({"from": "~/site", "to": "pi:~/site", "exclude": exclude, "checksum": true}),
        &[],
    ));
    assert_eq!(by_hash["files_sent"], 1, "{by_hash}");
    assert_eq!(fs::read(dst.join("index.html")).unwrap(), b"<h1>two</h1>");
}

fn a_dry_run_changes_nothing() {
    let laptop = node("laptop");
    let pi = node("pi");
    fleet(&laptop, &[("pi", &pi, "node", vec![])]);
    let src = laptop.home().join("docs");
    fs::create_dir_all(src.join("a")).unwrap();
    fs::write(src.join("a/one.txt"), b"1").unwrap();
    fs::write(src.join("two.txt"), b"22").unwrap();
    fs::create_dir_all(pi.home().join("docs")).unwrap();
    fs::write(pi.home().join("docs/extra.txt"), b"x").unwrap();
    let before = tree_of(&pi.home());

    let plan = ok(&transfer(
        &laptop,
        json!({"from": "~/docs", "to": "pi:~/docs", "delete": true, "dry_run": true}),
        &[],
    ));
    assert_eq!(plan["dry_run"], true);
    assert_eq!(plan["files_sent"], 2);
    assert_eq!(plan["bytes_sent"], 3);
    assert_eq!(plan["deleted"], 1);
    assert_eq!(plan["would_send"], json!(["a", "a/one.txt", "two.txt"]));
    assert_eq!(plan["would_delete"], json!(["extra.txt"]));
    assert_eq!(tree_of(&pi.home()), before);
    let file = ok(&transfer(
        &laptop,
        json!({"from": "~/docs/two.txt", "to": "pi:~/new/two.txt", "dry_run": true}),
        &[],
    ));
    assert_eq!(file["files_sent"], 1);
    assert!(!pi.home().join("new").exists());
    let changing = ["stream.put", "stream.unpack", "stream.delete"];
    assert!(
        stream_lines(&pi)
            .iter()
            .all(|e| !changing.contains(&e["tool"].as_str().unwrap()))
    );

    let cp = run_bounded(
        &laptop,
        &[
            "cp",
            "--dry-run",
            "~/docs",
            "pi:~/docs",
            "--delete",
            "--json",
        ],
    );
    assert_eq!(ok(&cp)["would_delete"], json!(["extra.txt"]));
    assert_eq!(tree_of(&pi.home()), before);
}

fn an_interrupted_file_resumes_where_it_stopped() {
    let laptop = node("laptop");
    let pi = node("pi");
    let marker = laptop.home().join("cut-marker");
    let limit = 1_000_000u64;
    fleet(
        &laptop,
        &[(
            "pi",
            &pi,
            "cut",
            vec![marker.to_string_lossy().into_owned(), limit.to_string()],
        )],
    );
    let big = data(3 * 1024 * 1024, 3);
    fs::write(laptop.home().join("big.bin"), &big).unwrap();

    let cut = refused(&transfer(
        &laptop,
        json!({"from": "~/big.bin", "to": "pi:~/big.bin"}),
        &[],
    ));
    assert_eq!(cut["machine"], "pi", "{cut}");
    assert!(marker.exists());
    assert!(!pi.home().join("big.bin").exists());
    let partial = pi.home().join("big.bin.legion-part");
    assert_eq!(fs::metadata(&partial).unwrap().len(), limit);

    let dry = ok(&transfer(
        &laptop,
        json!({"from": "~/big.bin", "to": "pi:~/big.bin", "dry_run": true}),
        &[],
    ));
    assert_eq!(dry["resumed_from"], limit);
    let resumed = ok(&transfer(
        &laptop,
        json!({"from": "~/big.bin", "to": "pi:~/big.bin"}),
        &[],
    ));
    assert_eq!(resumed["resumed_from"], limit, "{resumed}");
    assert_eq!(resumed["bytes_sent"], big.len() as u64 - limit);
    assert_eq!(fs::read(pi.home().join("big.bin")).unwrap(), big);
    assert!(!partial.exists());

    // A partial file that is not a prefix of the source (it changed since) is dropped once its
    // hash fails, and the file is sent whole.
    fs::write(pi.home().join("other.bin.legion-part"), data(1000, 99)).unwrap();
    fs::write(laptop.home().join("other.bin"), data(5000, 4)).unwrap();
    let restarted = ok(&transfer(
        &laptop,
        json!({"from": "~/other.bin", "to": "pi:~/other.bin"}),
        &[],
    ));
    assert!(restarted.get("resumed_from").is_none(), "{restarted}");
    assert_eq!(
        fs::read(pi.home().join("other.bin")).unwrap(),
        data(5000, 4)
    );
}

fn a_cancelled_transfer_leaves_nothing_half_written_under_a_final_name() {
    let laptop = node("laptop");
    let pi = node("pi");
    fleet(&laptop, &[("pi", &pi, "slow", vec![])]);
    fs::write(laptop.home().join("huge.bin"), data(24 * 1024 * 1024, 5)).unwrap();
    let tree = laptop.home().join("tree");
    fs::create_dir_all(&tree).unwrap();
    for at in 0..4 {
        fs::write(
            tree.join(format!("part{at}.bin")),
            data(6 * 1024 * 1024, at),
        )
        .unwrap();
    }

    for (id, from, to) in [
        ("file", "~/huge.bin", "pi:~/huge.bin"),
        ("tree", "~/tree", "pi:~/tree"),
    ] {
        let mut rpc = laptop.session(&[]);
        rpc.send_json(&json!({"jsonrpc": "2.0", "id": id, "method": "call", "params": {"tool": "fs.transfer", "args": {"from": from, "to": to}}}));
        // Wait until bytes are moving.
        loop {
            let message = rpc.next();
            if message["method"] == "event" && message["params"]["kind"] == "progress" {
                assert!(message["params"]["data"]["bytes_total"].as_u64().unwrap() > 0);
                break;
            }
            assert!(
                message.get("id").is_none(),
                "the transfer ended before it could be cancelled: {message}"
            );
        }
        let started = Instant::now();
        rpc.send_json(
            &json!({"jsonrpc": "2.0", "id": "stop", "method": "cancel", "params": {"id": id}}),
        );
        let (reply, _) = rpc.reply_to(&json!(id));
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "{:?}",
            started.elapsed()
        );
        let error = &reply["error"]["data"];
        assert_eq!(error["code"], "cancelled", "{reply}");
        assert!(
            error["details"]["bytes_sent"].as_u64().unwrap() > 0,
            "{reply}"
        );
        assert_eq!(error["details"]["deleted"], 0, "{reply}");
        assert!(
            error["message"]
                .as_str()
                .unwrap()
                .contains("not what the destination confirmed"),
            "{reply}"
        );
        assert_eq!(rpc.finish(), 0);
    }
    // Both ends were waited for before the reply. Whatever has its final name by then (a part
    // that finished before the cancel came) is whole and right; nothing is half written under one.
    let names: Vec<(PathBuf, String)> = fs::read_dir(pi.home())
        .unwrap()
        .chain(fs::read_dir(pi.home().join("tree")).into_iter().flatten())
        .map(|entry| {
            let entry = entry.unwrap();
            (
                entry.path(),
                entry.file_name().to_string_lossy().into_owned(),
            )
        })
        .collect();
    for (path, name) in &names {
        if name == "huge.bin" {
            assert_eq!(fs::read(path).unwrap(), data(24 * 1024 * 1024, 5), "{name}");
        }
        if let Some(at) = name
            .strip_prefix("part")
            .and_then(|rest| rest.strip_suffix(".bin"))
        {
            let seed: u64 = at.parse().unwrap();
            assert_eq!(
                fs::read(path).unwrap(),
                data(6 * 1024 * 1024, seed),
                "{name}"
            );
        }
    }
    let audit = laptop.audit();
    assert!(
        audit
            .iter()
            .any(|e| e["tool"] == "fs.transfer" && e["outcome"] == "cancelled"),
        "{audit:?}"
    );
}

const POLICY: &str = r#"
[principals."agent:x"]
role = "confined"

[principals.reader]
role = "observer"

[principals.writer]
role = "keeper"

[roles.confined]
allow = ["effect:read", "effect:write"]
paths = ["~/projects"]

[roles.observer]
allow = ["effect:read"]

[roles.keeper]
allow = ["effect:read", "effect:write"]
"#;

fn each_end_applies_its_own_policy_and_says_which_machine_refused() {
    let laptop = node("laptop");
    let pi = node("pi");
    fleet(&laptop, &[("pi", &pi, "node", vec![])]);
    // On this node every principal may transfer anything; pi decides for itself.
    laptop.write_config(
        "policy.toml",
        "[principals.\"agent:x\"]\nrole = \"mover\"\n[principals.reader]\nrole = \"mover\"\n[principals.writer]\nrole = \"mover\"\n[roles.mover]\nallow = [\"effect:read\", \"effect:write\"]\n",
    );
    pi.write_config("policy.toml", POLICY);
    fs::write(laptop.home().join("a.txt"), data(2 * 1024 * 1024, 8)).unwrap();
    fs::create_dir_all(pi.home().join("secret")).unwrap();
    fs::write(pi.home().join("secret/key.txt"), b"k").unwrap();

    // The destination confines agent:x to ~/projects.
    let error = refused(&transfer(
        &laptop,
        json!({"from": "~/a.txt", "to": "pi:~/outside.txt"}),
        &["--as", "agent:x"],
    ));
    assert_eq!(error["code"], "denied", "{error}");
    assert_eq!(error["machine"], "pi");
    assert!(
        error["message"].as_str().unwrap().contains("confines"),
        "{error}"
    );
    assert!(!pi.home().join("outside.txt").exists());
    ok(&transfer(
        &laptop,
        json!({"from": "~/a.txt", "to": "pi:~/projects/inside.txt"}),
        &["--as", "agent:x"],
    ));
    let put = stream_lines(&pi)
        .into_iter()
        .find(|e| e["tool"] == "stream.put")
        .unwrap();
    assert_eq!(put["principal"], "agent:x");

    // A role without write may look but not put.
    let error = refused(&transfer(
        &laptop,
        json!({"from": "~/a.txt", "to": "pi:~/projects/r.txt"}),
        &["--as", "reader"],
    ));
    assert_eq!(error["code"], "denied");
    assert_eq!(error["machine"], "pi");
    assert!(
        error["message"].as_str().unwrap().contains("fs.write"),
        "{error}"
    );
    let error = refused(&transfer(
        &laptop,
        json!({"from": "~/projects", "to": "pi:~/projects/tree"}),
        &["--as", "reader"],
    ));
    assert_eq!(
        (error["code"].as_str(), error["machine"].as_str()),
        (Some("denied"), Some("pi")),
        "{error}"
    );

    // The source refuses too: agent:x may not read outside its paths on pi.
    let error = refused(&transfer(
        &laptop,
        json!({"from": "pi:~/secret/key.txt", "to": "~/key.txt"}),
        &["--as", "agent:x"],
    ));
    assert_eq!(
        (error["code"].as_str(), error["machine"].as_str()),
        (Some("denied"), Some("pi")),
        "{error}"
    );
    assert!(!laptop.home().join("key.txt").exists());
    let error = refused(&transfer(
        &laptop,
        json!({"from": "pi:~/secret", "to": "~/stolen"}),
        &["--as", "agent:x"],
    ));
    assert_eq!(error["machine"], "pi", "{error}");

    // Legion's own configuration is admin's, whatever a role may write elsewhere.
    let target = format!(
        "pi:{}",
        config_of(&pi).join("policy.toml").to_string_lossy()
    );
    let error = refused(&transfer(
        &laptop,
        json!({"from": "~/a.txt", "to": target}),
        &["--as", "writer"],
    ));
    assert_eq!(
        (error["code"].as_str(), error["machine"].as_str()),
        (Some("denied"), Some("pi")),
        "{error}"
    );
    assert!(
        error["message"].as_str().unwrap().contains("configuration"),
        "{error}"
    );
    assert_eq!(
        fs::read_to_string(config_of(&pi).join("policy.toml")).unwrap(),
        POLICY
    );

    // This node's own policy holds for its local end the same way: agent:x may transfer here,
    // but not write a file.
    laptop.write_config(
        "policy.toml",
        "[principals.\"agent:x\"]\nrole = \"fetcher\"\n[roles.fetcher]\nallow = [\"effect:read\", \"fs.transfer\"]\n",
    );
    let error = refused(&transfer(
        &laptop,
        json!({"from": "pi:~/projects/inside.txt", "to": "~/here.txt"}),
        &["--as", "agent:x"],
    ));
    assert_eq!(
        (error["code"].as_str(), error["machine"].as_str()),
        (Some("denied"), Some("laptop")),
        "{error}"
    );
    assert!(!laptop.home().join("here.txt").exists());
    let local = stream_lines(&laptop);
    assert!(
        local
            .iter()
            .any(|e| e["outcome"] == "denied" && e["principal"] == "agent:x"),
        "{local:?}"
    );
}

fn a_stream_that_fails_early_never_leaves_the_other_side_waiting() {
    let pi = node("pi");
    pi.write_config("policy.toml", POLICY);
    // A receiver that refuses answers and exits while the sender still has megabytes to send.
    for header in [
        json!({"op": "put", "path": "~/projects/x.bin", "size": 64 << 20, "hash": format!("blake3:{}", "0".repeat(64)), "as": "reader", "protocol": 1}),
        json!({"op": "unpack", "root": "~/projects/x", "names": [], "as": "reader", "protocol": 1}),
    ] {
        let mut child = pi
            .command(&["stream"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let mut stdin = child.stdin.take().unwrap();
        let writer = std::thread::spawn(move || {
            writeln!(stdin, "{header}").unwrap();
            let chunk = vec![0u8; 1 << 20];
            for _ in 0..64 {
                if stdin.write_all(&chunk).is_err() {
                    return false;
                }
            }
            true
        });
        let started = Instant::now();
        let output = child.wait_with_output().unwrap();
        assert!(started.elapsed() < PATIENCE);
        assert_eq!(output.status.code(), Some(1));
        let reply: Value =
            serde_json::from_slice(output.stdout.split(|b| *b == b'\n').next().unwrap()).unwrap();
        assert_eq!(reply["error"]["code"], "denied", "{reply}");
        assert_eq!(reply["error"]["machine"], "pi");
        assert!(String::from_utf8_lossy(&output.stderr).contains("legion stream:"));
        assert!(
            !writer.join().unwrap(),
            "the sender should have found the pipe closed"
        );
    }
    // A source that cannot send says so at once.
    let run = run_bounded(&pi, &["stream"]);
    assert_eq!(run.code, 1);
    let missing = pi
        .command(&["stream"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut missing = missing;
    writeln!(
        missing.stdin.take().unwrap(),
        "{}",
        json!({"op": "get", "path": "~/none.bin", "protocol": 1})
    )
    .unwrap();
    let output = missing.wait_with_output().unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&output.stdout).contains("not_found"));

    // Through fs.transfer: a destination that refuses while the source is ready to send ends the
    // call promptly, with both pipes closed.
    let laptop = node("laptop");
    fleet(&laptop, &[("pi", &pi, "node", vec![])]);
    laptop.write_config("policy.toml", "[principals.reader]\nrole = \"mover\"\n[roles.mover]\nallow = [\"effect:read\", \"effect:write\"]\n");
    fs::write(laptop.home().join("large.bin"), data(32 * 1024 * 1024, 11)).unwrap();
    let started = Instant::now();
    let error = refused(&transfer(
        &laptop,
        json!({"from": "~/large.bin", "to": "pi:~/projects/large.bin"}),
        &["--as", "reader"],
    ));
    assert_eq!(error["machine"], "pi");
    assert!(
        started.elapsed() < Duration::from_secs(20),
        "{:?}",
        started.elapsed()
    );
    // And a source that refuses touches nothing at the destination.
    let error = refused(&transfer(
        &laptop,
        json!({"from": "pi:~/none.bin", "to": "~/none.bin"}),
        &[],
    ));
    assert_eq!(
        (error["code"].as_str(), error["machine"].as_str()),
        (Some("not_found"), Some("pi")),
        "{error}"
    );
}

fn ends_are_parsed_and_unknown_machines_named() {
    let laptop = node("laptop");
    let pi = node("pi");
    fleet(&laptop, &[("pi", &pi, "node", vec![])]);
    fs::write(laptop.home().join("a.txt"), b"a").unwrap();

    let error = refused(&transfer(
        &laptop,
        json!({"from": "~/a.txt", "to": "tower:~/a.txt"}),
        &[],
    ));
    assert_eq!(error["code"], "not_found");
    assert!(error["hint"].as_str().unwrap().contains("pi"), "{error}");
    let error = refused(&transfer(
        &laptop,
        json!({"from": "~/a.txt", "to": "x:a.txt"}),
        &[],
    ));
    assert_eq!(error["code"], "invalid_args");
    assert!(error["hint"].as_str().unwrap().contains("drive"), "{error}");
    // This node's own name is a local path; a relative one is refused by the node that owns it.
    ok(&transfer(
        &laptop,
        json!({"from": "laptop:~/a.txt", "to": "~/b.txt"}),
        &[],
    ));
    assert_eq!(fs::read(laptop.home().join("b.txt")).unwrap(), b"a");
    let error = refused(&transfer(
        &laptop,
        json!({"from": "~/a.txt", "to": "pi:relative/a.txt"}),
        &[],
    ));
    assert_eq!(
        (error["code"].as_str(), error["machine"].as_str()),
        (Some("invalid_args"), Some("pi")),
        "{error}"
    );
    // A drive path is local on every platform: on Windows it is a path, elsewhere a relative one.
    let drive = refused(&transfer(
        &laptop,
        json!({"from": "C:/nowhere/a.txt", "to": "~/c.txt"}),
        &[],
    ));
    assert_eq!(drive["machine"], "laptop", "{drive}");

    let usage = run_bounded(&laptop, &["cp", "only-one"]);
    assert_eq!(usage.code, 2);
    let help = run_bounded(&laptop, &["help", "cp"]);
    assert!(help.stdout.contains("--exclude"), "{}", help.stdout);
}

fn gate_serves_only_the_connectors_commands_pinned_to_its_principal() {
    let pi = node("pi");
    pi.write_config("policy.toml", POLICY);
    fs::write(pi.home().join("projects/a.txt"), b"a").unwrap();
    let gate = |original: Option<&str>| {
        let mut command = pi.command(&["gate", "--as", "agent:x"]);
        command.env_remove("SSH_ORIGINAL_COMMAND");
        if let Some(original) = original {
            command.env("SSH_ORIGINAL_COMMAND", original);
        }
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        command.spawn().unwrap()
    };

    // rpc, as the connector sends it to a Unix machine: calls act as agent:x whatever they ask.
    let rpc = remote_command(RemoteOs::Unix, Subcommand::Rpc, None).unwrap();
    let mut child = gate(Some(&rpc));
    let mut stdin = child.stdin.take().unwrap();
    writeln!(stdin, "{}", json!({"jsonrpc": "2.0", "id": 1, "method": "call", "params": {"tool": "meta.whoami", "args": {}, "as": "owner"}})).unwrap();
    drop(stdin);
    let output = child.wait_with_output().unwrap();
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let reply: Value =
        serde_json::from_slice(output.stdout.split(|b| *b == b'\n').next().unwrap()).unwrap();
    assert_eq!(reply["result"]["principal"], "agent:x", "{reply}");

    // stream, as the connector sends it to a Windows machine.
    let stream = remote_command(RemoteOs::Windows, Subcommand::Stream, None).unwrap();
    for (path, outcome) in [("~/projects/a.txt", "ok"), ("~/.ssh", "denied")] {
        let mut child = gate(Some(&stream));
        writeln!(
            child.stdin.take().unwrap(),
            "{}",
            json!({"op": "stat", "path": path, "as": "owner", "protocol": 1})
        )
        .unwrap();
        let output = child.wait_with_output().unwrap();
        let reply: Value =
            serde_json::from_slice(output.stdout.split(|b| *b == b'\n').next().unwrap()).unwrap();
        if outcome == "ok" {
            assert_eq!(reply["kind"], "file", "{reply}");
        } else {
            assert_eq!(reply["error"]["code"], "denied", "{reply}");
        }
        let line = pi.audit().pop().unwrap();
        assert_eq!(
            (
                line["door"].as_str(),
                line["principal"].as_str(),
                line["outcome"].as_str()
            ),
            (Some("stream"), Some("agent:x"), Some(outcome))
        );
    }

    // Anything else is refused without being run, with one line saying what is accepted.
    let marker = pi.home().join("ran");
    let attack = format!("{rpc}; touch {}", marker.to_string_lossy());
    for original in [
        None,
        Some("sh"),
        Some("legion mcp"),
        Some("legion rpc --as owner"),
        Some(attack.as_str()),
        Some("legion fs.read path=/etc/passwd"),
    ] {
        let output = gate(original).wait_with_output().unwrap();
        assert_eq!(output.status.code(), Some(1), "{original:?}");
        assert!(output.stdout.is_empty(), "{original:?}");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(stderr.lines().count(), 1, "{stderr}");
        assert!(
            stderr.contains("legion rpc") && stderr.contains("legion stream"),
            "{stderr}"
        );
    }
    assert!(!marker.exists());
    let usage = run_bounded(&pi, &["gate"]);
    assert_eq!(usage.code, 2);
    let help = run_bounded(&pi, &["help", "gate"]);
    assert!(
        help.stdout
            .contains("command=\"/home/me/.local/bin/legion gate --as agent:example\",restrict"),
        "{}",
        help.stdout
    );
}

fn a_source_cannot_send_what_was_not_planned() {
    let laptop = node("laptop");
    let evil = node("evil");
    fleet(
        &laptop,
        &[
            ("evil", &evil, "evil", vec!["extra-entries".to_owned()]),
            ("huge", &evil, "evil", vec!["oversized".to_owned()]),
            ("chatty", &evil, "evil", vec!["extra-bytes".to_owned()]),
        ],
    );
    let export = evil.home().join("export");
    fs::create_dir_all(&export).unwrap();
    fs::write(export.join("readme.txt"), b"read me").unwrap();
    fs::write(export.join(".env"), b"SECRET=from-the-source").unwrap();
    fs::write(export.join("other.txt"), b"never listed").unwrap();
    fs::write(export.join("big.bin"), data(2 * 1024 * 1024, 21)).unwrap();
    let work = laptop.home().join("projects/work");
    fs::create_dir_all(&work).unwrap();
    fs::write(work.join(".env"), b"SECRET=mine").unwrap();

    // Entries it never listed are refused, the transfer fails and says which machine sent them.
    let error = refused(&transfer(
        &laptop,
        json!({"from": "evil:~/export", "to": "~/projects/work", "exclude": [".env"]}),
        &[],
    ));
    assert_eq!(
        (error["code"].as_str(), error["machine"].as_str()),
        (Some("failed"), Some("evil")),
        "{error}"
    );
    assert!(error.to_string().contains("other.txt"), "{error}");
    assert_eq!(fs::read(work.join(".env")).unwrap(), b"SECRET=mine");
    assert!(!work.join("other.txt").exists());
    assert_eq!(fs::read(work.join("readme.txt")).unwrap(), b"read me");

    // More bytes than the plan allows are cut off before they arrive.
    let error = refused(&transfer(
        &laptop,
        json!({"from": "huge:~/export", "to": "~/projects/huge"}),
        &[],
    ));
    assert_eq!(
        (error["code"].as_str(), error["machine"].as_str()),
        (Some("failed"), Some("huge")),
        "{error}"
    );
    assert!(!laptop.home().join("projects/huge/big.bin").exists());

    // A file's source may not send more than the size it announced; nothing past it is passed on.
    let error = refused(&transfer(
        &laptop,
        json!({"from": "chatty:~/export/readme.txt", "to": "~/readme.txt"}),
        &[],
    ));
    assert_eq!(
        (error["code"].as_str(), error["machine"].as_str()),
        (Some("failed"), Some("chatty")),
        "{error}"
    );
    // Whatever arrived is exactly what was announced and hashed.
    assert!(
        fs::read(laptop.home().join("readme.txt"))
            .ok()
            .is_none_or(|bytes| bytes == b"read me")
    );
}

fn extra_entries_go_only_once_everything_new_has_arrived() {
    let laptop = node("laptop");
    let broken = node("broken");
    let pi = node("pi");
    fleet(
        &laptop,
        &[
            ("broken", &broken, "evil", vec!["fail-pack".to_owned()]),
            ("pi", &pi, "node", vec![]),
        ],
    );
    let src = broken.home().join("site");
    fs::create_dir_all(&src).unwrap();
    fs::write(src.join("index.html"), b"new").unwrap();
    let dst = laptop.home().join("site");
    fs::create_dir_all(&dst).unwrap();
    fs::write(dst.join("index.html"), b"old!").unwrap();
    fs::write(
        dst.join("extra.txt"),
        b"still needed until the new site is here",
    )
    .unwrap();

    // The source fails before anything new arrives: nothing at the destination is removed.
    let error = refused(&transfer(
        &laptop,
        json!({"from": "broken:~/site", "to": "~/site", "delete": true}),
        &[],
    ));
    assert_eq!(error["machine"], "broken", "{error}");
    assert!(error.to_string().contains("left in place"), "{error}");
    assert!(dst.join("extra.txt").exists());
    assert_eq!(fs::read(dst.join("index.html")).unwrap(), b"old!");

    // When it works, an entry of another kind is replaced and the extras go last.
    let tree = laptop.home().join("tree");
    fs::create_dir_all(tree.join("was-a-file")).unwrap();
    fs::write(tree.join("was-a-file/inside.txt"), b"in").unwrap();
    fs::write(tree.join("kept.txt"), b"kept").unwrap();
    let there = pi.home().join("tree");
    fs::create_dir_all(&there).unwrap();
    fs::write(there.join("was-a-file"), b"a file").unwrap();
    fs::write(there.join("extra.txt"), b"x").unwrap();
    let done = ok(&transfer(
        &laptop,
        json!({"from": "~/tree", "to": "pi:~/tree", "delete": true}),
        &[],
    ));
    assert_eq!(done["deleted"], 2, "{done}");
    assert_eq!(
        fs::read(there.join("was-a-file/inside.txt")).unwrap(),
        b"in"
    );
    assert!(!there.join("extra.txt").exists());
}

fn names_that_differ_only_in_case_are_held_back_where_they_would_collide() {
    let laptop = node("laptop");
    let mac = node("mac");
    let pi = node("pi");
    fleet(
        &laptop,
        &[
            ("mac", &mac, "evil", vec!["caseless".to_owned()]),
            ("pi", &pi, "node", vec![]),
        ],
    );
    // The source must hold both names, which a file system that ignores case cannot.
    fs::write(laptop.home().join("Probe"), b"").unwrap();
    if laptop.home().join("probe").exists() {
        return;
    }
    let src = laptop.home().join("docs");
    fs::create_dir_all(src.join("Notes")).unwrap();
    fs::create_dir_all(src.join("notes")).unwrap();
    fs::write(src.join("Readme.md"), b"upper").unwrap();
    fs::write(src.join("readme.md"), b"lower").unwrap();
    fs::write(src.join("Notes/a.txt"), b"a").unwrap();
    fs::write(src.join("notes/b.txt"), b"b").unwrap();
    fs::write(src.join("plain.txt"), b"plain").unwrap();

    let first = ok(&transfer(
        &laptop,
        json!({"from": "~/docs", "to": "mac:~/docs"}),
        &[],
    ));
    assert_eq!(first["files_sent"], 1, "{first}");
    let conflicts = first["conflicts"].as_array().unwrap();
    let held: Vec<&str> = conflicts
        .iter()
        .map(|c| c["path"].as_str().unwrap())
        .collect();
    for name in ["Readme.md", "readme.md", "Notes", "notes"] {
        assert!(held.contains(&name), "{name} missing from {held:?}");
    }
    assert!(
        conflicts[0]["reason"]
            .as_str()
            .unwrap()
            .contains("cannot hold both"),
        "{first}"
    );
    let dst = mac.home().join("docs");
    assert!(dst.join("plain.txt").exists());
    for name in ["Readme.md", "readme.md", "Notes", "notes"] {
        assert!(!dst.join(name).exists(), "{name} was sent");
    }
    // A second run sends nothing and says the same.
    let second = ok(&transfer(
        &laptop,
        json!({"from": "~/docs", "to": "mac:~/docs"}),
        &[],
    ));
    assert_eq!(second["files_sent"], 0, "{second}");
    assert_eq!(second["conflicts"], first["conflicts"]);

    // A file system that tells them apart takes both.
    let both = ok(&transfer(
        &laptop,
        json!({"from": "~/docs", "to": "pi:~/docs"}),
        &[],
    ));
    assert_eq!(both["files_sent"], 5, "{both}");
    assert_eq!(both["conflicts"], json!([]), "{both}");
}

fn an_end_that_speaks_another_protocol_is_told_apart_from_a_banner() {
    let laptop = node("laptop");
    let old = node("old");
    fleet(&laptop, &[("old", &old, "evil", vec!["old".to_owned()])]);
    fs::create_dir_all(old.home().join("export")).unwrap();
    fs::write(old.home().join("export/readme.txt"), b"read me").unwrap();

    let started = Instant::now();
    let error = refused(&transfer(
        &laptop,
        json!({"from": "old:~/export", "to": "~/export"}),
        &[],
    ));
    assert!(started.elapsed() < Duration::from_secs(20));
    assert_eq!(
        (error["code"].as_str(), error["machine"].as_str()),
        (Some("unsupported"), Some("old")),
        "{error}"
    );
    assert!(
        error["hint"].as_str().unwrap().contains("Update legion"),
        "{error}"
    );
    assert!(!laptop.home().join("export/readme.txt").exists());

    // Asked for a protocol it does not speak, a node says which it does.
    let mut child = old
        .command(&["stream"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    writeln!(
        child.stdin.take().unwrap(),
        "{}",
        json!({"op": "stat", "path": "~", "protocol": 2})
    )
    .unwrap();
    let output = child.wait_with_output().unwrap();
    assert_eq!(output.status.code(), Some(1));
    let reply: Value =
        serde_json::from_slice(output.stdout.split(|b| *b == b'\n').next().unwrap()).unwrap();
    assert_eq!(reply["error"]["code"], "unsupported", "{reply}");
    assert_eq!(reply["error"]["details"]["protocol"], json!([1]), "{reply}");
}

fn what_arrives_is_checked_and_a_file_keeps_its_time() {
    let laptop = node("laptop");
    let edited = node("edited");
    let pi = node("pi");
    fleet(
        &laptop,
        &[
            ("edited", &edited, "evil", vec!["mutate".to_owned()]),
            ("pi", &pi, "node", vec![]),
        ],
    );
    let export = edited.home().join("export");
    fs::create_dir_all(&export).unwrap();
    fs::write(export.join("readme.txt"), b"read me").unwrap();
    fs::write(export.join("steady.txt"), b"steady").unwrap();

    // With checksum, a file that changed between its listing and its packing is caught.
    let error = refused(&transfer(
        &laptop,
        json!({"from": "edited:~/export", "to": "~/export", "checksum": true}),
        &[],
    ));
    assert_eq!(
        (error["code"].as_str(), error["machine"].as_str()),
        (Some("failed"), Some("edited")),
        "{error}"
    );
    assert!(error.to_string().contains("readme.txt"), "{error}");
    assert!(!error.to_string().contains("steady.txt"), "{error}");

    // A file sent on its own keeps its time, so a tree that holds it later finds it unchanged.
    let dir = laptop.home().join("dir");
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("a.txt"), b"alpha").unwrap();
    fs::write(dir.join("b.txt"), b"beta").unwrap();
    let old = std::time::SystemTime::UNIX_EPOCH + Duration::from_secs(1_600_000_000);
    fs::File::options()
        .write(true)
        .open(dir.join("a.txt"))
        .unwrap()
        .set_modified(old)
        .unwrap();
    ok(&transfer(
        &laptop,
        json!({"from": "~/dir/a.txt", "to": "pi:~/dir/a.txt"}),
        &[],
    ));
    assert_eq!(
        fs::metadata(pi.home().join("dir/a.txt"))
            .unwrap()
            .modified()
            .unwrap(),
        old
    );
    let tree = ok(&transfer(
        &laptop,
        json!({"from": "~/dir", "to": "pi:~/dir"}),
        &[],
    ));
    assert_eq!(tree["files_sent"], 1, "{tree}");
    assert_eq!(tree["files_unchanged"], 1, "{tree}");
}

fn an_end_that_is_stopped_is_given_time_to_clean_up() {
    let laptop = node("laptop");
    let broken = node("broken");
    let pi = node("pi");
    fleet(
        &laptop,
        &[
            ("broken", &broken, "evil", vec!["truncate".to_owned()]),
            ("pi", &pi, "evil", vec!["slow-cleanup".to_owned()]),
        ],
    );
    fs::create_dir_all(broken.home().join("export")).unwrap();
    fs::write(broken.home().join("export/big.bin"), data(1024 * 1024, 31)).unwrap();

    // The source fails half way through a file; the destination, stopped, still removes what
    // it was writing before it goes.
    let error = refused(&transfer(
        &laptop,
        json!({"from": "broken:~/export", "to": "pi:~/copy"}),
        &[],
    ));
    assert_eq!(error["machine"], "broken", "{error}");
    let left: Vec<String> = fs::read_dir(pi.home().join("copy"))
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert!(left.is_empty(), "{left:?}");
}
