//! End-to-end extension tool tests through the `legion` binary, including forwarding a call to an
//! extension tool that exists ONLY on a remote node reached through the `command` connector.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::PathBuf;
use std::process::{Command, ExitCode};
use std::time::Instant;

use common::Home;
use serde_json::{Value, json};

const LEGION_BIN: &str = env!("CARGO_BIN_EXE_legion");
const HELPER_FLAG: &str = "--ext-cli-helper";

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
            "calls_extension_only_present_on_remote_node_over_command_connector",
            calls_extension_only_present_on_remote_node_over_command_connector,
        ),
        (
            "cli_tools_and_help_include_extension_tools",
            cli_tools_and_help_include_extension_tools,
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

fn run_helper(args: &[String]) -> ExitCode {
    let Some((mode, rest)) = args.split_first() else {
        eprintln!("missing helper mode");
        return ExitCode::from(2);
    };
    match mode.as_str() {
        // Connector wrapper: spawns `legion rpc` under the target node's LEGION_HOME.
        "rpc" => {
            let status = Command::new(LEGION_BIN)
                .arg("rpc")
                .env("LEGION_HOME", &rest[0])
                .env("HOME", &rest[1])
                .env("USERPROFILE", &rest[1])
                .env_remove("XDG_CONFIG_HOME")
                .env_remove("XDG_STATE_HOME")
                .status()
                .unwrap();
            ExitCode::from(status.code().unwrap_or(1).clamp(0, 255) as u8)
        }
        // Extension command helper: echoes LEGION_* env vars and stdin JSON.
        "echo-ext" => {
            let mut stdin_bytes = Vec::new();
            let _ = std::io::stdin().read_to_end(&mut stdin_bytes);
            let stdin_json: Value = serde_json::from_slice(&stdin_bytes).unwrap_or(Value::Null);
            let env: BTreeMap<String, String> = std::env::vars()
                .filter(|(k, _)| k.starts_with("LEGION_"))
                .collect();
            let mut stdout = std::io::stdout();
            let _ = writeln!(stdout, "{}", json!({"env": env, "stdin": stdin_json}));
            let _ = stdout.flush();
            ExitCode::SUCCESS
        }
        other => {
            eprintln!("unknown helper mode {other}");
            ExitCode::from(2)
        }
    }
}

fn this_binary() -> String {
    std::env::current_exe()
        .unwrap()
        .to_string_lossy()
        .into_owned()
}

fn legion_home_dir(home: &Home) -> PathBuf {
    // Home::state() is <root>/legion/state, so its parent is <root>/legion.
    home.state().parent().unwrap().to_path_buf()
}

fn calls_extension_only_present_on_remote_node_over_command_connector() {
    let laptop = Home::new();
    let tower = Home::new();

    laptop.write_config("node.toml", "name = \"laptop\"\n");
    tower.write_config("node.toml", "name = \"tower\"\n");
    // A call has to be allowed by every node it passes through, so the forwarding node knows the
    // observer as well as the node that runs the tool.
    let policy = r#"
[principals.observer]
role = "observer"

[roles.observer]
allow = ["effect:read"]
"#;
    laptop.write_config("policy.toml", policy);
    tower.write_config("policy.toml", policy);

    // Point laptop -> tower via the command connector.
    let fleet_toml = format!(
        "[machines.tower]\ncommand = {}\n",
        serde_json::to_string(&[
            this_binary(),
            HELPER_FLAG.to_owned(),
            "rpc".to_owned(),
            legion_home_dir(&tower).to_string_lossy().into_owned(),
            tower.home().to_string_lossy().into_owned(),
        ])
        .unwrap()
    );
    laptop.write_config("fleet.toml", &fleet_toml);

    // Define extensions ONLY on tower (laptop has no tools.d at all).
    let ext_argv = [this_binary(), HELPER_FLAG.to_owned(), "echo-ext".to_owned()];
    let define_read = tower.run(&[
        "ext.define",
        "--args",
        &json!({
            "name": "gpu.probe",
            "description": "Probe remote GPU.",
            "effect": "read",
            "run": ext_argv,
            "params": {
                "index": {"type": "integer", "default": 0},
                "label": {"type": "string", "required": true}
            }
        })
        .to_string(),
    ]);
    assert_eq!(define_read.code, 0, "stderr: {}", define_read.stderr);

    let define_exec = tower.run(&[
        "ext.define",
        "--args",
        &json!({
            "name": "gpu.reset",
            "description": "Reset remote GPU.",
            "effect": "exec",
            "run": ext_argv
        })
        .to_string(),
    ]);
    assert_eq!(define_exec.code, 0, "stderr: {}", define_exec.stderr);

    // Confirm laptop does NOT have gpu.probe locally.
    let local_err = laptop.run(&["gpu.probe", "label=test", "--json"]);
    assert_ne!(local_err.code, 0);
    assert_eq!(local_err.json()["code"], json!("unknown_tool"));

    // Call gpu.probe on tower from laptop: forwarded and executed on tower!
    let remote = laptop.run(&["gpu.probe", "label=remote-ok", "--on", "tower", "--json"]);
    assert_eq!(
        remote.code, 0,
        "stdout={} stderr={}",
        remote.stdout, remote.stderr
    );
    let reply = remote.json();
    assert_eq!(reply["env"]["LEGION_NODE"], json!("tower"));
    assert_eq!(reply["env"]["LEGION_TOOL"], json!("gpu.probe"));
    assert_eq!(reply["env"]["LEGION_PRINCIPAL"], json!("owner"));
    assert_eq!(reply["env"]["LEGION_ARG_LABEL"], json!("remote-ok"));
    assert_eq!(reply["env"]["LEGION_ARG_INDEX"], json!("0"));
    assert_eq!(reply["stdin"], json!({"index": 0, "label": "remote-ok"}));

    // Remote policy applies to forwarded extension calls by declared effect.
    let remote_read = laptop.run(&[
        "gpu.probe",
        "label=obs",
        "--on",
        "tower",
        "--as",
        "observer",
        "--json",
    ]);
    assert_eq!(remote_read.code, 0, "stderr={}", remote_read.stderr);
    assert_eq!(
        remote_read.json()["env"]["LEGION_PRINCIPAL"],
        json!("observer")
    );

    let remote_exec = laptop.run(&["gpu.reset", "--on", "tower", "--as", "observer", "--json"]);
    assert_ne!(remote_exec.code, 0);
    assert_eq!(remote_exec.json()["code"], json!("denied"));
    // The forwarding node asks the target for the tool's effect and applies its own policy first,
    // so the refusal comes from the node the call started on.
    assert_eq!(remote_exec.json()["machine"], json!("laptop"));
}

fn cli_tools_and_help_include_extension_tools() {
    let home = Home::new();
    let ext_argv = [this_binary(), HELPER_FLAG.to_owned(), "echo-ext".to_owned()];
    let defined = home.run(&[
        "ext.define",
        "--args",
        &json!({
            "name": "lab.check",
            "description": "Check lab sensor.",
            "effect": "read",
            "run": ext_argv,
            "params": {
                "sensor": {"type": "string", "description": "Sensor id.", "required": true}
            }
        })
        .to_string(),
    ]);
    assert_eq!(defined.code, 0, "stderr={}", defined.stderr);

    let tools = home.run(&["tools", "--json"]);
    assert_eq!(tools.code, 0);
    let tools_json = tools.json();
    let names: Vec<&str> = tools_json["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert!(names.contains(&"lab.check"), "{names:?}");
    assert!(names.contains(&"ext.define"), "{names:?}");

    let help = home.run(&["help", "lab.check", "--json"]);
    assert_eq!(help.code, 0, "stderr={}", help.stderr);
    assert_eq!(help.json()["name"], json!("lab.check"));
    assert_eq!(help.json()["input_schema"]["required"], json!(["sensor"]));
}
