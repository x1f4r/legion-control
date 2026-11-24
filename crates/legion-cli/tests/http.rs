//! `legion serve` end to end: the built binary on an ephemeral loopback port under a temporary
//! `LEGION_HOME`, spoken to in plain HTTP/1.1 over a socket, so what is tested is what a phone or
//! a script would see.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::PathBuf;
use std::process::{Child, Command, ExitCode, Stdio};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use common::{Home, PATIENCE};
use serde_json::{Value, json};

const LEGION_BIN: &str = env!("CARGO_BIN_EXE_legion");
const HELPER_FLAG: &str = "--http-helper";

const POLICY: &str = "\
[principals.phone]
role = \"observer\"

[principals.admin]
role = \"admin\"

[roles.observer]
allow = [\"effect:read\"]

[roles.admin]
allow = [\"*\"]
delegate = true
";

type Test = (&'static str, fn());

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some(HELPER_FLAG) {
        return helper(&args[1..]);
    }
    let filter = args
        .iter()
        .find(|arg| !arg.starts_with('-'))
        .map(String::as_str);
    let tests: &[Test] = &[
        ("health_needs_no_token", health_needs_no_token),
        (
            "every_other_path_needs_a_good_token",
            every_other_path_needs_a_good_token,
        ),
        (
            "the_token_principal_acts_and_is_audited",
            the_token_principal_acts_and_is_audited,
        ),
        ("tools_are_filtered_by_role", tools_are_filtered_by_role),
        (
            "every_error_code_has_its_status",
            every_error_code_has_its_status,
        ),
        ("fan_out_shapes", fan_out_shapes),
        (
            "an_event_stream_sends_events_then_the_end",
            an_event_stream_sends_events_then_the_end,
        ),
        (
            "a_client_that_goes_away_cancels_its_call",
            a_client_that_goes_away_cancels_its_call,
        ),
        ("origin_and_host_are_checked", origin_and_host_are_checked),
        ("only_loopback_is_listened_on", only_loopback_is_listened_on),
        ("a_body_too_large_is_refused", a_body_too_large_is_refused),
        (
            "mcp_over_http_in_both_revisions",
            mcp_over_http_in_both_revisions,
        ),
        (
            "the_token_never_reaches_a_log",
            the_token_never_reaches_a_log,
        ),
        (
            "token_tools_guard_principals_and_keep_comments",
            token_tools_guard_principals_and_keep_comments,
        ),
        (
            "shutting_down_cancels_and_records_calls",
            shutting_down_cancels_and_records_calls,
        ),
    ];
    let mut failed = Vec::new();
    let mut ran = 0;
    for (name, test) in tests {
        if filter.is_some_and(|wanted| !name.contains(wanted)) {
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

/// `--http-helper rpc <legion_home> <user_home>`: `legion rpc` for another node, which the
/// `command` connector starts with this node's environment.
fn helper(args: &[String]) -> ExitCode {
    let [mode, legion_home, user_home] = args else {
        eprintln!("the helper takes rpc, a LEGION_HOME and a home");
        return ExitCode::from(2);
    };
    assert_eq!(mode, "rpc");
    let status = Command::new(LEGION_BIN)
        .arg("rpc")
        .env("LEGION_HOME", legion_home)
        .env("HOME", user_home)
        .env("USERPROFILE", user_home)
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("XDG_STATE_HOME")
        .status()
        .unwrap();
    ExitCode::from(status.code().unwrap_or(1).clamp(0, 255) as u8)
}

/// A home with the test policy and a token for each of its principals.
struct Fixture {
    home: Home,
    phone: String,
    admin: String,
}

impl Fixture {
    fn new() -> Fixture {
        let home = Home::new();
        home.write_config("node.toml", "name = \"laptop\"\n");
        home.write_config("policy.toml", POLICY);
        let phone = make_token(&home, &["phone", "phone"]);
        let admin = make_token(&home, &["admin", "admin"]);
        Fixture { home, phone, admin }
    }
}

fn make_token(home: &Home, words: &[&str]) -> String {
    let mut line = vec!["token", "add"];
    line.extend_from_slice(words);
    line.push("--json");
    let made = home.run(&line);
    assert_eq!(made.code, 0, "{}{}", made.stdout, made.stderr);
    made.json()["token"].as_str().unwrap().to_owned()
}

/// A running `legion serve`.
struct Server {
    child: Child,
    port: u16,
    stderr: Arc<Mutex<String>>,
}

impl Server {
    fn start(home: &Home) -> Server {
        let mut child = home
            .command(&["serve", "--listen", "127.0.0.1:0"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let stderr = child.stderr.take().unwrap();
        let seen = Arc::new(Mutex::new(String::new()));
        let (ports, port) = mpsc::channel();
        let collected = Arc::clone(&seen);
        std::thread::spawn(move || {
            for line in BufReader::new(stderr).lines() {
                let Ok(line) = line else { break };
                if let Some(address) = line.strip_prefix("legion serve: listening on http://") {
                    let port = address.rsplit(':').next().unwrap().parse::<u16>().unwrap();
                    let _ = ports.send(port);
                }
                let mut all = collected.lock().unwrap();
                all.push_str(&line);
                all.push('\n');
            }
        });
        let port = port
            .recv_timeout(PATIENCE)
            .expect("legion serve did not say where it listens");
        Server {
            child,
            port,
            stderr: seen,
        }
    }

    fn stderr(&self) -> String {
        self.stderr.lock().unwrap().clone()
    }

    /// Asks the server to stop as a service manager would, and returns its exit code once it has.
    fn stop(&mut self) -> i32 {
        #[cfg(unix)]
        {
            Command::new("kill")
                .args(["-TERM", &self.child.id().to_string()])
                .status()
                .unwrap();
        }
        #[cfg(not(unix))]
        {
            let _ = self.child.kill();
        }
        let deadline = Instant::now() + PATIENCE;
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                // Let the reader thread take the last lines.
                std::thread::sleep(Duration::from_millis(100));
                return status.code().unwrap_or(-1);
            }
            assert!(Instant::now() < deadline, "legion serve did not stop");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn get(&self, path: &str, token: Option<&str>) -> Reply {
        http(self.port, "GET", path, &auth(token), b"")
    }

    fn post(&self, path: &str, token: Option<&str>, body: &Value) -> Reply {
        let mut headers = auth(token);
        headers.push(("Content-Type", "application/json".to_owned()));
        http(
            self.port,
            "POST",
            path,
            &headers,
            body.to_string().as_bytes(),
        )
    }

    fn call(&self, token: &str, body: Value) -> Reply {
        self.post("/v1/call", Some(token), &body)
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn auth(token: Option<&str>) -> Vec<(&'static str, String)> {
    token
        .map(|token| vec![("Authorization", format!("Bearer {token}"))])
        .unwrap_or_default()
}

/// One response.
struct Reply {
    status: u16,
    headers: HashMap<String, String>,
    body: Vec<u8>,
}

impl Reply {
    fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap_or_else(|error| {
            panic!(
                "{} is not JSON ({error}): {}",
                self.status,
                String::from_utf8_lossy(&self.body)
            )
        })
    }

    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .get(&name.to_ascii_lowercase())
            .map(String::as_str)
    }

    fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }

    /// The JSON-RPC messages in an MCP reply, whether it came as JSON or as an event stream.
    fn messages(&self) -> Vec<Value> {
        if self
            .header("content-type")
            .is_some_and(|kind| kind.starts_with("application/json"))
        {
            return vec![self.json()];
        }
        self.text()
            .lines()
            .filter_map(|line| line.strip_prefix("data:"))
            .filter_map(|data| serde_json::from_str::<Value>(data.trim()).ok())
            .collect()
    }
}

/// Sends a request and leaves the connection open for its response.
fn send(port: u16, method: &str, path: &str, headers: &[(&str, String)], body: &[u8]) -> TcpStream {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
    stream.set_read_timeout(Some(PATIENCE)).unwrap();
    let mut head = format!("{method} {path} HTTP/1.1\r\nConnection: close\r\n");
    if !headers
        .iter()
        .any(|(name, _)| name.eq_ignore_ascii_case("host"))
    {
        head.push_str(&format!("Host: 127.0.0.1:{port}\r\n"));
    }
    for (name, value) in headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    head.push_str(&format!("Content-Length: {}\r\n\r\n", body.len()));
    stream.write_all(head.as_bytes()).unwrap();
    // A server that refuses early may close before taking all of a large body.
    let _ = stream.write_all(body);
    let _ = stream.flush();
    stream
}

fn http(port: u16, method: &str, path: &str, headers: &[(&str, String)], body: &[u8]) -> Reply {
    let stream = send(port, method, path, headers, body);
    let mut reader = BufReader::new(stream);
    let (status, headers) = read_head(&mut reader);
    let mut rest = Vec::new();
    let _ = reader.read_to_end(&mut rest);
    let body = if headers
        .get("transfer-encoding")
        .is_some_and(|coding| coding.contains("chunked"))
    {
        dechunk(&rest)
    } else {
        rest
    };
    Reply {
        status,
        headers,
        body,
    }
}

fn read_head(reader: &mut BufReader<TcpStream>) -> (u16, HashMap<String, String>) {
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    let status = line
        .split_whitespace()
        .nth(1)
        .unwrap_or_else(|| panic!("{line:?} is not a status line"))
        .parse()
        .unwrap();
    let mut headers = HashMap::new();
    loop {
        line.clear();
        reader.read_line(&mut line).unwrap();
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        let (name, value) = line.split_once(':').unwrap();
        headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_owned());
    }
    (status, headers)
}

fn dechunk(mut bytes: &[u8]) -> Vec<u8> {
    let mut body = Vec::new();
    loop {
        let Some(end) = bytes.windows(2).position(|pair| pair == b"\r\n") else {
            return body;
        };
        let size = usize::from_str_radix(
            std::str::from_utf8(&bytes[..end])
                .unwrap()
                .split(';')
                .next()
                .unwrap()
                .trim(),
            16,
        )
        .unwrap();
        if size == 0 {
            return body;
        }
        let start = end + 2;
        body.extend_from_slice(&bytes[start..start + size]);
        bytes = &bytes[start + size + 2..];
    }
}

/// Server-sent events, read as they arrive.
struct Events {
    reader: BufReader<TcpStream>,
    status: u16,
    headers: HashMap<String, String>,
    buffered: String,
}

impl Events {
    fn open(port: u16, token: &str, body: &Value) -> Events {
        let stream = send(
            port,
            "POST",
            "/v1/call",
            &[
                ("Authorization", format!("Bearer {token}")),
                ("Accept", "text/event-stream".to_owned()),
                ("Content-Type", "application/json".to_owned()),
            ],
            body.to_string().as_bytes(),
        );
        let mut reader = BufReader::new(stream);
        let (status, headers) = read_head(&mut reader);
        Events {
            reader,
            status,
            headers,
            buffered: String::new(),
        }
    }

    /// The next event's name and data, or `None` once the stream has ended.
    fn next(&mut self) -> Option<(String, Value)> {
        loop {
            if let Some(end) = self.buffered.find("\n\n") {
                let event: String = self.buffered.drain(..end + 2).collect();
                let mut name = String::new();
                let mut data = String::new();
                for line in event.lines() {
                    if let Some(value) = line.strip_prefix("event:") {
                        name = value.trim().to_owned();
                    } else if let Some(value) = line.strip_prefix("data:") {
                        data.push_str(value.trim());
                    }
                }
                return Some((name, serde_json::from_str(&data).unwrap()));
            }
            let mut size = String::new();
            if self.reader.read_line(&mut size).ok()? == 0 {
                return None;
            }
            let size = usize::from_str_radix(size.trim(), 16).unwrap();
            if size == 0 {
                return None;
            }
            let mut chunk = vec![0; size + 2];
            self.reader.read_exact(&mut chunk).ok()?;
            self.buffered
                .push_str(std::str::from_utf8(&chunk[..size]).unwrap());
        }
    }
}

/// Waits for the audit log to hold a line that `wanted` accepts, and returns it.
fn audited(home: &Home, wanted: impl Fn(&Value) -> bool) -> Value {
    let deadline = Instant::now() + PATIENCE;
    loop {
        if let Some(line) = home.audit().into_iter().find(|line| wanted(line)) {
            return line;
        }
        assert!(
            Instant::now() < deadline,
            "no such audit line in {:?}",
            home.audit()
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn health_needs_no_token() {
    let fixture = Fixture::new();
    let server = Server::start(&fixture.home);
    let health = server.get("/v1/health", None);
    assert_eq!(health.status, 200);
    assert_eq!(
        health.json(),
        json!({"ok": true, "node": "laptop", "version": env!("CARGO_PKG_VERSION")})
    );
}

fn every_other_path_needs_a_good_token() {
    let fixture = Fixture::new();
    let expired = "lgn_expired-for-a-year-already";
    let hash = legion_core::tokens::hash(expired);
    let file = fixture
        .home
        .state()
        .parent()
        .unwrap()
        .join("config")
        .join("tokens.toml");
    let mut tokens = std::fs::read_to_string(&file).unwrap();
    tokens.push_str(&format!(
        "\n[tokens.old]\nprincipal = \"phone\"\nhash = \"{hash}\"\ncreated = \"2024-01-01T00:00:00Z\"\nexpires = \"2025-01-01T00:00:00Z\"\n"
    ));
    fixture.home.write_config("tokens.toml", &tokens);
    let server = Server::start(&fixture.home);
    let wrong = format!("{}x", fixture.phone);
    let mut messages = Vec::new();
    for (method, path) in [
        ("GET", "/v1/tools"),
        ("POST", "/v1/call"),
        ("POST", "/mcp"),
        ("GET", "/nothing/here"),
    ] {
        for (token, header) in [
            (None, None),
            (Some(wrong.as_str()), None),
            (Some(expired), None),
            (None, Some(format!("Basic {}", fixture.phone))),
            (None, Some(fixture.phone.clone())),
        ] {
            let mut headers = auth(token);
            if let Some(header) = header {
                headers.push(("Authorization", header));
            }
            let started = Instant::now();
            let reply = http(server.port, method, path, &headers, b"{}");
            assert_eq!(reply.status, 401, "{method} {path} {headers:?}");
            assert!(started.elapsed() >= Duration::from_millis(150));
            assert!(
                reply
                    .header("www-authenticate")
                    .is_some_and(|challenge| challenge.starts_with("Bearer")),
                "{:?}",
                reply.headers
            );
            let body = reply.json();
            assert_eq!(body["code"], json!("denied"));
            if token.is_some() {
                messages.push(body["message"].clone());
            }
        }
    }
    // A wrong token and an expired one are refused in the same words.
    messages.dedup();
    assert_eq!(messages, [json!("The bearer token is invalid or expired.")]);
    assert_eq!(server.get("/v1/tools", Some(&fixture.phone)).status, 200);
    assert_eq!(
        server.get("/nothing/here", Some(&fixture.phone)).status,
        404
    );
}

fn the_token_principal_acts_and_is_audited() {
    let fixture = Fixture::new();
    let server = Server::start(&fixture.home);
    let whoami = server.call(
        &fixture.phone,
        json!({"tool": "meta.whoami", "args": {}, "as": "admin"}),
    );
    assert_eq!(whoami.status, 200, "{}", whoami.text());
    let whoami = whoami.json();
    assert_eq!(whoami["principal"], json!("phone"));
    assert_eq!(whoami["door"], json!("http"));
    let line = audited(&fixture.home, |line| line["tool"] == json!("meta.whoami"));
    assert_eq!(line["principal"], json!("phone"));
    assert_eq!(line["door"], json!("http"));

    // A role that delegates may act as someone else, as on every other door.
    let delegated = server
        .call(
            &fixture.admin,
            json!({"tool": "meta.whoami", "as": "phone"}),
        )
        .json();
    assert_eq!(delegated["principal"], json!("phone"));
    let as_self = server
        .call(&fixture.admin, json!({"tool": "meta.whoami"}))
        .json();
    assert_eq!(as_self["principal"], json!("admin"));
}

fn tools_are_filtered_by_role() {
    let fixture = Fixture::new();
    let server = Server::start(&fixture.home);
    let names = |token: &str| -> Vec<String> {
        let reply = server.get("/v1/tools", Some(token));
        assert_eq!(reply.status, 200);
        reply.json()["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|tool| tool["name"].as_str().unwrap().to_owned())
            .collect()
    };
    let phone = names(&fixture.phone);
    assert!(phone.contains(&"meta.whoami".to_owned()));
    assert!(!phone.contains(&"exec.run".to_owned()), "{phone:?}");
    assert!(!phone.contains(&"token.add".to_owned()), "{phone:?}");
    let admin = names(&fixture.admin);
    assert!(admin.contains(&"exec.run".to_owned()));
    assert!(admin.contains(&"token.add".to_owned()));

    // A policy that does not parse lists nothing.
    fixture
        .home
        .write_config("policy.toml", "[roles.observer\n");
    let broken = server.get("/v1/tools", Some(&fixture.phone));
    assert_eq!(broken.status, 500);
    assert_eq!(broken.json()["code"], json!("failed"));
}

fn every_error_code_has_its_status() {
    let fixture = Fixture::new();
    let server = Server::start(&fixture.home);
    let ok = server.call(
        &fixture.phone,
        json!({"tool": "test.echo", "args": {"a": 1}}),
    );
    assert_eq!(ok.status, 200);
    assert_eq!(ok.json(), json!({"a": 1}));
    assert_eq!(ok.header("content-type"), Some("application/json"));
    for (code, status) in [
        ("invalid_args", 400),
        ("denied", 403),
        ("not_found", 404),
        ("unknown_tool", 404),
        ("exists", 409),
        ("conflict", 409),
        ("busy", 423),
        ("unsupported", 501),
        ("unreachable", 502),
        ("timeout", 504),
        ("cancelled", 499),
        ("failed", 500),
        ("internal", 500),
    ] {
        let reply = server.call(
            &fixture.phone,
            json!({"tool": "test.fail", "args": {"code": code}}),
        );
        assert_eq!(reply.status, status, "{code}");
        let body = reply.json();
        assert_eq!(body["code"], json!(code));
        assert_eq!(body["machine"], json!("laptop"));
    }
    // The same codes from the node itself.
    let unknown = server.call(&fixture.phone, json!({"tool": "fs.frobnicate"}));
    assert_eq!(unknown.status, 404);
    assert_eq!(unknown.json()["code"], json!("unknown_tool"));
    assert_eq!(
        unknown.json()["hint"],
        json!("GET /v1/tools lists the tools you may use here.")
    );
    let denied = server.call(
        &fixture.phone,
        json!({"tool": "exec.run", "args": {"argv": ["x"]}}),
    );
    assert_eq!(denied.status, 403);
    let invalid = server.call(
        &fixture.phone,
        json!({"tool": "test.sleep", "args": {"secs": 1}}),
    );
    assert_eq!(invalid.status, 400);
    assert_eq!(invalid.json()["code"], json!("invalid_args"));
    for body in [
        "not json",
        "{\"args\": {}}",
        "{\"tool\": \"meta.whoami\", \"args\": [1]}",
        "{\"tool\": \"meta.whoami\", \"extra\": 1}",
        "{\"tool\": \"meta.whoami\", \"on\": 5}",
    ] {
        let reply = http(
            server.port,
            "POST",
            "/v1/call",
            &auth(Some(&fixture.phone)),
            body.as_bytes(),
        );
        assert_eq!(reply.status, 400, "{body}");
        assert_eq!(reply.json()["code"], json!("invalid_args"), "{body}");
    }
}

fn fan_out_shapes() {
    let fixture = Fixture::new();
    let pi = Home::new();
    pi.write_config("node.toml", "name = \"pi\"\n");
    // The target decides by its own policy, which must know the principal too.
    pi.write_config("policy.toml", POLICY);
    let pi_home = pi.state().parent().unwrap().to_path_buf();
    let helper = vec![
        std::env::current_exe()
            .unwrap()
            .to_string_lossy()
            .into_owned(),
        HELPER_FLAG.to_owned(),
        "rpc".to_owned(),
        pi_home.to_string_lossy().into_owned(),
        pi.home().to_string_lossy().into_owned(),
    ];
    let nowhere: PathBuf = fixture.home.home().join("no-such-program");
    fixture.home.write_config(
        "fleet.toml",
        &format!(
            "[machines.laptop]\nssh = \"laptop.example\"\n\n[machines.pi]\ncommand = {}\n\n[machines.tower]\ncommand = [{}]\n",
            serde_json::to_string(&helper).unwrap(),
            serde_json::to_string(&nowhere.to_string_lossy()).unwrap()
        ),
    );
    let server = Server::start(&fixture.home);
    let all = server.call(&fixture.admin, json!({"tool": "meta.version", "on": "all"}));
    assert_eq!(all.status, 200, "{}", all.text());
    let results = &all.json()["results"];
    assert_eq!(results["laptop"]["ok"]["node"], json!("laptop"));
    assert_eq!(results["pi"]["ok"]["node"], json!("pi"));
    assert_eq!(results["tower"]["error"]["code"], json!("unreachable"));

    let one = server.call(&fixture.admin, json!({"tool": "meta.version", "on": "pi"}));
    assert_eq!(one.status, 200);
    assert_eq!(one.json()["node"], json!("pi"));

    let failed = server.call(
        &fixture.admin,
        json!({"tool": "meta.version", "on": ["tower", "ghost"]}),
    );
    assert_eq!(failed.status, 502, "{}", failed.text());
    let results = &failed.json()["results"];
    assert_eq!(results["tower"]["error"]["code"], json!("unreachable"));
    assert_eq!(results["ghost"]["error"]["code"], json!("not_found"));

    let unknown = server.call(
        &fixture.admin,
        json!({"tool": "meta.version", "on": "ghost"}),
    );
    assert_eq!(unknown.status, 404);
}

fn an_event_stream_sends_events_then_the_end() {
    let fixture = Fixture::new();
    let server = Server::start(&fixture.home);
    let mut events = Events::open(
        server.port,
        &fixture.phone,
        &json!({"tool": "test.sleep", "args": {"seconds": 0.3}}),
    );
    assert_eq!(events.status, 200);
    assert_eq!(
        events.headers.get("content-type").map(String::as_str),
        Some("text/event-stream")
    );
    let mut seen = Vec::new();
    while let Some(event) = events.next() {
        seen.push(event);
    }
    let (last, rest) = seen.split_last().unwrap();
    assert_eq!(last.0, "result");
    assert_eq!(last.1, json!({"slept": 0.3}));
    assert!(rest.len() >= 3, "{rest:?}");
    for (name, data) in rest {
        assert_eq!(name, "event");
        assert_eq!(data["kind"], json!("progress"));
        assert!(data["data"]["waited"].is_number(), "{data}");
    }

    let mut failing = Events::open(
        server.port,
        &fixture.phone,
        &json!({"tool": "test.fail", "args": {"code": "busy"}}),
    );
    let (name, data) = failing.next().unwrap();
    assert_eq!(name, "error");
    assert_eq!(data["code"], json!("busy"));
    assert!(failing.next().is_none());
}

fn a_client_that_goes_away_cancels_its_call() {
    let fixture = Fixture::new();
    let server = Server::start(&fixture.home);
    for streaming in [false, true] {
        let started = Instant::now();
        if streaming {
            let mut events = Events::open(
                server.port,
                &fixture.phone,
                &json!({"tool": "test.sleep", "args": {"seconds": 60}}),
            );
            assert_eq!(events.next().unwrap().0, "event");
            drop(events);
        } else {
            let stream = send(
                server.port,
                "POST",
                "/v1/call",
                &auth(Some(&fixture.phone)),
                json!({"tool": "test.sleep", "args": {"seconds": 60}})
                    .to_string()
                    .as_bytes(),
            );
            std::thread::sleep(Duration::from_millis(300));
            drop(stream);
        }
        let line = audited(&fixture.home, |line| {
            line["tool"] == json!("test.sleep") && line["outcome"] == json!("cancelled")
        });
        assert_eq!(line["door"], json!("http"));
        assert!(
            started.elapsed() < Duration::from_secs(20),
            "streaming: {streaming}"
        );
        std::fs::remove_file(fixture.home.state().join("audit.jsonl")).unwrap();
    }
}

fn origin_and_host_are_checked() {
    let fixture = Fixture::new();
    let server = Server::start(&fixture.home);
    let port = server.port;
    let get = |headers: &[(&str, String)]| {
        let mut all = auth(Some(&fixture.phone));
        all.extend_from_slice(headers);
        http(port, "GET", "/v1/tools", &all, b"")
    };
    assert_eq!(get(&[]).status, 200);
    let origin = get(&[("Origin", "https://app.example.net".to_owned())]);
    assert_eq!(origin.status, 403);
    assert_eq!(origin.json()["code"], json!("denied"));
    // Without a token too: a page learns nothing.
    let health = http(
        port,
        "GET",
        "/v1/health",
        &[("Origin", "https://app.example.net".to_owned())],
        b"",
    );
    assert_eq!(health.status, 403);
    for host in [
        "evil.example".to_owned(),
        format!("evil.example:{port}"),
        "127.0.0.1".to_owned(),
        format!("127.0.0.1:{}", port.wrapping_add(1)),
        "tower.example.ts.net".to_owned(),
    ] {
        let reply = get(&[("Host", host.clone())]);
        assert_eq!(reply.status, 421, "{host}");
    }
    assert_eq!(get(&[("Host", format!("localhost:{port}"))]).status, 200);

    fixture.home.write_config(
        "node.toml",
        "name = \"laptop\"\n\n[serve]\norigins = [\"https://app.example.net\"]\nhosts = [\"tower.example.ts.net\"]\n",
    );
    assert_eq!(
        get(&[("Origin", "https://app.example.net".to_owned())]).status,
        200
    );
    assert_eq!(
        get(&[("Origin", "https://evil.example".to_owned())]).status,
        403
    );
    assert_eq!(
        get(&[("Host", "tower.example.ts.net".to_owned())]).status,
        200
    );
    assert_eq!(get(&[("Host", "evil.example".to_owned())]).status, 421);
}

fn only_loopback_is_listened_on() {
    let home = Home::new();
    for wide in ["0.0.0.0:0", "[::]:0", "192.0.2.1:7117"] {
        let refused = home.run(&["serve", "--listen", wide]);
        assert_eq!(refused.code, 1, "{wide}");
        assert!(
            refused
                .stderr
                .contains("tailscale serve --bg --https=8443 http://127.0.0.1:"),
            "{}",
            refused.stderr
        );
    }
    let help = home.run(&["serve", "--help"]);
    assert_eq!(help.code, 0);
    assert!(help.stdout.contains("loopback"), "{}", help.stdout);
    assert!(
        help.stdout
            .contains("tailscale serve --bg --https=8443 http://127.0.0.1:7117"),
        "{}",
        help.stdout
    );
}

fn a_body_too_large_is_refused() {
    let fixture = Fixture::new();
    let server = Server::start(&fixture.home);
    for path in ["/v1/call", "/mcp"] {
        // Only the headers: a server that refuses a body it has not read closes with the body
        // unread, which some systems turn into a reset before the client reads the reply.
        let head = format!(
            "POST {path} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nAuthorization: Bearer {}\r\nAccept: application/json, text/event-stream\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            server.port,
            fixture.phone,
            5 * 1024 * 1024
        );
        let mut stream = TcpStream::connect(("127.0.0.1", server.port)).unwrap();
        stream.set_read_timeout(Some(PATIENCE)).unwrap();
        stream.write_all(head.as_bytes()).unwrap();
        let (status, _) = read_head(&mut BufReader::new(stream));
        assert_eq!(status, 413, "{path}");
    }
    let fits = format!(
        "{{\"tool\": \"test.echo\", \"args\": {{\"x\": \"{}\"}}}}",
        "a".repeat(1024 * 1024)
    );
    let reply = http(
        server.port,
        "POST",
        "/v1/call",
        &auth(Some(&fixture.phone)),
        fits.as_bytes(),
    );
    assert_eq!(reply.status, 200);
}

/// An MCP client over HTTP.
struct Mcp<'a> {
    server: &'a Server,
    token: &'a str,
    path: &'a str,
    session: Option<String>,
    /// The revision without sessions, whose requests carry their own `_meta` and headers.
    inline: bool,
    next_id: u64,
}

impl Mcp<'_> {
    fn headers(&self, method: &str, name: Option<&str>) -> Vec<(&'static str, String)> {
        let mut headers = auth(Some(self.token));
        headers.push(("Accept", "application/json, text/event-stream".to_owned()));
        headers.push(("Content-Type", "application/json".to_owned()));
        if let Some(session) = &self.session {
            headers.push(("Mcp-Session-Id", session.clone()));
            headers.push(("MCP-Protocol-Version", "2025-11-25".to_owned()));
        }
        if self.inline {
            headers.push(("MCP-Protocol-Version", "2026-07-28".to_owned()));
            headers.push(("Mcp-Method", method.to_owned()));
            if let Some(name) = name {
                headers.push(("Mcp-Name", name.to_owned()));
            }
        }
        headers
    }

    fn post(&mut self, message: &Value, method: &str, name: Option<&str>) -> Reply {
        let headers = self.headers(method, name);
        http(
            self.server.port,
            "POST",
            self.path,
            &headers,
            message.to_string().as_bytes(),
        )
    }

    fn request(&mut self, method: &str, mut params: Value) -> Value {
        self.next_id += 1;
        if self.inline {
            params.as_object_mut().unwrap().insert(
                "_meta".to_owned(),
                json!({
                    "io.modelcontextprotocol/protocolVersion": "2026-07-28",
                    "io.modelcontextprotocol/clientCapabilities": {},
                    "io.modelcontextprotocol/clientInfo": {"name": "test", "version": "0"}
                }),
            );
        }
        let name = params["name"].as_str().map(str::to_owned);
        let message =
            json!({"jsonrpc": "2.0", "id": self.next_id, "method": method, "params": params});
        let reply = self.post(&message, method, name.as_deref());
        assert_eq!(reply.status, 200, "{method}: {}", reply.text());
        if method == "initialize" {
            self.session = reply.header("mcp-session-id").map(str::to_owned);
        }
        reply
            .messages()
            .into_iter()
            .find(|message| message["id"] == json!(self.next_id))
            .unwrap_or_else(|| panic!("no reply to {method} in {}", reply.text()))
    }

    fn tool_names(&mut self) -> Vec<String> {
        self.request("tools/list", json!({}))["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|tool| tool["name"].as_str().unwrap().to_owned())
            .collect()
    }

    fn call(&mut self, tool: &str, arguments: Value) -> Value {
        self.request("tools/call", json!({"name": tool, "arguments": arguments}))["result"].clone()
    }
}

fn mcp_over_http_in_both_revisions() {
    let fixture = Fixture::new();
    let server = Server::start(&fixture.home);

    // The revision with a handshake and a session.
    let mut mcp = Mcp {
        server: &server,
        token: &fixture.phone,
        path: "/mcp",
        session: None,
        inline: false,
        next_id: 0,
    };
    let init = mcp.request(
        "initialize",
        json!({
            "protocolVersion": "2025-11-25",
            "capabilities": {},
            "clientInfo": {"name": "test", "version": "0"}
        }),
    );
    assert_eq!(init["result"]["protocolVersion"], json!("2025-11-25"));
    assert!(mcp.session.is_some(), "no session header");
    let initialized = mcp.post(
        &json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
        "notifications/initialized",
        None,
    );
    assert_eq!(initialized.status, 202, "{}", initialized.text());
    let names = mcp.tool_names();
    assert!(names.contains(&"meta_whoami".to_owned()));
    assert!(!names.contains(&"exec_run".to_owned()), "{names:?}");
    let whoami = mcp.call("meta_whoami", json!({}));
    assert_eq!(whoami["isError"], json!(false));
    let text: Value = serde_json::from_str(whoami["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(text["principal"], json!("phone"));
    assert_eq!(text["door"], json!("http"));
    let failed = mcp.call("test_fail", json!({"code": "busy"}));
    assert_eq!(failed["isError"], json!(true));
    let error: Value =
        serde_json::from_str(failed["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(error["code"], json!("busy"));
    let denied = mcp.call("exec_run", json!({"argv": ["x"]}));
    assert_eq!(denied["isError"], json!(true));

    // The server offers no stream of its own, and a session ends when the client says so.
    let get = http(
        server.port,
        "GET",
        "/mcp",
        &[
            ("Authorization", format!("Bearer {}", fixture.phone)),
            ("Accept", "text/event-stream".to_owned()),
        ],
        b"",
    );
    assert_eq!(get.status, 405);
    assert!(
        get.header("allow")
            .is_some_and(|allow| allow.contains("POST"))
    );
    let mut ended = mcp.headers("", None);
    ended.retain(|(name, _)| *name != "Content-Type");
    let deleted = http(server.port, "DELETE", "/mcp", &ended, b"");
    assert!((200..300).contains(&deleted.status), "{}", deleted.status);

    // The revision without sessions.
    let mut inline = Mcp {
        server: &server,
        token: &fixture.phone,
        path: "/mcp",
        session: None,
        inline: true,
        next_id: 0,
    };
    let names = inline.tool_names();
    assert!(names.contains(&"meta_whoami".to_owned()));
    assert!(!names.contains(&"exec_run".to_owned()));
    let whoami = inline.call("meta_whoami", json!({}));
    let text: Value = serde_json::from_str(whoami["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(text["principal"], json!("phone"));
    let failed = inline.call("test_fail", json!({"code": "denied"}));
    assert_eq!(failed["isError"], json!(true));

    // Compact mode, chosen by the query or by a header.
    let mut compact = Mcp {
        server: &server,
        token: &fixture.phone,
        path: "/mcp?compact=1",
        session: None,
        inline: true,
        next_id: 0,
    };
    let mut names = compact.tool_names();
    names.sort();
    assert_eq!(names, ["legion_call", "legion_describe", "legion_tools"]);
    let called = compact.call("legion_call", json!({"tool": "meta.whoami"}));
    let text: Value = serde_json::from_str(called["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(text["principal"], json!("phone"));
    let mut headers = Mcp {
        server: &server,
        token: &fixture.phone,
        path: "/mcp",
        session: None,
        inline: true,
        next_id: 0,
    }
    .headers("tools/list", None);
    headers.push(("Legion-Compact", "1".to_owned()));
    let listed = http(
        server.port,
        "POST",
        "/mcp",
        &headers,
        json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list", "params": {"_meta": {
            "io.modelcontextprotocol/protocolVersion": "2026-07-28",
            "io.modelcontextprotocol/clientCapabilities": {},
            "io.modelcontextprotocol/clientInfo": {"name": "test", "version": "0"}
        }}})
        .to_string()
        .as_bytes(),
    );
    let tools = &listed.messages()[0]["result"]["tools"];
    assert_eq!(tools.as_array().unwrap().len(), 3, "{tools}");

    let line = audited(&fixture.home, |line| line["tool"] == json!("test.fail"));
    assert_eq!(line["door"], json!("http"));
    assert_eq!(line["principal"], json!("phone"));
}

fn the_token_never_reaches_a_log() {
    let fixture = Fixture::new();
    let mut server = Server::start(&fixture.home);
    let token = fixture.phone.clone();
    let echoed = server.call(
        &token,
        json!({"tool": "test.echo", "args": {"pasted": token}}),
    );
    assert_eq!(echoed.status, 200);
    server.call(
        &token,
        json!({"tool": "test.fail", "args": {"code": token}}),
    );
    server.get("/v1/tools", Some(&format!("{token}x")));
    http(
        server.port,
        "GET",
        &format!("/v1/tools?token={token}"),
        &auth(Some(&token)),
        b"",
    );
    server.stop();
    let stderr = server.stderr();
    assert!(stderr.contains("POST /v1/call 200 phone"), "{stderr}");
    assert!(stderr.contains("GET /v1/tools 401 -"), "{stderr}");
    let audit = std::fs::read_to_string(fixture.home.state().join("audit.jsonl")).unwrap();
    for secret in [&fixture.phone, &fixture.admin] {
        let body = secret.trim_start_matches("lgn_");
        assert!(!stderr.contains(body), "{stderr}");
        assert!(!audit.contains(body), "{audit}");
    }
    // token.add's own reply is the one place a token is written, and it is not recorded.
    assert!(audit.contains("\"tool\":\"token.add\""), "{audit}");
}

fn token_tools_guard_principals_and_keep_comments() {
    let home = Home::new();
    home.write_config("policy.toml", POLICY);
    home.write_config(
        "tokens.toml",
        "# Tokens for legion serve; only hashes live here.\n\n",
    );
    let unknown = home.run(&["token", "add", "x", "stranger", "--json"]);
    assert_eq!(unknown.code, 1);
    assert_eq!(unknown.json()["code"], json!("not_found"));
    let owner = home.run(&["token", "add", "root", "owner", "--json"]);
    assert_eq!(owner.code, 1);
    assert_eq!(owner.json()["code"], json!("denied"));
    let token = make_token(
        &home,
        &["root", "owner", "allow_owner=true", "expires_days=30"],
    );
    assert!(token.starts_with("lgn_"));
    let phone = make_token(&home, &["phone", "phone"]);
    let again = home.run(&["token", "add", "phone", "phone", "--json"]);
    assert_eq!(again.json()["code"], json!("exists"));

    let listed = home.run(&["token", "list", "--json"]).json();
    let tokens = listed["tokens"].as_array().unwrap();
    assert_eq!(tokens.len(), 2);
    assert_eq!(tokens[0]["name"], json!("phone"));
    assert!(tokens[1]["expires"].is_string());
    let text = listed.to_string();
    assert!(!text.contains("blake3") && !text.contains("lgn_"), "{text}");

    // Removing one takes effect at once on a running server.
    let server = Server::start(&home);
    assert_eq!(server.get("/v1/tools", Some(&phone)).status, 200);
    let removed = home.run(&["token", "remove", "phone", "--json"]);
    assert_eq!(removed.json(), json!({"name": "phone", "removed": true}));
    assert_eq!(server.get("/v1/tools", Some(&phone)).status, 401);
    let missing = home.run(&["token", "remove", "phone", "--json"]);
    assert_eq!(missing.json()["code"], json!("not_found"));
    home.run(&["token", "remove", "root", "--json"]);

    let config = home.state().parent().unwrap().join("config");
    let file = std::fs::read_to_string(config.join("tokens.toml")).unwrap();
    assert!(
        file.starts_with("# Tokens for legion serve; only hashes live here.\n"),
        "{file}"
    );
    assert!(!file.contains("[tokens."), "{file}");
}

fn shutting_down_cancels_and_records_calls() {
    let fixture = Fixture::new();
    let mut server = Server::start(&fixture.home);
    let stream = send(
        server.port,
        "POST",
        "/v1/call",
        &auth(Some(&fixture.phone)),
        json!({"tool": "test.sleep", "args": {"seconds": 60}})
            .to_string()
            .as_bytes(),
    );
    std::thread::sleep(Duration::from_millis(300));
    let started = Instant::now();
    assert_eq!(server.stop(), if cfg!(unix) { 0 } else { 1 });
    assert!(started.elapsed() < Duration::from_secs(15));
    drop(stream);
    if cfg!(unix) {
        let line = audited(&fixture.home, |line| line["tool"] == json!("test.sleep"));
        assert_eq!(line["outcome"], json!("cancelled"));
        assert!(server.stderr().contains("legion serve: stopped"));
    }
}
