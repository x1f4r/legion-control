//! End-to-end tests for first-run setup (`meta.init` / `legion init`), AI agent registration
//! (`agent.*` / `legion agents`), multi-node configuration sync (`fleet.sync` / `config.*`), and
//! the new `meta.doctor` checks through real `legion` processes with isolated `LEGION_HOME`s.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::time::Instant;

use common::Home;
use serde_json::{Value, json};

const LEGION_BIN: &str = env!("CARGO_BIN_EXE_legion");
const HELPER_FLAG: &str = "--setup-helper";

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
            "meta_init_empty_idempotent_and_generated_roles",
            meta_init_empty_idempotent_and_generated_roles,
        ),
        (
            "agent_add_list_remove_against_fake_program_on_path",
            agent_add_list_remove_against_fake_program_on_path,
        ),
        (
            "fleet_sync_status_push_pull_conflicts_force_validation_backups_tools_d_and_permissions",
            fleet_sync_status_push_pull_conflicts_force_validation_backups_tools_d_and_permissions,
        ),
        (
            "meta_doctor_fleet_reachability_quick_and_policy_warning",
            meta_doctor_fleet_reachability_quick_and_policy_warning,
        ),
        (
            "secret_locations_refused_and_skipped_in_recursive_tools",
            secret_locations_refused_and_skipped_in_recursive_tools,
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
        // run <legion_bin> <legion_home> <user_home>
        "run" => {
            let status = Command::new(&rest[0])
                .arg("rpc")
                .env("LEGION_HOME", &rest[1])
                .env("HOME", &rest[2])
                .env("USERPROFILE", &rest[2])
                .env_remove("XDG_CONFIG_HOME")
                .env_remove("XDG_STATE_HOME")
                .stdin(Stdio::inherit())
                .stdout(Stdio::inherit())
                .stderr(Stdio::inherit())
                .status()
                .unwrap();
            exit_from_status(status)
        }
        // fail: simulates an unreachable machine immediately.
        "fail" => {
            eprintln!("connection refused");
            ExitCode::from(1)
        }
        // fake-claude <state_file> <log_file> <argv...>
        "fake-claude" => {
            let state_file = Path::new(&rest[0]);
            let log_file = Path::new(&rest[1]);
            let forwarded = &rest[2..];

            let mut file = OpenOptions::new()
                .create(true)
                .append(true)
                .open(log_file)
                .unwrap();
            writeln!(file, "{}", serde_json::to_string(forwarded).unwrap()).unwrap();

            match forwarded {
                [mcp, list] if mcp == "mcp" && list == "list" => {
                    if state_file.exists() {
                        println!(
                            "Checking MCP server health...\nlegion: /fake/legion mcp --as agent:claude --compact - ✓ Connected"
                        );
                    } else {
                        println!(
                            "No MCP servers configured. Use `claude mcp add` to add a server."
                        );
                    }
                    ExitCode::SUCCESS
                }
                [mcp, add, ..] if mcp == "mcp" && add == "add" => {
                    fs::write(state_file, b"registered").unwrap();
                    println!("Added stdio MCP server legion");
                    ExitCode::SUCCESS
                }
                [mcp, remove, ..] if mcp == "mcp" && remove == "remove" => {
                    let _ = fs::remove_file(state_file);
                    println!("Removed MCP server legion");
                    ExitCode::SUCCESS
                }
                _ => {
                    eprintln!("unexpected fake-claude args: {forwarded:?}");
                    ExitCode::from(1)
                }
            }
        }
        other => {
            eprintln!("unknown helper mode: {other}");
            ExitCode::from(2)
        }
    }
}

fn exit_from_status(status: std::process::ExitStatus) -> ExitCode {
    let code = status.code().unwrap_or(1);
    ExitCode::from(u8::try_from(code).unwrap_or(1))
}

fn self_exe() -> String {
    std::env::current_exe()
        .unwrap()
        .to_string_lossy()
        .into_owned()
}

fn run_argv(home: &Home) -> Vec<String> {
    vec![
        self_exe(),
        HELPER_FLAG.to_owned(),
        "run".to_owned(),
        LEGION_BIN.to_owned(),
        home.state()
            .parent()
            .unwrap()
            .to_string_lossy()
            .into_owned(),
        home.home().to_string_lossy().into_owned(),
    ]
}

fn fail_argv() -> Vec<String> {
    vec![self_exe(), HELPER_FLAG.to_owned(), "fail".to_owned()]
}

fn toml_argv(argv: &[String]) -> String {
    serde_json::to_string(argv).unwrap()
}

fn config_dir(home: &Home) -> PathBuf {
    home.state().parent().unwrap().join("config")
}

fn meta_init_empty_idempotent_and_generated_roles() {
    let home = Home::new();

    // Dry run reports what would be created without writing anything.
    let dry = home.run(&["init", "--name", "tower", "--dry-run"]);
    assert_eq!(dry.code, 0, "{}", dry.stderr);
    let dry_json = dry.json();
    assert_eq!(dry_json["node"], json!("tower"));
    assert_eq!(dry_json["created"].as_array().unwrap().len(), 2);
    assert!(dry_json["kept"].as_array().unwrap().is_empty());
    assert!(!config_dir(&home).join("node.toml").exists());
    assert!(!config_dir(&home).join("policy.toml").exists());

    // First real run creates both node.toml and policy.toml.
    let first = home.run(&["init", "--name", "tower"]);
    assert_eq!(first.code, 0, "{}", first.stderr);
    let first_json = first.json();
    assert_eq!(first_json["node"], json!("tower"));
    assert_eq!(first_json["created"].as_array().unwrap().len(), 2);
    assert!(first_json["kept"].as_array().unwrap().is_empty());
    let next: Vec<String> = first_json["next"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_owned())
        .collect();
    let joined_next = next.join("\n");
    assert!(joined_next.contains("fleet.discover"), "{joined_next}");
    assert!(joined_next.contains("agents add"), "{joined_next}");
    assert!(joined_next.contains("meta.doctor"), "{joined_next}");

    // Verify node.toml contents include commented busy = "none" and section examples.
    let node_text = fs::read_to_string(config_dir(&home).join("node.toml")).unwrap();
    assert!(node_text.contains("name = \"tower\""), "{node_text}");
    assert!(node_text.contains("# busy = \"none\""), "{node_text}");
    assert!(node_text.contains("# [svc]"), "{node_text}");
    assert!(node_text.contains("# [power]"), "{node_text}");
    assert!(node_text.contains("# [serve]"), "{node_text}");

    // Customise node.toml and re-run init: nothing changes byte for byte.
    let custom_node = format!("{node_text}\n# custom note kept verbatim\n");
    fs::write(config_dir(&home).join("node.toml"), &custom_node).unwrap();
    let policy_bytes = fs::read(config_dir(&home).join("policy.toml")).unwrap();

    let second = home.run(&["meta.init"]);
    assert_eq!(second.code, 0, "{}", second.stderr);
    let second_json = second.json();
    assert!(second_json["created"].as_array().unwrap().is_empty());
    assert_eq!(second_json["kept"].as_array().unwrap().len(), 2);
    assert_eq!(
        fs::read_to_string(config_dir(&home).join("node.toml")).unwrap(),
        custom_node
    );
    assert_eq!(
        fs::read(config_dir(&home).join("policy.toml")).unwrap(),
        policy_bytes
    );

    // Append principals referencing the three generated roles and verify real enforcement.
    let mut policy_with_principals = String::from_utf8(policy_bytes).unwrap();
    assert!(!policy_with_principals.contains("[roles.maintainer]"));
    policy_with_principals.push_str(
        "\n[principals.viewer]\nrole = \"observer\"\n\
         [principals.writer]\nrole = \"editor\"\n\
         [principals.ops]\nrole = \"operator\"\n",
    );
    home.write_config("policy.toml", &policy_with_principals);

    // Observer: can read inside ~/projects, denied outside ~/projects, cannot write or read audit.
    assert_eq!(home.run(&["meta.version", "--as", "viewer"]).code, 0);
    assert_eq!(
        home.run(&["fs.list", "path=~/projects", "--as", "viewer"])
            .code,
        0
    );
    let viewer_outside = home.run(&["fs.list", "path=~/.ssh", "--as", "viewer"]);
    assert_eq!(viewer_outside.code, 1);
    assert_eq!(viewer_outside.json()["code"], json!("denied"));
    let viewer_audit = home.run(&["audit.tail", "--as", "viewer"]);
    assert_eq!(viewer_audit.code, 1);
    assert_eq!(viewer_audit.json()["code"], json!("denied"));
    let viewer_write = home.run(&[
        "fs.write",
        "path=~/projects/app/nope.txt",
        "content=x",
        "--as",
        "viewer",
    ]);
    assert_eq!(viewer_write.code, 1);
    assert_eq!(viewer_write.json()["code"], json!("denied"));

    // Editor: can write under ~/projects, denied outside ~/projects, denied exec/destroy/admin.
    let ed_write = home.run(&[
        "fs.write",
        "path=~/projects/app/hello.txt",
        "content=hi",
        "--as",
        "writer",
    ]);
    assert_eq!(ed_write.code, 0, "{}", ed_write.stdout);
    let ed_outside = home.run(&["fs.list", "path=~/.ssh", "--as", "writer"]);
    assert_eq!(ed_outside.code, 1);
    assert_eq!(ed_outside.json()["code"], json!("denied"));
    let ed_exec = home.run(&["exec.run", "cmd=echo hi", "--as", "writer"]);
    assert_eq!(ed_exec.code, 1);
    assert_eq!(ed_exec.json()["code"], json!("denied"));
    let ed_admin = home.run(&["token.list", "--as", "writer"]);
    assert_eq!(ed_admin.code, 1);
    assert_eq!(ed_admin.json()["code"], json!("denied"));

    // Operator: can write, destroy, and read audit, but cannot administer or run power tools.
    let op_remove = home.run(&["fs.remove", "path=~/projects/app/hello.txt", "--as", "ops"]);
    assert_eq!(op_remove.code, 0, "{}", op_remove.stdout);
    assert_eq!(home.run(&["audit.tail", "--as", "ops"]).code, 0);
    let op_admin = home.run(&["token.list", "--as", "ops"]);
    assert_eq!(op_admin.code, 1);
    assert_eq!(op_admin.json()["code"], json!("denied"));
    let op_init = home.run(&["meta.init", "--as", "ops"]);
    assert_eq!(op_init.code, 1);
    assert_eq!(op_init.json()["code"], json!("denied"));
}

fn agent_add_list_remove_against_fake_program_on_path() {
    let home = Home::new();
    let fake_dir = tempfile::tempdir().unwrap();
    let tools_dir = fake_dir.path().join("fake_tools");
    fs::create_dir_all(&tools_dir).unwrap();
    let state_file = fake_dir.path().join("claude.state");
    let log_file = fake_dir.path().join("claude.log");

    install_fake_claude(&tools_dir, &state_file, &log_file);

    let run_with_fake_path = |args: &[&str]| {
        let output = home
            .command(args)
            .env("PATH", &tools_dir)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .unwrap();
        common::Run {
            code: output.status.code().unwrap_or(-1),
            stdout: String::from_utf8(output.stdout).unwrap(),
            stderr: String::from_utf8(output.stderr).unwrap(),
        }
    };

    // Registering as owner without --allow-owner is denied.
    let owner_denied = run_with_fake_path(&["agents", "add", "claude", "--as", "owner"]);
    assert_eq!(owner_denied.code, 1);
    assert_eq!(owner_denied.json()["code"], json!("denied"));

    // Unknown principal in policy.toml is refused with not_found.
    let missing_principal =
        run_with_fake_path(&["agents", "add", "claude", "--as", "agent:claude"]);
    assert_eq!(missing_principal.code, 1);
    assert_eq!(missing_principal.json()["code"], json!("not_found"));

    // Add principal to policy.toml.
    home.write_config(
        "policy.toml",
        "default = \"owner\"\n\n[roles.operator]\nallow = [\"effect:read\", \"effect:write\"]\npaths = [\"~/projects/**\"]\n\n[principals.\"agent:claude\"]\nrole = \"operator\"\n",
    );

    // dry_run returns the exact command without invoking the fake program.
    let dry = run_with_fake_path(&[
        "agents",
        "add",
        "claude",
        "--as",
        "agent:claude",
        "--dry-run",
    ]);
    assert_eq!(dry.code, 0, "{}", dry.stdout);
    let dry_json = dry.json();
    assert_eq!(dry_json["registered"], json!(false));
    assert!(!log_file.exists());
    let cmd = dry_json["command"].as_array().unwrap();
    assert_eq!(cmd[0], json!("claude"));
    assert_eq!(cmd[1], json!("mcp"));
    assert_eq!(cmd[2], json!("add"));
    assert_eq!(cmd[3], json!("-s"));
    assert_eq!(cmd[4], json!("user"));
    assert_eq!(cmd[5], json!("legion"));
    assert_eq!(cmd[6], json!("--"));
    assert!(Path::new(cmd[7].as_str().unwrap()).is_absolute());
    assert_eq!(cmd[8], json!("mcp"));
    assert_eq!(cmd[9], json!("--as"));
    assert_eq!(cmd[10], json!("agent:claude"));
    assert_eq!(cmd[11], json!("--compact"));

    // Real add runs the fake claude binary and records its exact argv.
    let added = run_with_fake_path(&["agents", "add", "claude", "--as", "agent:claude"]);
    assert_eq!(added.code, 0, "{}", added.stdout);
    assert_eq!(added.json()["registered"], json!(true));

    let calls: Vec<Vec<String>> = fs::read_to_string(&log_file)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let add_call = calls
        .iter()
        .find(|argv| argv.get(1).map(String::as_str) == Some("add"))
        .expect("fake claude should have received `mcp add`");
    assert_eq!(
        &add_call[..6],
        &["mcp", "add", "-s", "user", "legion", "--"]
    );
    assert_eq!(
        &add_call[7..],
        &["mcp", "--as", "agent:claude", "--compact"]
    );

    // agent.list reports claude installed & registered = yes, others not installed & unknown.
    let listed = run_with_fake_path(&["agents", "list"]);
    assert_eq!(listed.code, 0, "{}", listed.stdout);
    let agents = listed.json()["agents"].as_array().unwrap().clone();
    let claude = agents
        .iter()
        .find(|a| a["agent"] == json!("claude"))
        .unwrap();
    assert_eq!(claude["installed"], json!(true));
    assert_eq!(claude["registered"], json!("yes"));
    let codex = agents
        .iter()
        .find(|a| a["agent"] == json!("codex"))
        .unwrap();
    assert_eq!(codex["installed"], json!(false));
    assert_eq!(codex["registered"], json!("unknown"));

    // Uninstalled agent returns unsupported with a hint.
    let missing_prog = run_with_fake_path(&["agents", "add", "codex", "--as", "agent:claude"]);
    assert_eq!(missing_prog.code, 1);
    assert_eq!(missing_prog.json()["code"], json!("unsupported"));
    assert!(missing_prog.json()["hint"].is_string());

    // OpenCode remove returns unsupported cleanly.
    let oc_rm = run_with_fake_path(&["agents", "remove", "opencode", "--dry-run"]);
    assert_eq!(oc_rm.code, 1);
    assert_eq!(oc_rm.json()["code"], json!("unsupported"));

    // Remove claude via fake program, then list confirms registered = no.
    let removed = run_with_fake_path(&["agents", "remove", "claude"]);
    assert_eq!(removed.code, 0, "{}", removed.stdout);
    assert_eq!(removed.json()["removed"], json!(true));

    let after = run_with_fake_path(&["agents", "list"]);
    let after_claude = after.json()["agents"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["agent"] == json!("claude"))
        .unwrap()
        .clone();
    assert_eq!(after_claude["registered"], json!("no"));
}

fn install_fake_claude(dir: &Path, state_file: &Path, log_file: &Path) {
    let exe = self_exe();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let script_path = dir.join("claude");
        let script = format!(
            "#!/bin/sh\nexec '{}' '{}' fake-claude '{}' '{}' \"$@\"\n",
            exe.replace('\'', "'\\''"),
            HELPER_FLAG,
            state_file.display().to_string().replace('\'', "'\\''"),
            log_file.display().to_string().replace('\'', "'\\''"),
        );
        fs::write(&script_path, script).unwrap();
        fs::set_permissions(&script_path, fs::Permissions::from_mode(0o755)).unwrap();
    }
    #[cfg(windows)]
    {
        let script_path = dir.join("claude.cmd");
        let script = format!(
            "@echo off\r\n\"{}\" {} fake-claude \"{}\" \"{}\" %*\r\n",
            exe,
            HELPER_FLAG,
            state_file.display(),
            log_file.display(),
        );
        fs::write(&script_path, script).unwrap();
    }
}

fn fleet_sync_status_push_pull_conflicts_force_validation_backups_tools_d_and_permissions() {
    let laptop = Home::new();
    let pi = Home::new();

    laptop.write_config("node.toml", "name = \"laptop\"\n");
    pi.write_config("node.toml", "name = \"pi\"\n");

    let fleet_toml = format!("[machines.pi]\ncommand = {}\n", toml_argv(&run_argv(&pi)));
    laptop.write_config("fleet.toml", &fleet_toml);

    let policy_v1 = "\
default = \"owner\"

[roles.operator]
allow = [\"effect:read\", \"effect:write\", \"effect:exec\", \"effect:destroy\"]
deny = [\"effect:power\", \"effect:admin\"]

[principals.ops]
role = \"operator\"

[principals.semi]
role = \"owner\"
";
    let projects_v1 = "[projects.demo.on]\nlaptop = \"~/projects/demo\"\n";
    let tool_v1 = "name = \"ext.hello\"\ndescription = \"Say hello.\"\n";

    laptop.write_config("policy.toml", policy_v1);
    laptop.write_config("projects.toml", projects_v1);
    fs::create_dir_all(config_dir(&laptop).join("tools.d")).unwrap();
    fs::write(
        config_dir(&laptop).join("tools.d").join("hello.toml"),
        tool_v1,
    )
    .unwrap();

    // 1. Disallowed document names are refused locally and remotely.
    for bad in [
        "node.toml",
        "tokens.toml",
        "tools.d/../node.toml",
        "other.toml",
    ] {
        let sync_bad = laptop.run(&["fleet.sync", &format!("documents=[\"{bad}\"]")]);
        assert_eq!(sync_bad.code, 1, "{bad}");
        assert_eq!(sync_bad.json()["code"], json!("invalid_args"), "{bad}");

        let read_bad = laptop.run(&["config.read", &format!("document={bad}")]);
        assert_eq!(read_bad.code, 1, "{bad}");
        assert_eq!(read_bad.json()["code"], json!("invalid_args"), "{bad}");

        let write_bad = laptop.run(&[
            "config.write",
            &format!("document={bad}"),
            "content=name = \"x\"\n",
        ]);
        assert_eq!(write_bad.code, 1, "{bad}");
        assert_eq!(write_bad.json()["code"], json!("invalid_args"), "{bad}");
    }

    // 2. Non-admin role is denied on the local end.
    let local_denied = laptop.run(&["fleet.sync", "--as", "ops"]);
    assert_eq!(local_denied.code, 1);
    assert_eq!(local_denied.json()["code"], json!("denied"));

    // 3. Initial status shows differs without writing anything to pi or sidecars.
    let status = laptop.run(&[
        "fleet.sync",
        "documents=[\"policy.toml\",\"projects.toml\",\"tools.d/hello.toml\"]",
    ]);
    assert_eq!(status.code, 0, "{}", status.stdout);
    let pi_status = &status.json()["results"]["pi"];
    assert_eq!(pi_status["policy.toml"]["status"], json!("differs"));
    assert_eq!(pi_status["projects.toml"]["status"], json!("differs"));
    assert_eq!(pi_status["tools.d/hello.toml"]["status"], json!("differs"));
    assert!(!config_dir(&pi).join("policy.toml").exists());

    // 4. Push sends all three documents to pi, records sidecars on laptop, and audits before/after hashes on pi.
    let pushed = laptop.run(&[
        "fleet.sync",
        "direction=push",
        "documents=[\"policy.toml\",\"projects.toml\",\"tools.d/hello.toml\"]",
    ]);
    assert_eq!(pushed.code, 0, "{}", pushed.stdout);
    let pi_pushed = &pushed.json()["results"]["pi"];
    assert_eq!(pi_pushed["policy.toml"]["status"], json!("pushed"));
    assert_eq!(pi_pushed["projects.toml"]["status"], json!("pushed"));
    assert_eq!(pi_pushed["tools.d/hello.toml"]["status"], json!("pushed"));

    assert_eq!(
        fs::read_to_string(config_dir(&pi).join("policy.toml")).unwrap(),
        policy_v1
    );
    assert_eq!(
        fs::read_to_string(config_dir(&pi).join("projects.toml")).unwrap(),
        projects_v1
    );
    assert_eq!(
        fs::read_to_string(config_dir(&pi).join("tools.d").join("hello.toml")).unwrap(),
        tool_v1
    );

    // Sidecar exists on laptop with per-peer hash.
    let sidecar_path = laptop
        .state()
        .join("sync")
        .join("sidecars")
        .join("policy.toml.json");
    let sidecar: Value = serde_json::from_str(&fs::read_to_string(&sidecar_path).unwrap()).unwrap();
    assert_eq!(sidecar["last_peer"], json!("pi"));
    assert!(
        sidecar["peers"]["pi"]["hash"]
            .as_str()
            .unwrap()
            .starts_with("blake3:")
    );

    // Audit log on pi records hash_before and hash_after for policy.toml.
    let pi_audit = pi.audit();
    assert!(
        pi_audit.iter().any(|entry| {
            entry["tool"] == json!("config.write.applied")
                && entry["args"]["document"] == json!("policy.toml")
                && entry["args"]["hash_before"].is_null()
                && entry["args"]["hash_after"]
                    .as_str()
                    .is_some_and(|h| h.starts_with("blake3:"))
        }),
        "{pi_audit:?}"
    );

    // 5. Status after push reports same.
    let status_same = laptop.run(&[
        "fleet.sync",
        "documents=[\"policy.toml\",\"projects.toml\",\"tools.d/hello.toml\"]",
    ]);
    assert_eq!(
        status_same.json()["results"]["pi"]["projects.toml"]["status"],
        json!("same")
    );

    // 6. Clean fast-forward push creates a backup on pi.
    let projects_v2 =
        "[projects.demo.on]\nlaptop = \"~/projects/demo\"\npi = \"~/projects/demo\"\n";
    laptop.write_config("projects.toml", projects_v2);
    let push_v2 = laptop.run(&[
        "fleet.sync",
        "direction=push",
        "documents=[\"projects.toml\"]",
    ]);
    assert_eq!(push_v2.code, 0, "{}", push_v2.stdout);
    let v2_entry = &push_v2.json()["results"]["pi"]["projects.toml"];
    assert_eq!(v2_entry["status"], json!("pushed"));
    let backup_path = v2_entry["backup"].as_str().expect("backup path expected");
    assert_eq!(fs::read_to_string(backup_path).unwrap(), projects_v1);
    assert_eq!(
        fs::read_to_string(config_dir(&pi).join("projects.toml")).unwrap(),
        projects_v2
    );

    // 7. Remote changed since last sync -> conflict (nothing overwritten), then force=true overwrites.
    let projects_pi_edit = "[projects.pi_only.on]\npi = \"~/projects/pi_only\"\n";
    let projects_laptop_edit = "[projects.laptop_only.on]\nlaptop = \"~/projects/laptop_only\"\n";
    pi.write_config("projects.toml", projects_pi_edit);
    laptop.write_config("projects.toml", projects_laptop_edit);

    let conflict = laptop.run(&[
        "fleet.sync",
        "direction=push",
        "documents=[\"projects.toml\"]",
    ]);
    assert_eq!(conflict.code, 0, "{}", conflict.stdout);
    assert_eq!(
        conflict.json()["results"]["pi"]["projects.toml"]["status"],
        json!("conflict")
    );
    assert_eq!(
        fs::read_to_string(config_dir(&pi).join("projects.toml")).unwrap(),
        projects_pi_edit
    );

    let forced = laptop.run(&[
        "fleet.sync",
        "direction=push",
        "documents=[\"projects.toml\"]",
        "force=true",
    ]);
    assert_eq!(forced.code, 0, "{}", forced.stdout);
    assert_eq!(
        forced.json()["results"]["pi"]["projects.toml"]["status"],
        json!("pushed")
    );
    assert_eq!(
        fs::read_to_string(config_dir(&pi).join("projects.toml")).unwrap(),
        projects_laptop_edit
    );
    assert_eq!(
        fs::read_to_string(
            pi.state()
                .join("sync")
                .join("backups")
                .join("projects.toml.bak")
        )
        .unwrap(),
        projects_pi_edit
    );

    // 8. Pull brings changes and new tools.d files back from pi, and detects local conflicts.
    let tool_v2 = "name = \"ext.hello\"\ndescription = \"Say hello from pi.\"\n";
    let tool_remote_only = "name = \"ext.ping\"\ndescription = \"Ping from pi.\"\n";
    fs::write(config_dir(&pi).join("tools.d").join("hello.toml"), tool_v2).unwrap();
    fs::write(
        config_dir(&pi).join("tools.d").join("remote_only.toml"),
        tool_remote_only,
    )
    .unwrap();

    let pulled = laptop.run(&["fleet.sync", "direction=pull", "from=pi"]);
    assert_eq!(pulled.code, 0, "{}", pulled.stdout);
    let pulled_pi = &pulled.json()["results"]["pi"];
    assert_eq!(pulled_pi["tools.d/hello.toml"]["status"], json!("pulled"));
    assert_eq!(
        pulled_pi["tools.d/remote_only.toml"]["status"],
        json!("pulled")
    );
    assert_eq!(
        fs::read_to_string(config_dir(&laptop).join("tools.d").join("hello.toml")).unwrap(),
        tool_v2
    );
    assert_eq!(
        fs::read_to_string(config_dir(&laptop).join("tools.d").join("remote_only.toml")).unwrap(),
        tool_remote_only
    );
    assert_eq!(
        fs::read_to_string(
            laptop
                .state()
                .join("sync")
                .join("backups")
                .join("tools.d")
                .join("hello.toml.bak")
        )
        .unwrap(),
        tool_v1
    );

    // Divergent local edit causes pull conflict until force=true.
    fs::write(
        config_dir(&laptop).join("tools.d").join("hello.toml"),
        "name = \"ext.hello\"\ndescription = \"Local edit.\"\n",
    )
    .unwrap();
    fs::write(
        config_dir(&pi).join("tools.d").join("hello.toml"),
        "name = \"ext.hello\"\ndescription = \"Remote edit.\"\n",
    )
    .unwrap();
    let pull_conflict = laptop.run(&[
        "fleet.sync",
        "direction=pull",
        "from=pi",
        "documents=[\"tools.d/hello.toml\"]",
    ]);
    assert_eq!(
        pull_conflict.json()["results"]["pi"]["tools.d/hello.toml"]["status"],
        json!("conflict")
    );

    // 9. Invalid policy refused by receiver with sender told why (existing policy kept).
    let invalid_policy = laptop.run(&[
        "config.write",
        "document=policy.toml",
        "content=default = 123\n",
        "--on",
        "pi",
    ]);
    assert_eq!(invalid_policy.code, 1);
    let err = invalid_policy.json();
    assert_eq!(err["code"], json!("failed"));
    assert!(
        err["message"].as_str().unwrap().contains("policy.toml")
            && err["message"].as_str().unwrap().contains("line 1"),
        "{err}"
    );
    assert_eq!(
        fs::read_to_string(config_dir(&pi).join("policy.toml")).unwrap(),
        policy_v1
    );

    // Also verify invalid projects.toml pushed through fleet.sync is refused by receiver and reported to sender.
    laptop.write_config("projects.toml", "[projects.bad]\nunknown_key = 1\n");
    let bad_push = laptop.run(&[
        "fleet.sync",
        "direction=push",
        "documents=[\"projects.toml\"]",
        "force=true",
    ]);
    assert_eq!(bad_push.code, 0, "{}", bad_push.stdout);
    let bad_entry = &bad_push.json()["results"]["pi"]["projects.toml"];
    assert_eq!(bad_entry["status"], json!("error"));
    assert!(
        bad_entry["error"]["message"]
            .as_str()
            .unwrap()
            .contains("unknown_key"),
        "{bad_entry}"
    );
    assert_eq!(
        fs::read_to_string(config_dir(&pi).join("projects.toml")).unwrap(),
        projects_laptop_edit
    );

    // 10. Non-admin role on the remote end is denied even when allowed on the local end.
    let pi_narrow_policy = "\
default = \"owner\"

[roles.operator]
allow = [\"effect:read\", \"effect:write\", \"effect:exec\", \"effect:destroy\"]
deny = [\"effect:power\", \"effect:admin\"]

[principals.semi]
role = \"operator\"
";
    pi.write_config("policy.toml", pi_narrow_policy);
    let remote_denied = laptop.run(&[
        "fleet.sync",
        "direction=status",
        "documents=[\"projects.toml\"]",
        "--as",
        "semi",
    ]);
    assert_eq!(remote_denied.code, 0, "{}", remote_denied.stdout);
    let semi_entry = &remote_denied.json()["results"]["pi"]["projects.toml"];
    assert_eq!(semi_entry["status"], json!("error"));
    assert_eq!(semi_entry["error"]["code"], json!("denied"));
}

fn meta_doctor_fleet_reachability_quick_and_policy_warning() {
    let laptop = Home::new();
    let pi = Home::new();

    laptop.run(&["init", "--name", "laptop"]);
    let mut policy = fs::read_to_string(config_dir(&laptop).join("policy.toml")).unwrap();
    policy.push_str(
        "\n[roles.confined_exec]\nallow = [\"effect:read\", \"effect:exec\"]\npaths = [\"~/projects/**\"]\n",
    );
    laptop.write_config("policy.toml", &policy);
    pi.write_config("node.toml", "name = \"pi\"\n");

    let fleet_toml = format!(
        "[machines.pi]\ncommand = {}\n\n[machines.ghost]\ncommand = {}\n",
        toml_argv(&run_argv(&pi)),
        toml_argv(&fail_argv()),
    );
    laptop.write_config("fleet.toml", &fleet_toml);

    // Full doctor pings fleet machines concurrently and warns about operator combining paths + exec.
    let full = laptop.run(&["meta.doctor"]);
    assert_eq!(full.code, 0, "{}", full.stderr);
    let full_json = full.json();
    assert_eq!(full_json["ok"], json!(false), "{full_json}");
    let checks = full_json["checks"].as_array().unwrap();

    let pi_check = checks
        .iter()
        .find(|c| c["name"] == json!("fleet: pi"))
        .unwrap();
    assert_eq!(pi_check["ok"], json!(true), "{pi_check}");
    let ghost_check = checks
        .iter()
        .find(|c| c["name"] == json!("fleet: ghost"))
        .unwrap();
    assert_eq!(ghost_check["ok"], json!(false), "{ghost_check}");

    let policy_warn = checks
        .iter()
        .find(|c| c["name"] == json!("policy exec with paths"))
        .unwrap();
    assert_eq!(policy_warn["ok"], json!(true), "{policy_warn}");
    assert_eq!(policy_warn["warning"], json!(true), "{policy_warn}");
    assert!(
        policy_warn["detail"]
            .as_str()
            .unwrap()
            .contains("as strong as the account"),
        "{policy_warn}"
    );

    // quick=true skips fleet reachability checks so the local node reports healthy.
    let quick = laptop.run(&["meta.doctor", "quick=true"]);
    assert_eq!(quick.code, 0, "{}", quick.stderr);
    let quick_json = quick.json();
    assert_eq!(quick_json["ok"], json!(true), "{quick_json}");
    let quick_checks = quick_json["checks"].as_array().unwrap();
    assert!(
        !quick_checks.iter().any(|c| c["name"]
            .as_str()
            .unwrap_or_default()
            .starts_with("fleet: ")),
        "{quick_checks:?}"
    );
}

fn secret_locations_refused_and_skipped_in_recursive_tools() {
    let home = Home::new();
    let user_home = home.home();

    fs::create_dir_all(user_home.join(".ssh")).unwrap();
    fs::write(user_home.join(".ssh").join("id_ed25519"), "SECRET_SSH_KEY").unwrap();
    fs::write(user_home.join(".claude.json"), "SECRET_CLAUDE_TOKEN").unwrap();
    fs::write(
        user_home.join("projects").join("app").join("pub.txt"),
        "PUBLIC_NOTE",
    )
    .unwrap();

    home.write_config(
        "policy.toml",
        "\
default = \"owner\"

[roles.wide]
allow = [\"effect:read\", \"effect:write\"]

[roles.home_only]
allow = [\"effect:read\"]
paths = [\"~/**\"]

[roles.ssh_allowed]
allow = [\"effect:read\"]
paths = [\"~/.ssh/**\"]

[principals.wide]
role = \"wide\"

[principals.homey]
role = \"home_only\"

[principals.sshuser]
role = \"ssh_allowed\"
",
    );

    // Direct stat/read on ~/.ssh/id_ed25519 and ~/.claude.json refused for unconfined non-admin and `~/**`.
    for principal in ["wide", "homey"] {
        for target in ["~/.ssh/id_ed25519", "~/.claude.json"] {
            let stat = home.run(&["fs.stat", &format!("path={target}"), "--as", principal]);
            assert_eq!(stat.code, 1, "{principal} {target}");
            assert_eq!(stat.json()["code"], json!("denied"));
            assert!(
                stat.json()["message"]
                    .as_str()
                    .unwrap()
                    .contains("secret locations"),
                "{}",
                stat.stdout
            );
        }
    }

    // Explicit opt-in in paths allows reading ~/.ssh/id_ed25519.
    let opted_in = home.run(&["fs.read", "path=~/.ssh/id_ed25519", "--as", "sshuser"]);
    assert_eq!(opted_in.code, 0, "{}", opted_in.stdout);
    assert!(
        opted_in.json()["content"]
            .as_str()
            .unwrap()
            .contains("SECRET_SSH_KEY")
    );

    // Recursive tools skip secret locations and report them in `skipped`.
    let listed = home.run(&["fs.list", "path=~", "hidden=true", "--as", "wide"]);
    assert_eq!(listed.code, 0, "{}", listed.stdout);
    let listed_json = listed.json();
    let skipped = listed_json["skipped"].as_array().unwrap();
    assert!(
        skipped
            .iter()
            .any(|p| p.as_str().is_some_and(|s| s.ends_with(".ssh"))),
        "{listed_json}"
    );
    assert!(
        skipped
            .iter()
            .any(|p| p.as_str().is_some_and(|s| s.ends_with(".claude.json"))),
        "{listed_json}"
    );
    assert!(
        !listed_json["entries"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["name"] == json!(".ssh") || e["name"] == json!(".claude.json")),
        "{listed_json}"
    );

    let found = home.run(&["fs.find", "path=~", "ignore=false", "--as", "wide"]);
    assert_eq!(found.code, 0, "{}", found.stdout);
    let found_json = found.json();
    assert!(
        !found_json["matches"]
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m["path"].as_str().is_some_and(|s| s.contains("id_ed25519"))),
        "{found_json}"
    );
    assert!(
        found_json["skipped"]
            .as_array()
            .unwrap()
            .iter()
            .any(|p| p.as_str().is_some_and(|s| s.ends_with(".ssh"))),
        "{found_json}"
    );

    let grepped = home.run(&[
        "fs.grep",
        "path=~",
        "pattern=SECRET_",
        "ignore=false",
        "--as",
        "wide",
    ]);
    assert_eq!(grepped.code, 0, "{}", grepped.stdout);
    let grepped_json = grepped.json();
    assert!(
        grepped_json["matches"].as_array().unwrap().is_empty(),
        "{grepped_json}"
    );
    assert!(
        grepped_json["skipped"]
            .as_array()
            .unwrap()
            .iter()
            .any(|p| p.as_str().is_some_and(|s| s.ends_with(".ssh"))),
        "{grepped_json}"
    );

    let manifest = home.run(&["fs.manifest", "path=~", "--as", "wide"]);
    assert_eq!(manifest.code, 0, "{}", manifest.stdout);
    let manifest_json = manifest.json();
    assert!(
        !manifest_json["entries"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["path"]
                .as_str()
                .is_some_and(|p| p.contains(".ssh") || p.contains(".claude.json"))),
        "{manifest_json}"
    );
    assert!(
        manifest_json["skipped"]
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s["path"] == json!(".ssh")),
        "{manifest_json}"
    );
    assert!(
        manifest_json["skipped"]
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s["path"] == json!(".claude.json")),
        "{manifest_json}"
    );

    // Owner (admin) is not blocked.
    let owner_read = home.run(&["fs.read", "path=~/.ssh/id_ed25519"]);
    assert_eq!(owner_read.code, 0, "{}", owner_read.stdout);
}
