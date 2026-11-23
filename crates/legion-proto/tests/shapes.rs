//! The JSON every type turns into, written out in full. A test failing here means a client built
//! against the old shape would misread a node built from the new one.

#![allow(clippy::unwrap_used)]

use std::collections::BTreeMap;

use legion_proto::*;
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

/// Serializes `value`, compares the JSON with `expected`, and reads it back to the same value.
fn pin<T>(value: T, expected: Value)
where
    T: Serialize + DeserializeOwned + PartialEq + std::fmt::Debug,
{
    let written = serde_json::to_value(&value).unwrap();
    assert_eq!(written, expected);
    let read: T = serde_json::from_value(expected).unwrap();
    assert_eq!(read, value);
}

#[test]
fn error_codes_are_the_closed_set_the_design_lists() {
    let written: Vec<Value> = ErrorCode::ALL
        .iter()
        .map(|code| serde_json::to_value(code).unwrap())
        .collect();
    assert_eq!(
        written,
        vec![
            json!("invalid_args"),
            json!("unknown_tool"),
            json!("not_found"),
            json!("exists"),
            json!("conflict"),
            json!("denied"),
            json!("unreachable"),
            json!("timeout"),
            json!("cancelled"),
            json!("busy"),
            json!("unsupported"),
            json!("failed"),
            json!("internal"),
        ]
    );
    for code in ErrorCode::ALL {
        assert_eq!(code.as_str().parse::<ErrorCode>().unwrap(), code);
        assert_eq!(json!(code.as_str()), serde_json::to_value(code).unwrap());
    }
    assert!(serde_json::from_value::<ErrorCode>(json!("oops")).is_err());
}

#[test]
fn an_error_body_leaves_out_what_it_does_not_have() {
    pin(
        ErrorBody::new(ErrorCode::Denied, "No."),
        json!({"code": "denied", "message": "No."}),
    );
    pin(
        ErrorBody::new(ErrorCode::Denied, "No.")
            .with_hint("Ask.")
            .with_machine("tower")
            .with_details(json!({"rule": "fs.*"})),
        json!({
            "code": "denied",
            "message": "No.",
            "hint": "Ask.",
            "machine": "tower",
            "details": {"rule": "fs.*"}
        }),
    );
}

#[test]
fn effects_are_lower_case_words() {
    let written: Vec<Value> = Effect::ALL
        .iter()
        .map(|effect| serde_json::to_value(effect).unwrap())
        .collect();
    assert_eq!(
        written,
        vec![
            json!("read"),
            json!("write"),
            json!("exec"),
            json!("destroy"),
            json!("power"),
            json!("admin")
        ]
    );
    assert_eq!("exec".parse::<Effect>().unwrap(), Effect::Exec);
    let wrong = "raed".parse::<Effect>().unwrap_err();
    assert!(wrong.contains("read, write, exec, destroy, power, admin"));
}

#[test]
fn a_tool_spec_carries_its_schema_as_json() {
    pin(
        ToolSpec {
            name: "meta.version".into(),
            description: "Says which build runs.".into(),
            effect: Effect::Read,
            input_schema: json!({"type": "object", "additionalProperties": false}),
        },
        json!({
            "name": "meta.version",
            "description": "Says which build runs.",
            "effect": "read",
            "input_schema": {"type": "object", "additionalProperties": false}
        }),
    );
}

#[test]
fn requests_and_notifications() {
    pin(
        Request::new(1, method::CALL, json!({"tool": "meta.version"})),
        json!({"jsonrpc": "2.0", "id": 1, "method": "call", "params": {"tool": "meta.version"}}),
    );
    pin(
        Request::new("a", method::TOOLS, Value::Null),
        json!({"jsonrpc": "2.0", "id": "a", "method": "tools"}),
    );
    pin(
        Request {
            jsonrpc: Version,
            id: None,
            method: method::CANCEL.into(),
            params: json!({"id": 4}),
        },
        json!({"jsonrpc": "2.0", "method": "cancel", "params": {"id": 4}}),
    );
    pin(
        Notification::new(
            method::EVENT,
            json!({"id": 3, "kind": "progress", "data": {"done": 1}}),
        ),
        json!({"jsonrpc": "2.0", "method": "event", "params": {"id": 3, "kind": "progress", "data": {"done": 1}}}),
    );
}

#[test]
fn a_request_speaking_another_jsonrpc_or_a_fractional_id_is_refused() {
    let other = json!({"jsonrpc": "1.0", "id": 1, "method": "call"});
    assert!(serde_json::from_value::<Request>(other).is_err());
    let fractional = json!({"jsonrpc": "2.0", "id": 1.5, "method": "call"});
    assert!(serde_json::from_value::<Request>(fractional).is_err());
}

#[test]
fn responses_have_a_result_or_an_error() {
    pin(
        Response::ok(Some(Id::Number(7)), json!({"version": "0.1.0"})),
        json!({"jsonrpc": "2.0", "id": 7, "result": {"version": "0.1.0"}}),
    );
    pin(
        Response::error(
            Some(Id::String("x".into())),
            RpcError::new(
                TOOL_ERROR,
                ErrorBody::new(ErrorCode::NotFound, "No such file."),
            ),
        ),
        json!({
            "jsonrpc": "2.0",
            "id": "x",
            "error": {
                "code": -32000,
                "message": "No such file.",
                "data": {"code": "not_found", "message": "No such file."}
            }
        }),
    );
    pin(
        Response::error(
            None,
            RpcError::new(
                PARSE_ERROR,
                ErrorBody::new(ErrorCode::InvalidArgs, "Not JSON."),
            ),
        ),
        json!({
            "jsonrpc": "2.0",
            "id": null,
            "error": {
                "code": -32700,
                "message": "Not JSON.",
                "data": {"code": "invalid_args", "message": "Not JSON."}
            }
        }),
    );
}

#[test]
fn hello() {
    pin(
        HelloParams {
            client: "legion".into(),
            version: "0.1.0".into(),
            protocol: PROTOCOL,
        },
        json!({"client": "legion", "version": "0.1.0", "protocol": 1}),
    );
    pin(
        HelloResult {
            node: "tower".into(),
            version: "0.1.0".into(),
            protocol: 1,
            os: "linux".into(),
            arch: "x86_64".into(),
            systems: vec![],
        },
        json!({
            "node": "tower",
            "version": "0.1.0",
            "protocol": 1,
            "os": "linux",
            "arch": "x86_64",
            "systems": []
        }),
    );
}

#[test]
fn tools() {
    pin(ToolsParams {}, json!({}));
    pin(ToolsResult { tools: vec![] }, json!({"tools": []}));
}

#[test]
fn call_spells_as_and_on_the_way_the_design_does() {
    pin(
        CallParams {
            tool: "fs.read".into(),
            args: json!({"path": "~/notes.txt"}),
            as_principal: Some("agent:claude".into()),
            on: Some(Target::One("tower".into())),
        },
        json!({"tool": "fs.read", "args": {"path": "~/notes.txt"}, "as": "agent:claude", "on": "tower"}),
    );
    let bare: CallParams = serde_json::from_value(json!({"tool": "meta.version"})).unwrap();
    assert_eq!(bare.args, json!({}));
    assert_eq!(bare.as_principal, None);
    assert_eq!(bare.on, None);
}

#[test]
fn params_accept_fields_a_newer_client_adds() {
    let call: CallParams =
        serde_json::from_value(json!({"tool": "meta.version", "deadline": 5})).unwrap();
    assert_eq!(call.tool, "meta.version");
}

#[test]
fn cancel_and_event() {
    pin(CancelParams { id: Id::Number(3) }, json!({"id": 3}));
    pin(CancelResult {}, json!({}));
    pin(
        EventParams {
            id: Id::String("c1".into()),
            kind: EventKind::Stdout,
            data: json!("hello\n"),
        },
        json!({"id": "c1", "kind": "stdout", "data": "hello\n"}),
    );
    for (kind, word) in [
        (EventKind::Stdout, "stdout"),
        (EventKind::Stderr, "stderr"),
        (EventKind::Progress, "progress"),
        (EventKind::Log, "log"),
    ] {
        assert_eq!(serde_json::to_value(kind).unwrap(), json!(word));
        assert_eq!(kind.as_str(), word);
    }
}

#[test]
fn a_target_is_a_name_a_list_or_all() {
    pin(Target::One("tower".into()), json!("tower"));
    pin(Target::All, json!("all"));
    pin(
        Target::Many(vec!["tower".into(), "pi".into()]),
        json!(["tower", "pi"]),
    );
    for wrong in [
        json!(""),
        json!([]),
        json!(["tower", ""]),
        json!(["all", "pi"]),
        json!(3),
        json!("two words"),
    ] {
        assert!(
            serde_json::from_value::<Target>(wrong.clone()).is_err(),
            "{wrong} was accepted"
        );
    }
}

#[test]
fn the_command_line_writes_a_target_with_commas() {
    assert_eq!(Target::parse("all").unwrap(), Target::All);
    assert_eq!(Target::parse("tower").unwrap(), Target::One("tower".into()));
    assert_eq!(
        Target::parse("tower, pi").unwrap(),
        Target::Many(vec!["tower".into(), "pi".into()])
    );
    assert!(Target::parse("").is_err());
    assert!(Target::parse("tower,").is_err());
    assert!(Target::parse("tower,all").is_err());
    assert_eq!(Target::One("pi".into()).single(), Some("pi"));
    assert_eq!(Target::Many(vec!["pi".into()]).single(), Some("pi"));
    assert_eq!(
        Target::Many(vec!["pi".into(), "tower".into()]).single(),
        None
    );
    assert_eq!(Target::All.single(), None);
}

#[test]
fn fan_out_gives_each_machine_ok_or_error() {
    let mut results = BTreeMap::new();
    results.insert("pi".into(), MachineOutcome::Ok(json!({"up": true})));
    results.insert(
        "tower".into(),
        MachineOutcome::Error(ErrorBody::new(ErrorCode::Unreachable, "Asleep.")),
    );
    pin(
        FanOut { results },
        json!({"results": {
            "pi": {"ok": {"up": true}},
            "tower": {"error": {"code": "unreachable", "message": "Asleep."}}
        }}),
    );
}
