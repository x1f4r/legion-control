//! `legion <tool>`, `legion tools` and `legion help`, run as a user would run them.

#![allow(clippy::unwrap_used)]

mod common;

use common::{Home, ends_with};
use serde_json::{Value, json};

const POLICY: &str = r#"
[principals.phone]
role = "observer"

[principals."agent:claude"]
role = "operator"

[roles.observer]
allow = ["effect:read"]

[roles.operator]
allow = ["effect:read", "effect:write"]
deny = ["test.write_path", "audit.*"]
paths = ["~/projects/**"]
"#;

#[test]
fn a_call_prints_its_result_as_json_when_stdout_is_not_a_terminal() {
    let home = Home::new();
    let run = home.run(&["meta.version"]);
    assert_eq!(run.code, 0, "{}", run.stderr);
    let version = run.json();
    assert_eq!(version["name"], json!("legion"));
    assert_eq!(version["version"], json!(env!("CARGO_PKG_VERSION")));
    assert_eq!(run.stdout.lines().count(), 1);
}

#[test]
fn tools_and_help_describe_the_registry() {
    let home = Home::new();
    let tools = home.run(&["tools"]);
    assert_eq!(tools.code, 0);
    let names: Vec<String> = tools.json()["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|spec| spec["name"].as_str().unwrap().to_owned())
        .collect();
    for expected in [
        "audit.tail",
        "meta.doctor",
        "meta.version",
        "meta.whoami",
        "test.echo",
    ] {
        assert!(
            names.contains(&expected.to_owned()),
            "{expected} missing from {names:?}"
        );
    }

    let help = home.run(&["help", "audit.tail"]);
    assert_eq!(help.code, 0);
    let spec = help.json();
    assert_eq!(spec["effect"], json!("read"));
    assert_eq!(
        spec["input_schema"]["properties"]["limit"]["default"],
        json!(20)
    );

    let unknown = home.run(&["help", "nope.nothing"]);
    assert_eq!(unknown.code, 1);
    assert_eq!(unknown.json()["code"], json!("unknown_tool"));

    let usage = home.run(&["help"]);
    assert_eq!(usage.code, 0);
    assert!(usage.stdout.starts_with("Usage:"));
    let version = home.run(&["--version"]);
    assert_eq!(
        version.stdout.trim(),
        format!("legion {}", env!("CARGO_PKG_VERSION"))
    );
}

#[test]
fn key_value_arguments_are_json_when_they_parse_and_strings_otherwise() {
    let home = Home::new();
    let run = home.run(&[
        "test.echo",
        "n=42",
        "ratio=0.5",
        "ok=false",
        "nothing=null",
        "list=[1,\"two\"]",
        "object={\"a\":{\"b\":1}}",
        "path=/tmp/x y",
        "quoted=\"42\"",
        "empty=",
        "expr=a=b",
    ]);
    assert_eq!(run.code, 0, "{}", run.stderr);
    assert_eq!(
        run.json(),
        json!({
            "n": 42, "ratio": 0.5, "ok": false, "nothing": null, "list": [1, "two"],
            "object": {"a": {"b": 1}}, "path": "/tmp/x y", "quoted": "42", "empty": "", "expr": "a=b"
        })
    );
}

#[test]
fn args_takes_the_whole_object_from_the_command_line_or_stdin() {
    let home = Home::new();
    let inline = home.run(&[
        "test.echo",
        "--args",
        r#"{"n": "42", "deep": [{"x": null}]}"#,
    ]);
    assert_eq!(inline.json(), json!({"n": "42", "deep": [{"x": null}]}));
    let piped = home.run_with_stdin(&["test.echo", "--args", "-"], "{\"from\": \"stdin\"}\n");
    assert_eq!(piped.json(), json!({"from": "stdin"}));
}

#[test]
fn a_command_line_that_cannot_run_exits_2_with_a_message_on_stderr() {
    let home = Home::new();
    for line in [
        &[][..],
        &["frob"],
        &["meta.version", "--frob"],
        &["meta.version", "stray"],
        &["test.echo", "a=1", "a=2"],
        &["test.echo", "a=1", "--args", "{}"],
        &["test.echo", "--args", "[1]"],
        &["test.echo", "--args", "{not json"],
        &["meta.version", "--as", "two words"],
        &["meta.version", "--on", "tower,"],
    ] {
        let run = home.run(line);
        assert_eq!(run.code, 2, "{line:?}: {}", run.stdout);
        assert!(run.stdout.is_empty(), "{line:?}");
        assert!(
            run.stderr.starts_with("legion: "),
            "{line:?}: {}",
            run.stderr
        );
    }
    let piped = home.run_with_stdin(&["test.echo", "--args", "-"], "nope");
    assert_eq!(piped.code, 2);
    // Nothing was called, so nothing was recorded.
    assert!(home.audit().is_empty());
}

#[test]
fn a_tool_error_exits_1_with_the_error_object_on_stdout() {
    let home = Home::new();
    let unknown = home.run(&["meta.versoin"]);
    assert_eq!(unknown.code, 1);
    let error = unknown.json();
    assert_eq!(error["code"], json!("unknown_tool"));
    assert!(error["hint"].as_str().unwrap().contains("meta.version"));

    let invalid = home.run(&["audit.tail", "limit=lots"]);
    assert_eq!(invalid.code, 1);
    let error = invalid.json();
    assert_eq!(error["code"], json!("invalid_args"));
    assert!(
        error["message"].as_str().unwrap().contains("`limit`"),
        "{error}"
    );
    assert_eq!(error["details"]["field"], json!("limit"));

    let extra = home.run(&["meta.version", "verbose=true"]);
    assert_eq!(extra.json()["code"], json!("invalid_args"));
    assert!(
        extra.json()["message"]
            .as_str()
            .unwrap()
            .contains("`verbose`")
    );
}

#[test]
fn on_runs_here_for_this_node_and_reports_unknown_machines_as_not_found() {
    let home = Home::new();
    home.write_config("node.toml", "name = \"tower\"\n");
    let here = home.run(&["meta.version", "--on", "tower"]);
    assert_eq!(here.code, 0, "{}", here.stdout);
    assert_eq!(here.json()["node"], json!("tower"));

    let unknown = home.run(&["meta.version", "--on", "pi"]);
    assert_eq!(unknown.code, 1);
    let error = unknown.json();
    assert_eq!(error["code"], json!("not_found"));
    assert_eq!(error["machine"], json!("pi"));
    assert!(error["hint"].as_str().unwrap().contains("tower"), "{error}");

    // Partial fan-out success exits 0 and returns per-machine outcomes.
    let partial = home.run(&["meta.version", "--on", "tower,pi"]);
    assert_eq!(partial.code, 0, "{}", partial.stdout);
    let body = partial.json();
    assert_eq!(body["results"]["tower"]["ok"]["node"], json!("tower"));
    assert_eq!(body["results"]["pi"]["error"]["code"], json!("not_found"));

    // Fan-out where every machine fails exits non-zero.
    let all_bad = home.run(&["meta.version", "--on", "pi,laptop"]);
    assert_eq!(all_bad.code, 1, "{}", all_bad.stdout);
    let body = all_bad.json();
    assert_eq!(body["results"]["pi"]["error"]["code"], json!("not_found"));
    assert_eq!(
        body["results"]["laptop"]["error"]["code"],
        json!("not_found")
    );

    // Empty fleet.toml returns an empty fan-out object.
    let empty_all = home.run(&["meta.version", "--on", "all"]);
    assert_eq!(empty_all.code, 0, "{}", empty_all.stdout);
    assert_eq!(empty_all.json(), json!({"results": {}}));
}

#[test]
fn policy_decides_what_each_principal_may_call() {
    let home = Home::new();
    home.write_config("policy.toml", POLICY);
    let code = |args: &[&str]| {
        let run = home.run(args);
        (
            run.code,
            run.json().get("code").cloned().unwrap_or(Value::Null),
        )
    };
    assert_eq!(code(&["meta.version", "--as", "phone"]).0, 0);
    assert_eq!(
        code(&["test.write_path", "path=~/projects/a", "--as", "phone"]),
        (1, json!("denied"))
    );
    // Allowed by its effect, denied by its name: deny wins.
    assert_eq!(
        code(&[
            "test.write_path",
            "path=~/projects/a",
            "--as",
            "agent:claude"
        ]),
        (1, json!("denied"))
    );
    assert_eq!(
        code(&["audit.tail", "--as", "agent:claude"]),
        (1, json!("denied"))
    );
    assert_eq!(code(&["meta.version", "--as", "agent:claude"]).0, 0);
    // A principal the policy does not name is nobody.
    let stranger = home.run(&["meta.version", "--as", "agent:gemini"]);
    assert_eq!(stranger.json()["code"], json!("denied"));
    assert!(
        stranger.json()["message"]
            .as_str()
            .unwrap()
            .contains("nobody")
    );

    let whoami = home.run(&["meta.whoami", "--as", "agent:claude"]).json();
    assert_eq!(whoami["role"], json!("operator"));
    assert_eq!(whoami["paths"], json!(["~/projects"]));
    assert!(
        !whoami["tools"]
            .as_array()
            .unwrap()
            .contains(&json!("test.write_path"))
    );

    // A default that does not delegate cannot be talked out of itself.
    home.write_config("policy.toml", &format!("default = \"phone\"\n{POLICY}"));
    assert_eq!(
        home.run(&["meta.whoami"]).json()["principal"],
        json!("phone")
    );
    assert_eq!(
        code(&["meta.version", "--as", "owner"]),
        (1, json!("denied"))
    );

    // A policy that does not parse fails the call instead of falling back to anything.
    home.write_config(
        "policy.toml",
        "[roles.observer]\nallow = [\"effect:everything\"]\n",
    );
    let broken = home.run(&["meta.version"]);
    assert_eq!(broken.code, 1);
    let error = broken.json();
    assert_eq!(error["code"], json!("failed"));
    assert!(
        error["message"]
            .as_str()
            .unwrap()
            .contains("policy.toml does not parse at line 2"),
        "{error}"
    );
    assert_eq!(error["details"]["line"], json!(2));
}

#[test]
fn a_confined_role_reaches_only_its_paths() {
    let home = Home::new();
    home.write_config("policy.toml", POLICY);
    let check = |path: &str| {
        home.run(&[
            "test.read_path",
            &format!("path={path}"),
            "--as",
            "agent:claude",
        ])
        .json()
    };
    let inside = check("~/projects/app/main.rs");
    assert!(
        ends_with(
            inside["path"].as_str().unwrap(),
            &["home", "projects", "app", "main.rs"]
        ),
        "{inside}"
    );
    assert_eq!(check("~/.ssh/id")["code"], json!("denied"));
    assert_eq!(check("~/projects/../.ssh/id")["code"], json!("denied"));
    assert_eq!(check("projects/app")["code"], json!("invalid_args"));
    let owner = home.run(&["test.read_path", "path=~/.ssh/id"]).json();
    assert!(
        ends_with(owner["path"].as_str().unwrap(), &["home", ".ssh", "id"]),
        "{owner}"
    );
}

#[test]
fn every_call_is_audited_with_contents_and_secrets_replaced_by_their_size() {
    let home = Home::new();
    home.write_config("policy.toml", POLICY);
    let run = home.run(&[
        "test.echo",
        "path=~/notes.txt",
        "content=twelve bytes",
        "token=abc123",
        "argv=[\"mysql\",\"--password=hunter2\"]",
    ]);
    assert_eq!(run.code, 0);
    // The result itself is not redacted: only what is kept.
    assert_eq!(run.json()["token"], json!("abc123"));
    home.run(&["meta.version", "--as", "phone"]);
    home.run(&["test.write_path", "path=/x", "--as", "phone"]);

    let audit = home.audit();
    assert_eq!(audit.len(), 3);
    let first = &audit[0];
    assert_eq!(first["principal"], json!("owner"));
    assert_eq!(first["door"], json!("cli"));
    assert_eq!(first["tool"], json!("test.echo"));
    assert_eq!(first["outcome"], json!("ok"));
    assert_eq!(
        first["args"],
        json!({
            "path": "~/notes.txt",
            "content": {"redacted_bytes": 12},
            "token": {"redacted_bytes": 6},
            "argv": ["mysql", {"redacted_bytes": 18}]
        })
    );
    assert!(
        !std::fs::read_to_string(home.state().join("audit.jsonl"))
            .unwrap()
            .contains("hunter2")
    );
    assert_eq!(audit[1]["principal"], json!("phone"));
    assert_eq!(audit[2]["outcome"], json!("denied"));
    assert!(
        audit[2]["message"]
            .as_str()
            .unwrap()
            .contains("phone may not use test.write_path")
    );

    // And audit.tail reads it back.
    let tail = home.run(&["audit.tail", "outcome=denied"]).json();
    assert_eq!(tail["entries"].as_array().unwrap().len(), 1);
    assert_eq!(tail["entries"][0]["tool"], json!("test.write_path"));
}
