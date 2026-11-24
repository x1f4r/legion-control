//! A node's calls, end to end through its public interface: principal, policy, the tool, the audit
//! log.

#![allow(clippy::unwrap_used)]

use std::sync::{Arc, Mutex};
use std::time::Duration;

use legion_core::proto::{Effect, ErrorCode, EventKind, HelloParams, PROTOCOL};
use legion_core::{Access, Call, Ctx, Door, EventSink, Node, Paths, Registry, ToolError};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

async fn echo(_ctx: &Ctx, args: Map<String, Value>) -> Result<Map<String, Value>, ToolError> {
    Ok(args)
}

#[derive(Serialize)]
struct Done {
    done: bool,
}

async fn write(_ctx: &Ctx, _args: Map<String, Value>) -> Result<Done, ToolError> {
    Ok(Done { done: true })
}

async fn wait(ctx: &Ctx, _args: Map<String, Value>) -> Result<Done, ToolError> {
    ctx.emit(EventKind::Progress, json!({"started": true}));
    tokio::time::sleep(Duration::from_secs(60)).await;
    Ok(Done { done: true })
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct MarkerArgs {
    marker: String,
}

/// Hands its work to a blocking thread, which cannot be dropped, so the work watches the token
/// itself, cleans up and leaves a marker once it has stopped.
async fn blocking(ctx: &Ctx, args: MarkerArgs) -> Result<Done, ToolError> {
    let token = ctx.cancel_token().clone();
    ctx.emit(EventKind::Progress, json!({"started": true}));
    let stopped = tokio::task::spawn_blocking(move || {
        while !token.is_cancelled() {
            std::thread::sleep(Duration::from_millis(5));
        }
        std::thread::sleep(Duration::from_millis(200));
        std::fs::write(&args.marker, "stopped")
    })
    .await;
    match stopped {
        Ok(Ok(())) => Err(ToolError::new(
            ErrorCode::Cancelled,
            "t.blocking stopped once it had cleaned up.",
        )),
        _ => Err(ToolError::new(
            ErrorCode::Internal,
            "the marker was not written",
        )),
    }
}

/// Stops when cancelled and says how far it got, which is its result.
async fn stops(ctx: &Ctx, _args: Map<String, Value>) -> Result<Done, ToolError> {
    ctx.emit(EventKind::Progress, json!({"started": true}));
    ctx.cancelled().await;
    Ok(Done { done: false })
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct PathArgs {
    path: String,
}

#[derive(Serialize)]
struct Resolved {
    path: String,
}

async fn path(ctx: &Ctx, args: PathArgs) -> Result<Resolved, ToolError> {
    let path = ctx.grant().check_path(&args.path, Access::Read)?;
    Ok(Resolved {
        path: path.to_string_lossy().into_owned(),
    })
}

struct Fixture {
    dir: tempfile::TempDir,
    node: Node,
}

impl Fixture {
    fn new() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let mut registry = Registry::new();
        registry.add_fn("t.echo", Effect::Read, "Echoes.", echo);
        registry.add_fn("t.write", Effect::Write, "Writes nothing.", write);
        registry.add_fn("t.wait", Effect::Read, "Waits a minute.", wait);
        registry.add_fn("t.path", Effect::Read, "Checks a path.", path);
        registry.add_fn("t.blocking", Effect::Write, "Blocks.", blocking);
        registry.add_fn("t.stops", Effect::Read, "Stops when asked.", stops);
        let home = dir.path().join("home");
        std::fs::create_dir_all(home.join("projects")).unwrap();
        let node = Node::new(Paths::under(dir.path()).with_home(home), registry);
        Fixture { dir, node }
    }

    fn write_config(&self, name: &str, text: &str) {
        let config = self.dir.path().join("config");
        std::fs::create_dir_all(&config).unwrap();
        legion_core::config::write_atomically(&config.join(name), text.as_bytes()).unwrap();
    }

    fn audit(&self) -> Vec<Value> {
        let text = std::fs::read_to_string(self.dir.path().join("state").join("audit.jsonl"))
            .unwrap_or_default();
        text.lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    async fn call(&self, tool: &str, args: Value) -> Result<Value, legion_core::proto::ErrorBody> {
        self.node.call(Call::new(Door::Cli, tool, args)).await
    }

    async fn call_as(
        &self,
        tool: &str,
        principal: &str,
    ) -> Result<Value, legion_core::proto::ErrorBody> {
        let mut call = Call::new(Door::Cli, tool, json!({}));
        call.as_principal = Some(principal.to_owned());
        self.node.call(call).await
    }
}

const POLICY: &str = r#"
[principals.phone]
role = "observer"

[principals."agent:claude"]
role = "operator"

[roles.observer]
allow = ["effect:read"]

[roles.operator]
allow = ["t.*"]
deny = ["t.write"]
paths = ["~/projects"]
"#;

#[tokio::test]
async fn the_owner_calls_a_tool_and_the_call_is_audited() {
    let fixture = Fixture::new();
    let result = fixture
        .call("t.echo", json!({"a": 1, "token": "s3cret"}))
        .await
        .unwrap();
    assert_eq!(result, json!({"a": 1, "token": "s3cret"}));

    let audit = fixture.audit();
    assert_eq!(audit.len(), 1);
    let line = &audit[0];
    assert_eq!(line["principal"], json!("owner"));
    assert_eq!(line["door"], json!("cli"));
    assert_eq!(line["tool"], json!("t.echo"));
    assert_eq!(line["target"], json!(fixture.node.name()));
    assert_eq!(line["outcome"], json!("ok"));
    assert_eq!(
        line["args"],
        json!({"a": 1, "token": {"redacted_bytes": 6}})
    );
    assert!(line["duration"].is_number());
    assert!(line["time"].as_str().unwrap().ends_with('Z'));
}

#[tokio::test]
async fn an_unknown_tool_names_its_siblings_and_is_audited_too() {
    let fixture = Fixture::new();
    let error = fixture.call("t.ech", json!({})).await.unwrap_err();
    assert_eq!(error.code, ErrorCode::UnknownTool);
    assert!(error.hint.as_deref().unwrap().contains("t.echo"));
    assert_eq!(error.machine, Some(fixture.node.name()));
    let error = fixture.call("nothing.here", json!({})).await.unwrap_err();
    assert!(error.hint.as_deref().unwrap().contains("legion tools"));
    let audit = fixture.audit();
    assert_eq!(audit[0]["outcome"], json!("unknown_tool"));
    assert_eq!(audit[0]["principal"], json!("owner"));
}

#[tokio::test]
async fn invalid_arguments_name_the_field() {
    let fixture = Fixture::new();
    let error = fixture
        .call("t.path", json!({"path": 3}))
        .await
        .unwrap_err();
    assert_eq!(error.code, ErrorCode::InvalidArgs);
    assert!(error.message.contains("`path`"), "{}", error.message);
    let error = fixture
        .call("t.path", json!({"path": "/", "extra": true}))
        .await
        .unwrap_err();
    assert!(error.message.contains("`extra`"), "{}", error.message);
}

#[tokio::test]
async fn policy_decides_and_an_edit_applies_to_the_next_call() {
    let fixture = Fixture::new();
    fixture.write_config("policy.toml", POLICY);
    assert!(fixture.call_as("t.echo", "phone").await.is_ok());
    let denied = fixture.call_as("t.write", "phone").await.unwrap_err();
    assert_eq!(denied.code, ErrorCode::Denied);
    assert_eq!(
        fixture
            .call_as("t.write", "agent:claude")
            .await
            .unwrap_err()
            .code,
        ErrorCode::Denied
    );
    assert_eq!(
        fixture
            .call_as("t.echo", "stranger")
            .await
            .unwrap_err()
            .code,
        ErrorCode::Denied
    );

    fixture.write_config(
        "policy.toml",
        &POLICY.replace(
            "allow = [\"effect:read\"]",
            "allow = [\"effect:read\", \"t.write\"]",
        ),
    );
    assert!(fixture.call_as("t.write", "phone").await.is_ok());

    let audit = fixture.audit();
    let principals: Vec<&str> = audit
        .iter()
        .map(|line| line["principal"].as_str().unwrap())
        .collect();
    assert_eq!(
        principals,
        ["phone", "phone", "agent:claude", "stranger", "phone"]
    );
    let outcomes: Vec<&str> = audit
        .iter()
        .map(|line| line["outcome"].as_str().unwrap())
        .collect();
    assert_eq!(outcomes, ["ok", "denied", "denied", "denied", "ok"]);
}

#[tokio::test]
async fn a_policy_that_does_not_parse_fails_every_call_instead_of_allowing_it() {
    let fixture = Fixture::new();
    fixture.write_config(
        "policy.toml",
        "[roles.observer]\nallow = [\"effect:look\"]\n",
    );
    let error = fixture.call("t.echo", json!({})).await.unwrap_err();
    assert_eq!(error.code, ErrorCode::Failed);
    assert!(error.message.contains("policy.toml"), "{}", error.message);
    assert!(error.message.contains("line 2"), "{}", error.message);
}

#[tokio::test]
async fn a_policy_file_with_nothing_in_it_fails_every_call_instead_of_allowing_it() {
    let fixture = Fixture::new();
    fixture.write_config("policy.toml", "[principals.phone]\nrole = \"nobody\"\n");
    // Caught half way through a rewrite that is not atomic, a policy can read as empty.
    for empty in ["", " \n\t\r\n", "# the policy for tower\n"] {
        fixture.write_config("policy.toml", empty);
        let error = fixture.call("t.echo", json!({})).await.unwrap_err();
        assert_eq!(error.code, ErrorCode::Failed, "{empty:?}");
        assert!(error.message.contains("policy.toml"), "{}", error.message);
        assert!(
            error.hint.as_deref().unwrap_or_default().contains("Delete"),
            "{error:?}"
        );
    }
}

#[tokio::test]
async fn a_refused_request_to_act_as_someone_else_is_recorded_as_the_caller() {
    let fixture = Fixture::new();
    fixture.write_config("policy.toml", &format!("default = \"phone\"\n{POLICY}"));
    let error = fixture.call_as("t.echo", "owner").await.unwrap_err();
    assert_eq!(error.code, ErrorCode::Denied);
    assert_eq!(fixture.audit()[0]["principal"], json!("phone"));
}

#[tokio::test]
async fn a_pinned_door_keeps_its_principal() {
    let fixture = Fixture::new();
    fixture.write_config("policy.toml", POLICY);
    let mut call = Call::new(Door::Rpc, "t.write", json!({}));
    call.pinned = Some("phone".to_owned());
    call.as_principal = Some("owner".to_owned());
    let error = fixture.node.call(call).await.unwrap_err();
    assert_eq!(error.code, ErrorCode::Denied);
    assert!(
        error.message.starts_with("phone may not use t.write"),
        "{}",
        error.message
    );
    assert_eq!(fixture.audit()[0]["door"], json!("rpc"));
}

#[tokio::test]
async fn the_grant_confines_paths_for_the_tool() {
    let fixture = Fixture::new();
    fixture.write_config("policy.toml", POLICY);
    let mut inside = Call::new(Door::Cli, "t.path", json!({"path": "~/projects/x"}));
    inside.as_principal = Some("agent:claude".to_owned());
    assert!(fixture.node.call(inside).await.is_ok());
    let mut outside = Call::new(Door::Cli, "t.path", json!({"path": "~/.ssh"}));
    outside.as_principal = Some("agent:claude".to_owned());
    assert_eq!(
        fixture.node.call(outside).await.unwrap_err().code,
        ErrorCode::Denied
    );
    // The owner is not confined.
    assert!(
        fixture
            .call("t.path", json!({"path": "~/.ssh"}))
            .await
            .is_ok()
    );
}

#[tokio::test]
async fn file_tools_keep_out_of_the_node_s_own_files_unless_the_role_administers_it() {
    let fixture = Fixture::new();
    fixture.write_config("policy.toml", POLICY);
    let policy_file = fixture.dir.path().join("config").join("policy.toml");
    let mut call = Call::new(Door::Cli, "t.path", json!({"path": policy_file}));
    call.as_principal = Some("phone".to_owned());
    let error = fixture.node.call(call).await.unwrap_err();
    assert_eq!(error.code, ErrorCode::Denied);
    assert!(error.message.contains("effect:admin"), "{}", error.message);
    // The owner's role allows everything, admin included.
    assert!(
        fixture
            .call("t.path", json!({"path": policy_file}))
            .await
            .is_ok()
    );
}

// The clock is paused and moves on only when nothing else can, so the grace a tool that never looks
// at its token is given passes at once.
#[tokio::test(start_paused = true)]
async fn events_reach_the_sink_and_cancel_ends_a_call_that_never_looks_once_its_grace_is_up() {
    let fixture = Fixture::new();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let (started_tx, started_rx) = tokio::sync::oneshot::channel();
    let started_tx = Mutex::new(Some(started_tx));
    let sink_seen = Arc::clone(&seen);
    let mut call = Call::new(Door::Rpc, "t.wait", json!({}));
    call.events = EventSink::new(move |kind, data| {
        sink_seen.lock().unwrap().push((kind, data));
        if let Some(tx) = started_tx.lock().unwrap().take() {
            let _ = tx.send(());
        }
    });
    let cancel = call.cancel.clone();
    let node = fixture.node.clone();
    let running = tokio::spawn(async move { node.call(call).await });
    started_rx.await.unwrap();
    let cancelled_at = tokio::time::Instant::now();
    cancel.cancel();
    let error = tokio::time::timeout(Duration::from_secs(10), running)
        .await
        .unwrap()
        .unwrap()
        .unwrap_err();
    assert_eq!(error.code, ErrorCode::Cancelled);
    assert!(cancelled_at.elapsed() >= legion_core::CANCEL_GRACE);
    assert_eq!(
        *seen.lock().unwrap(),
        vec![(EventKind::Progress, json!({"started": true}))]
    );
    assert_eq!(fixture.audit()[0]["outcome"], json!("cancelled"));
}

/// Starts a call, waits for its first event, cancels it and returns what it came to.
async fn cancel_once_started(
    fixture: &Fixture,
    tool: &str,
    args: Value,
) -> Result<Value, legion_core::proto::ErrorBody> {
    let (started_tx, started_rx) = tokio::sync::oneshot::channel();
    let started_tx = Mutex::new(Some(started_tx));
    let mut call = Call::new(Door::Rpc, tool, args);
    call.events = EventSink::new(move |_, _| {
        if let Some(tx) = started_tx.lock().unwrap().take() {
            let _ = tx.send(());
        }
    });
    let cancel = call.cancel.clone();
    let node = fixture.node.clone();
    let running = tokio::spawn(async move { node.call(call).await });
    started_rx.await.unwrap();
    cancel.cancel();
    tokio::time::timeout(Duration::from_secs(30), running)
        .await
        .unwrap()
        .unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_cancelled_call_is_not_reported_until_its_blocking_work_has_stopped() {
    let fixture = Fixture::new();
    let marker = fixture.dir.path().join("stopped");
    let error = cancel_once_started(
        &fixture,
        "t.blocking",
        json!({"marker": marker.to_string_lossy()}),
    )
    .await
    .unwrap_err();
    assert!(
        marker.exists(),
        "the call was reported while its work went on"
    );
    assert_eq!(error.code, ErrorCode::Cancelled);
    assert_eq!(error.message, "t.blocking stopped once it had cleaned up.");
    assert_eq!(fixture.audit()[0]["outcome"], json!("cancelled"));
}

#[tokio::test]
async fn a_tool_that_answers_a_cancel_in_time_has_its_answer_reported() {
    let fixture = Fixture::new();
    let result = cancel_once_started(&fixture, "t.stops", json!({})).await;
    assert_eq!(result.unwrap(), json!({"done": false}));
    assert_eq!(fixture.audit()[0]["outcome"], json!("ok"));
}

#[tokio::test]
async fn hello_refuses_another_protocol() {
    let fixture = Fixture::new();
    let hello = |protocol| HelloParams {
        client: "test".into(),
        version: "0".into(),
        protocol,
    };
    let result = fixture.node.hello(&hello(PROTOCOL)).unwrap();
    assert_eq!(result.protocol, PROTOCOL);
    assert_eq!(result.node, fixture.node.name());
    assert_eq!(result.os, std::env::consts::OS);
    let error = fixture.node.hello(&hello(PROTOCOL + 1)).unwrap_err();
    assert_eq!(error.code, ErrorCode::Unsupported);
    assert_eq!(error.details, Some(json!({"supported": [PROTOCOL]})));
}

#[tokio::test]
async fn the_name_comes_from_node_toml_and_follows_its_edits() {
    let fixture = Fixture::new();
    let host = fixture.node.name();
    fixture.write_config("node.toml", "name = \"tower\"\n");
    assert_eq!(fixture.node.name(), "tower");
    fixture.write_config("node.toml", "name = \"pi\"\n");
    assert_eq!(fixture.node.name(), "pi");
    fixture.write_config("node.toml", "name = \"Not Valid\"\n");
    assert_eq!(fixture.node.name(), host);
    assert!(fixture.call("t.echo", json!({})).await.is_ok());
}
