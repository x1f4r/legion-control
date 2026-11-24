//! `legion mcp`, spoken to over its stdin and stdout in raw JSON lines, the way an agent's MCP
//! client does: both the revision that opens with `initialize` and the one where every request
//! carries its own `_meta`.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

use common::{Home, PATIENCE};
use serde_json::{Value, json};

/// The revision with the `initialize` handshake, which Claude Code speaks.
const HANDSHAKE: &str = "2025-11-25";
/// The revision without one.
const HANDSHAKE_LESS: &str = "2026-07-28";

/// A running `legion mcp`.
struct Mcp {
    child: Child,
    stdin: Option<ChildStdin>,
    lines: Receiver<String>,
    /// Whether requests carry the `_meta` the handshake-less revision requires.
    inline: bool,
    next_id: u64,
}

impl Mcp {
    fn start(home: &Home, args: &[&str]) -> Mcp {
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
        let (sender, lines) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                if sender.send(line).is_err() {
                    break;
                }
            }
        });
        Mcp {
            child,
            stdin,
            lines,
            inline: false,
            next_id: 0,
        }
    }

    /// A session that has done the `initialize` handshake.
    fn initialized(home: &Home, args: &[&str]) -> (Mcp, Value) {
        let mut mcp = Mcp::start(home, args);
        let init = mcp.request(
            "initialize",
            json!({
                "protocolVersion": HANDSHAKE,
                "capabilities": {},
                "clientInfo": {"name": "test", "version": "0"}
            }),
        );
        mcp.notify("notifications/initialized", json!({}));
        (mcp, init)
    }

    /// A session of the handshake-less revision, which has asked nothing yet.
    fn inline(home: &Home, args: &[&str]) -> Mcp {
        let mut mcp = Mcp::start(home, args);
        mcp.inline = true;
        mcp
    }

    fn send(&mut self, message: &Value) {
        let stdin = self.stdin.as_mut().expect("stdin is already closed");
        writeln!(stdin, "{message}").unwrap();
        stdin.flush().unwrap();
    }

    fn meta() -> Value {
        json!({
            "io.modelcontextprotocol/protocolVersion": HANDSHAKE_LESS,
            "io.modelcontextprotocol/clientCapabilities": {},
            "io.modelcontextprotocol/clientInfo": {"name": "test", "version": "0"}
        })
    }

    /// Sends a request, adding the revision's `_meta`, and returns its id.
    fn send_request(&mut self, method: &str, mut params: Value) -> Value {
        self.next_id += 1;
        let id = json!(self.next_id);
        if self.inline {
            let meta = params
                .as_object_mut()
                .unwrap()
                .entry("_meta")
                .or_insert_with(|| json!({}));
            for (key, value) in Mcp::meta().as_object().unwrap() {
                meta[key] = value.clone();
            }
        }
        self.send(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));
        id
    }

    fn notify(&mut self, method: &str, params: Value) {
        self.send(&json!({"jsonrpc": "2.0", "method": method, "params": params}));
    }

    /// The next line, which must be a JSON-RPC 2.0 message: nothing else may reach stdout.
    fn next(&mut self) -> Value {
        let line = self
            .lines
            .recv_timeout(PATIENCE)
            .unwrap_or_else(|error| panic!("no line from legion mcp: {error}"));
        let message: Value = serde_json::from_str(&line)
            .unwrap_or_else(|error| panic!("{line:?} on stdout is not JSON: {error}"));
        assert_eq!(message["jsonrpc"], json!("2.0"), "{line}");
        message
    }

    /// Messages until the reply to `id`, returning it and the notifications on the way.
    fn reply_to(&mut self, id: &Value) -> (Value, Vec<Value>) {
        let mut seen = Vec::new();
        loop {
            let message = self.next();
            if message.get("id") == Some(id) && message.get("method").is_none() {
                return (message, seen);
            }
            seen.push(message);
        }
    }

    /// A request's result, failing the test on a protocol error.
    fn request(&mut self, method: &str, params: Value) -> Value {
        let id = self.send_request(method, params);
        let (reply, _) = self.reply_to(&id);
        assert!(reply.get("error").is_none(), "{method} failed: {reply}");
        reply["result"].clone()
    }

    fn call(&mut self, tool: &str, arguments: Value) -> Value {
        self.request("tools/call", json!({"name": tool, "arguments": arguments}))
    }

    /// The names `tools/list` gives.
    fn tool_names(&mut self) -> Vec<String> {
        let list = self.request("tools/list", json!({}));
        list["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|tool| tool["name"].as_str().unwrap().to_owned())
            .collect()
    }

    /// Closes stdin and waits for the process to end, returning its exit code and whatever else
    /// it wrote to stdout.
    fn finish(&mut self) -> (i32, Vec<String>) {
        drop(self.stdin.take());
        let deadline = Instant::now() + PATIENCE;
        let code = loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                break status.code().unwrap_or(-1);
            }
            assert!(
                Instant::now() < deadline,
                "legion mcp did not end after its input did"
            );
            std::thread::sleep(Duration::from_millis(20));
        };
        let mut rest = Vec::new();
        while let Ok(line) = self.lines.recv_timeout(PATIENCE) {
            rest.push(line);
        }
        (code, rest)
    }
}

impl Drop for Mcp {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// The one JSON object a tool result carries: as one line of text, and only there.
fn payload(result: &Value) -> Value {
    assert_eq!(result.get("structuredContent"), None, "{result}");
    let content = result["content"].as_array().unwrap();
    assert_eq!(content.len(), 1, "{result}");
    let text = content[0]["text"].as_str().unwrap();
    assert!(!text.contains('\n'), "{text}");
    serde_json::from_str(text).unwrap()
}

/// A successful tool result's object.
fn ok(result: &Value) -> Value {
    assert_eq!(result["isError"], json!(false), "{result}");
    payload(result)
}

/// A failed tool result's error object, which must be a result and not a protocol error.
fn failed(result: &Value) -> Value {
    assert_eq!(result["isError"], json!(true), "{result}");
    let error = payload(result);
    assert!(error["message"].is_string(), "{error}");
    error
}

#[test]
fn the_handshake_revision_lists_and_calls_the_tools() {
    let home = Home::new();
    let (mut mcp, init) = Mcp::initialized(&home, &[]);
    assert_eq!(init["protocolVersion"], json!(HANDSHAKE));
    assert_eq!(init["serverInfo"]["name"], json!("legion"));
    assert_eq!(
        init["serverInfo"]["version"],
        json!(env!("CARGO_PKG_VERSION"))
    );
    assert!(init["capabilities"]["tools"].is_object());
    let instructions = init["instructions"].as_str().unwrap();
    assert!(instructions.contains("meta_whoami"), "{instructions}");
    assert!(instructions.contains("`on`"), "{instructions}");

    assert_eq!(mcp.request("ping", json!({})), json!({}));

    let list = mcp.request("tools/list", json!({}));
    assert_eq!(list.get("resultType"), None, "{list}");
    let tools = list["tools"].as_array().unwrap();
    let whoami = tools
        .iter()
        .find(|tool| tool["name"] == json!("meta_whoami"))
        .unwrap();
    assert_eq!(whoami["title"], json!("meta.whoami"));
    assert_eq!(whoami["annotations"]["readOnlyHint"], json!(true));
    assert!(whoami["inputSchema"]["properties"]["on"].is_object());
    // Anything that is not read-only asks for confirmation in a client that asks.
    for name in [
        "test_write_path",
        "exec_run",
        "fs_write",
        "fs_edit",
        "fs_move",
        "fs_remove",
    ] {
        let tool = tools
            .iter()
            .find(|tool| tool["name"] == json!(name))
            .unwrap();
        assert_eq!(tool["annotations"]["readOnlyHint"], json!(false), "{name}");
        assert_eq!(
            tool["annotations"]["destructiveHint"],
            json!(true),
            "{name}"
        );
    }
    for tool in tools {
        let name = tool["name"].as_str().unwrap();
        assert!(!name.contains('.'), "{name}");
        assert_eq!(tool["inputSchema"]["type"], json!("object"), "{name}");
        assert_eq!(
            legion_core::schema::problems(&tool["inputSchema"]),
            Vec::<String>::new(),
            "{name}"
        );
    }

    let version = mcp.call("meta_version", json!({}));
    assert_eq!(version.get("resultType"), None);
    assert_eq!(ok(&version)["name"], json!("legion"));
    assert_eq!(
        ok(&mcp.call("meta_whoami", json!({})))["door"],
        json!("mcp")
    );
    // The registry's own name is understood as well.
    assert_eq!(
        ok(&mcp.call("test.echo", json!({"a": [1]}))),
        json!({"a": [1]})
    );

    let (code, rest) = mcp.finish();
    assert_eq!(code, 0);
    assert_eq!(rest, Vec::<String>::new());
    let audit = home.audit();
    assert!(
        audit
            .iter()
            .any(|line| line["tool"] == json!("meta.version") && line["door"] == json!("mcp"))
    );
}

#[test]
fn the_handshake_less_revision_discovers_lists_and_calls_the_tools() {
    let home = Home::new();
    let mut mcp = Mcp::inline(&home, &[]);

    let discover = mcp.request("server/discover", json!({}));
    assert_eq!(discover["resultType"], json!("complete"));
    let versions = discover["supportedVersions"].as_array().unwrap();
    assert!(versions.contains(&json!(HANDSHAKE_LESS)), "{discover}");
    assert!(versions.contains(&json!(HANDSHAKE)), "{discover}");
    assert_eq!(
        discover["_meta"]["io.modelcontextprotocol/serverInfo"]["name"],
        json!("legion")
    );
    assert!(discover["capabilities"]["tools"].is_object());
    assert!(
        discover["instructions"]
            .as_str()
            .unwrap()
            .contains("meta_whoami")
    );

    let list = mcp.request("tools/list", json!({}));
    assert_eq!(list["resultType"], json!("complete"));
    assert!(list["ttlMs"].is_number(), "{list}");
    let names: Vec<&str> = list["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["name"].as_str().unwrap())
        .collect();
    assert!(names.contains(&"meta_version"), "{names:?}");

    let echo = mcp.call("test_echo", json!({"text": "hi"}));
    assert_eq!(echo["resultType"], json!("complete"));
    assert_eq!(ok(&echo), json!({"text": "hi"}));

    // Without the request's own `_meta` the revision cannot be told, so it is refused.
    mcp.send(&json!({"jsonrpc": "2.0", "id": "bare", "method": "tools/list", "params": {}}));
    let (bare, _) = mcp.reply_to(&json!("bare"));
    assert_eq!(bare["error"]["code"], json!(-32602), "{bare}");

    let (code, rest) = mcp.finish();
    assert_eq!(code, 0);
    assert_eq!(rest, Vec::<String>::new());
}

#[test]
fn failures_are_tool_results_carrying_the_error_object() {
    let home = Home::new();
    home.write_config("node.toml", "name = \"tower\"\n");
    for inline in [false, true] {
        let mut mcp = if inline {
            Mcp::inline(&home, &[])
        } else {
            Mcp::initialized(&home, &[]).0
        };

        let unknown = failed(&mcp.call("fs_nothing", json!({})));
        assert_eq!(unknown["code"], json!("unknown_tool"));
        assert_eq!(unknown["machine"], json!("tower"));
        assert!(
            unknown["hint"].as_str().unwrap().contains("tools/list"),
            "{unknown}"
        );

        let invalid = failed(&mcp.call("test_sleep", json!({"seconds": "long"})));
        assert_eq!(invalid["code"], json!("invalid_args"));
        assert!(
            invalid["message"].as_str().unwrap().contains("seconds"),
            "{invalid}"
        );
        let extra = failed(&mcp.call("meta_version", json!({"colour": 1})));
        assert_eq!(extra["code"], json!("invalid_args"));

        let elsewhere = failed(&mcp.call("meta_version", json!({"on": "pi"})));
        assert_eq!(elsewhere["code"], json!("not_found"));
        assert!(elsewhere["hint"].is_string());
        // The schema asks for one string, with commas between names; a list is taken too.
        for several in [json!("tower,pi"), json!(["tower", "pi"])] {
            let partial = ok(&mcp.call("meta_version", json!({"on": several})));
            assert_eq!(
                partial["results"]["tower"]["ok"]["node"],
                json!("tower"),
                "{several}"
            );
            assert_eq!(
                partial["results"]["pi"]["error"]["code"],
                json!("not_found"),
                "{several}"
            );
        }
        // A fan-out where every machine fails sets isError with the wire results object.
        for all_bad in [json!("pi,laptop"), json!(["pi", "laptop"])] {
            let call = mcp.call("meta_version", json!({"on": all_bad}));
            assert_eq!(call["isError"], json!(true), "{call}");
            let body = payload(&call);
            assert_eq!(body["results"]["pi"]["error"]["code"], json!("not_found"));
            assert_eq!(
                body["results"]["laptop"]["error"]["code"],
                json!("not_found")
            );
        }
        for bad in [json!(5), json!(""), json!("tower,,pi")] {
            let bad_on = failed(&mcp.call("meta_version", json!({"on": bad})));
            assert_eq!(bad_on["code"], json!("invalid_args"), "{bad}");
        }

        // `on` naming this node runs here, and `on` never reaches the tool.
        assert_eq!(
            ok(&mcp.call("test_echo", json!({"on": "tower", "kept": true}))),
            json!({"kept": true})
        );
        assert_eq!(ok(&mcp.call("test_echo", json!({"on": null}))), json!({}));

        let (code, rest) = mcp.finish();
        assert_eq!(code, 0);
        assert_eq!(rest, Vec::<String>::new());
    }
}

#[test]
fn a_pinned_principal_is_shown_and_allowed_only_what_its_role_allows() {
    let home = Home::new();
    home.write_config(
        "policy.toml",
        r#"
[principals."agent:claude"]
role = "observer"

[roles.observer]
allow = ["effect:read"]
deny = ["meta.doctor"]
"#,
    );
    let (mut mcp, _) = Mcp::initialized(&home, &["--as", "agent:claude"]);
    let names = mcp.tool_names();
    assert!(names.contains(&"meta_whoami".to_owned()), "{names:?}");
    assert!(names.contains(&"test_read_path".to_owned()), "{names:?}");
    assert!(!names.contains(&"meta_doctor".to_owned()), "{names:?}");
    assert!(!names.contains(&"test_write_path".to_owned()), "{names:?}");

    let whoami = ok(&mcp.call("meta_whoami", json!({})));
    assert_eq!(whoami["principal"], json!("agent:claude"));
    assert_eq!(whoami["role"], json!("observer"));

    // Not listed, and refused by the node all the same when called.
    let denied = failed(&mcp.call("test_write_path", json!({"path": "~/projects/x"})));
    assert_eq!(denied["code"], json!("denied"));
    assert!(denied["hint"].is_string());
    let denied = failed(&mcp.call("meta_doctor", json!({})));
    assert_eq!(denied["code"], json!("denied"));

    assert_eq!(mcp.finish().0, 0);
    let audit = home.audit();
    let refused = audit
        .iter()
        .find(|line| line["tool"] == json!("test.write_path"))
        .unwrap();
    assert_eq!(refused["principal"], json!("agent:claude"));
    assert_eq!(refused["door"], json!("mcp"));
    assert_eq!(refused["outcome"], json!("denied"));

    // The same policy without --as: the owner sees everything.
    let (mut owner, _) = Mcp::initialized(&home, &[]);
    assert!(owner.tool_names().contains(&"meta_doctor".to_owned()));
    assert_eq!(owner.finish().0, 0);
}

#[test]
fn compact_mode_serves_four_tools_that_reach_every_other() {
    let home = Home::new();
    for inline in [false, true] {
        let mut mcp = if inline {
            Mcp::inline(&home, &["--compact"])
        } else {
            Mcp::initialized(&home, &["--compact"]).0
        };
        assert_eq!(
            mcp.tool_names(),
            [
                "legion_tools",
                "legion_describe",
                "legion_read",
                "legion_call"
            ]
        );
        // Only legion_call may change anything; legion_read promises it does not.
        let listed = mcp.request("tools/list", json!({}));
        let hints: Vec<_> = listed["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|tool| {
                let a = &tool["annotations"];
                (
                    a["readOnlyHint"].clone(),
                    a["destructiveHint"].clone(),
                    a["idempotentHint"].clone(),
                )
            })
            .collect();
        assert_eq!(
            hints,
            [
                (json!(true), Value::Null, Value::Null),
                (json!(true), Value::Null, Value::Null),
                (json!(true), json!(false), json!(true)),
                (json!(false), json!(true), Value::Null),
            ]
        );

        let all = ok(&mcp.call("legion_tools", json!({})));
        let whoami = all["tools"]
            .as_array()
            .unwrap()
            .iter()
            .find(|tool| tool["name"] == json!("meta.whoami"))
            .unwrap()
            .clone();
        assert_eq!(whoami["effect"], json!("read"));
        assert!(!whoami["description"].as_str().unwrap().contains('\n'));

        let meta = ok(&mcp.call("legion_tools", json!({"family": "meta"})));
        assert!(
            meta["tools"]
                .as_array()
                .unwrap()
                .iter()
                .all(|tool| tool["name"].as_str().unwrap().starts_with("meta."))
        );
        let none = failed(&mcp.call("legion_tools", json!({"family": "nope"})));
        assert_eq!(none["code"], json!("not_found"));
        assert!(none["hint"].as_str().unwrap().contains("meta"), "{none}");

        let sleep = ok(&mcp.call("legion_describe", json!({"tool": "test.sleep"})));
        assert_eq!(sleep["effect"], json!("read"));
        assert_eq!(sleep["input_schema"]["required"], json!(["seconds"]));
        let unknown = failed(&mcp.call("legion_describe", json!({"tool": "fs.nothing"})));
        assert_eq!(unknown["code"], json!("unknown_tool"));

        assert_eq!(
            ok(&mcp.call(
                "legion_call",
                json!({"tool": "test.echo", "args": {"x": 1}})
            )),
            json!({"x": 1})
        );
        assert_eq!(
            ok(&mcp.call("legion_call", json!({"tool": "meta.whoami"})))["door"],
            json!("mcp")
        );
        let elsewhere = failed(&mcp.call(
            "legion_call",
            json!({"tool": "meta.version", "on": "elsewhere"}),
        ));
        assert_eq!(elsewhere["code"], json!("not_found"));
        let missing = failed(&mcp.call("legion_call", json!({"args": {}})));
        assert_eq!(missing["code"], json!("invalid_args"));
        let direct = failed(&mcp.call("meta_version", json!({})));
        assert_eq!(direct["code"], json!("unknown_tool"));
        assert!(
            direct["hint"].as_str().unwrap().contains("legion_read"),
            "{direct}"
        );
        let direct = failed(&mcp.call("exec_run", json!({})));
        assert!(
            direct["hint"].as_str().unwrap().contains("legion_call"),
            "{direct}"
        );

        let (code, rest) = mcp.finish();
        assert_eq!(code, 0);
        assert_eq!(rest, Vec::<String>::new());
    }
}

#[test]
fn legion_read_runs_read_tools_and_refuses_the_others_without_running_them() {
    let home = Home::new();
    home.write_config("node.toml", "name = \"tower\"\n");
    let written = home.home().join("projects").join("app").join("note.txt");
    for inline in [false, true] {
        let mut mcp = if inline {
            Mcp::inline(&home, &["--compact"])
        } else {
            Mcp::initialized(&home, &["--compact"]).0
        };
        let whoami = ok(&mcp.call("legion_read", json!({"tool": "meta.whoami"})));
        assert_eq!(whoami["door"], json!("mcp"));
        assert_eq!(
            ok(&mcp.call(
                "legion_read",
                json!({"tool": "test.echo", "args": {"x": 1}})
            )),
            json!({"x": 1})
        );
        // `on` is passed through: this node by name, and a machine it does not know.
        assert_eq!(
            ok(&mcp.call(
                "legion_read",
                json!({"tool": "meta.version", "on": "tower"})
            ))["node"],
            json!("tower")
        );
        let elsewhere = failed(&mcp.call(
            "legion_read",
            json!({"tool": "meta.version", "on": "elsewhere"}),
        ));
        assert_eq!(elsewhere["code"], json!("not_found"));

        for (tool, args, effect) in [
            (
                "fs.write",
                json!({"path": written.to_string_lossy(), "content": "x"}),
                "write",
            ),
            ("test.write_path", json!({"path": "~/projects/x"}), "write"),
            ("exec.run", json!({"argv": ["true"]}), "exec"),
            ("fs.remove", json!({"path": "~/projects/app"}), "destroy"),
            ("power.sleep", json!({}), "power"),
            ("config.write", json!({}), "admin"),
        ] {
            let refused = failed(&mcp.call("legion_read", json!({"tool": tool, "args": args})));
            assert_eq!(refused["code"], json!("denied"), "{tool}: {refused}");
            assert_eq!(refused["machine"], json!("tower"), "{tool}");
            assert_eq!(refused["details"]["effect"], json!(effect), "{tool}");
            let hint = refused["hint"].as_str().unwrap();
            assert!(
                hint.contains("legion_call") && hint.contains(tool),
                "{hint}"
            );
        }
        let unknown = failed(&mcp.call("legion_read", json!({"tool": "fs.nothing"})));
        assert_eq!(unknown["code"], json!("unknown_tool"));
        assert_eq!(
            unknown["hint"],
            json!("legion_tools lists the tools you may use here.")
        );
        assert_eq!(mcp.finish().0, 0);
    }
    assert!(!written.exists());
    assert!(home.home().join("projects").join("app").exists());
    // A refusal is the door's, before anything reaches the node, so the node records nothing.
    let audit = home.audit();
    for refused in [
        "fs.write",
        "test.write_path",
        "exec.run",
        "fs.remove",
        "power.sleep",
        "config.write",
    ] {
        assert!(
            !audit.iter().any(|line| line["tool"] == json!(refused)),
            "{refused} reached the audit log"
        );
    }
    assert!(
        audit
            .iter()
            .any(|line| line["tool"] == json!("meta.whoami") && line["outcome"] == json!("ok"))
    );
}

#[test]
fn legion_read_is_held_to_the_policy_like_any_call() {
    let home = Home::new();
    home.write_config(
        "policy.toml",
        r#"
[principals."agent:obs"]
role = "observer"

[principals."agent:writer"]
role = "writer"

[roles.observer]
allow = ["effect:read"]
deny = ["meta.doctor"]

[roles.writer]
allow = ["fs.write"]
"#,
    );
    let (mut obs, _) = Mcp::initialized(&home, &["--compact", "--as", "agent:obs"]);
    assert_eq!(
        ok(&obs.call("legion_read", json!({"tool": "meta.whoami"})))["principal"],
        json!("agent:obs")
    );
    let denied = failed(&obs.call("legion_read", json!({"tool": "meta.doctor"})));
    assert_eq!(denied["code"], json!("denied"));
    assert!(
        denied["message"].as_str().unwrap().contains("agent:obs"),
        "{denied}"
    );
    assert_eq!(obs.finish().0, 0);

    // A read tool beyond the role is refused by the node, not run because it only reads.
    let (mut writer, _) = Mcp::initialized(&home, &["--compact", "--as", "agent:writer"]);
    let denied = failed(&writer.call("legion_read", json!({"tool": "meta.whoami"})));
    assert_eq!(denied["code"], json!("denied"));
    assert!(
        denied["message"].as_str().unwrap().contains("agent:writer"),
        "{denied}"
    );
    assert_eq!(writer.finish().0, 0);

    let audit = home.audit();
    for (principal, tool) in [
        ("agent:obs", "meta.doctor"),
        ("agent:writer", "meta.whoami"),
    ] {
        let line = audit
            .iter()
            .find(|line| line["principal"] == json!(principal) && line["tool"] == json!(tool))
            .unwrap_or_else(|| panic!("{principal} {tool} is not in the audit log"));
        assert_eq!(line["outcome"], json!("denied"));
        assert_eq!(line["door"], json!("mcp"));
    }
}

/// The notifications among `messages` with this method.
fn notifications<'a>(messages: &'a [Value], method: &str) -> Vec<&'a Value> {
    messages
        .iter()
        .filter(|message| message["method"] == json!(method))
        .collect()
}

#[test]
fn events_become_the_notifications_a_client_asks_for() {
    let home = Home::new();
    let (mut mcp, _) = Mcp::initialized(&home, &[]);

    // Asked for nothing: nothing but the result.
    let id = mcp.send_request(
        "tools/call",
        json!({"name": "test_sleep", "arguments": {"seconds": 0.2}}),
    );
    let (reply, before) = mcp.reply_to(&id);
    assert_eq!(ok(&reply["result"]), json!({"slept": 0.2}));
    assert_eq!(before, Vec::<Value>::new());

    // A progress token: progress notifications for that token, counting up, before the result.
    let id = mcp.send_request(
        "tools/call",
        json!({
            "_meta": {"progressToken": "sleep-1"},
            "name": "test_sleep",
            "arguments": {"seconds": 0.2}
        }),
    );
    let (reply, before) = mcp.reply_to(&id);
    ok(&reply["result"]);
    let progress = notifications(&before, "notifications/progress");
    assert!(!progress.is_empty(), "{before:?}");
    assert!(
        progress
            .iter()
            .all(|n| n["params"]["progressToken"] == json!("sleep-1"))
    );
    assert_eq!(progress[0]["params"]["progress"], json!(1.0));
    assert!(progress[0]["params"]["message"].is_string());
    assert!(notifications(&before, "notifications/message").is_empty());

    // A log level: log messages from the tool, named after it.
    assert_eq!(
        mcp.request("logging/setLevel", json!({"level": "debug"})),
        json!({})
    );
    let id = mcp.send_request(
        "tools/call",
        json!({"name": "test_sleep", "arguments": {"seconds": 0.2}}),
    );
    let (_, before) = mcp.reply_to(&id);
    let logged = notifications(&before, "notifications/message");
    assert!(!logged.is_empty(), "{before:?}");
    assert_eq!(logged[0]["params"]["logger"], json!("test.sleep"));
    assert_eq!(logged[0]["params"]["level"], json!("debug"));
    assert_eq!(logged[0]["params"]["data"]["kind"], json!("progress"));
    assert_eq!(mcp.finish().0, 0);

    // The handshake-less revision asks for a level in the request's own `_meta`.
    let mut mcp = Mcp::inline(&home, &[]);
    let id = mcp.send_request(
        "tools/call",
        json!({
            "_meta": {"io.modelcontextprotocol/logLevel": "debug"},
            "name": "test_sleep",
            "arguments": {"seconds": 0.2}
        }),
    );
    let (_, before) = mcp.reply_to(&id);
    assert!(!notifications(&before, "notifications/message").is_empty());
    let id = mcp.send_request(
        "tools/call",
        json!({
            "_meta": {"io.modelcontextprotocol/logLevel": "info"},
            "name": "test_sleep",
            "arguments": {"seconds": 0.2}
        }),
    );
    // Progress ticks are detail, below info.
    let (_, before) = mcp.reply_to(&id);
    assert!(notifications(&before, "notifications/message").is_empty());
    assert_eq!(mcp.finish().0, 0);
}

#[test]
fn a_cancelled_call_stops_and_is_recorded_as_cancelled() {
    let home = Home::new();
    for inline in [false, true] {
        let mut mcp = if inline {
            Mcp::inline(&home, &[])
        } else {
            Mcp::initialized(&home, &[]).0
        };
        let id = mcp.send_request(
            "tools/call",
            json!({
                "_meta": {"progressToken": "long"},
                "name": "test_sleep",
                "arguments": {"seconds": 60}
            }),
        );
        // Running once it reports progress.
        loop {
            if mcp.next()["method"] == json!("notifications/progress") {
                break;
            }
        }
        mcp.notify(
            "notifications/cancelled",
            json!({"requestId": id, "reason": "changed my mind"}),
        );
        // The session goes on, and a cancelled request gets no reply.
        let version = mcp.call("meta_version", json!({}));
        assert_eq!(ok(&version)["name"], json!("legion"));
        let started = Instant::now();
        let (code, rest) = mcp.finish();
        assert_eq!(code, 0);
        assert!(started.elapsed() < Duration::from_secs(10));
        assert!(
            rest.iter().all(|line| !line.contains("\"slept\"")),
            "{rest:?}"
        );
    }
    let audit = home.audit();
    let sleeps: Vec<&Value> = audit
        .iter()
        .filter(|line| line["tool"] == json!("test.sleep"))
        .collect();
    assert_eq!(sleeps.len(), 2, "{audit:?}");
    assert!(
        sleeps
            .iter()
            .all(|line| line["outcome"] == json!("cancelled"))
    );
}

#[test]
fn a_session_ending_mid_call_cancels_it_and_still_records_it() {
    let home = Home::new();
    let (mut mcp, _) = Mcp::initialized(&home, &[]);
    mcp.send_request(
        "tools/call",
        json!({"_meta": {"progressToken": 1}, "name": "test_sleep", "arguments": {"seconds": 60}}),
    );
    loop {
        if mcp.next()["method"] == json!("notifications/progress") {
            break;
        }
    }
    let (code, _) = mcp.finish();
    assert_eq!(code, 0);
    let audit = home.audit();
    let sleep = audit
        .iter()
        .find(|line| line["tool"] == json!("test.sleep"))
        .unwrap();
    assert_eq!(sleep["outcome"], json!("cancelled"));
}

#[test]
fn input_that_ends_at_once_ends_the_session_quietly() {
    let home = Home::new();
    let mut mcp = Mcp::start(&home, &[]);
    let (code, rest) = mcp.finish();
    assert_eq!(code, 0);
    assert_eq!(rest, Vec::<String>::new());

    let run = home.run(&["mcp", "--json"]);
    assert_eq!(run.code, 2);
    assert_eq!(run.stdout, "");
}

/// The names `legion_tools` lists.
fn compact_names(mcp: &mut Mcp) -> Vec<String> {
    ok(&mcp.call("legion_tools", json!({})))["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["name"].as_str().unwrap().to_owned())
        .collect()
}

#[test]
fn compact_mode_tells_a_role_nothing_about_tools_beyond_it() {
    let home = Home::new();
    home.write_config("node.toml", "name = \"tower\"\n");
    home.write_config(
        "policy.toml",
        r#"
[principals."agent:obs"]
role = "observer"

[roles.observer]
allow = ["effect:read"]
deny = ["meta.doctor"]
"#,
    );
    let (mut mcp, _) = Mcp::initialized(&home, &["--compact", "--as", "agent:obs"]);
    let names = compact_names(&mut mcp);
    assert!(names.contains(&"meta.version".to_owned()), "{names:?}");
    for hidden in ["exec.run", "meta.doctor", "test.write_path"] {
        assert!(!names.contains(&hidden.to_owned()), "{names:?}");
    }

    // A tool beyond the role is described exactly as one that does not exist.
    let missing = failed(&mcp.call("legion_describe", json!({"tool": "nope.nothing"})));
    assert_eq!(missing["code"], json!("unknown_tool"));
    for hidden in ["exec.run", "exec_run", "meta.doctor", "test.write_path"] {
        let error = failed(&mcp.call("legion_describe", json!({"tool": hidden})));
        let expected = missing["message"]
            .as_str()
            .unwrap()
            .replace("nope.nothing", hidden);
        assert_eq!(error["message"], json!(expected), "{hidden}");
        assert_eq!(
            (&error["code"], &error["hint"], &error["details"]),
            (&missing["code"], &missing["hint"], &missing["details"]),
            "{hidden}"
        );
    }
    assert_eq!(
        ok(&mcp.call("legion_describe", json!({"tool": "meta.version"})))["name"],
        json!("meta.version")
    );

    // So is a full-mode name: only a tool the role may use is named in the hint.
    let unknown = failed(&mcp.call("nope_nothing", json!({})));
    let beyond = failed(&mcp.call("exec_run", json!({})));
    assert_eq!(beyond["hint"], unknown["hint"]);
    assert!(!beyond.to_string().contains("exec.run"), "{beyond}");
    let allowed = failed(&mcp.call("meta_version", json!({})));
    assert!(
        allowed["hint"]
            .as_str()
            .unwrap()
            .contains("\"meta.version\""),
        "{allowed}"
    );
    assert_eq!(mcp.finish().0, 0);

    // A principal the policy does not name may use nothing, and is shown nothing.
    let (mut stranger, _) = Mcp::initialized(&home, &["--compact", "--as", "stranger"]);
    assert_eq!(compact_names(&mut stranger), Vec::<String>::new());
    let error = failed(&stranger.call("legion_describe", json!({"tool": "meta.version"})));
    assert_eq!(error["code"], json!("unknown_tool"));
    assert_eq!(error["hint"], missing["hint"]);
    assert_eq!(stranger.finish().0, 0);
}

#[test]
fn a_policy_that_does_not_parse_shows_nothing_and_every_call_says_why() {
    let home = Home::new();
    home.write_config("policy.toml", "[roles.x]\nallow = [\"effect:raed\"]\n");

    let (mut full, _) = Mcp::initialized(&home, &[]);
    assert_eq!(full.tool_names(), Vec::<String>::new());
    let error = failed(&full.call("meta_version", json!({})));
    assert!(
        error["message"].as_str().unwrap().contains("policy.toml"),
        "{error}"
    );
    assert_eq!(full.finish().0, 0);

    let (mut compact, _) = Mcp::initialized(&home, &["--compact"]);
    for (tool, args) in [
        ("legion_tools", json!({})),
        ("legion_describe", json!({"tool": "meta.version"})),
        ("legion_read", json!({"tool": "meta.version"})),
        ("legion_call", json!({"tool": "meta.version"})),
    ] {
        let failure = failed(&compact.call(tool, args));
        assert_eq!(failure["code"], error["code"], "{tool}: {failure}");
        assert_eq!(failure["message"], error["message"], "{tool}");
    }
    assert_eq!(compact.finish().0, 0);
}

#[test]
fn a_result_is_sent_once_as_compact_json() {
    let home = Home::new();
    let (mut mcp, _) = Mcp::initialized(&home, &[]);
    let text = "x".repeat(200_000);
    let echo = mcp.call("test_echo", json!({"text": text}));
    assert_eq!(ok(&echo), json!({"text": text}));
    // The reply is the payload once, plus the envelope around it.
    let line = echo.to_string();
    assert!(line.len() < text.len() + 200, "{} bytes", line.len());
    assert_eq!(
        echo["content"][0]["text"],
        json!(json!({"text": text}).to_string())
    );

    let error = mcp.call("test_sleep", json!({"seconds": "long"}));
    assert_eq!(failed(&error)["code"], json!("invalid_args"));
    assert_eq!(mcp.finish().0, 0);
}

#[test]
fn hints_point_at_what_an_mcp_client_has() {
    let home = Home::new();
    home.write_config("node.toml", "name = \"tower\"\n");

    let (mut full, _) = Mcp::initialized(&home, &[]);
    let invalid = failed(&full.call("test_sleep", json!({"seconds": "long"})));
    let hint = invalid["hint"].as_str().unwrap();
    assert!(!hint.contains("legion help"), "{hint}");
    assert!(
        hint.contains("test_sleep") && hint.contains("tools/list"),
        "{hint}"
    );
    let unknown = failed(&full.call("fs_nothing", json!({})));
    assert!(
        !unknown["hint"].as_str().unwrap().contains("legion tools"),
        "{unknown}"
    );
    assert_eq!(full.finish().0, 0);

    let (mut compact, _) = Mcp::initialized(&home, &["--compact"]);
    let invalid = failed(&compact.call(
        "legion_call",
        json!({"tool": "test.sleep", "args": {"seconds": "long"}}),
    ));
    let hint = invalid["hint"].as_str().unwrap();
    assert!(!hint.contains("legion help"), "{hint}");
    assert!(
        hint.contains("legion_describe") && hint.contains("\"test.sleep\""),
        "{hint}"
    );
    assert_eq!(compact.finish().0, 0);

    // A principal that may use nothing is told so, not shown an empty list of families.
    let (mut stranger, _) = Mcp::initialized(&home, &["--compact", "--as", "stranger"]);
    let none = failed(&stranger.call("legion_tools", json!({"family": "fs"})));
    assert_eq!(none["code"], json!("denied"));
    assert_eq!(
        none["message"],
        json!("stranger may use no tools on tower.")
    );
    assert!(
        none["hint"].as_str().unwrap().contains("policy.toml"),
        "{none}"
    );
    assert_eq!(stranger.finish().0, 0);
}
