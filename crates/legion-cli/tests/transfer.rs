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
            "the_destination_takes_only_what_was_planned_as_it_was_planned",
            the_destination_takes_only_what_was_planned_as_it_was_planned,
        ),
        (
            "gate_serves_only_the_connectors_commands_pinned_to_its_principal",
            gate_serves_only_the_connectors_commands_pinned_to_its_principal,
        ),
        (
            "forwarding_node_authorises_and_audits_remote_stream_pipes",
            forwarding_node_authorises_and_audits_remote_stream_pipes,
        ),
        (
            "relayed_machine_refuses_transfer_with_relay_hint",
            relayed_machine_refuses_transfer_with_relay_hint,
        ),
        (
            "if_exists_modes_and_concurrent_rename_race",
            if_exists_modes_and_concurrent_rename_race,
        ),
        (
            "move_refusals_dry_run_and_kill_at_every_stage",
            move_refusals_dry_run_and_kill_at_every_stage,
        ),
        (
            "background_transfer_outlives_caller_progress_owner_cancel_resume_and_paths",
            background_transfer_outlives_caller_progress_owner_cancel_resume_and_paths,
        ),
        (
            "favourites_two_node_convergence_auto_push_and_hostile_peer",
            favourites_two_node_convergence_auto_push_and_hostile_peer,
        ),
        (
            "move_same_file_aliases_symlinks_hardlinks_and_case_normalization_rename",
            move_same_file_aliases_symlinks_hardlinks_and_case_normalization_rename,
        ),
        (
            "older_v1_peer_copies_work_and_new_features_are_refused",
            older_v1_peer_copies_work_and_new_features_are_refused,
        ),
        (
            "revoking_device_or_policy_stops_running_transfer_and_live_stream",
            revoking_device_or_policy_stops_running_transfer_and_live_stream,
        ),
        (
            "stale_partial_restart_reports_honest_per_attempt_progress",
            stale_partial_restart_reports_honest_per_attempt_progress,
        ),
        (
            "move_tree_shared_subfolder_and_inconclusive_probe_keep_source",
            move_tree_shared_subfolder_and_inconclusive_probe_keep_source,
        ),
        (
            "crossing_moves_same_node_and_two_nodes_never_lose_data",
            crossing_moves_same_node_and_two_nodes_never_lose_data,
        ),
        (
            "stream_rename_privilege_and_visible_interrupted_recovery",
            stream_rename_privilege_and_visible_interrupted_recovery,
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
        // Throttles `put` stdin or `get` stdout to ~4 KiB/s (1 KiB every 250ms) so revocation must
        // trigger via the 2-second time threshold at BOTH ends long before 4 MiB has moved.
        "very-slow" if subcommand == "stream" => {
            let mut input = BufReader::new(std::io::stdin());
            let mut header = Vec::new();
            input.read_until(b'\n', &mut header).unwrap();
            let header_val: Value = serde_json::from_slice(&header).unwrap_or(Value::Null);
            let op = header_val["op"].as_str().unwrap_or("");
            if op == "get" {
                let mut child = command
                    .stdin(Stdio::piped())
                    .stdout(Stdio::piped())
                    .spawn()
                    .unwrap();
                let mut child_in = child.stdin.take().unwrap();
                child_in.write_all(&header).unwrap();
                drop(child_in);
                let mut child_out = BufReader::new(child.stdout.take().unwrap());
                let mut stdout = std::io::stdout().lock();
                // Forward greeting + GetReady JSON lines immediately, then throttle raw file bytes.
                for _ in 0..2 {
                    let mut line = Vec::new();
                    let n = child_out.read_until(b'\n', &mut line).unwrap();
                    if n == 0 {
                        break;
                    }
                    stdout.write_all(&line).unwrap();
                    stdout.flush().unwrap();
                }
                let mut buf = [0u8; 1024];
                while let Ok(read) = child_out.read(&mut buf) {
                    if let Ok(Some(status)) = child.try_wait()
                        && !status.success()
                    {
                        break;
                    }
                    if read == 0 || stdout.write_all(&buf[..read]).is_err() {
                        break;
                    }
                    let _ = stdout.flush();
                    std::thread::sleep(Duration::from_millis(250));
                }
                return exit_of(child.wait().unwrap());
            }
            let mut child = command.stdin(Stdio::piped()).spawn().unwrap();
            let mut child_in = child.stdin.take().unwrap();
            child_in.write_all(&header).unwrap();
            let _ = child_in.flush();
            if op == "put" {
                std::thread::spawn(move || {
                    let mut buf = [0u8; 1024];
                    while let Ok(read) = input.read(&mut buf) {
                        if read == 0 || child_in.write_all(&buf[..read]).is_err() {
                            break;
                        }
                        let _ = child_in.flush();
                        std::thread::sleep(Duration::from_millis(250));
                    }
                });
            } else {
                std::thread::spawn(move || {
                    let _ = std::io::copy(&mut input, &mut child_in);
                });
            }
            exit_of(child.wait().unwrap())
        }
        // Stalls a `put` stream completely (0 B/s) after forwarding `limit` bytes, keeping stdin
        // open until the child exits on its own (proving stalled-stream revocation).
        "stall-put" if subcommand == "stream" => {
            let marker = Path::new(&extra[0]).to_path_buf();
            let limit: u64 = extra[1].parse().unwrap();
            let mut input = BufReader::new(std::io::stdin());
            let mut header = Vec::new();
            input.read_until(b'\n', &mut header).unwrap();
            let header_val: Value = serde_json::from_slice(&header).unwrap_or(Value::Null);
            let is_put = header_val["op"].as_str() == Some("put");
            let mut child = command.stdin(Stdio::piped()).spawn().unwrap();
            let mut child_in = child.stdin.take().unwrap();
            child_in.write_all(&header).unwrap();
            let _ = child_in.flush();
            if is_put {
                let _ = std::io::copy(&mut (&mut input).take(limit), &mut child_in);
                let _ = child_in.flush();
                let _ = fs::write(&marker, b"stalled");
                // Hold stdin open in a background thread without sending more bytes; exit as soon
                // as the child terminates (when its 2s auth timer fires!).
                let _holder = std::thread::spawn(move || {
                    let _keep_in = child_in;
                    let _keep_stdin = input;
                    std::thread::sleep(Duration::from_secs(60));
                });
                return exit_of(child.wait().unwrap());
            }
            std::thread::spawn(move || {
                let _ = std::io::copy(&mut input, &mut child_in);
            });
            exit_of(child.wait().unwrap())
        }
        // Simulates a case-folding filesystem where `stat` cannot tell case-sensitivity read-only
        // (omits `case_insensitive` and `parent_case_insensitive`) and both `EmptyDir` and
        // `emptydir` share the same `(file_id, parent_id)`. Proves `fs.transfer --move` asks
        // `probe_write` (`action = "behavior"`) and renames cleanly instead of assuming sensitive.
        "unknown-casefold-peer" if subcommand == "stream" => {
            let mut input = BufReader::new(std::io::stdin());
            let mut header = Vec::new();
            input.read_until(b'\n', &mut header).unwrap();
            let header_val: Value = serde_json::from_slice(&header).unwrap_or(Value::Null);
            let op = header_val["op"].as_str().unwrap_or("");
            let mut stdout = std::io::stdout().lock();
            let hello = json!({"protocol": 1, "capabilities": ["if_exists", "lock", "move", "probe", "rename"]});
            match op {
                "stat" => {
                    writeln!(stdout, "{hello}").unwrap();
                    writeln!(
                        stdout,
                        "{}",
                        json!({
                            "kind": "dir",
                            "resolved_path": header_val["path"],
                            "file_id": {"dev": 77, "ino": 5001, "nlink": 2},
                            "parent_id": {"dev": 77, "ino": 5000, "nlink": 3},
                        })
                    )
                    .unwrap();
                    stdout.flush().unwrap();
                    return ExitCode::SUCCESS;
                }
                "probe_write" if header_val["action"].as_str() == Some("behavior") => {
                    writeln!(stdout, "{hello}").unwrap();
                    writeln!(
                        stdout,
                        "{}",
                        json!({
                            "dir": header_val["dir"],
                            "case_insensitive": true,
                            "unicode_normalizing": false,
                        })
                    )
                    .unwrap();
                    stdout.flush().unwrap();
                    return ExitCode::SUCCESS;
                }
                "rename" => {
                    writeln!(stdout, "{hello}").unwrap();
                    writeln!(stdout, "{}", json!({"ok": true})).unwrap();
                    stdout.flush().unwrap();
                    return ExitCode::SUCCESS;
                }
                "delete" if header_val["dry_run"].as_bool() == Some(true) => {
                    writeln!(stdout, "{hello}").unwrap();
                    writeln!(stdout, "{}", json!({"removed": 0})).unwrap();
                    stdout.flush().unwrap();
                    return ExitCode::SUCCESS;
                }
                _ => {}
            }
            let mut child = command.stdin(Stdio::piped()).spawn().unwrap();
            let mut child_in = child.stdin.take().unwrap();
            child_in.write_all(&header).unwrap();
            std::thread::spawn(move || {
                let _ = std::io::copy(&mut input, &mut child_in);
            });
            exit_of(child.wait().unwrap())
        }
        // Makes `probe_write` (create) inject `network_fs: "nfs"` into the real stream reply so
        // moves targeting a network share treat the directory pair as inconclusive (copy completes
        // and source is kept with the network file system reason stated).
        "network-fs-peer" if subcommand == "stream" => {
            let mut input = BufReader::new(std::io::stdin());
            let mut header = Vec::new();
            input.read_until(b'\n', &mut header).unwrap();
            let header_val: Value = serde_json::from_slice(&header).unwrap_or(Value::Null);
            let is_probe_create = header_val["op"].as_str() == Some("probe_write")
                && header_val["action"].as_str() == Some("create");
            let mut child = command
                .stdin(Stdio::piped())
                .stdout(if is_probe_create {
                    Stdio::piped()
                } else {
                    Stdio::inherit()
                })
                .spawn()
                .unwrap();
            let mut child_in = child.stdin.take().unwrap();
            child_in.write_all(&header).unwrap();
            std::thread::spawn(move || {
                let _ = std::io::copy(&mut input, &mut child_in);
            });
            if is_probe_create {
                let mut child_out = BufReader::new(child.stdout.take().unwrap());
                let mut stdout = std::io::stdout().lock();
                let mut line = String::new();
                let mut idx = 0usize;
                while child_out.read_line(&mut line).unwrap() > 0 {
                    if idx == 1
                        && let Ok(mut reply) = serde_json::from_str::<Value>(&line)
                    {
                        reply["network_fs"] = json!("nfs");
                        writeln!(stdout, "{reply}").unwrap();
                    } else {
                        stdout.write_all(line.as_bytes()).unwrap();
                    }
                    stdout.flush().unwrap();
                    line.clear();
                    idx += 1;
                }
            }
            exit_of(child.wait().unwrap())
        }
        // Makes `probe_write` (create) inject `fs_locality: {"kind":"unknown",...}` (e.g. an
        // unverified FUSE mount like `fuse.mergerfs`) so moves treat the directory pair as
        // inconclusive instead of assuming local.
        "unknown-fs-peer" if subcommand == "stream" => {
            let mut input = BufReader::new(std::io::stdin());
            let mut header = Vec::new();
            input.read_until(b'\n', &mut header).unwrap();
            let header_val: Value = serde_json::from_slice(&header).unwrap_or(Value::Null);
            let is_probe_create = header_val["op"].as_str() == Some("probe_write")
                && header_val["action"].as_str() == Some("create");
            let mut child = command
                .stdin(Stdio::piped())
                .stdout(if is_probe_create {
                    Stdio::piped()
                } else {
                    Stdio::inherit()
                })
                .spawn()
                .unwrap();
            let mut child_in = child.stdin.take().unwrap();
            child_in.write_all(&header).unwrap();
            std::thread::spawn(move || {
                let _ = std::io::copy(&mut input, &mut child_in);
            });
            if is_probe_create {
                let mut child_out = BufReader::new(child.stdout.take().unwrap());
                let mut stdout = std::io::stdout().lock();
                let mut line = String::new();
                let mut idx = 0usize;
                while child_out.read_line(&mut line).unwrap() > 0 {
                    if idx == 1
                        && let Ok(mut reply) = serde_json::from_str::<Value>(&line)
                    {
                        if let Some(obj) = reply.as_object_mut() {
                            obj.remove("network_fs");
                            obj.insert(
                                "fs_locality".into(),
                                json!({
                                    "kind": "unknown",
                                    "detail": "unverified FUSE filesystem (fuse.mergerfs)"
                                }),
                            );
                        }
                        writeln!(stdout, "{reply}").unwrap();
                    } else {
                        stdout.write_all(line.as_bytes()).unwrap();
                    }
                    stdout.flush().unwrap();
                    line.clear();
                    idx += 1;
                }
            }
            exit_of(child.wait().unwrap())
        }
        // Drops a held `probe_write` connection right after sending the marker reply, before
        // answering `"confirm\n"` with `{"held":true}` (simulating a marker disappearing or the
        // peer crashing mid-probe). Move MUST treat this as inconclusive and keep the source.
        "unconfirmed-probe-peer" if subcommand == "stream" => {
            let mut input = BufReader::new(std::io::stdin());
            let mut header = Vec::new();
            input.read_until(b'\n', &mut header).unwrap();
            let header_val: Value = serde_json::from_slice(&header).unwrap_or(Value::Null);
            let is_held_probe = header_val["op"].as_str() == Some("probe_write")
                && header_val["action"].as_str() == Some("create")
                && header_val["hold"].as_bool() == Some(true);
            let mut child = command
                .stdin(Stdio::piped())
                .stdout(if is_held_probe {
                    Stdio::piped()
                } else {
                    Stdio::inherit()
                })
                .spawn()
                .unwrap();
            let mut child_in = child.stdin.take().unwrap();
            child_in.write_all(&header).unwrap();
            child_in.flush().unwrap();
            if is_held_probe {
                let mut child_out = BufReader::new(child.stdout.take().unwrap());
                let mut stdout = std::io::stdout().lock();
                // Forward greeting + ProbeReply, then immediately close the child's stdin and exit
                // without sending any ProbeConfirm line.
                for _ in 0..2 {
                    let mut line = String::new();
                    if child_out.read_line(&mut line).unwrap() == 0 {
                        break;
                    }
                    stdout.write_all(line.as_bytes()).unwrap();
                    stdout.flush().unwrap();
                }
                drop(child_in);
                let _ = child.wait();
                return ExitCode::SUCCESS;
            }
            std::thread::spawn(move || {
                let _ = std::io::copy(&mut input, &mut child_in);
            });
            exit_of(child.wait().unwrap())
        }
        // Makes `probe_write` return `{dir: null, marker: null, exists: false}` so directory
        // distinctness cannot be proven (proving move fails safe and keeps both copies).
        "inconclusive-probe" if subcommand == "stream" => {
            let mut input = BufReader::new(std::io::stdin());
            let mut header = Vec::new();
            input.read_until(b'\n', &mut header).unwrap();
            let header_val: Value = serde_json::from_slice(&header).unwrap_or(Value::Null);
            if header_val["op"].as_str() == Some("probe_write") {
                let mut stdout = std::io::stdout().lock();
                writeln!(
                    stdout,
                    "{}",
                    json!({"protocol": 1, "capabilities": ["if_exists", "lock", "move", "probe", "rename"]})
                )
                .unwrap();
                writeln!(
                    stdout,
                    "{}",
                    json!({"dir": null, "marker": null, "exists": false})
                )
                .unwrap();
                stdout.flush().unwrap();
                return ExitCode::SUCCESS;
            }
            let mut child = command.stdin(Stdio::piped()).spawn().unwrap();
            let mut child_in = child.stdin.take().unwrap();
            child_in.write_all(&header).unwrap();
            std::thread::spawn(move || {
                let _ = std::io::copy(&mut input, &mut child_in);
            });
            exit_of(child.wait().unwrap())
        }
        // Cuts a specific stream op once (marker file gates it). `op_match`:
        // - "put" / "unpack": cuts after `limit` bytes of stream body.
        // - "real-delete": cuts the first non-dry-run `delete` before forwarding it.
        "cut-op" if subcommand == "stream" => {
            let marker = Path::new(&extra[0]);
            let op_match = extra[1].as_str();
            let limit: u64 = extra[2].parse().unwrap();
            let mut input = BufReader::new(std::io::stdin());
            let mut header = Vec::new();
            input.read_until(b'\n', &mut header).unwrap();
            let header_val: Value = serde_json::from_slice(&header).unwrap_or(Value::Null);
            let op = header_val["op"].as_str().unwrap_or("");
            let is_dry = header_val["dry_run"].as_bool().unwrap_or(false);
            let should_cut = !marker.exists()
                && match op_match {
                    "real-delete" => op == "delete" && !is_dry,
                    other => op == other,
                };
            if should_cut && limit == 0 {
                fs::write(marker, b"cut").unwrap();
                return ExitCode::from(1);
            }
            let mut child = command.stdin(Stdio::piped()).spawn().unwrap();
            let mut child_in = child.stdin.take().unwrap();
            child_in.write_all(&header).unwrap();
            if should_cut {
                fs::write(marker, b"cut").unwrap();
                let _ = std::io::copy(&mut input.take(limit), &mut child_in);
                drop(child_in);
                let _ = child.wait();
                return ExitCode::from(1);
            }
            // Pump stdin in a background thread so when `child` exits (e.g. `pack`, `stat`,
            // `manifest`), the helper exits immediately and closes stdout without deadlocking.
            std::thread::spawn(move || {
                let _ = std::io::copy(&mut input, &mut child_in);
            });
            exit_of(child.wait().unwrap())
        }
        // Simulates an older v1 peer (before stream capabilities) by stripping `capabilities` from
        // the greeting line while forwarding everything else unchanged.
        "legacy-v1" if subcommand == "stream" => {
            let mut child = command
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .spawn()
                .unwrap();
            let mut child_in = child.stdin.take().unwrap();
            std::thread::spawn(move || {
                let mut stdin = std::io::stdin().lock();
                let _ = std::io::copy(&mut stdin, &mut child_in);
            });
            let mut child_out = BufReader::new(child.stdout.take().unwrap());
            let mut first = Vec::new();
            let n = child_out.read_until(b'\n', &mut first).unwrap();
            let mut stdout = std::io::stdout().lock();
            if n > 0 {
                if let Ok(mut greeting) = serde_json::from_slice::<Value>(&first)
                    && let Some(obj) = greeting.as_object_mut()
                    && obj.contains_key("protocol")
                {
                    obj.remove("capabilities");
                    writeln!(stdout, "{greeting}").unwrap();
                } else {
                    stdout.write_all(&first).unwrap();
                }
                stdout.flush().unwrap();
            }
            let _ = std::io::copy(&mut child_out, &mut stdout);
            stdout.flush().unwrap();
            exit_of(child.wait().unwrap())
        }
        // Holds a `put` stream open after forwarding `limit` bytes: touches `ready_marker` once
        // paused and waits until `release_marker` appears before forwarding the rest. Used to
        // test concurrent puts deterministically in both completion orders.
        "hold-put" if subcommand == "stream" => {
            let ready_marker = Path::new(&extra[0]);
            let release_marker = Path::new(&extra[1]);
            let limit: u64 = extra[2].parse().unwrap();
            let mut input = BufReader::new(std::io::stdin());
            let mut header = Vec::new();
            input.read_until(b'\n', &mut header).unwrap();
            let header_val: Value = serde_json::from_slice(&header).unwrap_or(Value::Null);
            let is_put = header_val["op"].as_str() == Some("put");
            let mut child = command.stdin(Stdio::piped()).spawn().unwrap();
            let mut child_in = child.stdin.take().unwrap();
            child_in.write_all(&header).unwrap();
            child_in.flush().unwrap();
            if is_put && !ready_marker.exists() {
                let _ = std::io::copy(&mut (&mut input).take(limit), &mut child_in);
                let _ = child_in.flush();
                fs::write(ready_marker, b"paused").unwrap();
                let wait_until = Instant::now() + Duration::from_secs(20);
                while !release_marker.exists() && Instant::now() < wait_until {
                    std::thread::sleep(Duration::from_millis(15));
                }
            }
            std::thread::spawn(move || {
                let _ = std::io::copy(&mut input, &mut child_in);
            });
            exit_of(child.wait().unwrap())
        }
        // A source that does not keep to the protocol, in the way `extra[0]` names.
        "evil" if subcommand == "stream" => evil(&extra[0], Path::new(home), command),
        "cut"
        | "cut-op"
        | "slow"
        | "evil"
        | "legacy-v1"
        | "hold-put"
        | "network-fs-peer"
        | "unknown-fs-peer"
        | "unconfirmed-probe-peer"
        | "inconclusive-probe" => exit_of(command.status().unwrap()),
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
        // Its file grows after it was listed.
        ("grow", "pack") => {
            fs::write(home.join("export/readme.txt"), b"read me, and more").unwrap();
        }
        // Its file is listed as a file and packed as a link to somewhere else.
        #[cfg(unix)]
        ("to-symlink", "pack") => {
            let file = home.join("export/readme.txt");
            fs::remove_file(&file).unwrap();
            std::os::unix::fs::symlink("/etc/passwd", &file).unwrap();
        }
        // Leaves out one of the entries it was asked for, and does not say so.
        ("omit", "pack") => {
            let names: Vec<Value> = header["names"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|name| *name != "steady.txt")
                .cloned()
                .collect();
            header["names"] = json!(names);
        }
        // Sends every entry twice, as two archives run together.
        ("twice", "pack") => return twice(header, command),
        // An unpack that, like a real one, has a temporary file to remove once its input ends,
        // and takes a moment to do it: killed at once, it leaves the file behind.
        ("slow-cleanup", "unpack") => {
            let root = home.join(header["root"].as_str().unwrap().trim_start_matches("~/"));
            fs::create_dir_all(&root).unwrap();
            let temporary = root.join(".legion-unpack-1-0.tmp");
            fs::write(&temporary, b"half").unwrap();
            println!("{}", json!({"protocol": 1}));
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
            println!("{}", json!({"protocol": 1}));
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
                if reply.get("protocol").is_some() {
                } else if reply.get("done").is_some() {
                    reply["entries"] = json!(kept);
                } else if reply["path"] != "readme.txt" {
                    continue;
                } else {
                    kept += 1;
                }
                writeln!(out, "{reply}").unwrap();
            }
        }
        // Speaks the protocol before this one, which did not open by saying which it speaks.
        ("old", "pack") => {
            let mut line = String::new();
            replies.read_line(&mut line).unwrap();
            std::io::copy(&mut replies, &mut out).unwrap();
        }
        // Says its file system ignores letter case, as macOS and Windows usually do.
        ("caseless", "stat") => {
            for line in replies.lines() {
                let mut reply: Value = serde_json::from_str(&line.unwrap()).unwrap();
                if reply.get("kind").is_some() {
                    reply["case_insensitive"] = json!(true);
                }
                writeln!(out, "{reply}").unwrap();
            }
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

/// Packs the same names twice and sends one archive holding both, each entry appearing twice.
fn twice(header: Value, mut command: Command) -> ExitCode {
    let mut archives = Vec::new();
    for _ in 0..2 {
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        writeln!(child.stdin.take().unwrap(), "{header}").unwrap();
        let mut output = Vec::new();
        child
            .stdout
            .take()
            .unwrap()
            .read_to_end(&mut output)
            .unwrap();
        child.wait().unwrap();
        archives.push(output);
    }
    // The lines before each archive, then the archive; the first loses its closing blocks.
    let split = |output: &[u8]| {
        let mut at = 0;
        while output.get(at) == Some(&b'{') {
            at += output[at..].iter().position(|b| *b == b'\n').unwrap() + 1;
        }
        at
    };
    let (lines, first) = archives[0].split_at(split(&archives[0]));
    let mut first = first.to_vec();
    while first.len() >= 512 && first[first.len() - 512..].iter().all(|b| *b == 0) {
        first.truncate(first.len() - 512);
    }
    let second = &archives[1][split(&archives[1])..];
    let mut out = std::io::stdout().lock();
    out.write_all(lines).unwrap();
    out.write_all(&first).unwrap();
    out.write_all(second).unwrap();
    out.flush().unwrap();
    ExitCode::SUCCESS
}

/// The first reply on a stream's output, after the line that opens every stream and says which
/// protocol it speaks.
fn first_reply(stdout: &[u8]) -> Value {
    let mut lines = stdout.split(|b| *b == b'\n');
    let opening: Value = serde_json::from_slice(lines.next().unwrap()).unwrap();
    assert_eq!(opening["protocol"], json!(1));
    assert_eq!(
        opening["capabilities"],
        json!(["if_exists", "lock", "move", "probe", "rename"])
    );
    serde_json::from_slice(lines.next().unwrap()).unwrap()
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
        json!({"op": "unpack", "root": "~/projects/x", "entries": [], "as": "reader", "protocol": 1}),
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
        let reply: Value = first_reply(&output.stdout);
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
    let pi_root = pi.state().parent().unwrap().to_string_lossy().into_owned();
    let gate = |original: Option<&str>| {
        let mut command = pi.command(&["gate", "--as", "agent:x", "--home", &pi_root]);
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
    assert_eq!(reply["result"]["door"], "ssh", "{reply}");
    let rpc_audit = pi.audit().pop().unwrap();
    assert_eq!(rpc_audit["door"], json!("ssh"), "{rpc_audit}");
    assert!(rpc_audit.get("device").is_none(), "{rpc_audit}");

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
        let reply: Value = first_reply(&output.stdout);
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
            (Some("ssh"), Some("agent:x"), Some(outcome))
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
    // The unpack stops at the first entry that was not planned.
    assert_eq!(error["details"]["violation"]["path"], ".env", "{error}");
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
    let reply: Value = first_reply(&output.stdout);
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

fn forwarding_node_authorises_and_audits_remote_stream_pipes() {
    let laptop = node("laptop");
    let pi = node("pi");
    fleet(&laptop, &[("pi", &pi, "node", vec![])]);

    // On pi every test principal has full access so only laptop's forwarding policy can refuse.
    pi.write_config(
        "policy.toml",
        r#"
[principals.fetcher]
role = "full"

[principals.mover]
role = "full"

[roles.full]
allow = ["*"]
"#,
    );
    laptop.write_config(
        "policy.toml",
        r#"
[principals.fetcher]
role = "fetch_only"

[principals.mover]
role = "move_only"

[roles.fetch_only]
allow = ["effect:read", "fs.transfer"]

[roles.move_only]
allow = ["effect:read", "effect:write"]
"#,
    );

    fs::write(laptop.home().join("note.txt"), b"hello").unwrap();

    // fetcher may call fs.transfer and fs.read on laptop, but NOT fs.write: pushing to pi must
    // be refused on laptop BEFORE opening a put pipe on pi, and audited on laptop.
    let err = refused(&transfer(
        &laptop,
        json!({"from": "~/note.txt", "to": "pi:~/note.txt"}),
        &["--as", "fetcher"],
    ));
    assert_eq!(err["code"], "denied", "{err}");
    assert_eq!(err["machine"], "laptop", "{err}");
    assert!(
        err["message"].as_str().unwrap().contains("fs.write"),
        "{err}"
    );
    assert!(!pi.home().join("note.txt").exists());
    assert!(
        stream_lines(&pi).iter().all(|e| e["tool"] != "stream.put"),
        "pi should never have spawned a put stream: {:?}",
        stream_lines(&pi)
    );
    assert!(
        laptop.audit().iter().any(|e| {
            e["target"] == "pi"
                && e["tool"] == "stream.put"
                && e["principal"] == "fetcher"
                && e["outcome"] == "denied"
                && e["door"] == "cli"
        }),
        "{:?}",
        laptop.audit()
    );

    // mover may read and write on laptop, but NOT fs.remove: delete=true on a remote directory
    // must be refused on laptop BEFORE opening a delete pipe on pi.
    fs::create_dir_all(laptop.home().join("tree")).unwrap();
    fs::write(laptop.home().join("tree/keep.txt"), b"keep").unwrap();
    fs::create_dir_all(pi.home().join("tree")).unwrap();
    fs::write(pi.home().join("tree/keep.txt"), b"keep").unwrap();
    fs::write(pi.home().join("tree/extra.txt"), b"extra").unwrap();
    let err = refused(&transfer(
        &laptop,
        json!({"from": "~/tree", "to": "pi:~/tree", "delete": true}),
        &["--as", "mover"],
    ));
    assert_eq!(err["code"], "denied", "{err}");
    assert_eq!(err["machine"], "laptop", "{err}");
    assert!(
        err["message"].as_str().unwrap().contains("fs.remove"),
        "{err}"
    );
    assert!(pi.home().join("tree/extra.txt").exists());
    assert!(
        stream_lines(&pi)
            .iter()
            .all(|e| e["tool"] != "stream.delete"),
        "pi should never have spawned a delete stream: {:?}",
        stream_lines(&pi)
    );
    assert!(
        laptop.audit().iter().any(|e| {
            e["target"] == "pi"
                && e["tool"] == "stream.delete"
                && e["principal"] == "mover"
                && e["outcome"] == "denied"
        }),
        "{:?}",
        laptop.audit()
    );

    // An allowed transfer audits every remote pipe opened on the forwarding node.
    ok(&transfer(
        &laptop,
        json!({"from": "~/note.txt", "to": "pi:~/ok.txt"}),
        &[],
    ));
    let forwarded_pipes: Vec<Value> = laptop
        .audit()
        .into_iter()
        .filter(|e| e["target"] == "pi" && e["principal"] == "owner" && e["outcome"] == "ok")
        .collect();
    for expected in ["stream.stat", "stream.put"] {
        assert!(
            forwarded_pipes.iter().any(|e| e["tool"] == expected),
            "{expected} missing from laptop forwarded pipe audit: {forwarded_pipes:?}"
        );
    }
}

fn relayed_machine_refuses_transfer_with_relay_hint() {
    let laptop = node("laptop");
    let pi = node("pi");
    let pi_argv = [
        std::env::current_exe()
            .unwrap()
            .to_string_lossy()
            .into_owned(),
        HELPER_FLAG.to_owned(),
        "node".to_owned(),
        LEGION_BIN.to_owned(),
        legion_home_of(&pi).to_string_lossy().into_owned(),
        pi.home().to_string_lossy().into_owned(),
        "rpc".to_owned(),
    ];
    let quoted: Vec<String> = pi_argv
        .iter()
        .map(|arg| serde_json::to_string(arg).unwrap())
        .collect();
    laptop.write_config(
        "fleet.toml",
        &format!(
            "[machines.pi]\ncommand = [{}]\n\n[machines.tower]\nvia = \"pi\"\n",
            quoted.join(", ")
        ),
    );
    fs::write(laptop.home().join("a.txt"), b"hello").unwrap();

    for (from, to) in [("~/a.txt", "tower:~/a.txt"), ("tower:~/a.txt", "~/b.txt")] {
        let err = refused(&transfer(&laptop, json!({"from": from, "to": to}), &[]));
        assert_eq!(err["code"], "unsupported", "{err}");
        assert_eq!(err["machine"], "laptop", "{err}");
        let hint = err["hint"].as_str().unwrap_or("");
        assert!(
            hint.contains("pi") && hint.contains("--on pi"),
            "hint should direct transfers to relay pi: {err}"
        );
    }

    let cp = refused(&run_bounded(
        &laptop,
        &["cp", "~/a.txt", "tower:~/a.txt", "--json"],
    ));
    assert_eq!(cp["code"], "unsupported", "{cp}");
    assert!(
        cp["hint"].as_str().unwrap_or("").contains("--on pi"),
        "{cp}"
    );
}

fn the_destination_takes_only_what_was_planned_as_it_was_planned() {
    let laptop = node("laptop");
    // Each source misbehaves in its own way; `true` where the planned file itself is what is
    // wrong, so the destination's must stay as it was.
    let mut machines = vec![
        ("edited", "mutate", true),
        ("grown", "grow", true),
        ("omitting", "omit", false),
        ("doubled", "twice", false),
    ];
    if cfg!(unix) {
        machines.push(("symlinked", "to-symlink", true));
    }
    let homes: Vec<Home> = machines.iter().map(|(name, _, _)| node(name)).collect();
    let entries: Vec<(&str, &Home, &str, Vec<String>)> = machines
        .iter()
        .zip(&homes)
        .map(|((name, behaviour, _), home)| (*name, home, "evil", vec![(*behaviour).to_owned()]))
        .collect();
    fleet(&laptop, &entries);
    for ((name, behaviour, untouched), home) in machines.iter().zip(&homes) {
        let export = home.home().join("export");
        fs::create_dir_all(&export).unwrap();
        fs::write(export.join("readme.txt"), b"read me").unwrap();
        fs::write(export.join("steady.txt"), b"steady").unwrap();
        // What the destination already has, which a refused transfer must leave as it was.
        let dest = laptop.home().join(name);
        fs::create_dir_all(&dest).unwrap();
        fs::write(dest.join("readme.txt"), b"OLD ME!").unwrap();

        let error = refused(&transfer(
            &laptop,
            json!({"from": format!("{name}:~/export"), "to": format!("~/{name}"), "checksum": true}),
            &[],
        ));
        assert_eq!(
            (error["code"].as_str(), error["machine"].as_str()),
            (Some("failed"), Some(*name)),
            "{behaviour}: {error}"
        );
        assert!(
            fs::symlink_metadata(dest.join("readme.txt"))
                .unwrap()
                .file_type()
                .is_file(),
            "{behaviour}"
        );
        let now = fs::read(dest.join("readme.txt")).unwrap();
        if *untouched {
            assert_eq!(now, b"OLD ME!", "{behaviour}");
        } else {
            assert!(now == b"OLD ME!" || now == b"read me", "{behaviour}");
        }
    }
}

fn call_tool(home: &Home, tool: &str, args: Value, extra: &[&str]) -> Run {
    let text = args.to_string();
    let mut line = vec![tool, "--args", text.as_str()];
    line.extend_from_slice(extra);
    run_bounded(home, &line)
}

fn if_exists_modes_and_concurrent_rename_race() {
    let laptop = node("laptop");
    let pi = node("pi");
    fleet(&laptop, &[("pi", &pi, "node", vec![])]);

    // --- Single file: fail, skip, replace, rename ---
    fs::write(laptop.home().join("doc.txt"), b"new-content").unwrap();
    fs::write(pi.home().join("doc.txt"), b"existing-content").unwrap();

    // fail: refuses with `exists`, leaves destination untouched
    let err = refused(&transfer(
        &laptop,
        json!({"from": "~/doc.txt", "to": "pi:~/doc.txt", "if_exists": "fail"}),
        &[],
    ));
    assert_eq!(err["code"], "exists", "{err}");
    assert_eq!(
        fs::read(pi.home().join("doc.txt")).unwrap(),
        b"existing-content"
    );

    // skip: succeeds with files_skipped=1, leaves destination untouched
    let skipped = ok(&transfer(
        &laptop,
        json!({"from": "~/doc.txt", "to": "pi:~/doc.txt", "if_exists": "skip"}),
        &[],
    ));
    assert_eq!(skipped["files_sent"], 0, "{skipped}");
    assert_eq!(skipped["files_skipped"], 1, "{skipped}");
    assert_eq!(
        fs::read(pi.home().join("doc.txt")).unwrap(),
        b"existing-content"
    );

    // rename: writes `doc (2).txt`, then `doc (3).txt` and preserves caller's `~/` form with `/`
    // on every OS (including Windows).
    let ren1 = ok(&transfer(
        &laptop,
        json!({"from": "~/doc.txt", "to": "pi:~/doc.txt", "if_exists": "rename"}),
        &[],
    ));
    assert_eq!(ren1["files_sent"], 1, "{ren1}");
    assert_eq!(ren1["to"], "pi:~/doc (2).txt", "{ren1}");
    assert_eq!(
        fs::read(pi.home().join("doc (2).txt")).unwrap(),
        b"new-content"
    );
    assert_eq!(
        fs::read(pi.home().join("doc.txt")).unwrap(),
        b"existing-content"
    );

    let ren2 = ok(&transfer(
        &laptop,
        json!({"from": "~/doc.txt", "to": "pi:~/doc.txt", "if_exists": "rename"}),
        &[],
    ));
    assert_eq!(ren2["to"], "pi:~/doc (3).txt", "{ren2}");
    assert_eq!(
        fs::read(pi.home().join("doc (3).txt")).unwrap(),
        b"new-content"
    );

    // replace (default): overwrites destination
    let rep = ok(&transfer(
        &laptop,
        json!({"from": "~/doc.txt", "to": "pi:~/doc.txt", "if_exists": "replace"}),
        &[],
    ));
    assert_eq!(rep["files_sent"], 1, "{rep}");
    assert_eq!(fs::read(pi.home().join("doc.txt")).unwrap(), b"new-content");

    // --- Tree: fail, skip, rename, replace ---
    let src_tree = laptop.home().join("src_tree");
    let dst_tree = pi.home().join("dst_tree");
    fs::create_dir_all(&src_tree).unwrap();
    fs::create_dir_all(&dst_tree).unwrap();
    fs::write(src_tree.join("a.txt"), b"src-a").unwrap();
    fs::write(src_tree.join("b.txt"), b"src-b").unwrap();
    fs::write(dst_tree.join("a.txt"), b"dst-a").unwrap();

    let err = refused(&transfer(
        &laptop,
        json!({"from": "~/src_tree", "to": "pi:~/dst_tree", "if_exists": "fail"}),
        &[],
    ));
    assert_eq!(err["code"], "exists", "{err}");
    assert!(!dst_tree.join("b.txt").exists());

    let skip_tree = ok(&transfer(
        &laptop,
        json!({"from": "~/src_tree", "to": "pi:~/dst_tree", "if_exists": "skip"}),
        &[],
    ));
    assert_eq!(skip_tree["files_sent"], 1, "{skip_tree}");
    assert_eq!(skip_tree["files_skipped"], 1, "{skip_tree}");
    assert_eq!(fs::read(dst_tree.join("a.txt")).unwrap(), b"dst-a");
    assert_eq!(fs::read(dst_tree.join("b.txt")).unwrap(), b"src-b");

    let ren_tree = ok(&transfer(
        &laptop,
        json!({"from": "~/src_tree", "to": "pi:~/dst_tree", "if_exists": "rename"}),
        &[],
    ));
    assert_eq!(ren_tree["files_sent"], 2, "{ren_tree}");
    assert_eq!(fs::read(dst_tree.join("a (2).txt")).unwrap(), b"src-a");
    assert_eq!(fs::read(dst_tree.join("b (2).txt")).unwrap(), b"src-b");
    assert_eq!(fs::read(dst_tree.join("a.txt")).unwrap(), b"dst-a");

    // --- Deterministic concurrent transfers to the same destination name under `rename` ---
    // Hold the first `put` open mid-stream while the second `put` runs to completion, in BOTH
    // orders (A held while B runs, then B held while A runs), plus an unthrottled race.
    let payload_a = data(512 * 1024, 101);
    let payload_b = data(512 * 1024, 202);
    fs::write(laptop.home().join("race_a.bin"), &payload_a).unwrap();
    fs::write(laptop.home().join("race_b.bin"), &payload_b).unwrap();

    for (order_idx, (held_src, held_bytes, fast_src, fast_bytes)) in [
        ("race_a.bin", &payload_a, "race_b.bin", &payload_b),
        ("race_b.bin", &payload_b, "race_a.bin", &payload_a),
    ]
    .into_iter()
    .enumerate()
    {
        let target_name = format!("det_race_{order_idx}.bin");
        let target_2 = format!("det_race_{order_idx} (2).bin");
        let target_3 = format!("det_race_{order_idx} (3).bin");
        fs::write(pi.home().join(&target_name), b"initial").unwrap();

        let ready = laptop.home().join(format!("hold_ready_{order_idx}"));
        let release = laptop.home().join(format!("hold_release_{order_idx}"));
        fleet(
            &laptop,
            &[(
                "pi",
                &pi,
                "hold-put",
                vec![
                    ready.to_string_lossy().into_owned(),
                    release.to_string_lossy().into_owned(),
                    "131072".to_owned(),
                ],
            )],
        );

        let lh = legion_home_of(&laptop);
        let uh = laptop.home().to_path_buf();
        let target_for_held = target_name.clone();
        let held_handle = std::thread::spawn(move || {
            let args = json!({
                "from": format!("~/{held_src}"),
                "to": format!("pi:~/{target_for_held}"),
                "if_exists": "rename",
            })
            .to_string();
            let out = Command::new(LEGION_BIN)
                .args(["fs.transfer", "--args", &args])
                .env("LEGION_HOME", &lh)
                .env("HOME", &uh)
                .env("USERPROFILE", &uh)
                .env_remove("XDG_CONFIG_HOME")
                .env_remove("XDG_STATE_HOME")
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "held {held_src} failed: stdout={} stderr={}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            );
            serde_json::from_slice::<Value>(&out.stdout).unwrap()
        });

        // Wait until the held `put` is open and paused at 128 KiB on `pi`.
        let deadline = Instant::now() + Duration::from_secs(15);
        while !ready.exists() {
            assert!(
                Instant::now() < deadline,
                "held put did not pause in time (order {order_idx})"
            );
            std::thread::sleep(Duration::from_millis(15));
        }

        // Run the second transfer to completion while the first `put` is still open!
        let fast_res = ok(&transfer(
            &laptop,
            json!({
                "from": format!("~/{fast_src}"),
                "to": format!("pi:~/{target_name}"),
                "if_exists": "rename",
            }),
            &[],
        ));
        assert_eq!(fast_res["to"], format!("pi:~/{target_2}"), "{fast_res}");
        assert_eq!(fs::read(pi.home().join(&target_2)).unwrap(), *fast_bytes);

        // Release the held transfer; it finishes without seeing fast_src's bytes and takes (3).
        fs::write(&release, b"go").unwrap();
        let held_res = held_handle.join().unwrap();
        assert_eq!(held_res["to"], format!("pi:~/{target_3}"), "{held_res}");
        assert_eq!(fs::read(pi.home().join(&target_3)).unwrap(), *held_bytes);
        assert_eq!(fs::read(pi.home().join(&target_name)).unwrap(), b"initial");
    }

    // Also run an unthrottled concurrent race with the plain connector.
    fleet(&laptop, &[("pi", &pi, "node", vec![])]);
    fs::write(pi.home().join("race.bin"), b"initial").unwrap();

    let legion_home = legion_home_of(&laptop);
    let user_home = laptop.home().to_path_buf();
    let run_one = |src_name: &'static str| {
        let lh = legion_home.clone();
        let uh = user_home.clone();
        std::thread::spawn(move || {
            let args = json!({
                "from": format!("~/{src_name}"),
                "to": "pi:~/race.bin",
                "if_exists": "rename",
            })
            .to_string();
            let out = Command::new(LEGION_BIN)
                .args(["fs.transfer", "--args", &args])
                .env("LEGION_HOME", &lh)
                .env("HOME", &uh)
                .env("USERPROFILE", &uh)
                .env_remove("XDG_CONFIG_HOME")
                .env_remove("XDG_STATE_HOME")
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "{src_name} failed: stdout={} stderr={}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            );
            serde_json::from_slice::<Value>(&out.stdout).unwrap()
        })
    };
    let t1 = run_one("race_a.bin");
    let t2 = run_one("race_b.bin");
    let r1 = t1.join().unwrap();
    let r2 = t2.join().unwrap();
    let to1 = r1["to"].as_str().unwrap();
    let to2 = r2["to"].as_str().unwrap();
    assert_ne!(
        to1, to2,
        "concurrent renames must land at distinct paths: {r1} vs {r2}"
    );
    assert!(
        (to1 == "pi:~/race (2).bin" && to2 == "pi:~/race (3).bin")
            || (to1 == "pi:~/race (3).bin" && to2 == "pi:~/race (2).bin"),
        "unexpected race destinations: {to1} and {to2}"
    );
    let b2 = fs::read(pi.home().join("race (2).bin")).unwrap();
    let b3 = fs::read(pi.home().join("race (3).bin")).unwrap();
    assert!(
        (b2 == payload_a && b3 == payload_b) || (b2 == payload_b && b3 == payload_a),
        "both concurrent payloads must arrive intact"
    );
    assert_eq!(fs::read(pi.home().join("race.bin")).unwrap(), b"initial");
}

fn move_refusals_dry_run_and_kill_at_every_stage() {
    let laptop = node("laptop");
    let pi = node("pi");
    let tower = node("tower");
    fleet(
        &laptop,
        &[
            ("pi", &pi, "node", vec![]),
            ("tower", &tower, "node", vec![]),
        ],
    );

    // 1. Move without `fs.remove` is refused up front BEFORE any byte moves (local & remote).
    laptop.write_config(
        "policy.toml",
        r#"
[principals.no_rm]
role = "write_no_remove"

[roles.write_no_remove]
allow = ["effect:read", "effect:write"]
deny = ["fs.remove"]
"#,
    );
    pi.write_config(
        "policy.toml",
        r#"
[principals.no_rm]
role = "write_no_remove"

[roles.write_no_remove]
allow = ["effect:read", "effect:write"]
deny = ["fs.remove"]
"#,
    );
    fs::write(laptop.home().join("keep_local.bin"), b"local-bytes").unwrap();
    let err = refused(&transfer(
        &laptop,
        json!({"from": "~/keep_local.bin", "to": "pi:~/moved_local.bin", "move": true}),
        &["--as", "no_rm"],
    ));
    assert_eq!(err["code"], "denied", "{err}");
    assert!(laptop.home().join("keep_local.bin").exists());
    assert!(!pi.home().join("moved_local.bin").exists());
    assert!(
        stream_lines(&pi).iter().all(|e| e["tool"] != "stream.put"),
        "must refuse move before opening put stream"
    );

    // Also test remote source without `fs.remove` on the source machine (while laptop allows it).
    laptop.write_config(
        "policy.toml",
        r#"
[principals.no_rm]
role = "full"

[roles.full]
allow = ["*"]
"#,
    );
    fs::write(pi.home().join("keep_remote.bin"), b"remote-bytes").unwrap();
    let err = refused(&transfer(
        &laptop,
        json!({"from": "pi:~/keep_remote.bin", "to": "~/got_remote.bin", "move": true}),
        &["--as", "no_rm"],
    ));
    assert_eq!(err["code"], "denied", "{err}");
    assert_eq!(err["machine"], "pi", "{err}");
    assert!(pi.home().join("keep_remote.bin").exists());
    assert!(!laptop.home().join("got_remote.bin").exists());

    // 2. Move onto itself and move into own subtree are refused up front.
    let err = refused(&transfer(
        &laptop,
        json!({"from": "~/keep_local.bin", "to": "~/keep_local.bin", "move": true}),
        &[],
    ));
    assert_eq!(err["code"], "invalid_args", "{err}");
    assert!(laptop.home().join("keep_local.bin").exists());

    let self_tree = laptop.home().join("self_tree");
    fs::create_dir_all(self_tree.join("sub")).unwrap();
    fs::write(self_tree.join("a.txt"), b"a").unwrap();
    let err = refused(&transfer(
        &laptop,
        json!({"from": "~/self_tree", "to": "~/self_tree/sub/inside", "move": true}),
        &[],
    ));
    assert_eq!(err["code"], "invalid_args", "{err}");
    assert!(self_tree.join("a.txt").exists());

    // 3. Dry run reports what would be removed without touching source or destination.
    let dry = ok(&transfer(
        &laptop,
        json!({"from": "~/self_tree", "to": "tower:~/self_tree", "move": true, "dry_run": true}),
        &[],
    ));
    assert_eq!(dry["dry_run"], true, "{dry}");
    assert_eq!(dry["moved"], true, "{dry}");
    assert!(
        dry["would_remove"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v == "a.txt"),
        "{dry}"
    );
    assert!(self_tree.join("a.txt").exists());
    assert!(!tower.home().join("self_tree").exists());

    // 4. Kill a single-file move mid-`put`: source stays, destination has .legion-part -> re-run completes.
    let stage1_laptop = node("laptop");
    let stage1_pi = node("pi");
    let m_put = stage1_laptop.home().join("m_put");
    fleet(
        &stage1_laptop,
        &[(
            "pi",
            &stage1_pi,
            "cut-op",
            vec![
                m_put.to_string_lossy().into_owned(),
                "put".to_owned(),
                "100000".to_owned(),
            ],
        )],
    );
    let big = data(400_000, 17);
    fs::write(stage1_laptop.home().join("film.bin"), &big).unwrap();
    let cut = refused(&transfer(
        &stage1_laptop,
        json!({"from": "~/film.bin", "to": "pi:~/film.bin", "move": true}),
        &[],
    ));
    assert_eq!(cut["machine"], "pi", "{cut}");
    // Source MUST still exist because put did not finish.
    assert_eq!(
        fs::read(stage1_laptop.home().join("film.bin")).unwrap(),
        big
    );
    assert!(!stage1_pi.home().join("film.bin").exists());
    // Re-running resumes and completes the move.
    let done = ok(&transfer(
        &stage1_laptop,
        json!({"from": "~/film.bin", "to": "pi:~/film.bin", "move": true}),
        &[],
    ));
    assert_eq!(done["moved"], true, "{done}");
    assert_eq!(done["source_removed"], 1, "{done}");
    assert!(!stage1_laptop.home().join("film.bin").exists());
    assert_eq!(fs::read(stage1_pi.home().join("film.bin")).unwrap(), big);

    // 5. Kill a single-file move AFTER `put` succeeded, right before source `delete` runs.
    let stage2_laptop = node("laptop");
    let stage2_src = node("src");
    let stage2_dst = node("dst");
    let m_del = stage2_laptop.home().join("m_del");
    fleet(
        &stage2_laptop,
        &[
            (
                "src",
                &stage2_src,
                "cut-op",
                vec![
                    m_del.to_string_lossy().into_owned(),
                    "real-delete".to_owned(),
                    "0".to_owned(),
                ],
            ),
            ("dst", &stage2_dst, "node", vec![]),
        ],
    );
    fs::write(stage2_src.home().join("solo.bin"), &big).unwrap();
    let cut = refused(&transfer(
        &stage2_laptop,
        json!({"from": "src:~/solo.bin", "to": "dst:~/solo.bin", "move": true}),
        &[],
    ));
    assert_eq!(cut["machine"], "src", "{cut}");
    // File is now in BOTH places (never zero places!).
    assert_eq!(fs::read(stage2_src.home().join("solo.bin")).unwrap(), big);
    assert_eq!(fs::read(stage2_dst.home().join("solo.bin")).unwrap(), big);
    // Re-run finishes by verifying hash at dst and removing src.
    let done = ok(&transfer(
        &stage2_laptop,
        json!({"from": "src:~/solo.bin", "to": "dst:~/solo.bin", "move": true}),
        &[],
    ));
    assert_eq!(done["source_removed"], 1, "{done}");
    assert!(!stage2_src.home().join("solo.bin").exists());
    assert_eq!(fs::read(stage2_dst.home().join("solo.bin")).unwrap(), big);

    // 6. Kill a tree move mid-`unpack`, then kill another tree move at source `delete` after unpack:
    //    at EVERY interrupted point, every single file exists in at least one place!
    for (stage_name, cut_target, op_match, limit) in [
        ("mid-unpack", "dst", "unpack", "65536"),
        ("after-unpack-before-delete", "src", "real-delete", "0"),
    ] {
        let ctl = node("ctl");
        let src = node("src");
        let dst = node("dst");
        let marker = ctl.home().join(format!("m_{stage_name}"));
        let src_mode = if cut_target == "src" {
            "cut-op"
        } else {
            "node"
        };
        let dst_mode = if cut_target == "dst" {
            "cut-op"
        } else {
            "node"
        };
        let src_extra = if cut_target == "src" {
            vec![
                marker.to_string_lossy().into_owned(),
                op_match.to_owned(),
                limit.to_owned(),
            ]
        } else {
            vec![]
        };
        let dst_extra = if cut_target == "dst" {
            vec![
                marker.to_string_lossy().into_owned(),
                op_match.to_owned(),
                limit.to_owned(),
            ]
        } else {
            vec![]
        };
        fleet(
            &ctl,
            &[
                ("src", &src, src_mode, src_extra),
                ("dst", &dst, dst_mode, dst_extra),
            ],
        );
        let src_dir = src.home().join("album");
        fs::create_dir_all(src_dir.join("nested")).unwrap();
        let expected: Vec<(&str, Vec<u8>)> = vec![
            ("one.bin", data(80_000, 31)),
            ("two.bin", data(80_000, 32)),
            ("nested/three.bin", data(80_000, 33)),
        ];
        for (rel, bytes) in &expected {
            fs::write(src_dir.join(rel), bytes).unwrap();
        }

        let _cut = refused(&transfer(
            &ctl,
            json!({"from": "src:~/album", "to": "dst:~/album", "move": true}),
            &[],
        ));
        assert!(marker.exists(), "{stage_name} marker was not triggered");
        // Invariant: NO file is EVER in zero places!
        for (rel, bytes) in &expected {
            let in_src = fs::read(src_dir.join(rel)).ok();
            let in_dst = fs::read(dst.home().join("album").join(rel)).ok();
            let present = in_src.as_deref() == Some(bytes.as_slice())
                || in_dst.as_deref() == Some(bytes.as_slice());
            assert!(
                present,
                "{stage_name}: file {rel} lost from both source and destination!"
            );
        }

        // Re-running completes the move cleanly.
        let done = ok(&transfer(
            &ctl,
            json!({"from": "src:~/album", "to": "dst:~/album", "move": true}),
            &[],
        ));
        assert_eq!(done["moved"], true, "{stage_name}: {done}");
        assert!(
            !src_dir.exists(),
            "{stage_name}: source root should be removed"
        );
        for (rel, bytes) in &expected {
            assert_eq!(
                fs::read(dst.home().join("album").join(rel)).unwrap(),
                *bytes,
                "{stage_name}: {rel}"
            );
        }
    }

    // 7. Kill the detached `transfer-supervise` worker process itself with SIGKILL mid-flight:
    //    every file must still exist in at least one place, and re-running completes the move.
    {
        let ctl = node("ctl");
        let dst = node("dst");
        fleet(&ctl, &[("dst", &dst, "slow", vec![])]);
        let src_dir = ctl.home().join("worker_kill_album");
        fs::create_dir_all(src_dir.join("sub")).unwrap();
        let expected: Vec<(&str, Vec<u8>)> = vec![
            ("a.bin", data(120_000, 61)),
            ("b.bin", data(120_000, 62)),
            ("sub/c.bin", data(120_000, 63)),
        ];
        for (rel, bytes) in &expected {
            fs::write(src_dir.join(rel), bytes).unwrap();
        }
        let started = ok(&transfer(
            &ctl,
            json!({
                "from": "~/worker_kill_album",
                "to": "dst:~/worker_kill_album",
                "move": true,
                "background": true
            }),
            &[],
        ));
        let job_id = started["job"]["id"].as_str().unwrap().to_owned();
        let deadline = Instant::now() + Duration::from_secs(10);
        let worker_pid = loop {
            let st = ok(&call_tool(&ctl, "job.status", json!({"id": &job_id}), &[]));
            if st["progress"]["bytes_done"].as_u64().unwrap_or(0) > 0
                && let Some(pid) = st["detach"]["process"]["pid"].as_u64()
            {
                break u32::try_from(pid).unwrap();
            }
            assert!(Instant::now() < deadline, "worker made no progress: {st}");
            std::thread::sleep(Duration::from_millis(30));
        };
        #[cfg(unix)]
        {
            let _ = Command::new("kill")
                .args(["-9", &worker_pid.to_string()])
                .status();
        }
        #[cfg(windows)]
        {
            let _ = Command::new("taskkill")
                .args(["/F", "/PID", &worker_pid.to_string()])
                .status();
        }
        // Wait until the worker process is gone and job.status sees `interrupted` (or finished).
        let kill_deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let st = ok(&call_tool(&ctl, "job.status", json!({"id": &job_id}), &[]));
            let state = st["state"].as_str().unwrap();
            if state != "running" && state != "starting" {
                break;
            }
            assert!(
                Instant::now() < kill_deadline,
                "worker did not stop after SIGKILL: {st}"
            );
            std::thread::sleep(Duration::from_millis(40));
        }
        // Invariant: NO file is ever in zero places after killing the worker process.
        for (rel, bytes) in &expected {
            let in_src = fs::read(src_dir.join(rel)).ok();
            let in_dst = fs::read(dst.home().join("worker_kill_album").join(rel)).ok();
            assert!(
                in_src.as_deref() == Some(bytes.as_slice())
                    || in_dst.as_deref() == Some(bytes.as_slice()),
                "worker SIGKILL lost {rel} from both source and destination!"
            );
        }
        // Re-running completes cleanly.
        fleet(&ctl, &[("dst", &dst, "node", vec![])]);
        let done = ok(&transfer(
            &ctl,
            json!({
                "from": "~/worker_kill_album",
                "to": "dst:~/worker_kill_album",
                "move": true
            }),
            &[],
        ));
        assert_eq!(done["moved"], true, "{done}");
        assert!(!src_dir.exists());
        for (rel, bytes) in &expected {
            assert_eq!(
                fs::read(dst.home().join("worker_kill_album").join(rel)).unwrap(),
                *bytes
            );
        }
    }
}

fn background_transfer_outlives_caller_progress_owner_cancel_resume_and_paths() {
    let laptop = node("laptop");
    let pi = node("pi");
    fleet(&laptop, &[("pi", &pi, "slow", vec![])]);

    laptop.write_config(
        "policy.toml",
        r#"
[principals.mover]
role = "rw_no_exec"

[principals.other]
role = "rw_no_exec"

[principals.confined]
role = "confined"

[roles.rw_no_exec]
allow = ["effect:read", "effect:write"]

[roles.confined]
allow = ["effect:read", "effect:write"]
paths = ["~/allowed"]
"#,
    );
    pi.write_config(
        "policy.toml",
        r#"
[principals.mover]
role = "rw_no_exec"

[principals.other]
role = "rw_no_exec"

[principals.confined]
role = "confined"

[roles.rw_no_exec]
allow = ["effect:read", "effect:write"]

[roles.confined]
allow = ["effect:read", "effect:write"]
paths = ["~/allowed"]
"#,
    );

    // 1. Start a background copy over an RPC session, close the calling session immediately,
    //    and watch it finish via `job.list`, `job.status`, and `job.wait` with monotonic progress.
    let clip = data(2 * 1024 * 1024, 55);
    fs::write(laptop.home().join("Holiday.mp4"), &clip).unwrap();

    let mut rpc = laptop.session(&[]);
    rpc.send_json(&json!({
        "jsonrpc": "2.0",
        "id": "bg1",
        "method": "call",
        "params": {
            "tool": "fs.transfer",
            "args": {
                "from": "~/Holiday.mp4",
                "to": "pi:~/Holiday.mp4",
                "background": true
            }
        }
    }));
    let (reply, _) = rpc.reply_to(&json!("bg1"));
    let started = &reply["result"];
    let job_id = started["job"]["id"].as_str().unwrap().to_owned();
    assert_eq!(started["job"]["machine"], "laptop", "{reply}");
    // Disconnect the calling session right away!
    assert_eq!(rpc.finish(), 0);

    // Check `job.list` shows `kind: "transfer"` and readable `title`.
    let listed = ok(&call_tool(&laptop, "job.list", json!({}), &[]));
    let summary = listed["jobs"]
        .as_array()
        .unwrap()
        .iter()
        .find(|j| j["id"] == job_id)
        .expect("background transfer must appear in job.list");
    assert_eq!(summary["kind"], "transfer", "{summary}");
    assert_eq!(summary["title"], "Copy Holiday.mp4 to pi", "{summary}");

    // Watch `job.wait` via RPC to verify progress events are monotonic and final result matches foreground.
    let mut watcher = laptop.session(&[]);
    watcher.send_json(&json!({
        "jsonrpc": "2.0",
        "id": "w1",
        "method": "call",
        "params": {
            "tool": "job.wait",
            "args": { "id": &job_id, "timeout_s": 60 }
        }
    }));
    let (wait_reply, events) = watcher.reply_to(&json!("w1"));
    assert_eq!(watcher.finish(), 0);
    let waited = &wait_reply["result"];
    assert_eq!(waited["state"], "succeeded", "{waited}");
    assert_eq!(waited["kind"], "transfer", "{waited}");
    assert_eq!(waited["title"], "Copy Holiday.mp4 to pi", "{waited}");
    assert_eq!(waited["result"]["kind"], "file", "{waited}");
    assert_eq!(waited["result"]["files_sent"], 1, "{waited}");
    assert_eq!(
        waited["result"]["bytes_sent"],
        clip.len() as u64,
        "{waited}"
    );
    assert_eq!(fs::read(pi.home().join("Holiday.mp4")).unwrap(), clip);

    let mut prev_bytes = 0u64;
    let mut prev_files = 0u64;
    for ev in &events {
        if ev["kind"] == "progress" {
            let b = ev["data"]["bytes_done"].as_u64().unwrap();
            let f = ev["data"]["files_done"].as_u64().unwrap();
            assert!(
                b >= prev_bytes,
                "bytes_done went backwards: {b} < {prev_bytes}"
            );
            assert!(
                f >= prev_files,
                "files_done went backwards: {f} < {prev_files}"
            );
            prev_bytes = b;
            prev_files = f;
        }
    }
    assert!(
        waited["progress"]["bytes_done"].as_u64().unwrap() == clip.len() as u64,
        "{waited}"
    );

    // 2. Owner cancel WITHOUT `effect:exec`, non-owner refused, exec job cancel refused, resume after cancel!
    let exec_started = ok(&call_tool(
        &laptop,
        "job.start",
        json!({"argv": ["sleep", "30"]}),
        &[],
    ));
    let exec_id = exec_started["id"].as_str().unwrap().to_owned();

    let huge = data(12 * 1024 * 1024, 77);
    fs::write(laptop.home().join("huge.bin"), &huge).unwrap();
    let mover_bg = ok(&transfer(
        &laptop,
        json!({"from": "~/huge.bin", "to": "pi:~/huge.bin", "background": true}),
        &["--as", "mover"],
    ));
    let mover_job_id = mover_bg["job"]["id"].as_str().unwrap().to_owned();

    // Wait until some bytes have moved into the partial file.
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let st = ok(&call_tool(
            &laptop,
            "job.status",
            json!({"id": &mover_job_id}),
            &["--as", "mover"],
        ));
        if st["progress"]["bytes_done"].as_u64().unwrap_or(0) > 0 {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "background transfer made no progress: {st}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }

    // Another principal without `effect:exec` CANNOT cancel `mover`'s transfer job!
    let denied_other = refused(&call_tool(
        &laptop,
        "job.cancel",
        json!({"id": &mover_job_id}),
        &["--as", "other"],
    ));
    assert_eq!(denied_other["code"], "denied", "{denied_other}");

    // `mover` CANNOT cancel an `exec` job!
    let denied_exec = refused(&call_tool(
        &laptop,
        "job.cancel",
        json!({"id": &exec_id}),
        &["--as", "mover"],
    ));
    assert_eq!(denied_exec["code"], "denied", "{denied_exec}");

    // `mover` CAN cancel its OWN transfer job without `effect:exec`!
    let cancelled = ok(&call_tool(
        &laptop,
        "job.cancel",
        json!({"id": &mover_job_id}),
        &["--as", "mover"],
    ));
    assert_eq!(cancelled["state"], "cancelled", "{cancelled}");
    // Clean up owner's sleep exec job.
    let _ = ok(&call_tool(
        &laptop,
        "job.cancel",
        json!({"id": &exec_id}),
        &[],
    ));

    // Partial file is kept on pi so restarting the same transfer resumes from > 0!
    let part_path = pi.home().join("huge.bin.legion-part");
    assert!(
        part_path.exists(),
        "partial file should be kept after cancel"
    );
    let part_len = fs::metadata(&part_path).unwrap().len();
    assert!(
        part_len > 0 && part_len < huge.len() as u64,
        "part_len={part_len}"
    );

    // Use a fast node connector for the resume completion check.
    fleet(&laptop, &[("pi", &pi, "node", vec![])]);
    let resumed = ok(&transfer(
        &laptop,
        json!({"from": "~/huge.bin", "to": "pi:~/huge.bin"}),
        &["--as", "mover"],
    ));
    assert_eq!(resumed["resumed_from"], part_len, "{resumed}");
    assert_eq!(fs::read(pi.home().join("huge.bin")).unwrap(), huge);

    // 3. A role confined by `paths` cannot use a background transfer to read or write outside `paths`.
    fs::create_dir_all(laptop.home().join("allowed")).unwrap();
    fs::create_dir_all(pi.home().join("allowed")).unwrap();
    fs::write(laptop.home().join("allowed/ok.txt"), b"ok").unwrap();
    fs::write(laptop.home().join("outside.txt"), b"secret").unwrap();

    // Local read outside paths -> refused up front.
    let bad_local = refused(&transfer(
        &laptop,
        json!({"from": "~/outside.txt", "to": "pi:~/allowed/stolen.txt", "background": true}),
        &["--as", "confined"],
    ));
    assert_eq!(bad_local["code"], "denied", "{bad_local}");
    assert!(!pi.home().join("allowed/stolen.txt").exists());

    // Remote write outside paths -> background job runs as `confined`, fails with `denied` on `pi`.
    let bad_remote_start = ok(&transfer(
        &laptop,
        json!({"from": "~/allowed/ok.txt", "to": "pi:~/outside_dst.txt", "background": true}),
        &["--as", "confined"],
    ));
    let bad_remote_id = bad_remote_start["job"]["id"].as_str().unwrap().to_owned();
    let bad_remote_wait = ok(&call_tool(
        &laptop,
        "job.wait",
        json!({"id": &bad_remote_id, "timeout_s": 30}),
        &["--as", "confined"],
    ));
    assert_eq!(bad_remote_wait["state"], "failed", "{bad_remote_wait}");
    assert_eq!(
        bad_remote_wait["error"]["code"], "denied",
        "{bad_remote_wait}"
    );
    assert!(!pi.home().join("outside_dst.txt").exists());
}

fn favourites_two_node_convergence_auto_push_and_hostile_peer() {
    let tower = node("tower");
    let pi = node("pi");
    fleet(&tower, &[("pi", &pi, "node", vec![])]);
    fleet(&pi, &[("tower", &tower, "node", vec![])]);

    fs::create_dir_all(tower.home().join("photos")).unwrap();
    fs::write(tower.home().join("photos/beach.jpg"), b"jpg").unwrap();
    fs::write(pi.home().join("notes.md"), b"# notes").unwrap();

    // 1. `fav.add` on tower automatically pushes `favorites.toml` to pi with node authority.
    let added = ok(&call_tool(
        &tower,
        "fav.add",
        json!({
            "path": "~/photos/beach.jpg",
            "label": "Beach",
            "pinned": false
        }),
        &[],
    ));
    let id1 = added["entry"]["id"].as_str().unwrap().to_owned();
    assert_eq!(added["entry"]["machine"], "tower", "{added}");
    assert_eq!(added["entry"]["category"], "image", "{added}");
    assert_eq!(added["entry"]["exists"], true, "{added}");
    assert_eq!(added["fleet"]["pi"]["status"], "synced", "{added}");

    // pi sees the favourite immediately via `fav.list`!
    let pi_list = ok(&call_tool(&pi, "fav.list", json!({}), &[]));
    let entries = pi_list["entries"].as_array().unwrap();
    assert_eq!(entries.len(), 1, "{pi_list}");
    assert_eq!(entries[0]["id"], id1);
    assert_eq!(entries[0]["label"], "Beach");
    assert!(entries[0].get("exists").is_none());

    // Add a second favourite on pi that tower will later remove while partitioned.
    let added2 = ok(&call_tool(
        &pi,
        "fav.add",
        json!({
            "path": "~/notes.md",
            "label": "Notes"
        }),
        &[],
    ));
    let id2 = added2["entry"]["id"].as_str().unwrap().to_owned();
    assert_eq!(added2["fleet"]["tower"]["status"], "synced", "{added2}");

    // 2. Partition both nodes (empty `fleet.toml`), make concurrent edits to the SAME entry (`id1`),
    //    plus a removal of `id2` on tower and a new entry on pi.
    tower.write_config("fleet.toml", "");
    pi.write_config("fleet.toml", "");

    // Wait 5ms so stamps are strictly newer than initial add.
    std::thread::sleep(Duration::from_millis(5));
    ok(&call_tool(
        &tower,
        "fav.set",
        json!({"id": &id1, "label": "Summer Beach"}),
        &[],
    ));
    ok(&call_tool(&tower, "fav.remove", json!({"id": &id2}), &[]));

    std::thread::sleep(Duration::from_millis(5));
    ok(&call_tool(
        &pi,
        "fav.set",
        json!({"id": &id1, "pinned": true}),
        &[],
    ));
    let added3 = ok(&call_tool(
        &pi,
        "fav.add",
        json!({"machine": "tower", "path": "~/photos", "kind": "dir", "pinned": true}),
        &[],
    ));
    let id3 = added3["entry"]["id"].as_str().unwrap().to_owned();

    // 3. Reconnect and sync `favorites.toml` via `fleet.sync direction=push`.
    fleet(&tower, &[("pi", &pi, "node", vec![])]);
    fleet(&pi, &[("tower", &tower, "node", vec![])]);

    let synced = ok(&call_tool(
        &tower,
        "fleet.sync",
        json!({"direction": "push", "documents": ["favorites.toml"]}),
        &[],
    ));
    assert_eq!(
        synced["results"]["pi"]["favorites.toml"]["status"], "pushed",
        "{synced}"
    );

    // Both nodes must have converged to identical `favorites.toml` containing:
    // - id1 with BOTH `label: "Summer Beach"` AND `pinned: true`
    // - id3 present
    // - id2 removed (tombstoned)
    let tower_toml = fs::read_to_string(config_of(&tower).join("favorites.toml")).unwrap();
    let pi_toml = fs::read_to_string(config_of(&pi).join("favorites.toml")).unwrap();
    assert_eq!(
        tower_toml, pi_toml,
        "nodes did not converge to identical favorites.toml"
    );

    let tower_after = ok(&call_tool(&tower, "fav.list", json!({}), &[]));
    let items = tower_after["entries"].as_array().unwrap();
    assert_eq!(items.len(), 2, "{tower_after}");
    let item1 = items.iter().find(|e| e["id"] == id1).unwrap();
    assert_eq!(item1["label"], "Summer Beach", "{item1}");
    assert_eq!(item1["pinned"], true, "{item1}");
    assert!(items.iter().any(|e| e["id"] == id3), "{tower_after}");
    assert!(items.iter().all(|e| e["id"] != id2), "{tower_after}");

    // 4. Hostile peer payload sent to `config.write favorites.toml` is rejected without corrupting pi.
    let bad = refused(&call_tool(
        &pi,
        "config.write",
        json!({
            "document": "favorites.toml",
            "content": "version = 1\n[entries.evil]\nmachine = \"pi\"\npath = \"/tmp/../etc/passwd\"\nkind = \"file\"\npinned = false\nadded_at = \"2026-11-01T00:00:00.000Z\"\nadded_by = \"owner\"\nupdated_at = \"2026-11-01T00:00:00.000Z\"\nupdated_on = \"pi\"\nlabel_updated_at = \"2026-11-01T00:00:00.000Z\"\nlabel_updated_on = \"pi\"\npinned_updated_at = \"2026-11-01T00:00:00.000Z\"\npinned_updated_on = \"pi\"\n"
        }),
        &[],
    ));
    assert_eq!(bad["code"], "failed", "{bad}");
    assert!(bad["message"].as_str().unwrap().contains("`..`"), "{bad}");
    assert_eq!(
        fs::read_to_string(config_of(&pi).join("favorites.toml")).unwrap(),
        pi_toml
    );
}

fn move_same_file_aliases_symlinks_hardlinks_and_case_normalization_rename() {
    let laptop = node("laptop");
    // Register `twin` in `fleet.toml` pointing at `laptop`'s OWN home & config (same-machine alias).
    fleet(&laptop, &[("twin", &laptop, "node", vec![])]);

    // 1. Single-file move onto same file via fleet alias `twin`: MUST be refused, file intact!
    let payload = b"only-copy-must-never-be-deleted";
    fs::write(laptop.home().join("only-copy.bin"), payload).unwrap();
    let err = refused(&transfer(
        &laptop,
        json!({"from": "~/only-copy.bin", "to": "twin:~/only-copy.bin", "move": true}),
        &[],
    ));
    assert_eq!(err["code"], "invalid_args", "{err}");
    assert_eq!(
        fs::read(laptop.home().join("only-copy.bin")).unwrap(),
        payload,
        "only-copy.bin must remain intact after refused alias move"
    );

    // 2. Directory tree move onto itself and onto its own subtree via alias `twin`: MUST be refused!
    let tree = laptop.home().join("alias_tree");
    fs::create_dir_all(tree.join("sub")).unwrap();
    fs::write(tree.join("root.txt"), b"root-data").unwrap();
    fs::write(tree.join("sub/leaf.txt"), b"leaf-data").unwrap();

    let err_same_tree = refused(&transfer(
        &laptop,
        json!({"from": "~/alias_tree", "to": "twin:~/alias_tree", "move": true}),
        &[],
    ));
    assert_eq!(err_same_tree["code"], "invalid_args", "{err_same_tree}");
    assert_eq!(fs::read(tree.join("root.txt")).unwrap(), b"root-data");
    assert_eq!(fs::read(tree.join("sub/leaf.txt")).unwrap(), b"leaf-data");

    let err_subtree = refused(&transfer(
        &laptop,
        json!({"from": "~/alias_tree", "to": "twin:~/alias_tree/sub/inside", "move": true}),
        &[],
    ));
    assert_eq!(err_subtree["code"], "invalid_args", "{err_subtree}");
    assert_eq!(fs::read(tree.join("root.txt")).unwrap(), b"root-data");
    assert_eq!(fs::read(tree.join("sub/leaf.txt")).unwrap(), b"leaf-data");

    // 3. Same-node hard link, symlink, and symlinked parent directory: ALL refused!
    let hl_orig = laptop.home().join("hl_orig.bin");
    let hl_link = laptop.home().join("hl_link.bin");
    fs::write(&hl_orig, b"hardlink-data").unwrap();
    fs::hard_link(&hl_orig, &hl_link).unwrap();
    let err_hl = refused(&transfer(
        &laptop,
        json!({"from": "~/hl_orig.bin", "to": "~/hl_link.bin", "move": true}),
        &[],
    ));
    assert_eq!(err_hl["code"], "invalid_args", "{err_hl}");
    assert_eq!(fs::read(&hl_orig).unwrap(), b"hardlink-data");
    assert_eq!(fs::read(&hl_link).unwrap(), b"hardlink-data");

    #[cfg(unix)]
    {
        let sym_orig = laptop.home().join("sym_orig.bin");
        let sym_link = laptop.home().join("sym_link.bin");
        fs::write(&sym_orig, b"symlink-data").unwrap();
        std::os::unix::fs::symlink(&sym_orig, &sym_link).unwrap();
        let err_sym = refused(&transfer(
            &laptop,
            json!({"from": "~/sym_orig.bin", "to": "~/sym_link.bin", "move": true}),
            &[],
        ));
        assert_eq!(err_sym["code"], "invalid_args", "{err_sym}");
        assert_eq!(fs::read(&sym_orig).unwrap(), b"symlink-data");

        let real_dir = laptop.home().join("real_dir");
        let link_dir = laptop.home().join("link_dir");
        fs::create_dir_all(&real_dir).unwrap();
        fs::write(real_dir.join("inside.txt"), b"inside-data").unwrap();
        std::os::unix::fs::symlink(&real_dir, &link_dir).unwrap();
        let err_dir_sym = refused(&transfer(
            &laptop,
            json!({"from": "~/real_dir/inside.txt", "to": "~/link_dir/inside.txt", "move": true}),
            &[],
        ));
        assert_eq!(err_dir_sym["code"], "invalid_args", "{err_dir_sym}");
        assert_eq!(
            fs::read(real_dir.join("inside.txt")).unwrap(),
            b"inside-data"
        );

        let err_tree_sym = refused(&transfer(
            &laptop,
            json!({"from": "~/real_dir", "to": "~/link_dir", "move": true}),
            &[],
        ));
        assert_eq!(err_tree_sym["code"], "invalid_args", "{err_tree_sym}");
        assert_eq!(
            fs::read(real_dir.join("inside.txt")).unwrap(),
            b"inside-data"
        );
    }

    // 4. Case-only and Unicode-normalization-only renames via both `fs.transfer` (`move: true`)
    // and `stream` `rename`, verifying the resulting directory entry's exact spelling by listing
    // the directory rather than relying on `exists()`.
    let dir_entry_names = |dir: PathBuf| -> Vec<Vec<u8>> {
        fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().as_encoded_bytes().to_vec())
            .collect()
    };

    // Probe whether this runner's temp volume is case-insensitive and/or Unicode-normalizing.
    fs::write(laptop.home().join("FsCaseProbe.tmp"), b"").unwrap();
    let host_case_insensitive = fs::symlink_metadata(laptop.home().join("fscaseprobe.tmp")).is_ok();
    let _ = fs::remove_file(laptop.home().join("FsCaseProbe.tmp"));

    fs::write(laptop.home().join("FsNormProbe-e\u{0301}.tmp"), b"").unwrap();
    let host_unicode_normalizing =
        fs::symlink_metadata(laptop.home().join("FsNormProbe-\u{00e9}.tmp")).is_ok();
    let _ = fs::remove_file(laptop.home().join("FsNormProbe-e\u{0301}.tmp"));

    // 4a. ASCII case-only rename via `stream` `rename`: strictly limited to same-file
    // case/normalization renames. On case-insensitive volumes this succeeds and ends with the exact
    // requested spelling `report.txt`; on case-sensitive volumes `Report.txt` and `report.txt` are
    // distinct entries so `stream` `rename` refuses with `invalid_args`.
    fs::write(laptop.home().join("Report.txt"), b"report-contents").unwrap();
    let rename_raw = stream_request_raw(
        &laptop,
        json!({"op": "rename", "from": "~/Report.txt", "to": "~/report.txt"}),
    );
    if host_case_insensitive {
        assert_eq!(rename_raw["ok"], true, "{rename_raw}");
        let names = dir_entry_names(laptop.home());
        assert!(
            names.iter().any(|n| n == b"report.txt"),
            "expected exact 'report.txt' in dir listing"
        );
        assert!(
            !names.iter().any(|n| n == b"Report.txt"),
            "old spelling 'Report.txt' still present in dir listing"
        );
        assert_eq!(
            fs::read(laptop.home().join("report.txt")).unwrap(),
            b"report-contents"
        );
    } else {
        assert_eq!(
            rename_raw["error"]["code"], "invalid_args",
            "case-sensitive host must refuse distinct stream rename: {rename_raw}"
        );
        // Move `Report.txt` -> `report.txt` via `fs.transfer` (`move: true`) for step 4b.
        ok(&transfer(
            &laptop,
            json!({"from": "~/Report.txt", "to": "~/report.txt", "move": true}),
            &[],
        ));
    }

    // 4b. ASCII case-only rename via `fs.transfer` (`move: true`): on case-insensitive volumes
    // (Windows NTFS, default macOS APFS) this takes the atomic folding-rename path (`renamed`
    // populated); on case-sensitive volumes it moves normally. Either way the directory entry ends
    // spelled `Report.txt` and `report.txt` is gone.
    let moved_case = ok(&transfer(
        &laptop,
        json!({"from": "~/report.txt", "to": "~/Report.txt", "move": true}),
        &[],
    ));
    assert_eq!(moved_case["moved"], true, "{moved_case}");
    if host_case_insensitive {
        assert_eq!(
            moved_case["renamed"],
            json!([{"from": "~/report.txt", "to": "~/Report.txt"}]),
            "case-insensitive host must use folding rename: {moved_case}"
        );
    }
    let names = dir_entry_names(laptop.home());
    assert!(names.iter().any(|n| n == b"Report.txt"));
    assert!(!names.iter().any(|n| n == b"report.txt"));
    assert_eq!(
        fs::read(laptop.home().join("Report.txt")).unwrap(),
        b"report-contents"
    );

    // 4c. Identical byte-for-byte spelling is always refused as renaming/moving onto itself.
    let self_rename = stream_request_raw(
        &laptop,
        json!({"op": "rename", "from": "~/Report.txt", "to": "~/Report.txt"}),
    );
    assert_eq!(
        self_rename["error"]["code"], "invalid_args",
        "{self_rename}"
    );
    let self_move = refused(&transfer(
        &laptop,
        json!({"from": "~/Report.txt", "to": "~/Report.txt", "move": true}),
        &[],
    ));
    assert_eq!(self_move["code"], "invalid_args", "{self_move}");
    assert_eq!(
        fs::read(laptop.home().join("Report.txt")).unwrap(),
        b"report-contents"
    );

    // 4d. Non-ASCII case rename (`Ärger.txt` -> `ärger.txt`) via `fs.transfer` (`move: true`).
    let upper_umlaut = "\u{00c4}rger.txt";
    let lower_umlaut = "\u{00e4}rger.txt";
    fs::write(laptop.home().join(upper_umlaut), b"umlaut-contents").unwrap();
    let umlaut_moved = ok(&transfer(
        &laptop,
        json!({"from": format!("~/{upper_umlaut}"), "to": format!("~/{lower_umlaut}"), "move": true}),
        &[],
    ));
    assert_eq!(umlaut_moved["moved"], true, "{umlaut_moved}");
    let names = dir_entry_names(laptop.home());
    assert!(
        names.iter().any(|n| n == lower_umlaut.as_bytes()),
        "expected exact '{lower_umlaut}' in dir listing"
    );
    assert!(!names.iter().any(|n| n == upper_umlaut.as_bytes()));
    assert_eq!(
        fs::read(laptop.home().join(lower_umlaut)).unwrap(),
        b"umlaut-contents"
    );

    // 4d2. Case-only renames of an empty folder, a folder holding only subfolders, a folder with
    // files, and a single file inside clean parent directories with no sibling regular files.
    let dir_cases = laptop.home().join("case_dir_tests");
    fs::create_dir_all(dir_cases.join("p1/EmptyDir")).unwrap();
    fs::create_dir_all(dir_cases.join("p2/FolderOfFolders/SubA/SubB")).unwrap();
    fs::create_dir_all(dir_cases.join("p3/FolderWithFiles")).unwrap();
    fs::write(dir_cases.join("p3/FolderWithFiles/doc.txt"), b"inside").unwrap();
    fs::create_dir_all(dir_cases.join("p4")).unwrap();
    fs::write(dir_cases.join("p4/SoloFile.txt"), b"solo").unwrap();

    for (rel_from, rel_to, expected_kind, parent_rel, expected_name) in [
        (
            "~/case_dir_tests/p1/EmptyDir",
            "~/case_dir_tests/p1/emptydir",
            "tree",
            "case_dir_tests/p1",
            "emptydir",
        ),
        (
            "~/case_dir_tests/p2/FolderOfFolders",
            "~/case_dir_tests/p2/folderoffolders",
            "tree",
            "case_dir_tests/p2",
            "folderoffolders",
        ),
        (
            "~/case_dir_tests/p3/FolderWithFiles",
            "~/case_dir_tests/p3/folderwithfiles",
            "tree",
            "case_dir_tests/p3",
            "folderwithfiles",
        ),
        (
            "~/case_dir_tests/p4/SoloFile.txt",
            "~/case_dir_tests/p4/solofile.txt",
            "file",
            "case_dir_tests/p4",
            "solofile.txt",
        ),
    ] {
        let moved = ok(&transfer(
            &laptop,
            json!({"from": rel_from, "to": rel_to, "move": true}),
            &[],
        ));
        assert_eq!(moved["moved"], true, "{rel_from} -> {rel_to}: {moved}");
        assert_eq!(moved["kind"], expected_kind, "{moved}");
        if host_case_insensitive {
            assert_eq!(
                moved["renamed"],
                json!([{"from": rel_from, "to": rel_to}]),
                "case-insensitive host must use folding rename for {rel_from}: {moved}"
            );
        }
        let entries = dir_entry_names(laptop.home().join(parent_rel));
        assert!(
            entries.iter().any(|n| n == expected_name.as_bytes()),
            "expected {expected_name} in {parent_rel}: {entries:?}"
        );
    }
    assert!(dir_cases.join("p2/folderoffolders/SubA/SubB").is_dir());
    assert_eq!(
        fs::read(dir_cases.join("p3/folderwithfiles/doc.txt")).unwrap(),
        b"inside"
    );
    assert_eq!(
        fs::read(dir_cases.join("p4/solofile.txt")).unwrap(),
        b"solo"
    );

    // 4d3. Even when `stat` leaves `case_insensitive` and `parent_case_insensitive` unknown
    // (`None`) on a remote peer, `fs.transfer --move` asks `probe_write action="behavior"` rather
    // than assuming case-sensitivity, so renaming an empty folder on a case-folding volume works.
    let vfat_peer = node("vfatbox");
    fleet(
        &laptop,
        &[("vfatbox", &vfat_peer, "unknown-casefold-peer", vec![])],
    );
    let remote_renamed = ok(&transfer(
        &laptop,
        json!({
            "from": "vfatbox:~/EmptyDir",
            "to": "vfatbox:~/emptydir",
            "move": true,
        }),
        &[],
    ));
    assert_eq!(remote_renamed["moved"], true, "{remote_renamed}");
    assert_eq!(remote_renamed["kind"], "tree", "{remote_renamed}");
    assert_eq!(
        remote_renamed["renamed"],
        json!([{"from": "~/EmptyDir", "to": "~/emptydir"}]),
        "{remote_renamed}"
    );

    // 4e. Unicode normalization rename (NFD `cafe\u{0301}.txt` -> NFC `caf\u{00e9}.txt`) via
    // `fs.transfer` (`move: true`) and back via `stream` `rename`.
    let nfd_name = "cafe\u{0301}.txt";
    let nfc_name = "caf\u{00e9}.txt";
    fs::write(laptop.home().join(nfd_name), b"unicode-contents").unwrap();
    let unicode_moved = ok(&transfer(
        &laptop,
        json!({"from": format!("~/{nfd_name}"), "to": format!("~/{nfc_name}"), "move": true}),
        &[],
    ));
    assert_eq!(unicode_moved["moved"], true, "{unicode_moved}");
    if host_unicode_normalizing {
        assert_eq!(
            unicode_moved["renamed"],
            json!([{"from": format!("~/{nfd_name}"), "to": format!("~/{nfc_name}")}]),
            "Unicode-normalizing host must use folding rename: {unicode_moved}"
        );
    }
    let names = dir_entry_names(laptop.home());
    assert!(
        names.iter().any(|n| n == nfc_name.as_bytes()),
        "expected exact NFC '{nfc_name}' in dir listing"
    );
    assert!(!names.iter().any(|n| n == nfd_name.as_bytes()));
    assert_eq!(
        fs::read(laptop.home().join(nfc_name)).unwrap(),
        b"unicode-contents"
    );

    let unicode_back = stream_request_raw(
        &laptop,
        json!({"op": "rename", "from": format!("~/{nfc_name}"), "to": format!("~/{nfd_name}")}),
    );
    if host_unicode_normalizing {
        assert_eq!(unicode_back["ok"], true, "{unicode_back}");
        let names = dir_entry_names(laptop.home());
        assert!(names.iter().any(|n| n == nfd_name.as_bytes()));
        assert!(!names.iter().any(|n| n == nfc_name.as_bytes()));
    } else {
        assert_eq!(
            unicode_back["error"]["code"], "invalid_args",
            "non-normalizing host must refuse distinct stream rename: {unicode_back}"
        );
    }

    // 4f. On case-sensitive volumes (Linux, case-sensitive APFS):
    // - Two distinct files whose names differ only in case are NOT a folding rename: `stream`
    //   `rename` refuses with `invalid_args`, and `fs.transfer` follows `if_exists`.
    // - Two hardlinks whose names differ only in case are distinct links to the same file, so both
    //   `fs.transfer` (`move: true`) and `stream` `rename` must refuse with `invalid_args`.
    if !host_case_insensitive {
        fs::write(laptop.home().join("CaseA.txt"), b"upper-file").unwrap();
        fs::write(laptop.home().join("casea.txt"), b"lower-file").unwrap();
        let conflict_rename = stream_request_raw(
            &laptop,
            json!({"op": "rename", "from": "~/CaseA.txt", "to": "~/casea.txt"}),
        );
        assert_eq!(
            conflict_rename["error"]["code"], "invalid_args",
            "{conflict_rename}"
        );
        let fail_move = refused(&transfer(
            &laptop,
            json!({"from": "~/CaseA.txt", "to": "~/casea.txt", "move": true, "if_exists": "fail"}),
            &[],
        ));
        assert_eq!(fail_move["code"], "exists", "{fail_move}");
        assert_eq!(
            fs::read(laptop.home().join("CaseA.txt")).unwrap(),
            b"upper-file"
        );
        assert_eq!(
            fs::read(laptop.home().join("casea.txt")).unwrap(),
            b"lower-file"
        );

        let replace_move = ok(&transfer(
            &laptop,
            json!({"from": "~/CaseA.txt", "to": "~/casea.txt", "move": true, "if_exists": "replace"}),
            &[],
        ));
        assert_eq!(replace_move["moved"], true, "{replace_move}");
        assert_eq!(
            fs::read(laptop.home().join("casea.txt")).unwrap(),
            b"upper-file"
        );
        assert!(!laptop.home().join("CaseA.txt").exists());

        fs::write(laptop.home().join("HardCase.txt"), b"hard-body").unwrap();
        fs::hard_link(
            laptop.home().join("HardCase.txt"),
            laptop.home().join("hardcase.txt"),
        )
        .unwrap();
        let hl_move = refused(&transfer(
            &laptop,
            json!({"from": "~/HardCase.txt", "to": "~/hardcase.txt", "move": true}),
            &[],
        ));
        assert_eq!(hl_move["code"], "invalid_args", "{hl_move}");
        let hl_rename = stream_request_raw(
            &laptop,
            json!({"op": "rename", "from": "~/HardCase.txt", "to": "~/hardcase.txt"}),
        );
        assert_eq!(hl_rename["error"]["code"], "invalid_args", "{hl_rename}");
        assert_eq!(
            fs::read(laptop.home().join("HardCase.txt")).unwrap(),
            b"hard-body"
        );
        assert_eq!(
            fs::read(laptop.home().join("hardcase.txt")).unwrap(),
            b"hard-body"
        );
    }
}

fn stream_request_raw(target: &Home, mut header: Value) -> Value {
    if let Some(obj) = header.as_object_mut() {
        obj.entry("protocol").or_insert_with(|| json!(1));
    }
    let mut child = Command::new(LEGION_BIN)
        .arg("stream")
        .env("LEGION_HOME", target.state().parent().unwrap())
        .env("HOME", target.home())
        .env("USERPROFILE", target.home())
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("XDG_STATE_HOME")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    {
        let mut stdin = child.stdin.take().unwrap();
        writeln!(stdin, "{header}").unwrap();
    }
    let out = child.wait_with_output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let lines: Vec<&str> = stdout.lines().collect();
    assert!(lines.len() >= 2, "expected greeting + reply: {stdout}");
    serde_json::from_str(lines[1]).unwrap()
}

fn stream_request(target: &Home, header: Value) -> Value {
    let reply = stream_request_raw(target, header);
    assert!(reply.get("error").is_none(), "stream failed: {reply}");
    reply
}

fn older_v1_peer_copies_work_and_new_features_are_refused() {
    let laptop = node("laptop");
    let old_pi = node("old_pi");
    fleet(&laptop, &[("old_pi", &old_pi, "legacy-v1", vec![])]);

    // 1. Plain single-file copy in both directions keeps working against a v1 peer without caps.
    fs::write(laptop.home().join("plain.txt"), b"from-new").unwrap();
    let pushed = ok(&transfer(
        &laptop,
        json!({"from": "~/plain.txt", "to": "old_pi:~/plain.txt"}),
        &[],
    ));
    assert_eq!(pushed["files_sent"], 1, "{pushed}");
    assert_eq!(
        fs::read(old_pi.home().join("plain.txt")).unwrap(),
        b"from-new"
    );

    fs::write(old_pi.home().join("from_old.txt"), b"from-old").unwrap();
    let pulled = ok(&transfer(
        &laptop,
        json!({"from": "old_pi:~/from_old.txt", "to": "~/from_old.txt"}),
        &[],
    ));
    assert_eq!(pulled["files_sent"], 1, "{pulled}");
    assert_eq!(
        fs::read(laptop.home().join("from_old.txt")).unwrap(),
        b"from-old"
    );

    // Plain tree copy also works.
    let src_dir = laptop.home().join("plain_dir");
    fs::create_dir_all(&src_dir).unwrap();
    fs::write(src_dir.join("a.txt"), b"tree-a").unwrap();
    let tree_ok = ok(&transfer(
        &laptop,
        json!({"from": "~/plain_dir", "to": "old_pi:~/plain_dir"}),
        &[],
    ));
    assert_eq!(tree_ok["files_sent"], 1, "{tree_ok}");
    assert_eq!(
        fs::read(old_pi.home().join("plain_dir/a.txt")).unwrap(),
        b"tree-a"
    );

    // 2. Every `if_exists` mode other than `replace` is refused with `unsupported`, naming `old_pi`.
    fs::write(laptop.home().join("updated.txt"), b"new-body").unwrap();
    for mode in ["fail", "skip", "rename"] {
        let err = refused(&transfer(
            &laptop,
            json!({"from": "~/updated.txt", "to": "old_pi:~/plain.txt", "if_exists": mode}),
            &[],
        ));
        assert_eq!(err["code"], "unsupported", "mode={mode}: {err}");
        assert_eq!(err["machine"], "old_pi", "mode={mode}: {err}");
        assert!(
            err["message"].as_str().unwrap().contains("old_pi"),
            "mode={mode}: {err}"
        );
        assert!(
            err["message"]
                .as_str()
                .unwrap()
                .contains(&format!("if_exists={mode}")),
            "expected lowercase if_exists={mode} in error message: {err}"
        );
        assert_eq!(
            fs::read(old_pi.home().join("plain.txt")).unwrap(),
            b"from-new",
            "old_pi destination must stay untouched on mode={mode}"
        );
    }

    // 3. `move: true` is refused in BOTH directions with `unsupported`, naming `old_pi`, and leaves
    //    both source and destination untouched.
    let err_push_move = refused(&transfer(
        &laptop,
        json!({"from": "~/updated.txt", "to": "old_pi:~/moved_to_old.txt", "move": true}),
        &[],
    ));
    assert_eq!(err_push_move["code"], "unsupported", "{err_push_move}");
    assert_eq!(err_push_move["machine"], "old_pi", "{err_push_move}");
    assert!(laptop.home().join("updated.txt").exists());
    assert!(!old_pi.home().join("moved_to_old.txt").exists());

    let err_pull_move = refused(&transfer(
        &laptop,
        json!({"from": "old_pi:~/from_old.txt", "to": "~/moved_from_old.txt", "move": true}),
        &[],
    ));
    assert_eq!(err_pull_move["code"], "unsupported", "{err_pull_move}");
    assert_eq!(err_pull_move["machine"], "old_pi", "{err_pull_move}");
    assert!(old_pi.home().join("from_old.txt").exists());
    assert!(!laptop.home().join("moved_from_old.txt").exists());
}

fn revoking_device_or_policy_stops_running_transfer_and_live_stream() {
    let laptop = node("laptop");
    let pi = node("pi");
    fleet(&laptop, &[("pi", &pi, "slow", vec![])]);

    let pairing_id = "abcdef0123456789abcdef0123456789";
    let device_policy = r#"
[principals.write_only]
role = "write_only"

[principals.rev_bg]
role = "mover"

[principals.stream_rev]
role = "mover"

[principals."device:phone"]
role = "mover"
device = "abcdef0123456789abcdef0123456789"

[roles.write_only]
allow = ["effect:read", "effect:write"]

[roles.mover]
allow = ["effect:read", "effect:write"]

[roles.ro]
allow = ["effect:read"]
"#;
    laptop.write_config("policy.toml", device_policy);
    pi.write_config("policy.toml", device_policy);

    // 1. Background `move: true` requires `effect:destroy` up front (not merely `effect:write`).
    fs::write(laptop.home().join("no_destroy.bin"), b"keep-me").unwrap();
    let err_destroy = refused(&transfer(
        &laptop,
        json!({
            "from": "~/no_destroy.bin",
            "to": "pi:~/no_destroy.bin",
            "move": true,
            "background": true
        }),
        &["--as", "write_only"],
    ));
    assert_eq!(err_destroy["code"], "denied", "{err_destroy}");
    assert!(laptop.home().join("no_destroy.bin").exists());

    // 2. Removing a paired device via `device.remove` cancels its running background transfer job!
    let auth_keys_path = laptop.home().join(".ssh/authorized_keys");
    fs::create_dir_all(auth_keys_path.parent().unwrap()).unwrap();
    fs::write(&auth_keys_path, "").unwrap();
    laptop.write_config(
        "node.toml",
        &format!(
            "name = \"laptop\"\n[devices]\nauthorized_keys = {:?}\n",
            auth_keys_path.to_string_lossy()
        ),
    );
    laptop.write_config(
        "devices.toml",
        &format!(
            r#"
version = 1

[devices.{pairing_id}]
name = "phone"
principal = "device:phone"
role = "mover"
public_key = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAINdamAGCsQq31Uv+08lkBzoO4XLz2qYjJa8CGmj3B1Ea"
paired_at = "2026-11-01T00:00:00Z"
paired_on = "laptop"
updated_at = "2026-11-01T00:00:00Z"
updated_on = "laptop"
name_updated_at = "2026-11-01T00:00:00Z"
name_updated_on = "laptop"
role_updated_at = "2026-11-01T00:00:00Z"
role_updated_on = "laptop"
"#
        ),
    );
    // Reconcile `authorized_keys` from `devices.toml` so `legion gate` accepts the paired key.
    ok(&call_tool(&laptop, "device.list", json!({}), &[]));
    let big = data(12 * 1024 * 1024, 91);
    fs::write(laptop.home().join("phone_big.bin"), &big).unwrap();

    // Start the background transfer through `legion gate --home <dir> --as device:phone --device phone --pairing <id>`.
    let rpc_cmd = remote_command(RemoteOs::Unix, Subcommand::Rpc, None).unwrap();
    let mut gate_child = Command::new(LEGION_BIN)
        .args([
            "gate",
            "--home",
            laptop.state().parent().unwrap().to_str().unwrap(),
            "--as",
            "device:phone",
            "--device",
            "phone",
            "--pairing",
            pairing_id,
        ])
        .env("LEGION_HOME", laptop.state().parent().unwrap())
        .env("HOME", laptop.home())
        .env("USERPROFILE", laptop.home())
        .env("SSH_ORIGINAL_COMMAND", &rpc_cmd)
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("XDG_STATE_HOME")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut gate_in = gate_child.stdin.take().unwrap();
    let mut gate_out = BufReader::new(gate_child.stdout.take().unwrap());
    writeln!(
        gate_in,
        "{}",
        json!({
            "jsonrpc": "2.0",
            "id": "g1",
            "method": "call",
            "params": {
                "tool": "fs.transfer",
                "args": {
                    "from": "~/phone_big.bin",
                    "to": "pi:~/phone_big.bin",
                    "background": true
                }
            }
        })
    )
    .unwrap();
    gate_in.flush().unwrap();
    let mut line = String::new();
    gate_out.read_line(&mut line).unwrap();
    drop(gate_in);
    let mut gate_err = String::new();
    let _ = gate_child
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut gate_err);
    let _ = gate_child.wait();
    let gate_reply: Value = serde_json::from_str(&line)
        .unwrap_or_else(|e| panic!("gate failed ({e}): stdout={line:?} stderr={gate_err:?}"));
    let phone_job_id = gate_reply["result"]["job"]["id"]
        .as_str()
        .unwrap_or_else(|| panic!("gate start failed: {gate_reply}"))
        .to_owned();

    // Wait until the transfer job is actively moving bytes.
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let st = ok(&call_tool(
            &laptop,
            "job.status",
            json!({"id": &phone_job_id}),
            &[],
        ));
        if st["progress"]["bytes_done"].as_u64().unwrap_or(0) > 0 {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "phone transfer made no progress: {st}"
        );
        std::thread::sleep(Duration::from_millis(40));
    }

    // Owner removes `phone`: background transfer MUST be cancelled immediately and NOT left running!
    let removed = ok(&call_tool(
        &laptop,
        "device.remove",
        json!({"name": "phone"}),
        &[],
    ));
    assert_eq!(removed["removed"], true, "{removed}");
    assert!(
        removed["jobs_left_running"]
            .as_array()
            .unwrap()
            .iter()
            .all(|id| id != &phone_job_id),
        "transfer job should not be left running after device.remove: {removed}"
    );
    let st_after_remove = ok(&call_tool(
        &laptop,
        "job.status",
        json!({"id": &phone_job_id}),
        &[],
    ));
    assert_eq!(st_after_remove["state"], "cancelled", "{st_after_remove}");

    // 3. Downgrading a principal's role via `config.write policy.toml` cancels its running transfer!
    let bg_rev = ok(&transfer(
        &laptop,
        json!({
            "from": "~/phone_big.bin",
            "to": "pi:~/rev_bg.bin",
            "background": true
        }),
        &["--as", "rev_bg"],
    ));
    let rev_job_id = bg_rev["job"]["id"].as_str().unwrap().to_owned();
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let st = ok(&call_tool(
            &laptop,
            "job.status",
            json!({"id": &rev_job_id}),
            &[],
        ));
        if st["progress"]["bytes_done"].as_u64().unwrap_or(0) > 0 {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "rev_bg transfer made no progress: {st}"
        );
        std::thread::sleep(Duration::from_millis(40));
    }
    let downgraded_policy = device_policy.replace(
        "[principals.rev_bg]\nrole = \"mover\"",
        "[principals.rev_bg]\nrole = \"ro\"",
    );
    ok(&call_tool(
        &laptop,
        "config.write",
        json!({"document": "policy.toml", "content": downgraded_policy}),
        &[],
    ));
    let st_after_policy = ok(&call_tool(
        &laptop,
        "job.wait",
        json!({"id": &rev_job_id, "timeout_s": 15}),
        &[],
    ));
    assert_eq!(st_after_policy["state"], "cancelled", "{st_after_policy}");

    // 4. Mid-stream 2-second time-based live policy re-check on a VERY slow link (~4 KiB/s, far
    //    below the 4 MiB byte threshold) stops `put` on `pi` within a few seconds!
    let pi_slow = node("pi_slow");
    let pi_stall = node("pi_stall");
    let stall_marker = laptop.home().join("stall.marker");
    pi_slow.write_config("policy.toml", device_policy);
    pi_stall.write_config("policy.toml", device_policy);
    fleet(
        &laptop,
        &[
            ("pi", &pi, "slow", vec![]),
            ("pi_slow", &pi_slow, "very-slow", vec![]),
            (
                "pi_stall",
                &pi_stall,
                "stall-put",
                vec![
                    stall_marker.to_string_lossy().into_owned(),
                    (16 * 1024).to_string(),
                ],
            ),
        ],
    );

    let slow_body = data(2 * 1024 * 1024, 92);
    fs::write(laptop.home().join("stream_rev.bin"), &slow_body).unwrap();
    let bg_stream = ok(&transfer(
        &laptop,
        json!({
            "from": "~/stream_rev.bin",
            "to": "pi_slow:~/stream_rev.bin",
            "background": true
        }),
        &["--as", "stream_rev"],
    ));
    let stream_job_id = bg_stream["job"]["id"].as_str().unwrap().to_owned();
    let part = pi_slow.home().join("stream_rev.bin.legion-part");
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if fs::metadata(&part).map_or(0, |m| m.len()) >= 2 * 1024 {
            break;
        }
        assert!(Instant::now() < deadline, "stream_rev partial never grew");
        std::thread::sleep(Duration::from_millis(40));
    }
    // Revoke `stream_rev`'s write permission on `pi_slow` while the slow stream (~4 KiB/s) is
    // mid-flight (nowhere near 4 MiB).
    let revoked_at = Instant::now();
    pi_slow.write_config(
        "policy.toml",
        &device_policy.replace(
            "[principals.stream_rev]\nrole = \"mover\"",
            "[principals.stream_rev]\nrole = \"ro\"",
        ),
    );
    let stream_done = ok(&call_tool(
        &laptop,
        "job.wait",
        json!({"id": &stream_job_id, "timeout_s": 12}),
        &[],
    ));
    assert_eq!(stream_done["state"], "failed", "{stream_done}");
    assert_eq!(stream_done["error"]["code"], "denied", "{stream_done}");
    assert!(
        revoked_at.elapsed() < Duration::from_secs(10),
        "slow-link revocation took too long: {:?}",
        revoked_at.elapsed()
    );
    assert!(
        fs::metadata(&part).map_or(0, |m| m.len()) < 512 * 1024,
        "slow-link revocation must fire on the 2s timer long before 4 MiB"
    );
    assert!(!pi_slow.home().join("stream_rev.bin").exists());

    // 4b. Mid-stream 2-second time-based live policy re-check on the SENDING (`get`) end at ~4 KiB/s!
    pi_slow.write_config("policy.toml", device_policy);
    fs::write(pi_slow.home().join("pull_rev.bin"), &slow_body).unwrap();
    let bg_pull = ok(&transfer(
        &laptop,
        json!({
            "from": "pi_slow:~/pull_rev.bin",
            "to": "~/pull_rev.bin",
            "background": true
        }),
        &["--as", "stream_rev"],
    ));
    let pull_job_id = bg_pull["job"]["id"].as_str().unwrap().to_owned();
    let pull_part = laptop.home().join("pull_rev.bin.legion-part");
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if fs::metadata(&pull_part).map_or(0, |m| m.len()) >= 2 * 1024 {
            break;
        }
        assert!(Instant::now() < deadline, "pull_rev partial never grew");
        std::thread::sleep(Duration::from_millis(40));
    }
    let pull_revoked_at = Instant::now();
    // Remove `stream_rev` from `pi_slow`'s policy so `get` on `pi_slow` is denied mid-stream!
    pi_slow.write_config(
        "policy.toml",
        &device_policy.replace("[principals.stream_rev]\nrole = \"mover\"", ""),
    );
    let pull_done = ok(&call_tool(
        &laptop,
        "job.wait",
        json!({"id": &pull_job_id, "timeout_s": 12}),
        &[],
    ));
    assert_eq!(pull_done["state"], "failed", "{pull_done}");
    assert!(
        pull_revoked_at.elapsed() < Duration::from_secs(10),
        "slow-link get revocation took too long: {:?}",
        pull_revoked_at.elapsed()
    );
    assert!(
        fs::metadata(&pull_part).map_or(0, |m| m.len()) < 512 * 1024,
        "slow-link get revocation must fire on the 2s timer long before 4 MiB"
    );
    assert!(!laptop.home().join("pull_rev.bin").exists());

    // 5. Completely stalled stream (0 B/s) ALSO stops within a few seconds after revocation!
    let bg_stall = ok(&transfer(
        &laptop,
        json!({
            "from": "~/stream_rev.bin",
            "to": "pi_stall:~/stalled.bin",
            "background": true
        }),
        &["--as", "stream_rev"],
    ));
    let stall_job_id = bg_stall["job"]["id"].as_str().unwrap().to_owned();
    let deadline = Instant::now() + Duration::from_secs(15);
    while !stall_marker.exists() {
        assert!(
            Instant::now() < deadline,
            "stall-put never reached stall point"
        );
        std::thread::sleep(Duration::from_millis(40));
    }
    let stall_revoked_at = Instant::now();
    pi_stall.write_config(
        "policy.toml",
        &device_policy.replace(
            "[principals.stream_rev]\nrole = \"mover\"",
            "[principals.stream_rev]\nrole = \"ro\"",
        ),
    );
    let stall_done = ok(&call_tool(
        &laptop,
        "job.wait",
        json!({"id": &stall_job_id, "timeout_s": 12}),
        &[],
    ));
    assert_eq!(stall_done["state"], "failed", "{stall_done}");
    assert_eq!(stall_done["error"]["code"], "denied", "{stall_done}");
    assert!(
        stall_revoked_at.elapsed() < Duration::from_secs(10),
        "stalled-stream revocation took too long: {:?}",
        stall_revoked_at.elapsed()
    );
    assert!(!pi_stall.home().join("stalled.bin").exists());

    // 6. Unparsable `policy.toml` fails closed and stops an active background transfer!
    let bg_corrupt = ok(&transfer(
        &laptop,
        json!({
            "from": "~/phone_big.bin",
            "to": "pi:~/corrupt_policy.bin",
            "background": true
        }),
        &["--as", "stream_rev"],
    ));
    let corrupt_job_id = bg_corrupt["job"]["id"].as_str().unwrap().to_owned();
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let st = ok(&call_tool(
            &laptop,
            "job.status",
            json!({"id": &corrupt_job_id}),
            &[],
        ));
        if st["progress"]["bytes_done"].as_u64().unwrap_or(0) > 0 {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "corrupt_policy job never started"
        );
        std::thread::sleep(Duration::from_millis(40));
    }
    laptop.write_config("policy.toml", "not valid [ toml ===\n");
    // Restore valid policy ONLY after verifying the job stopped; inspect status via direct read
    // or wait for exit file because `job.wait` itself parses policy.toml.
    let job_dir = laptop.state().join("jobs").join(&corrupt_job_id);
    let deadline = Instant::now() + Duration::from_secs(12);
    while !job_dir.join("exit.json").exists() {
        assert!(
            Instant::now() < deadline,
            "transfer job did not fail closed on unparsable policy.toml"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    laptop.write_config("policy.toml", device_policy);
    let corrupt_done = ok(&call_tool(
        &laptop,
        "job.status",
        json!({"id": &corrupt_job_id}),
        &[],
    ));
    assert!(
        matches!(corrupt_done["state"].as_str(), Some("cancelled" | "failed")),
        "corrupt policy.toml must stop the transfer job: {corrupt_done}"
    );
}

fn stale_partial_restart_reports_honest_per_attempt_progress() {
    let laptop = node("laptop");
    let pi = node("pi");
    let cut_marker = laptop.home().join("restart-cut.marker");
    fleet(
        &laptop,
        &[(
            "pi",
            &pi,
            "cut",
            vec![
                cut_marker.to_string_lossy().into_owned(),
                (64 * 1024).to_string(),
            ],
        )],
    );

    let body = data(128 * 1024, 99);
    fs::write(laptop.home().join("restart.bin"), &body).unwrap();
    // First transfer is cut at 64 KiB so `pi` creates a real `.legion-part` plus its state-dir
    // metadata sidecar (leaving ZERO `.lock` or `.meta` files in `pi`'s user folder).
    let _ = refused(&transfer(
        &laptop,
        json!({"from": "~/restart.bin", "to": "pi:~/restart.bin"}),
        &[],
    ));
    let part = pi.home().join("restart.bin.legion-part");
    assert!(part.exists(), "expected partial file after cut transfer");
    assert!(
        !pi.home().join("restart.bin.legion-part.meta").exists(),
        "meta sidecar must live in state dir, not user folder"
    );
    assert!(
        !pi.home().join("restart.bin.legion-part.lock").exists(),
        "lock sidecar must live in state dir, not user folder"
    );
    // Corrupt the partial bytes in place so attempt 1 resumes from 64 KiB, detects the hash
    // mismatch on finish, and retries cleanly on attempt 2 from offset 0.
    let part_len = fs::metadata(&part).unwrap().len() as usize;
    fs::write(&part, vec![0xAA; part_len]).unwrap();

    let started = ok(&transfer(
        &laptop,
        json!({
            "from": "~/restart.bin",
            "to": "pi:~/restart.bin",
            "background": true
        }),
        &[],
    ));
    let job_id = started["job"]["id"].as_str().unwrap().to_owned();
    let waited = ok(&call_tool(
        &laptop,
        "job.wait",
        json!({"id": &job_id, "timeout_s": 30}),
        &[],
    ));
    assert_eq!(waited["state"], "succeeded", "{waited}");
    assert_eq!(waited["progress"]["attempt"], 2, "{waited}");
    assert_eq!(
        waited["progress"]["bytes_done"],
        body.len() as u64,
        "{waited}"
    );
    assert_eq!(fs::read(pi.home().join("restart.bin")).unwrap(), body);
}

fn move_tree_shared_subfolder_and_inconclusive_probe_keep_source() {
    let laptop = node("laptop");

    // 1. Real Linux bind-mounted subfolder inside a same-node tree move (`dst/shared` bind-mounted
    //    to `src/shared` in an unprivileged user+mount namespace): `src` and `dst` are distinct
    //    roots on the same machine, yet `src/shared` and `dst/shared` are the exact same
    //    directory. Probing every directory pair MUST detect `SameDirectory` for `shared`, keep
    //    `shared/vital.txt` intact, still move distinct `other/ok.txt`, and report both folders!
    #[cfg(target_os = "linux")]
    {
        let can_unshare = Command::new("unshare")
            .args(["-rm", "true"])
            .status()
            .is_ok_and(|s| s.success());
        if can_unshare {
            let src = laptop.home().join("bind_src");
            let dst = laptop.home().join("bind_dst");
            fs::create_dir_all(src.join("shared")).unwrap();
            fs::create_dir_all(src.join("other")).unwrap();
            fs::create_dir_all(dst.join("shared")).unwrap();
            fs::write(src.join("shared/vital.txt"), b"bind-mount-must-survive").unwrap();
            fs::write(src.join("other/ok.txt"), b"move-me").unwrap();

            let script = r#"
                set -euo pipefail
                mount --bind "$1" "$2"
                "$3" fs.transfer from=~/bind_src to=~/bind_dst move=true --json
            "#;
            let out = Command::new("unshare")
                .args([
                    "-rm",
                    "bash",
                    "-c",
                    script,
                    "--",
                    src.join("shared").to_str().unwrap(),
                    dst.join("shared").to_str().unwrap(),
                    LEGION_BIN,
                ])
                .env("LEGION_HOME", legion_home_of(&laptop))
                .env("HOME", laptop.home())
                .env("USERPROFILE", laptop.home())
                .env_remove("XDG_CONFIG_HOME")
                .env_remove("XDG_STATE_HOME")
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "bind-mount tree move failed: stdout={} stderr={}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            );
            let moved: Value = serde_json::from_slice(&out.stdout).unwrap();
            assert_eq!(
                fs::read(src.join("shared/vital.txt")).unwrap(),
                b"bind-mount-must-survive",
                "bind-mounted subfolder file MUST survive tree move: {moved}"
            );
            assert_eq!(
                fs::read(dst.join("other/ok.txt")).unwrap(),
                b"move-me",
                "distinct subfolder file should be moved: {moved}"
            );
            assert!(!src.join("other/ok.txt").exists());
            let skipped = moved["skipped"].as_array().unwrap();
            assert!(
                skipped.iter().any(|entry| {
                    let reason = entry["reason"].as_str().unwrap_or("");
                    reason.contains("bind_src/shared") && reason.contains("bind_dst/shared")
                }),
                "skipped must name both bind-mounted folders: {moved}"
            );
        }
    }

    // 2. Cross-platform inconclusive probe (probe returns `dir: null, marker: null`):
    //    NEVER treat unknown as distinct! Both copy and source stay intact and the report names
    //    the folders that could not be told apart (both single-file and tree moves).
    let pi_inc = node("pi_inc");
    fleet(
        &laptop,
        &[("pi_inc", &pi_inc, "inconclusive-probe", vec![])],
    );

    fs::write(laptop.home().join("inc_one.txt"), b"keep-both-single").unwrap();
    let single_inc = ok(&transfer(
        &laptop,
        json!({"from": "~/inc_one.txt", "to": "pi_inc:~/inc_one.txt", "move": true}),
        &[],
    ));
    assert_eq!(
        single_inc["source_removed"].as_u64().unwrap_or(0),
        0,
        "{single_inc}"
    );
    assert_eq!(
        fs::read(laptop.home().join("inc_one.txt")).unwrap(),
        b"keep-both-single"
    );
    assert_eq!(
        fs::read(pi_inc.home().join("inc_one.txt")).unwrap(),
        b"keep-both-single"
    );
    assert!(
        single_inc["skipped"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["reason"]
                .as_str()
                .unwrap_or("")
                .contains("could not be proven distinct")),
        "expected inconclusive reason in single-file skipped: {single_inc}"
    );

    let inc_src = laptop.home().join("inc_tree");
    fs::create_dir_all(inc_src.join("sub")).unwrap();
    fs::write(inc_src.join("sub/keep.txt"), b"keep-both-tree").unwrap();
    let tree_inc = ok(&transfer(
        &laptop,
        json!({"from": "~/inc_tree", "to": "pi_inc:~/inc_tree", "move": true}),
        &[],
    ));
    assert_eq!(
        tree_inc["source_removed"].as_u64().unwrap_or(0),
        0,
        "{tree_inc}"
    );
    assert_eq!(
        fs::read(inc_src.join("sub/keep.txt")).unwrap(),
        b"keep-both-tree"
    );
    assert_eq!(
        fs::read(pi_inc.home().join("inc_tree/sub/keep.txt")).unwrap(),
        b"keep-both-tree"
    );
    assert!(
        tree_inc["skipped"].as_array().unwrap().iter().any(|entry| {
            let reason = entry["reason"].as_str().unwrap_or("");
            reason.contains("inc_tree/sub") && reason.contains("could not be proven distinct")
        }),
        "skipped must name both folders that could not be told apart: {tree_inc}"
    );

    // 2b. Network file system peer (`network_fs: "nfs"`): even when the probe marker is not seen
    //     on the source (e.g. cached NFS listing on a shared mount), BOTH a normal copy-move AND
    //     an identical-content move treat the pair as inconclusive, keeping the source and stating
    //     the network file system reason.
    let pi_nfs = node("pi_nfs");
    fleet(&laptop, &[("pi_nfs", &pi_nfs, "network-fs-peer", vec![])]);

    fs::write(laptop.home().join("nfs_copy.txt"), b"nfs-payload").unwrap();
    let nfs_moved = ok(&transfer(
        &laptop,
        json!({"from": "~/nfs_copy.txt", "to": "pi_nfs:~/nfs_copy.txt", "move": true}),
        &[],
    ));
    assert_eq!(nfs_moved["files_sent"], 1, "{nfs_moved}");
    assert_eq!(
        nfs_moved["source_removed"].as_u64().unwrap_or(0),
        0,
        "network fs move must keep source: {nfs_moved}"
    );
    assert_eq!(
        fs::read(laptop.home().join("nfs_copy.txt")).unwrap(),
        b"nfs-payload"
    );
    assert_eq!(
        fs::read(pi_nfs.home().join("nfs_copy.txt")).unwrap(),
        b"nfs-payload"
    );
    assert!(
        nfs_moved["skipped"].as_array().unwrap().iter().any(|e| {
            let r = e["reason"].as_str().unwrap_or("");
            r.contains("network file system") && r.contains("nfs")
        }),
        "expected network file system reason in skipped: {nfs_moved}"
    );

    // Identical-content branch (file already exists on destination with exact same bytes):
    // must STILL keep source instead of deleting the only copy on a shared mount!
    let nfs_same = ok(&transfer(
        &laptop,
        json!({"from": "~/nfs_copy.txt", "to": "pi_nfs:~/nfs_copy.txt", "move": true}),
        &[],
    ));
    assert_eq!(nfs_same["files_unchanged"], 1, "{nfs_same}");
    assert_eq!(
        nfs_same["source_removed"].as_u64().unwrap_or(0),
        0,
        "identical-content move on network fs must keep source: {nfs_same}"
    );
    assert_eq!(
        fs::read(laptop.home().join("nfs_copy.txt")).unwrap(),
        b"nfs-payload"
    );
    assert!(
        nfs_same["skipped"].as_array().unwrap().iter().any(|e| {
            let r = e["reason"].as_str().unwrap_or("");
            r.contains("network file system") && r.contains("nfs")
        }),
        "expected network file system reason on identical-content move: {nfs_same}"
    );

    // 2c. Unknown filesystem locality (`fs_locality: {"kind":"unknown",...}`, e.g. `fuse.mergerfs`):
    //     only proven-local filesystems may conclude distinctness; `Unknown` keeps the source and
    //     states the reason.
    let pi_unk = node("pi_unk");
    fleet(&laptop, &[("pi_unk", &pi_unk, "unknown-fs-peer", vec![])]);
    fs::write(laptop.home().join("unk_copy.txt"), b"unknown-fs-payload").unwrap();
    let unk_moved = ok(&transfer(
        &laptop,
        json!({"from": "~/unk_copy.txt", "to": "pi_unk:~/unk_copy.txt", "move": true}),
        &[],
    ));
    assert_eq!(unk_moved["files_sent"], 1, "{unk_moved}");
    assert_eq!(
        unk_moved["source_removed"].as_u64().unwrap_or(0),
        0,
        "unknown fs move must keep source: {unk_moved}"
    );
    assert_eq!(
        fs::read(laptop.home().join("unk_copy.txt")).unwrap(),
        b"unknown-fs-payload"
    );
    assert_eq!(
        fs::read(pi_unk.home().join("unk_copy.txt")).unwrap(),
        b"unknown-fs-payload"
    );
    assert!(
        unk_moved["skipped"].as_array().unwrap().iter().any(|e| {
            let r = e["reason"].as_str().unwrap_or("");
            r.contains("fuse.mergerfs") && r.contains("local file system")
        }),
        "expected unknown fs reason in skipped: {unk_moved}"
    );

    // 2d. Unconfirmed held probe marker (peer exits without replying `{"held":true}` to
    //     `"confirm\n"`): move treats the pair as inconclusive and keeps the source.
    let pi_unconf = node("pi_unconf");
    fleet(
        &laptop,
        &[("pi_unconf", &pi_unconf, "unconfirmed-probe-peer", vec![])],
    );
    fs::write(laptop.home().join("unconf_copy.txt"), b"unconfirmed-payload").unwrap();
    let unconf_moved = ok(&transfer(
        &laptop,
        json!({"from": "~/unconf_copy.txt", "to": "pi_unconf:~/unconf_copy.txt", "move": true}),
        &[],
    ));
    assert_eq!(unconf_moved["files_sent"], 1, "{unconf_moved}");
    assert_eq!(
        unconf_moved["source_removed"].as_u64().unwrap_or(0),
        0,
        "unconfirmed held probe must keep source: {unconf_moved}"
    );
    assert_eq!(
        fs::read(laptop.home().join("unconf_copy.txt")).unwrap(),
        b"unconfirmed-payload"
    );
    assert_eq!(
        fs::read(pi_unconf.home().join("unconf_copy.txt")).unwrap(),
        b"unconfirmed-payload"
    );
    assert!(
        unconf_moved["skipped"].as_array().unwrap().iter().any(|e| {
            let r = e["reason"].as_str().unwrap_or("");
            r.contains("probe marker") && r.contains("not confirmed held")
        }),
        "expected unconfirmed probe reason in skipped: {unconf_moved}"
    );

    // 3. Real OS read-only destination subfolder (Unix): `already_verified` matches the file in
    //    `dst/ro_sub`, `probe_write` fails with permission denied -> inconclusive -> source kept!
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let src = laptop.home().join("inc_src");
        let dst = laptop.home().join("inc_dst");
        fs::create_dir_all(src.join("ro_sub")).unwrap();
        fs::create_dir_all(dst.join("ro_sub")).unwrap();
        fs::write(src.join("ro_sub/keep.txt"), b"same-content").unwrap();
        fs::write(dst.join("ro_sub/keep.txt"), b"same-content").unwrap();
        fs::set_permissions(dst.join("ro_sub"), fs::Permissions::from_mode(0o555)).unwrap();

        let moved = ok(&transfer(
            &laptop,
            json!({"from": "~/inc_src", "to": "~/inc_dst", "move": true}),
            &[],
        ));
        let _ = fs::set_permissions(dst.join("ro_sub"), fs::Permissions::from_mode(0o755));

        assert_eq!(
            fs::read(src.join("ro_sub/keep.txt")).unwrap(),
            b"same-content",
            "inconclusive probe MUST keep source file intact: {moved}"
        );
        assert_eq!(
            fs::read(dst.join("ro_sub/keep.txt")).unwrap(),
            b"same-content",
            "destination copy must also stay intact: {moved}"
        );
        assert_eq!(moved["source_removed"].as_u64().unwrap_or(0), 0, "{moved}");
        let skipped = moved["skipped"].as_array().unwrap();
        assert!(
            skipped.iter().any(|entry| {
                let reason = entry["reason"].as_str().unwrap_or("");
                reason.contains("inc_src/ro_sub") && reason.contains("inc_dst/ro_sub")
            }),
            "skipped must name both folders that could not be told apart: {moved}"
        );
    }
}

fn crossing_moves_same_node_and_two_nodes_never_lose_data() {
    let payload = b"identical-crossing-move-payload-must-survive";

    // 1. Same-node crossing moves: `p/a -> q/a` and `q/a -> p/a` simultaneously for 20 rounds.
    let laptop = node("laptop");
    fs::create_dir_all(laptop.home().join("p")).unwrap();
    fs::create_dir_all(laptop.home().join("q")).unwrap();
    let p_a = laptop.home().join("p/a.bin");
    let q_a = laptop.home().join("q/a.bin");

    for round in 0..20 {
        fs::write(&p_a, payload).unwrap();
        fs::write(&q_a, payload).unwrap();
        let barrier = std::sync::Barrier::new(2);
        let (r1, r2) = std::thread::scope(|scope| {
            let h1 = scope.spawn(|| {
                barrier.wait();
                transfer(
                    &laptop,
                    json!({"from": "~/p/a.bin", "to": "~/q/a.bin", "move": true}),
                    &[],
                )
            });
            let h2 = scope.spawn(|| {
                barrier.wait();
                transfer(
                    &laptop,
                    json!({"from": "~/q/a.bin", "to": "~/p/a.bin", "move": true}),
                    &[],
                )
            });
            (h1.join().unwrap(), h2.join().unwrap())
        });
        let p_ok = fs::read(&p_a).ok().as_deref() == Some(payload.as_slice());
        let q_ok = fs::read(&q_a).ok().as_deref() == Some(payload.as_slice());
        assert!(
            p_ok || q_ok,
            "same-node crossing move round {round} lost BOTH copies! r1={} / {} r2={} / {}",
            r1.stdout,
            r1.stderr,
            r2.stdout,
            r2.stderr
        );
    }

    // 2. Two-node crossing moves: `laptop:~/c/a -> pi:~/c/a` and `pi:~/c/a -> laptop:~/c/a`
    //    simultaneously for 12 rounds.
    let pi = node("pi");
    fleet(&laptop, &[("pi", &pi, "node", vec![])]);
    fleet(&pi, &[("laptop", &laptop, "node", vec![])]);
    fs::create_dir_all(laptop.home().join("c")).unwrap();
    fs::create_dir_all(pi.home().join("c")).unwrap();
    let la = laptop.home().join("c/a.bin");
    let pa = pi.home().join("c/a.bin");

    for round in 0..12 {
        fs::write(&la, payload).unwrap();
        fs::write(&pa, payload).unwrap();
        let barrier = std::sync::Barrier::new(2);
        let (r1, r2) = std::thread::scope(|scope| {
            let h1 = scope.spawn(|| {
                barrier.wait();
                transfer(
                    &laptop,
                    json!({"from": "~/c/a.bin", "to": "pi:~/c/a.bin", "move": true}),
                    &[],
                )
            });
            let h2 = scope.spawn(|| {
                barrier.wait();
                transfer(
                    &pi,
                    json!({"from": "~/c/a.bin", "to": "laptop:~/c/a.bin", "move": true}),
                    &[],
                )
            });
            (h1.join().unwrap(), h2.join().unwrap())
        });
        let l_ok = fs::read(&la).ok().as_deref() == Some(payload.as_slice());
        let p_ok = fs::read(&pa).ok().as_deref() == Some(payload.as_slice());
        assert!(
            l_ok || p_ok,
            "two-node crossing move round {round} lost BOTH copies! r1={} / {} r2={} / {}",
            r1.stdout,
            r1.stderr,
            r2.stdout,
            r2.stderr
        );
    }
}

fn stream_rename_privilege_and_visible_interrupted_recovery() {
    let laptop = node("laptop");
    laptop.write_config(
        "policy.toml",
        r#"
[principals.nonadmin]
role = "power"

[roles.power]
allow = ["effect:read", "effect:write", "effect:destroy"]
"#,
    );
    fs::create_dir_all(laptop.home().join(".config")).unwrap();
    fs::write(laptop.home().join(".config/policy.toml"), b"keep-config").unwrap();
    fs::write(laptop.home().join("plain.txt"), b"keep-plain").unwrap();

    // 1. Non-admin with read+write+destroy cannot use stream `rename` on `~/.config`, `~`, or
    //    distinct entries (`plain.txt -> other.txt`).
    let run_as = |header: Value| -> Value {
        let mut h = header;
        h["protocol"] = json!(1);
        let mut child = Command::new(LEGION_BIN)
            .args(["stream", "--as", "nonadmin"])
            .env("LEGION_HOME", legion_home_of(&laptop))
            .env("HOME", laptop.home())
            .env("USERPROFILE", laptop.home())
            .env_remove("XDG_CONFIG_HOME")
            .env_remove("XDG_STATE_HOME")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        {
            let mut stdin = child.stdin.take().unwrap();
            writeln!(stdin, "{h}").unwrap();
        }
        let out = child.wait_with_output().unwrap();
        let stdout = String::from_utf8_lossy(&out.stdout);
        let lines: Vec<&str> = stdout.lines().collect();
        serde_json::from_str(lines[1]).unwrap()
    };

    let r_cfg = run_as(json!({"op": "rename", "from": "~/.config", "to": "~/.config2"}));
    assert_eq!(r_cfg["error"]["code"], "denied", "{r_cfg}");
    assert!(laptop.home().join(".config/policy.toml").exists());

    let parent_home2 = laptop
        .home()
        .parent()
        .unwrap()
        .join("home2")
        .to_string_lossy()
        .into_owned();
    let r_home = run_as(json!({"op": "rename", "from": "~", "to": parent_home2}));
    assert_eq!(r_home["error"]["code"], "denied", "{r_home}");

    let r_distinct = run_as(json!({"op": "rename", "from": "~/plain.txt", "to": "~/other.txt"}));
    assert_eq!(r_distinct["error"]["code"], "invalid_args", "{r_distinct}");
    assert!(laptop.home().join("plain.txt").exists());
    assert!(!laptop.home().join("other.txt").exists());

    // 2. Simulated interrupted two-step rename: visible `legion-renaming-*` file + state-dir
    //    intent JSON. A subsequent `stream` `manifest` recovers it cleanly to `target.txt` and
    //    clears the intent record!
    let rec_dir = laptop.home().join("rec");
    fs::create_dir_all(&rec_dir).unwrap();
    let temp_name = "legion-renaming-999-1-Target.txt";
    fs::write(rec_dir.join(temp_name), b"recovered-body").unwrap();
    let renames_dir = laptop.state().join("xfer").join("renames");
    fs::create_dir_all(&renames_dir).unwrap();
    let intent_path = renames_dir.join("test-intent.json");
    fs::write(
        &intent_path,
        serde_json::to_vec(&json!({
            "parent": rec_dir.to_string_lossy(),
            "from_name": "Target.txt",
            "temp_name": temp_name,
            "to_name": "target.txt",
        }))
        .unwrap(),
    )
    .unwrap();

    // Running `stat` on `rec/target.txt` triggers recovery automatically.
    let st = stream_request(&laptop, json!({"op": "stat", "path": "~/rec/target.txt"}));
    assert_eq!(st["kind"], "file", "{st}");
    assert_eq!(
        fs::read(rec_dir.join("target.txt")).unwrap(),
        b"recovered-body"
    );
    assert!(!rec_dir.join(temp_name).exists());
    assert!(!intent_path.exists());
}
