//! `exec.run`, called through a node the way every door calls it.

#![allow(clippy::unwrap_used, clippy::panic, missing_docs)]

mod support;

use std::process::ExitCode;
use std::time::{Duration, Instant};

use legion_core::CancellationToken;
use legion_core::proto::{ErrorCode, EventKind};
use serde_json::{Value, json};
use support::{Events, Fixture, HELPER, Options, Test, alive, read_pids, this_binary};

fn main() -> ExitCode {
    let tests: &[Test] = &[
        ("the_runner_reads_the_arguments_cargo_passes", || {
            Box::pin(the_runner_reads_the_arguments_cargo_passes())
        }),
        ("argv_runs_exactly_and_reports_the_exit_code", || {
            Box::pin(argv_runs_exactly_and_reports_the_exit_code())
        }),
        ("cmd_goes_through_the_platform_shell", || {
            Box::pin(cmd_goes_through_the_platform_shell())
        }),
        ("stdin_env_and_cwd_reach_the_command", || {
            Box::pin(stdin_env_and_cwd_reach_the_command())
        }),
        (
            "a_timeout_ends_the_command_and_everything_it_started",
            || Box::pin(a_timeout_ends_the_command_and_everything_it_started()),
        ),
        ("long_output_is_truncated_and_says_so", || {
            Box::pin(long_output_is_truncated_and_says_so())
        }),
        ("output_is_streamed_as_events", || {
            Box::pin(output_is_streamed_as_events())
        }),
        ("cancelling_the_call_stops_the_whole_tree", || {
            Box::pin(cancelling_the_call_stops_the_whole_tree())
        }),
        (
            "the_command_gets_the_environment_but_not_legions_own",
            || Box::pin(the_command_gets_the_environment_but_not_legions_own()),
        ),
        ("what_cannot_run_fails_with_a_closed_code", || {
            Box::pin(what_cannot_run_fails_with_a_closed_code())
        }),
        ("a_confined_role_runs_only_in_its_paths", || {
            Box::pin(a_confined_role_runs_only_in_its_paths())
        }),
        ("a_role_without_exec_is_denied", || {
            Box::pin(a_role_without_exec_is_denied())
        }),
    ];
    support::main(tests)
}

/// `exec.run` arguments that run the test binary as the named helper.
fn helper(mode: &str, args: &[&str]) -> Value {
    let mut argv = vec![this_binary()];
    argv.extend(args.iter().map(|arg| (*arg).to_owned()));
    json!({"argv": argv, "env": {HELPER: mode}})
}

fn with(mut args: Value, extra: Value) -> Value {
    for (key, value) in extra.as_object().unwrap() {
        if key == "env" {
            let env = args["env"].as_object_mut().unwrap();
            env.extend(value.as_object().unwrap().clone());
        } else {
            args[key] = value.clone();
        }
    }
    args
}

async fn the_runner_reads_the_arguments_cargo_passes() {
    let parse = |args: &[&str]| Options::parse(args.iter().map(|arg| (*arg).to_owned()));
    let options = parse(&[
        "--format",
        "pretty",
        "--nocapture",
        "--test-threads=1",
        "cancel",
    ])
    .unwrap();
    assert_eq!(options.filters, ["cancel"]);
    assert!(options.runs("cancelling_the_call_stops_the_whole_tree"));
    assert!(!options.runs("cmd_goes_through_the_platform_shell"));
    let options = parse(&["--list", "--format", "terse"]).unwrap();
    assert!(options.list && options.filters.is_empty());
    let options = parse(&["--exact", "cmd", "--skip", "shell"]).unwrap();
    assert!(!options.runs("cmd_goes_through_the_platform_shell"));
    assert!(parse(&["--ignored"]).unwrap().filters.is_empty());
    assert!(!parse(&["--ignored"]).unwrap().runs("anything"));
    assert!(parse(&["--frobnicate"]).is_err());
    assert!(parse(&["--format"]).is_err());

    // `--list` names the tests and runs none of them.
    let listed = std::process::Command::new(this_binary())
        .args(["--list", "argv_runs"])
        .output()
        .unwrap();
    assert!(listed.status.success());
    let stdout = String::from_utf8_lossy(&listed.stdout);
    assert!(
        stdout.starts_with("argv_runs_exactly_and_reports_the_exit_code: test\n"),
        "{stdout}"
    );
    assert!(!stdout.contains("... ok"), "{stdout}");
}

async fn argv_runs_exactly_and_reports_the_exit_code() {
    let fixture = Fixture::new();
    // Quotes, spaces and shell characters reach the program as they are: no shell is involved.
    let ran = fixture
        .call("exec.run", helper("args", &["a b", "'$HOME' \"x\"", "|;&"]))
        .await
        .unwrap();
    assert_eq!(ran["exit_code"], json!(0), "{ran:#}");
    assert_eq!(lines(&ran["stdout"]), ["a b", "'$HOME' \"x\"", "|;&"]);
    assert_eq!(ran["stderr"].as_str().unwrap().trim(), "to stderr");
    assert_eq!(ran["stdout_truncated"], json!(false));
    assert_eq!(ran["timed_out"], json!(false));
    assert!(ran["duration_ms"].is_u64());
    assert!(ran.get("signal").is_none());
    // Optional fields are left out, never null.
    assert!(
        ran.as_object()
            .unwrap()
            .values()
            .all(|value| !value.is_null())
    );

    let failed = fixture
        .call(
            "exec.run",
            with(
                helper("args", &[]),
                json!({"env": {"LEGION_TEST_EXIT": "3"}}),
            ),
        )
        .await
        .unwrap();
    assert_eq!(failed["exit_code"], json!(3));
}

async fn cmd_goes_through_the_platform_shell() {
    let fixture = Fixture::new();
    // `echo` and `exit` are built into both sh and PowerShell.
    let ran = fixture
        .call("exec.run", json!({"cmd": "echo hello"}))
        .await
        .unwrap();
    assert_eq!(ran["exit_code"], json!(0), "{ran:#}");
    assert_eq!(ran["stdout"].as_str().unwrap().trim(), "hello");
    let ran = fixture
        .call("exec.run", json!({"cmd": "exit 4"}))
        .await
        .unwrap();
    assert_eq!(ran["exit_code"], json!(4), "{ran:#}");
}

async fn stdin_env_and_cwd_reach_the_command() {
    let fixture = Fixture::new();
    let ran = fixture
        .call(
            "exec.run",
            with(
                helper("cat", &[]),
                json!({"stdin": "line one\nline two ✓\n"}),
            ),
        )
        .await
        .unwrap();
    assert_eq!(ran["stdout"], json!("line one\nline two ✓\n"));

    let ran = fixture
        .call(
            "exec.run",
            with(
                helper("env", &["LEGION_TEST_VALUE"]),
                json!({"env": {"LEGION_TEST_VALUE": "a=b c"}}),
            ),
        )
        .await
        .unwrap();
    assert_eq!(ran["stdout"], json!("a=b c"));

    let dir = fixture.home().join("work");
    std::fs::create_dir_all(&dir).unwrap();
    let ran = fixture
        .call(
            "exec.run",
            with(helper("cwd", &[]), json!({"cwd": dir.to_string_lossy()})),
        )
        .await
        .unwrap();
    let reported = std::path::PathBuf::from(ran["stdout"].as_str().unwrap());
    assert_eq!(
        dunce_like(&reported),
        dunce_like(&dir),
        "ran in {reported:?}"
    );

    let tilde = fixture
        .call(
            "exec.run",
            with(helper("cwd", &[]), json!({"cwd": "~/work"})),
        )
        .await
        .unwrap();
    assert_eq!(tilde["stdout"], ran["stdout"]);
}

/// A path with its symbolic links resolved, so a temporary directory behind a link (macOS's
/// `/var`) compares equal to itself.
fn dunce_like(path: &std::path::Path) -> std::path::PathBuf {
    std::fs::canonicalize(path).unwrap()
}

async fn a_timeout_ends_the_command_and_everything_it_started() {
    let fixture = Fixture::new();
    let pids = fixture.dir.path().join("pids");
    let started = Instant::now();
    let ran = fixture
        .call(
            "exec.run",
            with(
                helper("family", &[&pids.to_string_lossy(), "stubborn"]),
                json!({"timeout_s": 8}),
            ),
        )
        .await
        .unwrap();
    assert_eq!(ran["timed_out"], json!(true), "{ran:#}");
    assert!(ran.get("exit_code").is_none() || ran["exit_code"] != json!(0));
    // Generous: a cold Windows runner can take seconds to start a process.
    assert!(started.elapsed() < Duration::from_secs(60));
    assert_eq!(ran["stdout"].as_str().unwrap().trim(), "started");
    for pid in read_pids(&pids).await {
        assert!(!alive(pid), "process {pid} outlived the timeout");
    }
}

async fn long_output_is_truncated_and_says_so() {
    let fixture = Fixture::new();
    let ran = fixture
        .call(
            "exec.run",
            with(helper("flood", &["100000"]), json!({"max_output": 1000})),
        )
        .await
        .unwrap();
    let stdout = ran["stdout"].as_str().unwrap();
    assert_eq!(ran["stdout_truncated"], json!(true));
    assert!(stdout.starts_with("START"), "{stdout}");
    assert!(stdout.ends_with("END!!"), "{stdout}");
    assert!(stdout.contains("bytes omitted"), "{stdout}");
    assert!(stdout.len() < 1100, "{} bytes kept", stdout.len());

    let ran = fixture
        .call("exec.run", helper("flood", &["1000"]))
        .await
        .unwrap();
    assert_eq!(ran["stdout_truncated"], json!(false));
    assert_eq!(ran["stdout"].as_str().unwrap().len(), 1000);

    let error = fixture
        .call(
            "exec.run",
            with(helper("flood", &["1"]), json!({"max_output": 2_000_000})),
        )
        .await
        .unwrap_err();
    assert_eq!(error.code, ErrorCode::InvalidArgs);
}

async fn output_is_streamed_as_events() {
    let fixture = Fixture::new();
    let events = Events::default();
    let ran = fixture
        .call_with(
            "exec.run",
            helper("args", &["one", "two"]),
            &events,
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(
        events.text(EventKind::Stdout),
        ran["stdout"].as_str().unwrap()
    );
    assert_eq!(
        events.text(EventKind::Stderr),
        ran["stderr"].as_str().unwrap()
    );

    // A character the output never finishes ends both the events and the result the same way.
    let events = Events::default();
    let ran = fixture
        .call_with(
            "exec.run",
            helper("cut-short", &[]),
            &events,
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(ran["stdout"], json!("ok\u{fffd}"));
    assert_eq!(events.text(EventKind::Stdout), "ok\u{fffd}");

    // Events stop once the capture limit is passed, so a flood reaches the caller bounded.
    let events = Events::default();
    fixture
        .call_with(
            "exec.run",
            with(helper("flood", &["500000"]), json!({"max_output": 2000})),
            &events,
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let streamed = events.text(EventKind::Stdout);
    assert_eq!(streamed.len(), 2000);
    assert!(streamed.starts_with("START"));
}

async fn cancelling_the_call_stops_the_whole_tree() {
    let fixture = Fixture::new();
    let pids = fixture.dir.path().join("pids");
    let cancel = CancellationToken::new();
    let events = Events::default();
    // Both ignore the polite request, so only a tree that is really stopped is gone.
    let call = fixture.call_with(
        "exec.run",
        helper("family", &[&pids.to_string_lossy(), "stubborn"]),
        &events,
        cancel.clone(),
    );
    let cancelling = async {
        let family = read_pids(&pids).await;
        cancel.cancel();
        family
    };
    let (result, family) = tokio::join!(call, cancelling);
    let error = result.unwrap_err();
    assert_eq!(error.code, ErrorCode::Cancelled);
    assert!(error.message.contains("stopped"), "{}", error.message);
    assert_eq!(family.len(), 2);
    // Not after a while: the moment the call has returned.
    for pid in family {
        assert!(!alive(pid), "process {pid} outlived the cancelled call");
    }
}

async fn the_command_gets_the_environment_but_not_legions_own() {
    let output = std::process::Command::new(this_binary())
        .env(HELPER, "exec-env")
        .env("LEGION_TEST_INHERITED", "leaked")
        .env("TEST_INHERITED", "passed")
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    assert_eq!(String::from_utf8_lossy(&output.stdout), "|passed");
}

async fn what_cannot_run_fails_with_a_closed_code() {
    let fixture = Fixture::new();
    let error = fixture
        .call("exec.run", json!({"argv": ["legion-no-such-program-4f2a"]}))
        .await
        .unwrap_err();
    assert_eq!(error.code, ErrorCode::NotFound);
    assert!(
        error.message.contains("legion-no-such-program-4f2a"),
        "{}",
        error.message
    );

    let missing = fixture.home().join("missing");
    let error = fixture
        .call(
            "exec.run",
            with(
                helper("cwd", &[]),
                json!({"cwd": missing.to_string_lossy()}),
            ),
        )
        .await
        .unwrap_err();
    assert_eq!(error.code, ErrorCode::NotFound, "{}", error.message);

    for wrong in [
        json!({}),
        json!({"cmd": "x", "argv": ["x"]}),
        json!({"argv": []}),
        json!({"cmd": "echo hi", "timeout_s": 0}),
        json!({"cmd": "echo hi", "timeout_s": 3601}),
        json!({"cmd": "echo hi", "cwd": "relative/dir"}),
        json!({"argv": ["x", "a\0b"]}),
    ] {
        let error = fixture.call("exec.run", wrong.clone()).await.unwrap_err();
        assert_eq!(error.code, ErrorCode::InvalidArgs, "{wrong}");
    }
}

async fn a_confined_role_runs_only_in_its_paths() {
    let fixture = Fixture::new();
    fixture.write_config(
        "policy.toml",
        "[principals.\"agent:claude\"]\nrole = \"builder\"\n[roles.builder]\nallow = [\"effect:exec\"]\npaths = [\"~/projects/**\"]\n",
    );
    let inside = fixture.home().join("projects").join("app");
    std::fs::create_dir_all(&inside).unwrap();
    let ran = fixture
        .call_as(
            "agent:claude",
            "exec.run",
            with(helper("cwd", &[]), json!({"cwd": "~/projects/app"})),
        )
        .await
        .unwrap();
    assert_eq!(ran["exit_code"], json!(0));

    // Without `cwd` the command starts in the first root, not in legion's own directory.
    let ran = fixture
        .call_as("agent:claude", "exec.run", helper("cwd", &[]))
        .await
        .unwrap();
    let started = std::path::PathBuf::from(ran["stdout"].as_str().unwrap());
    assert_eq!(
        dunce_like(&started),
        dunce_like(&fixture.home().join("projects"))
    );

    let error = fixture
        .call_as(
            "agent:claude",
            "exec.run",
            with(helper("cwd", &[]), json!({"cwd": "~"})),
        )
        .await
        .unwrap_err();
    assert_eq!(error.code, ErrorCode::Denied, "{}", error.message);
}

async fn a_role_without_exec_is_denied() {
    let fixture = Fixture::new();
    fixture.write_config(
        "policy.toml",
        "[principals.phone]\nrole = \"observer\"\n[roles.observer]\nallow = [\"effect:read\"]\n",
    );
    let error = fixture
        .call_as("phone", "exec.run", json!({"cmd": "echo hi"}))
        .await
        .unwrap_err();
    assert_eq!(error.code, ErrorCode::Denied);
}

fn lines(text: &Value) -> Vec<String> {
    text.as_str()
        .unwrap()
        .lines()
        .map(|line| line.trim_end_matches('\r').to_owned())
        .collect()
}
