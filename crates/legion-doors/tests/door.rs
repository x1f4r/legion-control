//! The MCP door as a library, over an in-memory pipe and a router that only records what reaches
//! it: what the door hands on is exactly what the fleet's router will be given.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use legion_core::proto::{EventKind, Target};
use legion_core::{BoxFuture, Call, Door, Node, Paths, RouteOutcome, Router};
use legion_doors::mcp::{McpDoor, Options};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

/// What one call looked like when it reached the router.
#[derive(Debug, PartialEq)]
struct Routed {
    tool: String,
    args: Value,
    on: Option<Target>,
    door: Door,
    pinned: Option<String>,
}

#[derive(Default)]
struct Recorder {
    routed: Mutex<Vec<Routed>>,
    shut_down: AtomicBool,
}

impl Router for Recorder {
    fn call<'a>(
        &'a self,
        _node: &'a Node,
        call: Call,
        on: Option<Target>,
    ) -> BoxFuture<'a, RouteOutcome> {
        self.routed.lock().unwrap().push(Routed {
            tool: call.tool,
            args: call.args,
            on,
            door: call.door,
            pinned: call.pinned,
        });
        Box::pin(async { RouteOutcome::One(Ok(json!({"routed": true}))) })
    }

    fn shutdown(&self) -> BoxFuture<'_, ()> {
        self.shut_down.store(true, Ordering::SeqCst);
        Box::pin(async {})
    }
}

#[tokio::test]
async fn on_is_taken_out_of_the_arguments_and_handed_to_the_router() {
    let dir = tempfile::tempdir().unwrap();
    let node = Node::new(Paths::under(dir.path()), legion_tools::registry());
    let recorder = Arc::new(Recorder::default());
    let door = McpDoor::new(
        node,
        recorder.clone(),
        Options {
            pinned: Some("agent:test".to_owned()),
            compact: false,
        },
    )
    .unwrap();

    let (client, server) = tokio::io::duplex(1 << 16);
    let (server_in, server_out) = tokio::io::split(server);
    let serving = tokio::spawn(door.serve(server_in, server_out));
    let (client_in, mut client_out) = tokio::io::split(client);
    let mut replies = BufReader::new(client_in).lines();

    let requests = [
        json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
            "protocolVersion": "2025-11-25", "capabilities": {},
            "clientInfo": {"name": "test", "version": "0"}}}),
        json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
        json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": {
            "name": "test_echo", "arguments": {"on": ["tower", "pi"], "x": 1}}}),
        json!({"jsonrpc": "2.0", "id": 3, "method": "tools/call", "params": {
            "name": "meta_version"}}),
    ];
    for request in requests {
        client_out
            .write_all(format!("{request}\n").as_bytes())
            .await
            .unwrap();
    }
    let mut answered = Vec::new();
    while answered.len() < 3 {
        let line = replies.next_line().await.unwrap().unwrap();
        let reply: Value = serde_json::from_str(&line).unwrap();
        answered.push(reply["id"].clone());
        if reply["id"] != json!(1) {
            assert_eq!(reply["result"].get("structuredContent"), None);
            assert_eq!(
                reply["result"]["content"][0]["text"],
                json!(r#"{"routed":true}"#)
            );
        }
    }
    // Shutting the write half down is the end of the input; dropping it would not be, while the
    // read half keeps the pipe open.
    client_out.shutdown().await.unwrap();
    serving.await.unwrap().unwrap();

    let mut routed = std::mem::take(&mut *recorder.routed.lock().unwrap());
    assert!(recorder.shut_down.load(Ordering::SeqCst));
    routed.sort_by(|a, b| a.tool.cmp(&b.tool));
    assert_eq!(
        routed,
        [
            Routed {
                tool: "meta.version".to_owned(),
                args: json!({}),
                on: None,
                door: Door::Mcp,
                pinned: Some("agent:test".to_owned()),
            },
            Routed {
                tool: "test.echo".to_owned(),
                args: json!({"x": 1}),
                on: Some(Target::Many(vec!["tower".to_owned(), "pi".to_owned()])),
                door: Door::Mcp,
                pinned: Some("agent:test".to_owned()),
            },
        ]
    );
}

/// Says something through the call's events before answering.
struct Chatty;

impl Router for Chatty {
    fn call<'a>(
        &'a self,
        _node: &'a Node,
        call: Call,
        _on: Option<Target>,
    ) -> BoxFuture<'a, RouteOutcome> {
        call.events.emit(EventKind::Stdout, json!("compiling\n"));
        call.events.emit(EventKind::Log, json!("done"));
        Box::pin(async { RouteOutcome::One(Ok(json!({"said": 2}))) })
    }
}

#[tokio::test]
async fn events_reach_a_client_that_asked_for_progress_and_no_other() {
    let dir = tempfile::tempdir().unwrap();
    let node = Node::new(Paths::under(dir.path()), legion_tools::registry());
    let door = McpDoor::new(node, Arc::new(Chatty), Options::default()).unwrap();
    let (client, server) = tokio::io::duplex(1 << 16);
    let (server_in, server_out) = tokio::io::split(server);
    let serving = tokio::spawn(door.serve(server_in, server_out));
    let (client_in, mut client_out) = tokio::io::split(client);
    let mut lines = BufReader::new(client_in).lines();

    let requests = [
        json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
            "protocolVersion": "2025-11-25", "capabilities": {},
            "clientInfo": {"name": "test", "version": "0"}}}),
        json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": {
            "name": "meta_version"}}),
        json!({"jsonrpc": "2.0", "id": 3, "method": "tools/call", "params": {
            "_meta": {"progressToken": "t"}, "name": "meta_version"}}),
    ];
    let mut seen = Vec::new();
    for request in requests {
        let id = request["id"].clone();
        client_out
            .write_all(format!("{request}\n").as_bytes())
            .await
            .unwrap();
        loop {
            let line = lines.next_line().await.unwrap().unwrap();
            let message: Value = serde_json::from_str(&line).unwrap();
            let done = message["id"] == id;
            seen.push(message);
            if done {
                break;
            }
        }
    }
    client_out.shutdown().await.unwrap();
    serving.await.unwrap().unwrap();

    let methods: Vec<&Value> = seen.iter().map(|message| &message["method"]).collect();
    // The call without a progress token got nothing but its result.
    assert_eq!(
        methods,
        [
            &Value::Null,
            &Value::Null,
            &json!("notifications/progress"),
            &json!("notifications/progress"),
            &Value::Null
        ]
    );
    let progress: Vec<&Value> = seen[2..4]
        .iter()
        .map(|message| &message["params"])
        .collect();
    assert_eq!(
        progress,
        [
            &json!({"progressToken": "t", "progress": 1.0, "message": "stdout: compiling"}),
            &json!({"progressToken": "t", "progress": 2.0, "message": "done"}),
        ]
    );
}

#[tokio::test]
async fn extension_tool_is_visible_and_callable_through_mcp_without_restart() {
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("config");
    std::fs::create_dir_all(&config).unwrap();
    std::fs::write(
        config.join("policy.toml"),
        "[principals.observer]\nrole = \"observer\"\n[roles.observer]\nallow = [\"effect:read\"]\n",
    )
    .unwrap();
    let node = Node::new(Paths::under(dir.path()), legion_tools::registry());
    let door = McpDoor::new(
        node.clone(),
        Arc::new(legion_core::Local),
        Options {
            pinned: Some("observer".to_owned()),
            compact: false,
        },
    )
    .unwrap();

    let (client, server) = tokio::io::duplex(1 << 16);
    let (server_in, server_out) = tokio::io::split(server);
    let serving = tokio::spawn(door.serve(server_in, server_out));
    let (client_in, mut client_out) = tokio::io::split(client);
    let mut lines = BufReader::new(client_in).lines();

    let init = json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
        "protocolVersion": "2025-11-25", "capabilities": {},
        "clientInfo": {"name": "test", "version": "0"}
    }});
    client_out
        .write_all(format!("{init}\n").as_bytes())
        .await
        .unwrap();
    let _ = lines.next_line().await.unwrap().unwrap();
    client_out
        .write_all(b"{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}\n")
        .await
        .unwrap();

    // Add two extensions AFTER the MCP door has already started: one read, one exec.
    let tools_d = node.paths().tools_dir();
    std::fs::create_dir_all(&tools_d).unwrap();
    #[cfg(unix)]
    let cmd = "printf '{\"who\":\"%s\",\"arg\":\"%s\"}' \"$LEGION_PRINCIPAL\" \"$LEGION_ARG_TAG\"";
    #[cfg(windows)]
    let cmd = "Write-Output ('{\"who\":\"' + $env:LEGION_PRINCIPAL + '\",\"arg\":\"' + $env:LEGION_ARG_TAG + '\"}')";
    std::fs::write(
        tools_d.join("custom.hello.toml"),
        format!(
            "name = \"custom.hello\"\ndescription = \"Says hello.\"\neffect = \"read\"\ncmd = {}\n[params.tag]\ntype = \"string\"\nrequired = true\n",
            serde_json::to_string(cmd).unwrap()
        ),
    )
    .unwrap();
    std::fs::write(
        tools_d.join("custom.danger.toml"),
        "name = \"custom.danger\"\ndescription = \"Runs something.\"\neffect = \"exec\"\ncmd = \"echo no\"\n",
    )
    .unwrap();

    // tools/list sees custom_hello (read) and hides custom_danger (exec) for observer.
    let list_req = json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {}});
    client_out
        .write_all(format!("{list_req}\n").as_bytes())
        .await
        .unwrap();
    let list_reply: Value =
        serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
    let names: Vec<&str> = list_reply["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert!(names.contains(&"custom_hello"), "{names:?}");
    assert!(!names.contains(&"custom_danger"), "{names:?}");

    // Call custom_hello through MCP.
    let call_req = json!({"jsonrpc": "2.0", "id": 3, "method": "tools/call", "params": {
        "name": "custom_hello",
        "arguments": {"tag": "mcp-live"}
    }});
    client_out
        .write_all(format!("{call_req}\n").as_bytes())
        .await
        .unwrap();
    let call_reply: Value =
        serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
    assert_eq!(
        call_reply["result"]["isError"],
        json!(false),
        "{call_reply}"
    );
    let body: Value =
        serde_json::from_str(call_reply["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(body, json!({"who": "observer", "arg": "mcp-live"}));

    client_out.shutdown().await.unwrap();
    serving.await.unwrap().unwrap();
}
