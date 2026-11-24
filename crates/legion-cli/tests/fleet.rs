//! End-to-end multi-node fleet routing, sessions, fan-out, cancellation, events, multi-system
//! probing, hop-limit loop protection, and `fleet.*` tools using the real `legion` binary over the
//! `command` connector with isolated temporary `LEGION_HOME` directories.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, ExitCode, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

use common::{Home, PATIENCE};
use serde_json::{Value, json};

const LEGION_BIN: &str = env!("CARGO_BIN_EXE_legion");
const HELPER_FLAG: &str = "--fleet-helper";

type Test = (&'static str, fn());

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some(HELPER_FLAG) {
        return run_helper(&args[1..]);
    }

    let filter = args
        .iter()
        .find(|arg| !arg.starts_with('-'))
        .map(String::as_str);
    let tests: &[Test] = &[
        (
            "routes_call_to_remote_node_via_cli_rpc_and_mcp",
            routes_call_to_remote_node_via_cli_rpc_and_mcp,
        ),
        (
            "forwards_principal_and_applies_remote_policy_with_machine",
            forwards_principal_and_applies_remote_policy_with_machine,
        ),
        (
            "forwarded_calls_are_authorised_and_audited_on_the_forwarding_node",
            forwarded_calls_are_authorised_and_audited_on_the_forwarding_node,
        ),
        (
            "forwards_events_and_cancellation_to_remote_node",
            forwards_events_and_cancellation_to_remote_node,
        ),
        (
            "sigterm_and_sighup_cancel_remote_work_and_shut_down_cleanly",
            sigterm_and_sighup_cancel_remote_work_and_shut_down_cleanly,
        ),
        (
            "multiplexes_concurrent_calls_and_reuses_one_session",
            multiplexes_concurrent_calls_and_reuses_one_session,
        ),
        (
            "session_death_mid_call_is_unreachable_and_next_call_reconnects",
            session_death_mid_call_is_unreachable_and_next_call_reconnects,
        ),
        (
            "idle_session_death_reopens_transparently_when_unwritten",
            idle_session_death_reopens_transparently_when_unwritten,
        ),
        (
            "skips_noise_lines_before_hello_and_reports_protocol_mismatch",
            skips_noise_lines_before_hello_and_reports_protocol_mismatch,
        ),
        (
            "fan_out_all_partial_failure_all_failed_and_unknown_machine",
            fan_out_all_partial_failure_all_failed_and_unknown_machine,
        ),
        (
            "two_system_machine_falls_back_and_remembers_winner",
            two_system_machine_falls_back_and_remembers_winner,
        ),
        (
            "hop_limit_stops_deliberate_loop_and_allows_valid_multi_hop",
            hop_limit_stops_deliberate_loop_and_allows_valid_multi_hop,
        ),
        (
            "remote_machine_labels_are_overwritten_on_errors_and_events",
            remote_machine_labels_are_overwritten_on_errors_and_events,
        ),
        (
            "fleet_tools_list_ping_add_remove_and_discover_permissions",
            fleet_tools_list_ping_add_remove_and_discover_permissions,
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

/// Cross-platform child helper modes invoked by `fleet.toml` `command = [...]` entries.
fn run_helper(args: &[String]) -> ExitCode {
    let Some((mode, rest)) = args.split_first() else {
        eprintln!("missing helper mode");
        return ExitCode::from(2);
    };
    match mode.as_str() {
        // run <legion_bin> <legion_home> <user_home> [spawn_log]
        "run" => {
            let legion = &rest[0];
            let legion_home = &rest[1];
            let user_home = &rest[2];
            if let Some(log) = rest.get(3) {
                append_line(Path::new(log), "spawn");
            }
            let status = spawn_legion_rpc(legion, legion_home, user_home, false)
                .wait()
                .unwrap();
            exit_from_status(status)
        }
        // noise <legion_bin> <legion_home> <user_home>
        "noise" => {
            let mut stdout = std::io::stdout().lock();
            writeln!(stdout, "Welcome to pi.example!").unwrap();
            writeln!(stdout, "Last login: Mon Nov 23 20:00:00 2026").unwrap();
            writeln!(stdout, "{{\"banner\": true}}").unwrap();
            stdout.flush().unwrap();
            drop(stdout);
            let status = spawn_legion_rpc(&rest[0], &rest[1], &rest[2], false)
                .wait()
                .unwrap();
            exit_from_status(status)
        }
        // die-mid-call <legion_bin> <legion_home> <user_home> <marker_file>
        "die-mid-call" => {
            let marker = Path::new(&rest[3]);
            if marker.exists() {
                let status = spawn_legion_rpc(&rest[0], &rest[1], &rest[2], false)
                    .wait()
                    .unwrap();
                return exit_from_status(status);
            }
            fs::write(marker, b"1").unwrap();
            let mut child = spawn_legion_rpc(&rest[0], &rest[1], &rest[2], true);
            let mut child_in = child.stdin.take().unwrap();
            let child_out = child.stdout.take().unwrap();
            let pump = std::thread::spawn(move || {
                let mut out = std::io::stdout();
                for line in BufReader::new(child_out).lines() {
                    let Ok(line) = line else { break };
                    if writeln!(out, "{line}").is_err() {
                        break;
                    }
                    let _ = out.flush();
                }
            });
            for line in BufReader::new(std::io::stdin()).lines() {
                let Ok(line) = line else { break };
                if line.contains("\"method\":\"call\"") {
                    let _ = writeln!(std::io::stderr(), "simulated ssh transport drop");
                    let _ = std::io::stderr().flush();
                    let _ = child.kill();
                    let _ = child.wait();
                    return ExitCode::from(255);
                }
                if writeln!(child_in, "{line}").is_err() {
                    break;
                }
                let _ = child_in.flush();
            }
            drop(child_in);
            let _ = pump.join();
            let status = child.wait().unwrap();
            exit_from_status(status)
        }
        // die-after-first-call <legion_bin> <legion_home> <user_home> <marker_file>
        "die-after-first-call" => {
            let marker = Path::new(&rest[3]);
            if marker.exists() {
                let status = spawn_legion_rpc(&rest[0], &rest[1], &rest[2], false)
                    .wait()
                    .unwrap();
                return exit_from_status(status);
            }
            fs::write(marker, b"1").unwrap();
            let mut child = spawn_legion_rpc(&rest[0], &rest[1], &rest[2], true);
            let mut child_in = child.stdin.take().unwrap();
            let child_out = child.stdout.take().unwrap();
            let (done_tx, done_rx) = mpsc::channel();
            let pump = std::thread::spawn(move || {
                let mut out = std::io::stdout();
                let mut replies = 0usize;
                for line in BufReader::new(child_out).lines() {
                    let Ok(line) = line else { break };
                    if writeln!(out, "{line}").is_err() {
                        break;
                    }
                    let _ = out.flush();
                    if let Ok(val) = serde_json::from_str::<Value>(&line)
                        && val.get("method").is_none()
                    {
                        replies += 1;
                        // Reply 1 is hello, reply 2 is the first call.
                        if replies >= 2 {
                            let _ = done_tx.send(());
                            break;
                        }
                    }
                }
            });
            let reader = std::thread::spawn(move || {
                for line in BufReader::new(std::io::stdin()).lines() {
                    let Ok(line) = line else { break };
                    if writeln!(child_in, "{line}").is_err() {
                        break;
                    }
                    let _ = child_in.flush();
                }
            });
            let _ = done_rx.recv_timeout(PATIENCE);
            let _ = child.kill();
            let _ = child.wait();
            let _ = pump.join();
            drop(reader);
            ExitCode::from(1)
        }
        // mismatch
        "mismatch" => {
            let mut stdin = BufReader::new(std::io::stdin());
            let mut first = String::new();
            let _ = stdin.read_line(&mut first);
            let reply = json!({
                "jsonrpc": "2.0",
                "id": 1,
                "error": {
                    "code": -32000,
                    "message": "This node speaks protocol 99.",
                    "data": {
                        "code": "unsupported",
                        "message": "This node speaks protocol 99.",
                        "details": {"supported": [99]}
                    }
                }
            });
            println!("{reply}");
            let _ = std::io::stdout().flush();
            ExitCode::SUCCESS
        }
        // hold <pid_file>
        // Writes its own PID then waits up to 30s so signal tests can verify remote child cleanup.
        "hold" => {
            let pid_file = Path::new(&rest[0]);
            fs::write(pid_file, std::process::id().to_string()).unwrap();
            for _ in 0..600 {
                std::thread::sleep(Duration::from_millis(50));
            }
            ExitCode::SUCCESS
        }
        // fail <log_file> <stderr_message>
        "fail" => {
            append_line(Path::new(&rest[0]), "attempt");
            eprintln!("{}", rest[1]);
            ExitCode::from(1)
        }
        // spoof
        // Emits an event and an error falsely labelled `machine: "tower"` to verify the caller
        // overwrites remote-chosen machine labels.
        "spoof" => {
            let mut stdout = std::io::stdout().lock();
            for line in BufReader::new(std::io::stdin()).lines() {
                let Ok(line) = line else { break };
                let Ok(req) = serde_json::from_str::<Value>(&line) else {
                    continue;
                };
                let id = req.get("id").cloned().unwrap_or(Value::Null);
                match req["method"].as_str().unwrap_or_default() {
                    "hello" => {
                        let reply = json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "result": {
                                "node": "real-remote",
                                "version": "0.1.0",
                                "protocol": 1,
                                "os": std::env::consts::OS,
                                "arch": std::env::consts::ARCH,
                                "systems": []
                            }
                        });
                        writeln!(stdout, "{reply}").unwrap();
                    }
                    "call" => {
                        let ev = json!({
                            "jsonrpc": "2.0",
                            "method": "event",
                            "params": {
                                "id": id,
                                "kind": "progress",
                                "data": {"step": 1},
                                "machine": "tower"
                            }
                        });
                        writeln!(stdout, "{ev}").unwrap();
                        let reply = json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "error": {
                                "code": -32000,
                                "message": "spoofed failure",
                                "data": {
                                    "code": "failed",
                                    "message": "spoofed failure",
                                    "machine": "tower"
                                }
                            }
                        });
                        writeln!(stdout, "{reply}").unwrap();
                    }
                    _ => {
                        let reply = json!({"jsonrpc": "2.0", "id": id, "result": {}});
                        writeln!(stdout, "{reply}").unwrap();
                    }
                }
                stdout.flush().unwrap();
            }
            ExitCode::SUCCESS
        }
        // ext-tools <call_log>
        // Serves a fake remote node advertising extension tools `ext.probe` (read) and
        // `ext.zap` (exec) so forwarding-node extension authorisation can be tested end to end.
        "ext-tools" => {
            let call_log = Path::new(&rest[0]);
            let mut stdout = std::io::stdout().lock();
            for line in BufReader::new(std::io::stdin()).lines() {
                let Ok(line) = line else { break };
                let Ok(req) = serde_json::from_str::<Value>(&line) else {
                    continue;
                };
                let id = req.get("id").cloned().unwrap_or(Value::Null);
                let reply = match req["method"].as_str().unwrap_or_default() {
                    "hello" => json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": {
                            "node": "extbox",
                            "version": "0.1.0",
                            "protocol": 1,
                            "os": std::env::consts::OS,
                            "arch": std::env::consts::ARCH,
                            "systems": []
                        }
                    }),
                    "tools" => json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": {
                            "tools": [
                                {
                                    "name": "ext.probe",
                                    "description": "Read-only extension.",
                                    "effect": "read",
                                    "input_schema": {"type": "object"}
                                },
                                {
                                    "name": "ext.zap",
                                    "description": "Exec extension.",
                                    "effect": "exec",
                                    "input_schema": {"type": "object"}
                                }
                            ]
                        }
                    }),
                    "call" => {
                        let tool = req["params"]["tool"].as_str().unwrap_or_default();
                        append_line(call_log, tool);
                        if tool == "ext.probe" || tool == "ext.zap" {
                            json!({
                                "jsonrpc": "2.0",
                                "id": id,
                                "result": {"ran": tool}
                            })
                        } else {
                            json!({
                                "jsonrpc": "2.0",
                                "id": id,
                                "error": {
                                    "code": -32000,
                                    "message": format!("There is no tool called {tool}."),
                                    "data": {
                                        "code": "unknown_tool",
                                        "message": format!("There is no tool called {tool}."),
                                        "machine": "extbox"
                                    }
                                }
                            })
                        }
                    }
                    _ => json!({"jsonrpc": "2.0", "id": id, "result": {}}),
                };
                writeln!(stdout, "{reply}").unwrap();
                stdout.flush().unwrap();
            }
            ExitCode::SUCCESS
        }
        other => {
            eprintln!("unknown helper mode: {other}");
            ExitCode::from(2)
        }
    }
}

fn spawn_legion_rpc(legion: &str, legion_home: &str, user_home: &str, piped: bool) -> Child {
    let mut cmd = Command::new(legion);
    cmd.arg("rpc")
        .env("LEGION_HOME", legion_home)
        .env("HOME", user_home)
        .env("USERPROFILE", user_home)
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("XDG_STATE_HOME");
    if piped {
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
    } else {
        cmd.stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit());
    }
    cmd.spawn().unwrap()
}

fn exit_from_status(status: std::process::ExitStatus) -> ExitCode {
    let code = status.code().unwrap_or(1).clamp(0, 255) as u8;
    ExitCode::from(code)
}

fn append_line(path: &Path, line: &str) {
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .unwrap();
    writeln!(file, "{line}").unwrap();
}

fn count_lines(path: &Path) -> usize {
    fs::read_to_string(path).unwrap_or_default().lines().count()
}

fn this_test_exe() -> String {
    std::env::current_exe()
        .unwrap()
        .to_string_lossy()
        .into_owned()
}

fn legion_home_of(home: &Home) -> PathBuf {
    home.state().parent().unwrap().to_path_buf()
}

fn toml_argv(items: &[String]) -> String {
    let parts: Vec<String> = items
        .iter()
        .map(|s| serde_json::to_string(s).unwrap())
        .collect();
    format!("[{}]", parts.join(", "))
}

fn run_argv(target: &Home, spawn_log: Option<&Path>) -> Vec<String> {
    let mut argv = vec![
        this_test_exe(),
        HELPER_FLAG.to_owned(),
        "run".to_owned(),
        LEGION_BIN.to_owned(),
        legion_home_of(target).to_string_lossy().into_owned(),
        target.home().to_string_lossy().into_owned(),
    ];
    if let Some(log) = spawn_log {
        argv.push(log.to_string_lossy().into_owned());
    }
    argv
}

fn helper_argv(mode: &str, extra: &[String]) -> Vec<String> {
    let mut argv = vec![this_test_exe(), HELPER_FLAG.to_owned(), mode.to_owned()];
    argv.extend_from_slice(extra);
    argv
}

fn rpc_request(id: Value, method: &str, params: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params})
}

/// Minimal MCP client for verifying remote routing and fan-out through `legion mcp`.
struct McpClient {
    child: Child,
    stdin: Option<ChildStdin>,
    lines: Receiver<String>,
    next_id: u64,
}

impl McpClient {
    fn start(home: &Home, args: &[&str]) -> McpClient {
        let mut full = vec!["mcp"];
        full.extend_from_slice(args);
        let mut child = home
            .command(&full)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let stdin = child.stdin.take();
        let stdout = child.stdout.take().unwrap();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        let mut client = McpClient {
            child,
            stdin,
            lines: rx,
            next_id: 0,
        };
        let _ = client.request(
            "initialize",
            json!({
                "protocolVersion": "2025-11-25",
                "capabilities": {},
                "clientInfo": {"name": "test", "version": "0"}
            }),
        );
        client.notify("notifications/initialized", json!({}));
        client
    }

    fn send(&mut self, msg: &Value) {
        let stdin = self.stdin.as_mut().unwrap();
        writeln!(stdin, "{msg}").unwrap();
        stdin.flush().unwrap();
    }

    fn notify(&mut self, method: &str, params: Value) {
        self.send(&json!({"jsonrpc": "2.0", "method": method, "params": params}));
    }

    fn request(&mut self, method: &str, params: Value) -> Value {
        self.next_id += 1;
        let id = json!(self.next_id);
        self.send(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));
        loop {
            let line = self.lines.recv_timeout(PATIENCE).unwrap();
            let msg: Value = serde_json::from_str(&line).unwrap();
            if msg.get("id") == Some(&id) && msg.get("method").is_none() {
                assert!(msg.get("error").is_none(), "{msg}");
                return msg["result"].clone();
            }
        }
    }

    fn call_with_events(
        &mut self,
        tool: &str,
        arguments: Value,
        progress_token: Option<&str>,
    ) -> (Value, Vec<Value>) {
        self.next_id += 1;
        let id = json!(self.next_id);
        let mut params = json!({"name": tool, "arguments": arguments});
        if let Some(tok) = progress_token {
            params["_meta"] = json!({"progressToken": tok});
        }
        self.send(&json!({"jsonrpc": "2.0", "id": id, "method": "tools/call", "params": params}));
        let mut notes = Vec::new();
        loop {
            let line = self.lines.recv_timeout(PATIENCE).unwrap();
            let msg: Value = serde_json::from_str(&line).unwrap();
            if msg.get("id") == Some(&id) && msg.get("method").is_none() {
                assert!(msg.get("error").is_none(), "{msg}");
                return (msg["result"].clone(), notes);
            }
            notes.push(msg);
        }
    }

    fn finish(&mut self) -> i32 {
        drop(self.stdin.take());
        let deadline = Instant::now() + PATIENCE;
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                return status.code().unwrap_or(-1);
            }
            assert!(Instant::now() < deadline, "legion mcp did not exit");
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

impl Drop for McpClient {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Extracts the single compact JSON text payload from an MCP tool result.
fn mcp_payload(result: &Value) -> Value {
    assert_eq!(result.get("structuredContent"), None, "{result}");
    let text = result["content"][0]["text"].as_str().unwrap();
    assert!(!text.contains('\n'), "{text}");
    serde_json::from_str(text).unwrap()
}

fn routes_call_to_remote_node_via_cli_rpc_and_mcp() {
    let laptop = Home::new();
    let pi = Home::new();
    laptop.write_config("node.toml", "name = \"laptop\"\n");
    pi.write_config("node.toml", "name = \"pi\"\n");
    laptop.write_config(
        "fleet.toml",
        &format!(
            "[machines.pi]\ncommand = {}\n",
            toml_argv(&run_argv(&pi, None))
        ),
    );

    // 1. CLI one-shot
    let cli = laptop.run(&["meta.version", "--on", "pi"]);
    assert_eq!(cli.code, 0, "stderr: {}", cli.stderr);
    assert_eq!(cli.json()["node"], json!("pi"));

    // 2. legion rpc
    let mut rpc = laptop.session(&[]);
    rpc.send_json(&rpc_request(
        json!(1),
        "call",
        json!({"tool": "meta.version", "on": "pi"}),
    ));
    let (reply, _) = rpc.reply_to(&json!(1));
    assert_eq!(reply["result"]["node"], json!("pi"));
    assert_eq!(rpc.finish(), 0);

    // 3. legion mcp (also verifies all fleet.* tools register cleanly with MCP catalog)
    let mut mcp = McpClient::start(&laptop, &[]);
    let tools = mcp.request("tools/list", json!({}));
    let names: Vec<&str> = tools["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    for expected in [
        "fleet_list",
        "fleet_ping",
        "fleet_add",
        "fleet_remove",
        "fleet_discover",
    ] {
        assert!(
            names.contains(&expected),
            "{expected} missing from {names:?}"
        );
    }
    let (res, _) = mcp.call_with_events("meta_version", json!({"on": "pi"}), None);
    assert_eq!(res["isError"], json!(false), "{res}");
    assert_eq!(mcp_payload(&res)["node"], json!("pi"));
    assert_eq!(mcp.finish(), 0);
}

fn forwards_principal_and_applies_remote_policy_with_machine() {
    let laptop = Home::new();
    let pi = Home::new();
    laptop.write_config("node.toml", "name = \"laptop\"\n");
    pi.write_config("node.toml", "name = \"pi\"\n");
    // laptop allows `phone` both read and write, while pi confines `phone` to read only, so a
    // call to `test.write_path` passes laptop's policy and is denied by pi's policy.
    laptop.write_config(
        "policy.toml",
        r#"
[principals.phone]
role = "writer"

[roles.writer]
allow = ["effect:read", "effect:write"]
"#,
    );
    pi.write_config(
        "policy.toml",
        r#"
[principals.phone]
role = "observer"

[roles.observer]
allow = ["effect:read"]
"#,
    );
    laptop.write_config(
        "fleet.toml",
        &format!(
            "[machines.pi]\ncommand = {}\n",
            toml_argv(&run_argv(&pi, None))
        ),
    );

    // CLI --as forwards principal; remote allows read and denies write, tagging machine = "pi".
    let whoami = laptop.run(&["meta.whoami", "--on", "pi", "--as", "phone"]);
    assert_eq!(whoami.code, 0, "{}", whoami.stdout);
    assert_eq!(whoami.json()["principal"], json!("phone"));
    assert_eq!(whoami.json()["node"], json!("pi"));

    let denied = laptop.run(&[
        "test.write_path",
        "path=~/projects/a",
        "--on",
        "pi",
        "--as",
        "phone",
    ]);
    assert_eq!(denied.code, 1);
    let err = denied.json();
    assert_eq!(err["code"], json!("denied"));
    assert_eq!(err["machine"], json!("pi"));

    // Pinned MCP session cannot escalate on the remote node.
    let mut mcp = McpClient::start(&laptop, &["--as", "phone"]);
    let (res, _) = mcp.call_with_events("meta_whoami", json!({"on": "pi"}), None);
    assert_eq!(res["isError"], json!(false));
    assert_eq!(mcp_payload(&res)["principal"], json!("phone"));
    let (denied_mcp, _) = mcp.call_with_events(
        "test_write_path",
        json!({"path": "~/projects/a", "on": "pi"}),
        None,
    );
    assert_eq!(denied_mcp["isError"], json!(true));
    let denied_body = mcp_payload(&denied_mcp);
    assert_eq!(denied_body["code"], json!("denied"));
    assert_eq!(denied_body["machine"], json!("pi"));
    assert_eq!(mcp.finish(), 0);
}

fn forwarded_calls_are_authorised_and_audited_on_the_forwarding_node() {
    let laptop = Home::new();
    let pi = Home::new();
    let pi_spawns = laptop.state().join("pi-auth-spawns.log");
    let ext_calls = laptop.state().join("ext-calls.log");
    fs::create_dir_all(laptop.state()).unwrap();

    laptop.write_config("node.toml", "name = \"laptop\"\n");
    pi.write_config("node.toml", "name = \"pi\"\n");
    laptop.write_config(
        "policy.toml",
        r#"
[principals."agent:x"]
role = "observer"

[roles.observer]
allow = ["effect:read"]
"#,
    );
    pi.write_config(
        "policy.toml",
        r#"
[principals."agent:x"]
role = "observer"

[roles.observer]
allow = ["effect:read"]
"#,
    );
    let ext_argv = helper_argv("ext-tools", &[ext_calls.to_string_lossy().into_owned()]);
    laptop.write_config(
        "fleet.toml",
        &format!(
            "[machines.pi]\ncommand = {}\n\n[machines.extbox]\ncommand = {}\n",
            toml_argv(&run_argv(&pi, Some(&pi_spawns))),
            toml_argv(&ext_argv),
        ),
    );

    // 1. Role `nobody` (unnamed principal on pinned `legion rpc --as stranger`) is denied before
    //    spawning `pi` and audited on `laptop`.
    let mut stranger_rpc = laptop.session(&["--as", "stranger"]);
    stranger_rpc.send_json(&rpc_request(
        json!(1),
        "call",
        json!({"tool": "meta.version", "on": "pi"}),
    ));
    let (reply, _) = stranger_rpc.reply_to(&json!(1));
    assert_eq!(reply["error"]["data"]["code"], json!("denied"), "{reply}");
    assert_eq!(
        reply["error"]["data"]["machine"],
        json!("laptop"),
        "{reply}"
    );
    assert_eq!(
        count_lines(&pi_spawns),
        0,
        "denied forward must not spawn remote connector"
    );
    // Fan-out as `nobody` denies without spawning `pi` and writes one audit line per machine.
    stranger_rpc.send_json(&rpc_request(
        json!(2),
        "call",
        json!({"tool": "meta.version", "on": ["laptop", "pi"]}),
    ));
    let (fan_denied, _) = stranger_rpc.reply_to(&json!(2));
    assert_eq!(
        fan_denied["result"]["results"]["laptop"]["error"]["code"],
        json!("denied"),
        "{fan_denied}"
    );
    assert_eq!(
        fan_denied["result"]["results"]["pi"]["error"]["code"],
        json!("denied"),
        "{fan_denied}"
    );
    assert_eq!(count_lines(&pi_spawns), 0);
    assert_eq!(stranger_rpc.finish(), 0);

    let audit = laptop.audit();
    assert_eq!(audit.len(), 3, "{audit:?}");
    assert_eq!(audit[0]["door"], json!("rpc"));
    assert_eq!(audit[0]["principal"], json!("stranger"));
    assert_eq!(audit[0]["tool"], json!("meta.version"));
    assert_eq!(audit[0]["target"], json!("pi"));
    assert_eq!(audit[0]["outcome"], json!("denied"));

    // 2. Allowed principal `agent:x` via `legion rpc --as agent:x` is audited on laptop with
    //    door, principal, tool, target and outcome; disallowed tool is blocked on laptop.
    let mut agent_rpc = laptop.session(&["--as", "agent:x"]);
    agent_rpc.send_json(&rpc_request(
        json!(1),
        "call",
        json!({"tool": "meta.version", "on": "pi"}),
    ));
    let (ok_reply, _) = agent_rpc.reply_to(&json!(1));
    assert_eq!(ok_reply["result"]["node"], json!("pi"), "{ok_reply}");

    agent_rpc.send_json(&rpc_request(
        json!(2),
        "call",
        json!({"tool": "test.write_path", "args": {"path": "~/a"}, "on": "pi"}),
    ));
    let (write_denied, _) = agent_rpc.reply_to(&json!(2));
    assert_eq!(
        write_denied["error"]["data"]["code"],
        json!("denied"),
        "{write_denied}"
    );
    assert_eq!(
        write_denied["error"]["data"]["machine"],
        json!("laptop"),
        "{write_denied}"
    );

    // Fan-out as `agent:x` audits one line per machine on laptop.
    agent_rpc.send_json(&rpc_request(
        json!(3),
        "call",
        json!({"tool": "meta.version", "on": ["laptop", "pi"]}),
    ));
    let (fan_ok, _) = agent_rpc.reply_to(&json!(3));
    assert_eq!(
        fan_ok["result"]["results"]["laptop"]["ok"]["node"],
        json!("laptop")
    );
    assert_eq!(fan_ok["result"]["results"]["pi"]["ok"]["node"], json!("pi"));

    // 3. Extension tools on `extbox`: `agent:x` (`effect:read`) is allowed `ext.probe` (read),
    //    denied `ext.zap` (exec) without calling it, and denied `ext.unknown` (unknown effect).
    agent_rpc.send_json(&rpc_request(
        json!(4),
        "call",
        json!({"tool": "ext.probe", "on": "extbox"}),
    ));
    let (ext_ok, _) = agent_rpc.reply_to(&json!(4));
    assert_eq!(ext_ok["result"]["ran"], json!("ext.probe"), "{ext_ok}");

    agent_rpc.send_json(&rpc_request(
        json!(5),
        "call",
        json!({"tool": "ext.zap", "on": "extbox"}),
    ));
    let (ext_zap_denied, _) = agent_rpc.reply_to(&json!(5));
    assert_eq!(
        ext_zap_denied["error"]["data"]["code"],
        json!("denied"),
        "{ext_zap_denied}"
    );
    assert_eq!(
        ext_zap_denied["error"]["data"]["machine"],
        json!("laptop"),
        "{ext_zap_denied}"
    );

    agent_rpc.send_json(&rpc_request(
        json!(6),
        "call",
        json!({"tool": "ext.unknown", "on": "extbox"}),
    ));
    let (ext_unknown_denied, _) = agent_rpc.reply_to(&json!(6));
    assert_eq!(
        ext_unknown_denied["error"]["data"]["code"],
        json!("denied"),
        "{ext_unknown_denied}"
    );
    assert_eq!(agent_rpc.finish(), 0);

    // Only `ext.probe` was actually sent as a call to extbox; `ext.zap` and `ext.unknown` were
    // blocked by laptop after inspecting `tools`.
    let called = fs::read_to_string(&ext_calls).unwrap();
    assert_eq!(called.lines().collect::<Vec<_>>(), vec!["ext.probe"]);

    // Owner (`allow = ["*"]`) can forward `ext.unknown` to extbox (which returns `unknown_tool`).
    let owner_ext = laptop.run(&["ext.unknown", "--on", "extbox"]);
    assert_eq!(owner_ext.code, 1, "{}", owner_ext.stdout);
    assert_eq!(owner_ext.json()["code"], json!("unknown_tool"));

    // Confirm audit lines on laptop include forwarded `agent:x` calls targeting `pi` and `extbox`.
    let audit = laptop.audit();
    assert!(
        audit.iter().any(|e| {
            e["principal"] == json!("agent:x")
                && e["door"] == json!("rpc")
                && e["tool"] == json!("meta.version")
                && e["target"] == json!("pi")
                && e["outcome"] == json!("ok")
        }),
        "{audit:?}"
    );
    assert!(
        audit.iter().any(|e| {
            e["principal"] == json!("agent:x")
                && e["door"] == json!("rpc")
                && e["tool"] == json!("test.write_path")
                && e["target"] == json!("pi")
                && e["outcome"] == json!("denied")
        }),
        "{audit:?}"
    );
    assert!(
        audit.iter().any(|e| {
            e["principal"] == json!("agent:x")
                && e["tool"] == json!("ext.probe")
                && e["target"] == json!("extbox")
                && e["outcome"] == json!("ok")
        }),
        "{audit:?}"
    );
}

fn forwards_events_and_cancellation_to_remote_node() {
    let laptop = Home::new();
    let pi = Home::new();
    laptop.write_config("node.toml", "name = \"laptop\"\n");
    pi.write_config("node.toml", "name = \"pi\"\n");
    laptop.write_config(
        "fleet.toml",
        &format!(
            "[machines.pi]\ncommand = {}\n",
            toml_argv(&run_argv(&pi, None))
        ),
    );

    // RPC: remote progress events carry machine = "pi", and cancel stops remote test.sleep.
    let mut rpc = laptop.session(&[]);
    rpc.send_json(&rpc_request(
        json!("s1"),
        "call",
        json!({"tool": "test.sleep", "args": {"seconds": 30}, "on": "pi"}),
    ));
    loop {
        let msg = rpc.next();
        if msg["method"] == json!("event") && msg["params"]["id"] == json!("s1") {
            assert_eq!(msg["params"]["kind"], json!("progress"));
            assert_eq!(msg["params"]["machine"], json!("pi"));
            break;
        }
    }

    rpc.send_json(&rpc_request(json!("c1"), "cancel", json!({"id": "s1"})));
    let mut replies = HashMap::new();
    while replies.len() < 2 {
        let msg = rpc.next();
        if msg.get("method").is_none() {
            replies.insert(msg["id"].to_string(), msg);
        }
    }
    assert_eq!(replies["\"c1\""]["result"], json!({}));
    assert_eq!(
        replies["\"s1\""]["error"]["data"]["code"],
        json!("cancelled")
    );
    assert_eq!(replies["\"s1\""]["error"]["data"]["machine"], json!("pi"));

    // RPC fan-out: events carry each machine's name ("laptop" and "pi").
    rpc.send_json(&rpc_request(
        json!("fan"),
        "call",
        json!({"tool": "test.sleep", "args": {"seconds": 0.15}, "on": ["laptop", "pi"]}),
    ));
    let (fan_reply, fan_events) = rpc.reply_to(&json!("fan"));
    assert_eq!(
        fan_reply["result"]["results"]["laptop"]["ok"],
        json!({"slept": 0.15})
    );
    assert_eq!(
        fan_reply["result"]["results"]["pi"]["ok"],
        json!({"slept": 0.15})
    );
    let machines: Vec<&str> = fan_events
        .iter()
        .filter(|e| e["method"] == json!("event") && e["params"]["id"] == json!("fan"))
        .filter_map(|e| e["params"]["machine"].as_str())
        .collect();
    assert!(machines.contains(&"laptop"), "{machines:?}");
    assert!(machines.contains(&"pi"), "{machines:?}");
    assert_eq!(rpc.finish(), 0);

    // Remote audit log recorded the cancelled call.
    let pi_audit = pi.audit();
    assert!(
        pi_audit
            .iter()
            .any(|entry| entry["tool"] == json!("test.sleep")
                && entry["outcome"] == json!("cancelled")),
        "{pi_audit:?}"
    );

    // MCP: fan-out progress and log notifications include the source machine.
    let mut mcp = McpClient::start(&laptop, &[]);
    let _ = mcp.request("logging/setLevel", json!({"level": "debug"}));
    let (res, notes) = mcp.call_with_events(
        "test_sleep",
        json!({"seconds": 0.15, "on": ["laptop", "pi"]}),
        Some("tok-1"),
    );
    assert_eq!(res["isError"], json!(false), "{res}");
    let progress_msgs: Vec<&str> = notes
        .iter()
        .filter(|n| n["method"] == json!("notifications/progress"))
        .filter_map(|n| n["params"]["message"].as_str())
        .collect();
    assert!(
        progress_msgs.iter().any(|m| m.starts_with("[pi] ")),
        "{progress_msgs:?}"
    );
    assert!(
        progress_msgs.iter().any(|m| m.starts_with("[laptop] ")),
        "{progress_msgs:?}"
    );
    let log_machines: Vec<&str> = notes
        .iter()
        .filter(|n| n["method"] == json!("notifications/message"))
        .filter_map(|n| n["params"]["data"]["machine"].as_str())
        .collect();
    assert!(log_machines.contains(&"pi"), "{log_machines:?}");
    assert!(log_machines.contains(&"laptop"), "{log_machines:?}");
    assert_eq!(mcp.finish(), 0);
}

fn multiplexes_concurrent_calls_and_reuses_one_session() {
    let laptop = Home::new();
    let pi = Home::new();
    let spawn_log = laptop.state().join("pi-spawns.log");
    fs::create_dir_all(laptop.state()).unwrap();

    laptop.write_config("node.toml", "name = \"laptop\"\n");
    pi.write_config("node.toml", "name = \"pi\"\n");
    laptop.write_config(
        "fleet.toml",
        &format!(
            "[machines.pi]\ncommand = {}\n",
            toml_argv(&run_argv(&pi, Some(&spawn_log)))
        ),
    );

    let mut rpc = laptop.session(&[]);
    // Send 4 concurrent first calls (including one slow and three fast) before any reply arrives.
    rpc.send_json(&rpc_request(
        json!("slow"),
        "call",
        json!({"tool": "test.sleep", "args": {"seconds": 0.4}, "on": "pi"}),
    ));
    rpc.send_json(&rpc_request(
        json!("fast1"),
        "call",
        json!({"tool": "meta.version", "on": "pi"}),
    ));
    rpc.send_json(&rpc_request(
        json!("fast2"),
        "call",
        json!({"tool": "test.echo", "args": {"n": 2}, "on": "pi"}),
    ));
    rpc.send_json(&rpc_request(
        json!("fast3"),
        "call",
        json!({"tool": "test.echo", "args": {"n": 3}, "on": "pi"}),
    ));

    let mut order = Vec::new();
    let mut replies = HashMap::new();
    while replies.len() < 4 {
        let msg = rpc.next();
        if msg.get("method").is_none() {
            let id = msg["id"].as_str().unwrap().to_owned();
            order.push(id.clone());
            replies.insert(id, msg);
        }
    }
    assert_eq!(replies["fast1"]["result"]["node"], json!("pi"));
    assert_eq!(replies["fast2"]["result"], json!({"n": 2}));
    assert_eq!(replies["fast3"]["result"], json!({"n": 3}));
    assert_eq!(replies["slow"]["result"], json!({"slept": 0.4}));
    // At least one fast call finished before the slow 400ms call.
    assert_ne!(order.first().map(String::as_str), Some("slow"), "{order:?}");

    // Another subsequent call reuses the same session.
    rpc.send_json(&rpc_request(
        json!("after"),
        "call",
        json!({"tool": "meta.version", "on": "pi"}),
    ));
    let (after, _) = rpc.reply_to(&json!("after"));
    assert_eq!(after["result"]["node"], json!("pi"));
    assert_eq!(rpc.finish(), 0);

    // Exactly ONE process was spawned for all 5 calls (including concurrent first calls).
    assert_eq!(count_lines(&spawn_log), 1);
}

fn session_death_mid_call_is_unreachable_and_next_call_reconnects() {
    let laptop = Home::new();
    let pi = Home::new();
    let marker = laptop.state().join("died-once.flag");
    fs::create_dir_all(laptop.state()).unwrap();

    laptop.write_config("node.toml", "name = \"laptop\"\n");
    pi.write_config("node.toml", "name = \"pi\"\n");
    let argv = helper_argv(
        "die-mid-call",
        &[
            LEGION_BIN.to_owned(),
            legion_home_of(&pi).to_string_lossy().into_owned(),
            pi.home().to_string_lossy().into_owned(),
            marker.to_string_lossy().into_owned(),
        ],
    );
    laptop.write_config(
        "fleet.toml",
        &format!("[machines.pi]\ncommand = {}\n", toml_argv(&argv)),
    );

    let mut rpc = laptop.session(&[]);
    rpc.send_json(&rpc_request(
        json!(1),
        "call",
        json!({"tool": "meta.version", "on": "pi"}),
    ));
    let (first, _) = rpc.reply_to(&json!(1));
    let data = &first["error"]["data"];
    assert_eq!(data["code"], json!("unreachable"), "{first}");
    assert_eq!(data["machine"], json!("pi"), "{first}");
    assert!(
        data["message"].as_str().unwrap().contains("unknown"),
        "{first}"
    );
    assert!(
        data["hint"]
            .as_str()
            .unwrap_or_default()
            .contains("simulated ssh transport drop"),
        "{first}"
    );

    // Next call reconnects cleanly and succeeds.
    rpc.send_json(&rpc_request(
        json!(2),
        "call",
        json!({"tool": "meta.version", "on": "pi"}),
    ));
    let (second, _) = rpc.reply_to(&json!(2));
    assert_eq!(second["result"]["node"], json!("pi"), "{second}");
    assert_eq!(rpc.finish(), 0);
}

fn idle_session_death_reopens_transparently_when_unwritten() {
    let laptop = Home::new();
    let pi = Home::new();
    let marker = laptop.state().join("idle-death.flag");
    fs::create_dir_all(laptop.state()).unwrap();

    laptop.write_config("node.toml", "name = \"laptop\"\n");
    pi.write_config("node.toml", "name = \"pi\"\n");
    let argv = helper_argv(
        "die-after-first-call",
        &[
            LEGION_BIN.to_owned(),
            legion_home_of(&pi).to_string_lossy().into_owned(),
            pi.home().to_string_lossy().into_owned(),
            marker.to_string_lossy().into_owned(),
        ],
    );
    laptop.write_config(
        "fleet.toml",
        &format!("[machines.pi]\ncommand = {}\n", toml_argv(&argv)),
    );

    let mut rpc = laptop.session(&[]);
    rpc.send_json(&rpc_request(
        json!(1),
        "call",
        json!({"tool": "meta.version", "on": "pi"}),
    ));
    let (first, _) = rpc.reply_to(&json!(1));
    assert_eq!(first["result"]["node"], json!("pi"), "{first}");

    // Wait briefly for the first helper to terminate while idle so the session is dead before
    // call 2 is written.
    std::thread::sleep(Duration::from_millis(150));

    rpc.send_json(&rpc_request(
        json!(2),
        "call",
        json!({"tool": "meta.version", "on": "pi"}),
    ));
    let (second, _) = rpc.reply_to(&json!(2));
    assert_eq!(second["result"]["node"], json!("pi"), "{second}");
    assert_eq!(rpc.finish(), 0);
}

fn skips_noise_lines_before_hello_and_reports_protocol_mismatch() {
    let laptop = Home::new();
    let pi = Home::new();
    laptop.write_config("node.toml", "name = \"laptop\"\n");
    pi.write_config("node.toml", "name = \"pi\"\n");

    let noise_argv = helper_argv(
        "noise",
        &[
            LEGION_BIN.to_owned(),
            legion_home_of(&pi).to_string_lossy().into_owned(),
            pi.home().to_string_lossy().into_owned(),
        ],
    );
    let mismatch_argv = helper_argv("mismatch", &[]);
    laptop.write_config(
        "fleet.toml",
        &format!(
            "[machines.pi]\ncommand = {}\n\n[machines.old]\ncommand = {}\n",
            toml_argv(&noise_argv),
            toml_argv(&mismatch_argv),
        ),
    );

    let ok = laptop.run(&["meta.version", "--on", "pi"]);
    assert_eq!(ok.code, 0, "stderr: {}", ok.stderr);
    assert_eq!(ok.json()["node"], json!("pi"));

    let bad = laptop.run(&["meta.version", "--on", "old"]);
    assert_eq!(bad.code, 1);
    let err = bad.json();
    assert_eq!(err["code"], json!("unsupported"), "{err}");
    assert_eq!(err["machine"], json!("old"), "{err}");
    assert!(err["message"].as_str().unwrap().contains("99"), "{err}");
    assert!(err["message"].as_str().unwrap().contains('1'), "{err}");
}

fn fan_out_all_partial_failure_all_failed_and_unknown_machine() {
    let laptop = Home::new();
    let pi = Home::new();
    let fail_log = laptop.state().join("dead-fail.log");
    fs::create_dir_all(laptop.state()).unwrap();

    laptop.write_config("node.toml", "name = \"laptop\"\n");
    pi.write_config("node.toml", "name = \"pi\"\n");
    let fail_argv = helper_argv(
        "fail",
        &[
            fail_log.to_string_lossy().into_owned(),
            "ssh: connect to host tower.example: Connection refused".to_owned(),
        ],
    );
    // First, `fleet.toml` does NOT list this node (`laptop`): `all` must still include `laptop`
    // alongside every machine in `fleet.toml`.
    laptop.write_config(
        "fleet.toml",
        &format!(
            "[machines.pi]\ncommand = {}\n\n[machines.tower]\ncommand = {}\n",
            toml_argv(&run_argv(&pi, None)),
            toml_argv(&fail_argv),
        ),
    );
    let all_unlisted = laptop.run(&["meta.version", "--on", "all"]);
    assert_eq!(
        all_unlisted.code, 0,
        "partial fan-out should exit 0: {}",
        all_unlisted.stdout
    );
    let results_unlisted = &all_unlisted.json()["results"];
    assert_eq!(results_unlisted.as_object().unwrap().len(), 3);
    assert_eq!(results_unlisted["laptop"]["ok"]["node"], json!("laptop"));
    assert_eq!(results_unlisted["pi"]["ok"]["node"], json!("pi"));
    assert_eq!(
        results_unlisted["tower"]["error"]["code"],
        json!("unreachable")
    );

    // When `laptop` IS also listed in `fleet.toml`, `all` includes it once without duplicates.
    let before_audit = laptop.audit().len();
    laptop.write_config(
        "fleet.toml",
        &format!(
            "[machines.laptop]\nssh = \"laptop.example\"\n\n[machines.pi]\ncommand = {}\n\n[machines.tower]\ncommand = {}\n",
            toml_argv(&run_argv(&pi, None)),
            toml_argv(&fail_argv),
        ),
    );

    // `all` includes this node (`laptop`), `pi` (ok), and `tower` (unreachable).
    let all = laptop.run(&["meta.version", "--on", "all"]);
    assert_eq!(all.code, 0, "partial fan-out should exit 0: {}", all.stdout);
    let results = &all.json()["results"];
    assert_eq!(results.as_object().unwrap().len(), 3);
    assert_eq!(results["laptop"]["ok"]["node"], json!("laptop"));
    assert_eq!(results["pi"]["ok"]["node"], json!("pi"));
    assert_eq!(results["tower"]["error"]["code"], json!("unreachable"));
    assert_eq!(results["tower"]["error"]["machine"], json!("tower"));
    assert_eq!(
        laptop.audit().len() - before_audit,
        3,
        "listing this node in fleet.toml must not run or audit it twice"
    );
    assert!(
        results["tower"]["error"]["hint"]
            .as_str()
            .unwrap()
            .contains("Connection refused"),
        "{results}"
    );

    // Fan-out where every targeted machine fails exits 1 on CLI and sets isError: true on MCP.
    let all_fail = laptop.run(&["meta.version", "--on", "tower,ghost"]);
    assert_eq!(all_fail.code, 1, "all-failed fan-out must exit non-zero");
    let fail_res = &all_fail.json()["results"];
    assert_eq!(fail_res["tower"]["error"]["code"], json!("unreachable"));
    assert_eq!(fail_res["ghost"]["error"]["code"], json!("not_found"));

    let mut mcp = McpClient::start(&laptop, &[]);
    let (mcp_all_fail, _) =
        mcp.call_with_events("meta_version", json!({"on": ["tower", "ghost"]}), None);
    assert_eq!(mcp_all_fail["isError"], json!(true), "{mcp_all_fail}");
    assert_eq!(mcp.finish(), 0);

    // Unknown single machine returns not_found carrying this node's name and listing known names.
    let unknown = laptop.run(&["meta.version", "--on", "ghost"]);
    assert_eq!(unknown.code, 1);
    let err = unknown.json();
    assert_eq!(err["code"], json!("not_found"));
    assert_eq!(err["machine"], json!("laptop"));
    let hint = err["hint"].as_str().unwrap();
    assert!(hint.contains("pi") && hint.contains("tower"), "{hint}");
}

fn two_system_machine_falls_back_and_remembers_winner() {
    let laptop = Home::new();
    let tower = Home::new();
    let first_log = laptop.state().join("sys1.log");
    let second_log = laptop.state().join("sys2.log");
    fs::create_dir_all(laptop.state()).unwrap();

    laptop.write_config("node.toml", "name = \"laptop\"\n");
    tower.write_config("node.toml", "name = \"tower\"\n");

    let fail_argv = helper_argv(
        "fail",
        &[
            first_log.to_string_lossy().into_owned(),
            "first system offline".to_owned(),
        ],
    );
    let ok_argv = run_argv(&tower, Some(&second_log));
    laptop.write_config(
        "fleet.toml",
        &format!(
            "[[machines.tower.systems]]\nname = \"first\"\ncommand = {}\n\n[[machines.tower.systems]]\nname = \"second\"\ncommand = {}\n",
            toml_argv(&fail_argv),
            toml_argv(&ok_argv),
        ),
    );

    // Call 1: tries "first" (fails), falls back to "second" (succeeds) and persists "second".
    let ping1 = laptop.run(&["fleet.ping", "machines=[\"tower\"]"]);
    assert_eq!(ping1.code, 0, "{}", ping1.stdout);
    let r1 = &ping1.json()["results"][0];
    assert_eq!(r1["reachable"], json!(true), "{r1}");
    assert_eq!(r1["node"], json!("tower"));
    assert_eq!(r1["system"], json!("second"));
    assert_eq!(count_lines(&first_log), 1);
    assert_eq!(count_lines(&second_log), 1);

    // Call 2 (a brand-new CLI process sharing the same state dir): tries "second" FIRST, so
    // "first" is NOT attempted a second time!
    let call2 = laptop.run(&["meta.version", "--on", "tower"]);
    assert_eq!(call2.code, 0, "{}", call2.stdout);
    assert_eq!(call2.json()["node"], json!("tower"));
    assert_eq!(
        count_lines(&first_log),
        1,
        "first system should not have been retried"
    );
    assert_eq!(count_lines(&second_log), 2);
}

fn hop_limit_stops_deliberate_loop_and_allows_valid_multi_hop() {
    let node_a = Home::new();
    let node_b = Home::new();
    let node_c = Home::new();
    node_a.write_config("node.toml", "name = \"a\"\n");
    node_b.write_config("node.toml", "name = \"b\"\n");
    node_c.write_config("node.toml", "name = \"c\"\n");

    // 1. Direct alias never re-routes on the receiver: A maps `tower` directly to B, and B maps
    //    `tower` directly to C, and A also maps `b_alias` directly to B while B maps `b_alias` to
    //    itself. Both run directly on B because direct forwards carry no `on`.
    // 2. Explicit `via = "b"` relays through B with `on` naming the final target:
    //    - `c` succeeds because B has `[machines.c]` -> C.
    //    - `ghost` fails with `not_found` carrying relay B's machine name (never runs on B).
    // 3. Cross-node `via` cycle (`loopbox`: A via B, B via A) stops at the 3-hop limit.
    node_a.write_config(
        "fleet.toml",
        &format!(
            "[machines.b]\ncommand = {}\n\n[machines.tower]\ncommand = {}\n\n[machines.b_alias]\ncommand = {}\n\n[machines.c]\nvia = \"b\"\n\n[machines.ghost]\nvia = \"b\"\n\n[machines.loopbox]\nvia = \"b\"\n",
            toml_argv(&run_argv(&node_b, None)),
            toml_argv(&run_argv(&node_b, None)),
            toml_argv(&run_argv(&node_b, None)),
        ),
    );
    node_b.write_config(
        "fleet.toml",
        &format!(
            "[machines.a]\ncommand = {}\n\n[machines.c]\ncommand = {}\n\n[machines.tower]\ncommand = {}\n\n[machines.b_alias]\ncommand = {}\n\n[machines.loopbox]\nvia = \"a\"\n",
            toml_argv(&run_argv(&node_a, None)),
            toml_argv(&run_argv(&node_c, None)),
            toml_argv(&run_argv(&node_c, None)),
            toml_argv(&run_argv(&node_b, None)),
        ),
    );

    // Direct forward under an alias runs on B, NOT C, even though B maps `tower` to C.
    let direct_tower = node_a.run(&["meta.version", "--on", "tower"]);
    assert_eq!(direct_tower.code, 0, "{}", direct_tower.stdout);
    assert_eq!(direct_tower.json()["node"], json!("b"));

    // Direct forward where receiver's own name differs from the fleet alias does not self-loop.
    let direct_alias = node_a.run(&["meta.version", "--on", "b_alias"]);
    assert_eq!(direct_alias.code, 0, "{}", direct_alias.stdout);
    assert_eq!(direct_alias.json()["node"], json!("b"));

    // Explicit `via = "b"` reaches C through B.
    let reached = node_a.run(&["meta.version", "--on", "c"]);
    assert_eq!(reached.code, 0, "{}", reached.stdout);
    assert_eq!(reached.json()["node"], json!("c"));

    // Explicit `via = "b"` for a target B does not know fails with `not_found` from relay B.
    let missing_on_relay = node_a.run(&["meta.version", "--on", "ghost"]);
    assert_eq!(missing_on_relay.code, 1, "{}", missing_on_relay.stdout);
    let err = missing_on_relay.json();
    assert_eq!(err["code"], json!("not_found"), "{err}");
    assert_eq!(err["machine"], json!("b"), "{err}");

    // Deliberate cross-node `via` loop stops with `failed` after exceeding 3 hops.
    let looped = node_a.run(&["meta.version", "--on", "loopbox"]);
    assert_eq!(looped.code, 1, "{}", looped.stdout);
    let err = looped.json();
    assert_eq!(err["code"], json!("failed"), "{err}");
    assert_eq!(err["machine"], json!("loopbox"), "{err}");
    assert!(err["message"].as_str().unwrap().contains("3 hops"), "{err}");
}

fn remote_machine_labels_are_overwritten_on_errors_and_events() {
    let node_a = Home::new();
    let node_b = Home::new();
    node_a.write_config("node.toml", "name = \"a\"\n");
    node_b.write_config("node.toml", "name = \"b\"\n");
    let spoof_argv = helper_argv("spoof", &[]);

    node_a.write_config(
        "fleet.toml",
        &format!(
            "[machines.pi]\ncommand = {}\n\n[machines.b]\ncommand = {}\n\n[machines.c]\nvia = \"b\"\n",
            toml_argv(&spoof_argv),
            toml_argv(&run_argv(&node_b, None)),
        ),
    );
    node_b.write_config(
        "fleet.toml",
        &format!("[machines.c]\ncommand = {}\n", toml_argv(&spoof_argv)),
    );

    let mut rpc = node_a.session(&[]);

    // Direct call to `pi`: remote spoofed `"tower"`, caller overwrites both event and error to `pi`.
    rpc.send_json(&rpc_request(
        json!(1),
        "call",
        json!({"tool": "meta.version", "on": "pi"}),
    ));
    let (direct_reply, direct_events) = rpc.reply_to(&json!(1));
    assert_eq!(direct_events.len(), 1, "{direct_events:?}");
    assert_eq!(direct_events[0]["params"]["machine"], json!("pi"));
    assert_eq!(
        direct_reply["error"]["data"]["machine"],
        json!("pi"),
        "{direct_reply}"
    );

    // Relayed call to `c` via `b`: remote spoofed `"tower"`, caller stamps final target `c`.
    rpc.send_json(&rpc_request(
        json!(2),
        "call",
        json!({"tool": "meta.version", "on": "c"}),
    ));
    let (relayed_reply, relayed_events) = rpc.reply_to(&json!(2));
    assert_eq!(relayed_events.len(), 1, "{relayed_events:?}");
    assert_eq!(relayed_events[0]["params"]["machine"], json!("c"));
    assert_eq!(
        relayed_reply["error"]["data"]["machine"],
        json!("c"),
        "{relayed_reply}"
    );

    assert_eq!(rpc.finish(), 0);
}

fn fleet_tools_list_ping_add_remove_and_discover_permissions() {
    let laptop = Home::new();
    let pi = Home::new();
    laptop.write_config("node.toml", "name = \"laptop\"\n");
    pi.write_config("node.toml", "name = \"pi\"\n");
    laptop.write_config(
        "policy.toml",
        r#"
[principals.phone]
role = "observer"

[roles.observer]
allow = ["effect:read"]
"#,
    );
    laptop.write_config(
        "fleet.toml",
        &format!(
            "# Top comment kept forever\n\n# Always on\n[machines.pi]\ncommand = {}\n",
            toml_argv(&run_argv(&pi, None)),
        ),
    );

    // meta.doctor checks fleet.toml.
    let doctor = laptop.run(&["meta.doctor"]);
    assert_eq!(doctor.code, 0, "{}", doctor.stdout);
    let checks = doctor.json()["checks"].as_array().unwrap().clone();
    assert!(
        checks
            .iter()
            .any(|c| c["name"] == json!("fleet.toml") && c["ok"] == json!(true)),
        "{checks:?}"
    );

    // fleet.list reports configured machines without making network calls.
    let listed = laptop.run(&["fleet.list"]);
    assert_eq!(listed.code, 0, "{}", listed.stdout);
    assert_eq!(listed.json()["node"], json!("laptop"));
    assert_eq!(listed.json()["machines"][0]["name"], json!("pi"));
    assert_eq!(listed.json()["machines"][0]["this_node"], json!(false));
    assert_eq!(
        listed.json()["machines"][0]["systems"][0]["via"],
        json!("command")
    );

    // fleet.ping reaches pi, answers locally for this node, and reports unknown machines cleanly.
    let ping = laptop.run(&["fleet.ping", "machines=[\"laptop\",\"pi\",\"ghost\"]"]);
    assert_eq!(ping.code, 0, "{}", ping.stdout);
    let results = ping.json()["results"].as_array().unwrap().clone();
    assert_eq!(results.len(), 3, "{results:?}");
    assert_eq!(results[0]["machine"], json!("laptop"));
    assert_eq!(results[0]["reachable"], json!(true));
    assert_eq!(results[0]["latency_ms"], json!(0));
    assert_eq!(results[1]["machine"], json!("pi"));
    assert_eq!(results[1]["reachable"], json!(true));
    assert_eq!(results[1]["node"], json!("pi"));
    assert!(results[1]["latency_ms"].is_u64());
    assert_eq!(results[2]["machine"], json!("ghost"));
    assert_eq!(results[2]["reachable"], json!(false));
    assert_eq!(results[2]["error"]["code"], json!("not_found"));

    // fleet.add adds a machine while preserving comments; replacing reports replaced: true.
    let added = laptop.run(&[
        "fleet.add",
        "name=tower",
        "ssh=tower.example",
        "mac=02:00:00:00:00:01",
        "wake_via=[\"pi\"]",
    ]);
    assert_eq!(added.code, 0, "{}", added.stdout);
    assert_eq!(added.json(), json!({"name": "tower", "added": true}));

    let replaced = laptop.run(&["fleet.add", "name=tower", "ssh=tower2.example"]);
    assert_eq!(replaced.code, 0, "{}", replaced.stdout);
    assert_eq!(replaced.json(), json!({"name": "tower", "replaced": true}));

    let fleet_path = legion_home_of(&laptop).join("config").join("fleet.toml");
    let text = fs::read_to_string(&fleet_path).unwrap();
    assert!(text.contains("# Top comment kept forever"), "{text}");
    assert!(text.contains("# Always on"), "{text}");
    assert!(text.contains("tower2.example"), "{text}");

    // fleet.remove removes tower while preserving comments and pi.
    let removed = laptop.run(&["fleet.remove", "name=tower"]);
    assert_eq!(removed.code, 0, "{}", removed.stdout);
    assert_eq!(removed.json(), json!({"name": "tower", "removed": true}));
    let text = fs::read_to_string(&fleet_path).unwrap();
    assert!(text.contains("# Top comment kept forever"), "{text}");
    assert!(text.contains("# Always on"), "{text}");
    assert!(!text.contains("tower2.example"), "{text}");

    // Non-admin role is denied fleet.add, fleet.remove, and fleet.discover add=true.
    let add_denied = laptop.run(&[
        "fleet.add",
        "name=tower",
        "ssh=tower.example",
        "--as",
        "phone",
    ]);
    assert_eq!(add_denied.code, 1);
    assert_eq!(add_denied.json()["code"], json!("denied"));

    let discover_add_denied = laptop.run(&["fleet.discover", "add=true", "--as", "phone"]);
    assert_eq!(discover_add_denied.code, 1);
    assert_eq!(discover_add_denied.json()["code"], json!("denied"));
}

#[cfg(unix)]
fn send_signal(pid: u32, signal: &str) {
    let status = Command::new("kill")
        .args([&format!("-{signal}"), &pid.to_string()])
        .status()
        .unwrap();
    assert!(status.success(), "kill -{signal} {pid} failed: {status}");
}

#[cfg(unix)]
fn pid_alive(pid: u32) -> bool {
    Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

#[cfg(unix)]
fn wait_for_exit(child: &mut Child, timeout: Duration) -> std::process::ExitStatus {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            return status;
        }
        assert!(
            Instant::now() < deadline,
            "child {} did not exit within {timeout:?}",
            child.id()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[cfg(unix)]
fn sigterm_and_sighup_cancel_remote_work_and_shut_down_cleanly() {
    let laptop = Home::new();
    let pi = Home::new();
    laptop.write_config("node.toml", "name = \"laptop\"\n");
    pi.write_config("node.toml", "name = \"pi\"\n");
    laptop.write_config(
        "fleet.toml",
        &format!(
            "[machines.pi]\ncommand = {}\n",
            toml_argv(&run_argv(&pi, None))
        ),
    );

    // 1. One-shot CLI `exec.run` forwarded to `pi`, killed with SIGTERM: cancels remote work,
    //    stops the remote child process, audits `cancelled` on `pi`, and exits promptly.
    let pid_file = pi.state().join("remote-hold.pid");
    fs::create_dir_all(pi.state()).unwrap();
    let hold_argv = helper_argv("hold", &[pid_file.to_string_lossy().into_owned()]);
    let argv_json = serde_json::to_string(&hold_argv).unwrap();
    let mut cli = laptop
        .command(&["exec.run", &format!("argv={argv_json}"), "--on", "pi"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();

    let deadline = Instant::now() + PATIENCE;
    let remote_pid: u32 = loop {
        if let Ok(text) = fs::read_to_string(&pid_file)
            && let Ok(pid) = text.trim().parse::<u32>()
        {
            break pid;
        }
        assert!(
            Instant::now() < deadline,
            "remote hold process never started"
        );
        std::thread::sleep(Duration::from_millis(20));
    };

    send_signal(cli.id(), "TERM");
    let status = wait_for_exit(&mut cli, PATIENCE);
    assert!(!status.success(), "cancelled CLI should exit non-zero");

    // Wait briefly for OS reaping and confirm the remote child is dead and audited as cancelled.
    let dead_deadline = Instant::now() + Duration::from_secs(5);
    while pid_alive(remote_pid) {
        assert!(
            Instant::now() < dead_deadline,
            "remote child {remote_pid} survived SIGTERM on caller"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    let pi_audit = pi.audit();
    assert!(
        pi_audit
            .iter()
            .any(|e| e["tool"] == json!("exec.run") && e["outcome"] == json!("cancelled")),
        "{pi_audit:?}"
    );

    // 2. `legion rpc` with an in-flight forwarded call killed with SIGHUP.
    let mut rpc = laptop.session(&[]);
    rpc.send_json(&rpc_request(
        json!(1),
        "call",
        json!({"tool": "test.sleep", "args": {"seconds": 30}, "on": "pi"}),
    ));
    loop {
        let msg = rpc.next();
        if msg["method"] == json!("event") && msg["params"]["id"] == json!(1) {
            break;
        }
    }
    send_signal(rpc.pid(), "HUP");
    let _ = rpc.wait_exit(PATIENCE);

    let audit_deadline = Instant::now() + PATIENCE;
    loop {
        let count = pi
            .audit()
            .iter()
            .filter(|e| e["tool"] == json!("test.sleep") && e["outcome"] == json!("cancelled"))
            .count();
        if count >= 1 {
            break;
        }
        assert!(
            Instant::now() < audit_deadline,
            "pi did not record cancelled test.sleep after rpc SIGHUP"
        );
        std::thread::sleep(Duration::from_millis(20));
    }

    // 3. `legion mcp` with an in-flight forwarded call killed with SIGTERM.
    let mut mcp = McpClient::start(&laptop, &[]);
    mcp.next_id += 1;
    let call_id = json!(mcp.next_id);
    mcp.send(&json!({
        "jsonrpc": "2.0",
        "id": call_id,
        "method": "tools/call",
        "params": {
            "name": "test_sleep",
            "arguments": {"seconds": 30, "on": "pi"},
            "_meta": {"progressToken": "sig-tok"}
        }
    }));
    loop {
        let line = mcp.lines.recv_timeout(PATIENCE).unwrap();
        let msg: Value = serde_json::from_str(&line).unwrap();
        if msg["method"] == json!("notifications/progress") {
            break;
        }
    }
    send_signal(mcp.child.id(), "TERM");
    let _ = wait_for_exit(&mut mcp.child, PATIENCE);

    let audit_deadline = Instant::now() + PATIENCE;
    loop {
        let count = pi
            .audit()
            .iter()
            .filter(|e| e["tool"] == json!("test.sleep") && e["outcome"] == json!("cancelled"))
            .count();
        if count >= 2 {
            break;
        }
        assert!(
            Instant::now() < audit_deadline,
            "pi did not record cancelled test.sleep after mcp SIGTERM"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[cfg(not(unix))]
fn sigterm_and_sighup_cancel_remote_work_and_shut_down_cleanly() {}
