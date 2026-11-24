//! Extension tools (`tools.d/*.toml` and `ext.*`), called through a real `Node` under a temporary
//! `LEGION_HOME`. Re-executes its own test binary as the extension's command so every test works
//! identically on Linux, macOS and Windows.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, missing_docs)]

mod support;

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::path::Path;
use std::process::ExitCode;
use std::time::{Duration, Instant};

use legion_core::CancellationToken;
use legion_core::proto::{ErrorCode, EventKind};
use serde_json::{Value, json};
use support::{Events, Fixture, Test, alive, read_pids, this_binary};

const HELPER_FLAG: &str = "--ext-helper";

fn main() -> ExitCode {
    let raw_args: Vec<String> = std::env::args().skip(1).collect();
    if raw_args.first().map(String::as_str) == Some(HELPER_FLAG) {
        return run_helper(&raw_args[1..]);
    }
    let tests: &[Test] = &[
        ("define_show_list_replace_and_remove_lifecycle", || {
            Box::pin(define_show_list_replace_and_remove_lifecycle())
        }),
        (
            "call_passes_args_as_env_and_stdin_json_with_defaults",
            || Box::pin(call_passes_args_as_env_and_stdin_json_with_defaults()),
        ),
        (
            "result_shapes_json_object_scalar_text_and_truncated",
            || Box::pin(result_shapes_json_object_scalar_text_and_truncated()),
        ),
        (
            "failing_command_and_missing_program_return_closed_codes",
            || Box::pin(failing_command_and_missing_program_return_closed_codes()),
        ),
        ("timeout_stops_whole_process_tree", || {
            Box::pin(timeout_stops_whole_process_tree())
        }),
        ("cancellation_stops_whole_process_tree", || {
            Box::pin(cancellation_stops_whole_process_tree())
        }),
        ("argument_validation_fails_before_command_starts", || {
            Box::pin(argument_validation_fails_before_command_starts())
        }),
        (
            "shell_metacharacters_arrive_byte_for_byte_with_run_and_cmd",
            || Box::pin(shell_metacharacters_arrive_byte_for_byte_with_run_and_cmd()),
        ),
        (
            "policy_enforces_declared_effect_and_admin_for_define_remove",
            || Box::pin(policy_enforces_declared_effect_and_admin_for_define_remove()),
        ),
        ("builtin_name_and_reserved_families_are_refused", || {
            Box::pin(builtin_name_and_reserved_families_are_refused())
        }),
        (
            "every_builtin_registry_family_is_dynamically_reserved",
            || Box::pin(every_builtin_registry_family_is_dynamically_reserved()),
        ),
        (
            "broken_file_is_skipped_and_reported_while_others_work",
            || Box::pin(broken_file_is_skipped_and_reported_while_others_work()),
        ),
        (
            "hot_added_and_edited_files_are_picked_up_without_restart",
            || Box::pin(hot_added_and_edited_files_are_picked_up_without_restart()),
        ),
    ];
    support::main(tests)
}

fn run_helper(args: &[String]) -> ExitCode {
    let Some((mode, rest)) = args.split_first() else {
        eprintln!("missing helper mode");
        return ExitCode::from(2);
    };
    let mut stdout = std::io::stdout();
    match mode.as_str() {
        // Emits a JSON object describing all LEGION_* env vars, stdin JSON, and cwd.
        "inspect" => {
            let mut stdin_bytes = Vec::new();
            let _ = std::io::stdin().read_to_end(&mut stdin_bytes);
            let stdin_text = String::from_utf8_lossy(&stdin_bytes).into_owned();
            let stdin_json: Value = serde_json::from_str(&stdin_text).unwrap_or(Value::Null);
            let legion_env: BTreeMap<String, String> = std::env::vars()
                .filter(|(k, _)| k.starts_with("LEGION_"))
                .collect();
            let cwd = std::env::current_dir()
                .map(|p| p.display().to_string())
                .unwrap_or_default();
            let out = json!({
                "env": legion_env,
                "stdin": stdin_json,
                "cwd": cwd,
            });
            let _ = writeln!(stdout, "{out}");
        }
        // Prints literal text given as rest[0].
        "print" => {
            let _ = write!(stdout, "{}", rest[0]);
            if let Some(err) = rest.get(1) {
                let _ = write!(std::io::stderr(), "{err}");
            }
        }
        // Floods stdout with N bytes.
        "flood" => {
            let size: usize = rest[0].parse().unwrap();
            let middle = size.saturating_sub(10);
            let _ = stdout.write_all(b"START");
            let _ = stdout.write_all(&vec![b'x'; middle]);
            let _ = stdout.write_all(b"END!!");
        }
        // Writes stderr and exits with given code.
        "fail" => {
            let code: u8 = rest[0].parse().unwrap();
            let msg = rest.get(1).map_or("boom", String::as_str);
            let _ = write!(std::io::stderr(), "{msg}");
            let _ = std::io::stderr().flush();
            return ExitCode::from(code);
        }
        // Creates a marker file at rest[0].
        "touch" => {
            std::fs::write(&rest[0], b"ran").unwrap();
            let _ = writeln!(stdout, "{{\"touched\": true}}");
        }
        // Starts a stubborn sleeping grandchild, records both pids to rest[0], then waits.
        "family" => {
            let _ignoring = ignore_terminate();
            let mut grandchild = std::process::Command::new(this_binary())
                .args([HELPER_FLAG, "stubborn"])
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .unwrap();
            let pids = format!("{} {}", std::process::id(), grandchild.id());
            let path = Path::new(&rest[0]);
            let tmp = path.with_extension("partial");
            std::fs::write(&tmp, pids).unwrap();
            std::fs::rename(&tmp, path).unwrap();
            let _ = writeln!(stdout, "started");
            let _ = stdout.flush();
            let _ = grandchild.wait();
        }
        "stubborn" => {
            let _ignoring = ignore_terminate();
            std::thread::sleep(Duration::from_secs(60));
        }
        other => {
            let _ = writeln!(std::io::stderr(), "unknown helper mode {other}");
            return ExitCode::from(101);
        }
    }
    let _ = stdout.flush();
    ExitCode::SUCCESS
}

#[cfg(unix)]
fn ignore_terminate() -> Option<tokio::runtime::Runtime> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .ok()?;
    let signal = runtime.block_on(async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
    });
    drop(signal);
    Some(runtime)
}

#[cfg(not(unix))]
fn ignore_terminate() -> Option<tokio::runtime::Runtime> {
    None
}

fn helper_argv(mode: &str, extra: &[&str]) -> Vec<String> {
    let mut argv = vec![this_binary(), HELPER_FLAG.to_owned(), mode.to_owned()];
    argv.extend(extra.iter().map(|s| (*s).to_owned()));
    argv
}

fn helper_shell_cmd(mode: &str) -> String {
    #[cfg(unix)]
    {
        format!(
            "'{}' {} {mode}",
            this_binary().replace('\'', "'\\''"),
            HELPER_FLAG
        )
    }
    #[cfg(windows)]
    {
        format!(
            "& '{}' {} {mode}",
            this_binary().replace('\'', "''"),
            HELPER_FLAG
        )
    }
}

async fn define_show_list_replace_and_remove_lifecycle() {
    let fixture = Fixture::new();

    // Initially empty.
    let listed = fixture.call("ext.list", json!({})).await.unwrap();
    assert_eq!(listed, json!({"tools": [], "broken": []}));

    // Define an extension.
    let created = fixture
        .call(
            "ext.define",
            json!({
                "name": "gpu.free",
                "description": "Free video memory in MiB.",
                "effect": "read",
                "run": helper_argv("print", &["{\"free_mib\": 8192}"]),
                "timeout_s": 30,
                "cwd": "~",
                "params": {
                    "index": {
                        "type": "integer",
                        "description": "Which GPU.",
                        "required": false,
                        "default": 0
                    }
                }
            }),
        )
        .await
        .unwrap();
    assert_eq!(created["name"], json!("gpu.free"));
    assert_eq!(created["created"], json!(true));
    assert!(created.get("replaced").is_none());

    // Duplicate define without replace is refused with Exists.
    let dup = fixture
        .call(
            "ext.define",
            json!({
                "name": "gpu.free",
                "description": "Updated.",
                "effect": "read",
                "run": helper_argv("print", &["{\"free_mib\": 1}"])
            }),
        )
        .await
        .unwrap_err();
    assert_eq!(dup.code, ErrorCode::Exists);

    // ext.list shows it.
    let listed = fixture.call("ext.list", json!({})).await.unwrap();
    assert_eq!(listed["tools"].as_array().unwrap().len(), 1);
    assert_eq!(listed["tools"][0]["name"], json!("gpu.free"));
    assert_eq!(listed["tools"][0]["effect"], json!("read"));
    assert_eq!(listed["tools"][0]["params"], json!(["index"]));
    assert_eq!(listed["broken"], json!([]));

    // ext.show returns full stored definition.
    let shown = fixture
        .call("ext.show", json!({"name": "gpu.free"}))
        .await
        .unwrap();
    assert_eq!(shown["name"], json!("gpu.free"));
    assert_eq!(shown["timeout_s"], json!(30));
    assert_eq!(shown["cwd"], json!("~"));
    assert_eq!(shown["params"]["index"]["default"], json!(0));

    // Replace with replace=true.
    let replaced = fixture
        .call(
            "ext.define",
            json!({
                "name": "gpu.free",
                "description": "Replaced description.",
                "effect": "read",
                "run": helper_argv("print", &["{\"free_mib\": 4096}"]) ,
                "replace": true
            }),
        )
        .await
        .unwrap();
    assert_eq!(replaced["replaced"], json!(true));
    assert!(replaced.get("created").is_none());

    let out = fixture.call("gpu.free", json!({})).await.unwrap();
    assert_eq!(out, json!({"free_mib": 4096}));

    // Remove it.
    let removed = fixture
        .call("ext.remove", json!({"name": "gpu.free"}))
        .await
        .unwrap();
    assert_eq!(removed, json!({"name": "gpu.free", "removed": true}));
    assert_eq!(
        fixture.call("gpu.free", json!({})).await.unwrap_err().code,
        ErrorCode::UnknownTool
    );
    assert_eq!(
        fixture
            .call("ext.remove", json!({"name": "gpu.free"}))
            .await
            .unwrap_err()
            .code,
        ErrorCode::NotFound
    );
}

async fn call_passes_args_as_env_and_stdin_json_with_defaults() {
    let fixture = Fixture::new();
    fixture.write_config("node.toml", "name = \"tower\"\n");
    fixture
        .call(
            "ext.define",
            json!({
                "name": "demo.inspect",
                "description": "Inspect env, stdin and cwd.",
                "effect": "read",
                "run": helper_argv("inspect", &[]),
                "cwd": "~",
                "params": {
                    "msg": {"type": "string", "required": true},
                    "count": {"type": "integer", "default": 7},
                    "ratio": {"type": "number"},
                    "flag": {"type": "boolean", "default": true},
                    "optional_unset": {"type": "string"}
                }
            }),
        )
        .await
        .unwrap();

    let out = fixture
        .call("demo.inspect", json!({"msg": "hello world", "ratio": 2.5}))
        .await
        .unwrap();
    let env = out["env"].as_object().unwrap();
    assert_eq!(env["LEGION_TOOL"], json!("demo.inspect"));
    assert_eq!(env["LEGION_PRINCIPAL"], json!("owner"));
    assert_eq!(env["LEGION_NODE"], json!("tower"));
    assert_eq!(env["LEGION_ARG_MSG"], json!("hello world"));
    assert_eq!(env["LEGION_ARG_COUNT"], json!("7"));
    assert_eq!(env["LEGION_ARG_RATIO"], json!("2.5"));
    assert_eq!(env["LEGION_ARG_FLAG"], json!("true"));
    assert!(!env.contains_key("LEGION_ARG_OPTIONAL_UNSET"));

    assert_eq!(
        out["stdin"],
        json!({
            "count": 7,
            "flag": true,
            "msg": "hello world",
            "ratio": 2.5,
        })
    );

    let reported_cwd = std::fs::canonicalize(out["cwd"].as_str().unwrap()).unwrap();
    let expected_home = std::fs::canonicalize(fixture.home()).unwrap();
    assert_eq!(reported_cwd, expected_home);

    // Extension calls are audited just like built-ins.
    let audit = fixture.call("audit.tail", json!({})).await.unwrap();
    let entries = audit["entries"].as_array().unwrap();
    assert!(
        entries
            .iter()
            .any(|e| e["tool"] == json!("demo.inspect") && e["outcome"] == json!("ok")),
        "{entries:?}"
    );
}

async fn result_shapes_json_object_scalar_text_and_truncated() {
    let fixture = Fixture::new();

    // JSON object
    fixture
        .call(
            "ext.define",
            json!({
                "name": "out.obj",
                "description": "JSON object.",
                "effect": "read",
                "run": helper_argv("print", &["{\"ok\":true,\"n\":3}\n", "stderr note"])
            }),
        )
        .await
        .unwrap();
    let events = Events::default();
    let obj = fixture
        .call_with("out.obj", json!({}), &events, CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(obj, json!({"ok": true, "n": 3}));
    assert_eq!(events.text(EventKind::Stdout), "{\"ok\":true,\"n\":3}\n");
    assert_eq!(events.text(EventKind::Stderr), "stderr note");

    // JSON scalar & array
    fixture
        .call(
            "ext.define",
            json!({
                "name": "out.scalar",
                "description": "JSON scalar.",
                "effect": "read",
                "run": helper_argv("print", &["42\n"])
            }),
        )
        .await
        .unwrap();
    assert_eq!(
        fixture.call("out.scalar", json!({})).await.unwrap(),
        json!({"value": 42})
    );

    fixture
        .call(
            "ext.define",
            json!({
                "name": "out.array",
                "description": "JSON array.",
                "effect": "read",
                "run": helper_argv("print", &["[\"a\",\"b\"]"])
            }),
        )
        .await
        .unwrap();
    assert_eq!(
        fixture.call("out.array", json!({})).await.unwrap(),
        json!({"value": ["a", "b"]})
    );

    // Plain text
    fixture
        .call(
            "ext.define",
            json!({
                "name": "out.text",
                "description": "Plain text.",
                "effect": "read",
                "run": helper_argv("print", &["hello plain\n"])
            }),
        )
        .await
        .unwrap();
    assert_eq!(
        fixture.call("out.text", json!({})).await.unwrap(),
        json!({"output": "hello plain\n", "truncated": false})
    );

    // Truncated text (exceeds 64 KiB capture limit)
    fixture
        .call(
            "ext.define",
            json!({
                "name": "out.flood",
                "description": "Large text.",
                "effect": "read",
                "run": helper_argv("flood", &["100000"])
            }),
        )
        .await
        .unwrap();
    let flooded = fixture.call("out.flood", json!({})).await.unwrap();
    assert_eq!(flooded["truncated"], json!(true));
    let text = flooded["output"].as_str().unwrap();
    assert!(text.starts_with("START"), "{text}");
    assert!(text.ends_with("END!!"), "{text}");
    assert!(text.contains("bytes omitted"), "{text}");
}

async fn failing_command_and_missing_program_return_closed_codes() {
    let fixture = Fixture::new();

    fixture
        .call(
            "ext.define",
            json!({
                "name": "err.fail",
                "description": "Exits non-zero.",
                "effect": "read",
                "run": helper_argv("fail", &["9", "something broke on stderr\n"])
            }),
        )
        .await
        .unwrap();
    let err = fixture.call("err.fail", json!({})).await.unwrap_err();
    assert_eq!(err.code, ErrorCode::Failed);
    let details = err.details.unwrap();
    assert_eq!(details["exit_code"], json!(9));
    assert!(
        details["stderr"]
            .as_str()
            .unwrap()
            .contains("something broke on stderr")
    );

    fixture
        .call(
            "ext.define",
            json!({
                "name": "err.missing",
                "description": "Missing binary.",
                "effect": "read",
                "run": ["legion-no-such-ext-program-9b3e"]
            }),
        )
        .await
        .unwrap();
    let err = fixture.call("err.missing", json!({})).await.unwrap_err();
    assert_eq!(err.code, ErrorCode::NotFound);
    assert!(
        err.message.contains("legion-no-such-ext-program-9b3e"),
        "{}",
        err.message
    );
}

async fn timeout_stops_whole_process_tree() {
    let fixture = Fixture::new();
    let pids_file = fixture.dir.path().join("timeout-pids");
    fixture
        .call(
            "ext.define",
            json!({
                "name": "tree.slow",
                "description": "Times out with a stubborn grandchild.",
                "effect": "exec",
                "timeout_s": 2,
                "run": helper_argv("family", &[&pids_file.to_string_lossy()])
            }),
        )
        .await
        .unwrap();

    let started = Instant::now();
    let err = fixture.call("tree.slow", json!({})).await.unwrap_err();
    assert_eq!(err.code, ErrorCode::Timeout, "{err:?}");
    assert!(started.elapsed() < Duration::from_secs(30));
    for pid in read_pids(&pids_file).await {
        assert!(!alive(pid), "process {pid} outlived extension timeout");
    }
}

async fn cancellation_stops_whole_process_tree() {
    let fixture = Fixture::new();
    let pids_file = fixture.dir.path().join("cancel-pids");
    fixture
        .call(
            "ext.define",
            json!({
                "name": "tree.hang",
                "description": "Hangs until cancelled.",
                "effect": "exec",
                "run": helper_argv("family", &[&pids_file.to_string_lossy()])
            }),
        )
        .await
        .unwrap();

    let cancel = CancellationToken::new();
    let events = Events::default();
    let call = fixture.call_with("tree.hang", json!({}), &events, cancel.clone());
    let cancelling = async {
        let pids = read_pids(&pids_file).await;
        cancel.cancel();
        pids
    };
    let (result, pids) = tokio::join!(call, cancelling);
    let err = result.unwrap_err();
    assert_eq!(err.code, ErrorCode::Cancelled, "{err:?}");
    assert_eq!(pids.len(), 2);
    for pid in pids {
        assert!(!alive(pid), "process {pid} outlived cancelled extension");
    }
}

async fn argument_validation_fails_before_command_starts() {
    let fixture = Fixture::new();
    let marker = fixture.dir.path().join("should-not-exist");
    fixture
        .call(
            "ext.define",
            json!({
                "name": "guard.touch",
                "description": "Touches a file only when args validate.",
                "effect": "write",
                "run": helper_argv("touch", &[&marker.to_string_lossy()]),
                "params": {
                    "mode": {"type": "string", "required": true, "enum": ["a", "b"]},
                    "count": {"type": "integer"}
                }
            }),
        )
        .await
        .unwrap();

    for bad in [
        json!({}),
        json!({"mode": "c"}),
        json!({"mode": "a", "count": "not-an-int"}),
        json!({"mode": "a", "unknown": 1}),
    ] {
        let err = fixture.call("guard.touch", bad.clone()).await.unwrap_err();
        assert_eq!(err.code, ErrorCode::InvalidArgs, "{bad}");
        assert!(!marker.exists(), "command ran despite invalid args {bad}");
    }

    // Valid call runs and creates marker.
    assert_eq!(
        fixture
            .call("guard.touch", json!({"mode": "a", "count": 2}))
            .await
            .unwrap(),
        json!({"touched": true})
    );
    assert!(marker.exists());
}

async fn shell_metacharacters_arrive_byte_for_byte_with_run_and_cmd() {
    let fixture = Fixture::new();
    let tricky =
        "; echo pwned $(whoami) `id` 'single' \"double\" $HOME %PATH% | & > < \\\nline2\ttab";

    fixture
        .call(
            "ext.define",
            json!({
                "name": "meta_test.run_way",
                "description": "Direct argv.",
                "effect": "read",
                "run": helper_argv("inspect", &[]),
                "params": {"payload": {"type": "string", "required": true}}
            }),
        )
        .await
        .unwrap();
    fixture
        .call(
            "ext.define",
            json!({
                "name": "meta_test.cmd_way",
                "description": "Platform shell.",
                "effect": "read",
                "cmd": helper_shell_cmd("inspect"),
                "params": {"payload": {"type": "string", "required": true}}
            }),
        )
        .await
        .unwrap();

    for tool in ["meta_test.run_way", "meta_test.cmd_way"] {
        let out = fixture
            .call(tool, json!({"payload": tricky}))
            .await
            .unwrap();
        assert_eq!(out["env"]["LEGION_ARG_PAYLOAD"], json!(tricky), "{tool}");
        assert_eq!(out["stdin"]["payload"], json!(tricky), "{tool}");
    }
}

async fn policy_enforces_declared_effect_and_admin_for_define_remove() {
    let fixture = Fixture::new();
    fixture.write_config(
        "policy.toml",
        r#"
[principals.observer]
role = "observer"

[roles.observer]
allow = ["effect:read"]
"#,
    );

    // Owner defines a read extension and an exec extension.
    fixture
        .call(
            "ext.define",
            json!({
                "name": "pol.read",
                "description": "Read extension.",
                "effect": "read",
                "run": helper_argv("print", &["{\"ok\":true}"])
            }),
        )
        .await
        .unwrap();
    fixture
        .call(
            "ext.define",
            json!({
                "name": "pol.exec",
                "description": "Exec extension.",
                "effect": "exec",
                "run": helper_argv("print", &["{\"ok\":true}"])
            }),
        )
        .await
        .unwrap();

    // Observer can call pol.read, ext.list, ext.show, and sees pol.read (not pol.exec) in whoami.
    assert_eq!(
        fixture
            .call_as("observer", "pol.read", json!({}))
            .await
            .unwrap(),
        json!({"ok": true})
    );
    assert!(
        fixture
            .call_as("observer", "ext.list", json!({}))
            .await
            .is_ok()
    );
    assert!(
        fixture
            .call_as("observer", "ext.show", json!({"name": "pol.read"}))
            .await
            .is_ok()
    );
    let whoami = fixture
        .call_as("observer", "meta.whoami", json!({}))
        .await
        .unwrap();
    let allowed: Vec<&str> = whoami["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert!(allowed.contains(&"pol.read"), "{allowed:?}");
    assert!(!allowed.contains(&"pol.exec"), "{allowed:?}");

    // Observer is denied pol.exec, ext.define and ext.remove.
    assert_eq!(
        fixture
            .call_as("observer", "pol.exec", json!({}))
            .await
            .unwrap_err()
            .code,
        ErrorCode::Denied
    );
    assert_eq!(
        fixture
            .call_as(
                "observer",
                "ext.define",
                json!({
                    "name": "pol.new",
                    "description": "Attempt.",
                    "effect": "read",
                    "run": helper_argv("print", &["{}"])
                })
            )
            .await
            .unwrap_err()
            .code,
        ErrorCode::Denied
    );
    assert_eq!(
        fixture
            .call_as("observer", "ext.remove", json!({"name": "pol.read"}))
            .await
            .unwrap_err()
            .code,
        ErrorCode::Denied
    );
}

async fn builtin_name_and_reserved_families_are_refused() {
    let fixture = Fixture::new();

    // Refused at ext.define time.
    for forbidden in [
        "meta.version",
        "fs.custom",
        "ext.mine",
        "token.custom",
        "power.custom",
    ] {
        let err = fixture
            .call(
                "ext.define",
                json!({
                    "name": forbidden,
                    "description": "Shadow.",
                    "effect": "read",
                    "run": helper_argv("print", &["{}"])
                }),
            )
            .await
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::InvalidArgs, "{forbidden}");
    }

    // Refused at file load time too: hand-written file under tools.d is skipped and reported broken,
    // and built-in tool still runs unchanged.
    let tools_d = fixture.node.paths().tools_dir();
    std::fs::create_dir_all(&tools_d).unwrap();
    std::fs::write(
        tools_d.join("shadow.toml"),
        "name = \"meta.version\"\ndescription = \"Fake.\"\neffect = \"read\"\nrun = [\"echo\", \"fake\"]\n",
    )
    .unwrap();
    std::fs::write(
        tools_d.join("reserved.toml"),
        "name = \"fs.custom\"\ndescription = \"Reserved.\"\neffect = \"read\"\nrun = [\"echo\", \"no\"]\n",
    )
    .unwrap();

    let version = fixture.call("meta.version", json!({})).await.unwrap();
    assert_eq!(version["name"], json!("legion"));
    assert_eq!(
        fixture.call("fs.custom", json!({})).await.unwrap_err().code,
        ErrorCode::UnknownTool
    );
    let listed = fixture.call("ext.list", json!({})).await.unwrap();
    assert_eq!(listed["tools"], json!([]));
    assert_eq!(listed["broken"].as_array().unwrap().len(), 2);
}

async fn every_builtin_registry_family_is_dynamically_reserved() {
    let dir = tempfile::tempdir().unwrap();
    let mut registry = legion_tools::registry();
    // Register a synthetic built-in family not in the static RESERVED_FAMILIES list to prove the
    // reserved set is derived dynamically from the built-in registry.
    registry.add_fn(
        "synth.ping",
        legion_core::proto::Effect::Read,
        "Synthetic built-in.",
        |_ctx: &legion_core::Ctx, _args: legion_core::NoArgs| async { Ok(json!({"ok": true})) },
    );
    let mut families: Vec<String> = registry
        .names()
        .filter_map(|name| name.split('.').next().map(str::to_owned))
        .collect();
    families.push("config".to_owned());
    families.sort();
    families.dedup();

    let node = legion_core::Node::new(legion_core::Paths::under(dir.path()), registry);

    // Every family present in the built-in registry (including agent, config, and synth) is
    // refused by ext.define.
    for family in &families {
        let candidate = format!("{family}.custom_ext");
        let err = node
            .call(legion_core::Call::new(
                legion_core::Door::Cli,
                "ext.define",
                json!({
                    "name": candidate,
                    "description": "Should be refused.",
                    "effect": "read",
                    "run": helper_argv("print", &["{}"])
                }),
            ))
            .await
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::InvalidArgs, "{candidate}: {err:?}");
        assert!(
            err.message.contains("reserved"),
            "{candidate}: {}",
            err.message
        );
    }

    // Hand-written file under tools.d using the dynamically registered family `synth` is also
    // refused at load time and reported broken.
    let tools_d = node.paths().tools_dir();
    std::fs::create_dir_all(&tools_d).unwrap();
    std::fs::write(
        tools_d.join("synth.custom.toml"),
        "name = \"synth.custom\"\ndescription = \"Dynamic shadow.\"\neffect = \"read\"\nrun = [\"echo\"]\n",
    )
    .unwrap();
    assert!(node.spec("synth.custom").is_err());
    let listed = node
        .call(legion_core::Call::new(
            legion_core::Door::Cli,
            "ext.list",
            json!({}),
        ))
        .await
        .unwrap();
    assert_eq!(listed["tools"], json!([]));
    assert_eq!(listed["broken"].as_array().unwrap().len(), 1);
    assert!(
        listed["broken"][0]["reason"]
            .as_str()
            .unwrap()
            .contains("reserved")
    );
}

async fn broken_file_is_skipped_and_reported_while_others_work() {
    let fixture = Fixture::new();
    fixture
        .call(
            "ext.define",
            json!({
                "name": "good.ping",
                "description": "Works fine.",
                "effect": "read",
                "run": helper_argv("print", &["{\"alive\":true}"])
            }),
        )
        .await
        .unwrap();

    let tools_d = fixture.node.paths().tools_dir();
    std::fs::write(
        tools_d.join("broken.toml"),
        "name = \"bad.tool\"\ndescription = \"Oops\"\neffect = \"not_an_effect\"\nrun = [\"echo\"]\n",
    )
    .unwrap();

    // Good extension still works.
    assert_eq!(
        fixture.call("good.ping", json!({})).await.unwrap(),
        json!({"alive": true})
    );

    // ext.list reports both good and broken with reason & line.
    let listed = fixture.call("ext.list", json!({})).await.unwrap();
    assert_eq!(listed["tools"].as_array().unwrap().len(), 1);
    assert_eq!(listed["tools"][0]["name"], json!("good.ping"));
    let broken = listed["broken"].as_array().unwrap();
    assert_eq!(broken.len(), 1);
    assert!(broken[0]["file"].as_str().unwrap().ends_with("broken.toml"));
    assert!(broken[0]["reason"].as_str().unwrap().contains("line 3"));
    assert_eq!(broken[0]["line"], json!(3));

    // meta.doctor reports the broken file and marks ok=false.
    let doctor = fixture.call("meta.doctor", json!({})).await.unwrap();
    assert_eq!(doctor["ok"], json!(false), "{doctor:#}");
    let checks = doctor["checks"].as_array().unwrap();
    let bad_check = checks
        .iter()
        .find(|c| c["name"] == json!("tools.d/broken.toml"))
        .expect("doctor missing broken tools.d check");
    assert_eq!(bad_check["ok"], json!(false));
    assert!(bad_check["detail"].as_str().unwrap().contains("line 3"));
}

async fn hot_added_and_edited_files_are_picked_up_without_restart() {
    let fixture = Fixture::new();
    assert!(fixture.node.spec("live.echo").is_err());

    let tools_d = fixture.node.paths().tools_dir();
    std::fs::create_dir_all(&tools_d).unwrap();
    let file = tools_d.join("live.echo.toml");
    let toml_v1 = format!(
        "name = \"live.echo\"\ndescription = \"V1.\"\neffect = \"read\"\nrun = {}\n",
        serde_json::to_string(&helper_argv("print", &["{\"v\":1}"])).unwrap()
    );
    std::fs::write(&file, toml_v1).unwrap();

    // Immediately visible in spec, specs, and call without creating a new Node.
    assert_eq!(fixture.node.spec("live.echo").unwrap().description, "V1.");
    assert!(
        fixture
            .node
            .specs()
            .iter()
            .any(|spec| spec.name == "live.echo")
    );
    assert_eq!(
        fixture.call("live.echo", json!({})).await.unwrap(),
        json!({"v": 1})
    );

    // Two writes within the same second of different sizes -> picked up on next call.
    let toml_v2 = format!(
        "name = \"live.echo\"\ndescription = \"V2 updated.\"\neffect = \"read\"\nrun = {}\n",
        serde_json::to_string(&helper_argv("print", &["{\"v\":22}"])).unwrap()
    );
    std::fs::write(&file, toml_v2).unwrap();
    assert_eq!(
        fixture.node.spec("live.echo").unwrap().description,
        "V2 updated."
    );
    assert_eq!(
        fixture.call("live.echo", json!({})).await.unwrap(),
        json!({"v": 22})
    );

    let toml_v3 = format!(
        "name = \"live.echo\"\ndescription = \"V3.\"\neffect = \"read\"\nrun = {}\n",
        serde_json::to_string(&helper_argv("print", &["{\"v\":3}"])).unwrap()
    );
    std::fs::write(&file, toml_v3).unwrap();
    assert_eq!(fixture.node.spec("live.echo").unwrap().description, "V3.");
    assert_eq!(
        fixture.call("live.echo", json!({})).await.unwrap(),
        json!({"v": 3})
    );

    // Delete -> immediately gone.
    std::fs::remove_file(&file).unwrap();
    assert!(fixture.node.spec("live.echo").is_err());
}
