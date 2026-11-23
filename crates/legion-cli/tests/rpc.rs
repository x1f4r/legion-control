//! `legion rpc`, spoken to over its stdin and stdout the way another node or a client does.

#![allow(clippy::unwrap_used)]

mod common;

use common::Home;
use serde_json::{Value, json};

fn request(id: Value, method: &str, params: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params})
}

#[test]
fn a_session_runs_calls_concurrently_cancels_the_right_one_and_ends_with_its_input() {
    let home = Home::new();
    let mut session = home.session(&[]);

    session.send_json(&request(
        json!(1),
        "hello",
        json!({"client": "test", "version": "0", "protocol": 1}),
    ));
    let (hello, _) = session.reply_to(&json!(1));
    assert_eq!(hello["result"]["protocol"], json!(1));
    assert_eq!(hello["result"]["os"], json!(std::env::consts::OS));

    session.send_json(&request(json!("tools"), "tools", json!({})));
    let (tools, _) = session.reply_to(&json!("tools"));
    assert!(
        tools["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .any(|spec| spec["name"] == json!("meta.version"))
    );

    // A long call, then a short one: the short one answers first, while the long one is still
    // reporting progress under its own id.
    session.send_json(&request(
        json!("slow"),
        "call",
        json!({"tool": "test.sleep", "args": {"seconds": 60}}),
    ));
    session.send_json(&request(
        json!("other-slow"),
        "call",
        json!({"tool": "test.sleep", "args": {"seconds": 0.2}}),
    ));
    session.send_json(&request(json!(2), "call", json!({"tool": "meta.version"})));
    let (version, before) = session.reply_to(&json!(2));
    assert_eq!(version["result"]["name"], json!("legion"));
    let progress: Vec<&Value> = before
        .iter()
        .filter(|m| m["method"] == json!("event"))
        .collect();
    assert!(
        progress
            .iter()
            .all(|event| event["params"]["kind"] == json!("progress"))
    );

    // Make sure the slow call is running before it is cancelled.
    loop {
        let message = session.next();
        if message["method"] == json!("event") && message["params"]["id"] == json!("slow") {
            break;
        }
    }
    session.send_json(&request(json!(3), "cancel", json!({"id": "slow"})));
    let mut replies = std::collections::HashMap::new();
    while replies.len() < 3 {
        let message = session.next();
        if message.get("method").is_none() {
            replies.insert(message["id"].to_string(), message);
        }
    }
    assert_eq!(replies["3"]["result"], json!({}));
    assert_eq!(
        replies["\"slow\""]["error"]["data"]["code"],
        json!("cancelled")
    );
    assert_eq!(replies["\"slow\""]["error"]["code"], json!(-32000));
    // The other call under the same tool was not the one cancelled.
    assert_eq!(replies["\"other-slow\""]["result"], json!({"slept": 0.2}));

    assert_eq!(session.finish(), 0);
    let audit = home.audit();
    let slow = audit
        .iter()
        .find(|line| line["tool"] == json!("test.sleep") && line["outcome"] != json!("ok"))
        .unwrap();
    assert_eq!(slow["outcome"], json!("cancelled"));
    assert_eq!(slow["door"], json!("rpc"));
}

#[test]
fn malformed_lines_get_errors_and_the_session_goes_on() {
    let home = Home::new();
    let mut session = home.session(&[]);
    session.send("this is not json");
    let parse = session.next();
    assert_eq!(parse["id"], Value::Null);
    assert_eq!(parse["error"]["code"], json!(-32700));
    assert_eq!(parse["error"]["data"]["code"], json!("invalid_args"));

    session.send(r#"{"jsonrpc": "1.0", "id": 5, "method": "call"}"#);
    let invalid = session.next();
    assert_eq!(invalid["id"], json!(5));
    assert_eq!(invalid["error"]["code"], json!(-32600));

    session.send_json(&request(json!(6), "launch", json!({})));
    assert_eq!(session.next()["error"]["code"], json!(-32601));

    session.send_json(&request(json!(7), "call", json!({"args": {}})));
    assert_eq!(session.next()["error"]["code"], json!(-32602));

    session.send_json(&request(
        json!(8),
        "hello",
        json!({"client": "test", "version": "9", "protocol": 99}),
    ));
    let refused = session.next();
    assert_eq!(refused["error"]["data"]["code"], json!("unsupported"));
    assert_eq!(refused["error"]["data"]["details"]["supported"], json!([1]));

    session.send("");
    session.send_json(&request(json!(9), "call", json!({"tool": "nope.nothing"})));
    let unknown = session.next();
    assert_eq!(unknown["id"], json!(9));
    assert_eq!(unknown["error"]["data"]["code"], json!("unknown_tool"));

    session.send_json(&request(
        json!(10),
        "call",
        json!({"tool": "meta.version", "on": "elsewhere"}),
    ));
    assert_eq!(
        session.next()["error"]["data"]["code"],
        json!("unreachable")
    );

    assert_eq!(session.finish(), 0);
}

#[test]
fn input_that_ends_early_still_gets_every_reply() {
    let home = Home::new();
    let mut session = home.session(&[]);
    session.send_json(&request(
        json!(1),
        "call",
        json!({"tool": "test.sleep", "args": {"seconds": 0.3}}),
    ));
    session.send_json(&request(json!(2), "call", json!({"tool": "meta.whoami"})));
    // A notification gets no reply.
    session.send(r#"{"jsonrpc": "2.0", "method": "cancel", "params": {"id": 99}}"#);
    assert_eq!(session.finish(), 0);
    let replies: Vec<Value> = session
        .rest()
        .iter()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .filter(|message| message.get("method").is_none())
        .collect();
    assert_eq!(replies.len(), 2, "{replies:?}");
    assert!(
        replies
            .iter()
            .any(|reply| reply["id"] == json!(1) && reply["result"] == json!({"slept": 0.3}))
    );
    assert!(
        replies
            .iter()
            .any(|reply| reply["id"] == json!(2) && reply["result"]["principal"] == json!("owner"))
    );
}

#[test]
fn a_pinned_session_keeps_its_principal_whatever_a_call_asks_for() {
    let home = Home::new();
    home.write_config(
        "policy.toml",
        "[principals.phone]\nrole = \"observer\"\n[roles.observer]\nallow = [\"effect:read\"]\n",
    );
    let mut session = home.session(&["--as", "phone"]);
    session.send_json(&request(
        json!(1),
        "call",
        json!({"tool": "meta.whoami", "as": "owner"}),
    ));
    let (whoami, _) = session.reply_to(&json!(1));
    assert_eq!(whoami["result"]["principal"], json!("phone"));
    assert_eq!(whoami["result"]["door"], json!("rpc"));
    session.send_json(&request(
        json!(2),
        "call",
        json!({"tool": "test.write_path", "args": {"path": "/x"}, "as": "owner"}),
    ));
    let (denied, _) = session.reply_to(&json!(2));
    assert_eq!(denied["error"]["data"]["code"], json!("denied"));
    assert_eq!(session.finish(), 0);
    assert!(
        home.audit()
            .iter()
            .all(|line| line["principal"] == json!("phone"))
    );
}
