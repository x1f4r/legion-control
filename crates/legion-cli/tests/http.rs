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
role = \"owner\"

[roles.observer]
allow = [\"effect:read\"]
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
        (
            "a_token_gives_no_one_more_than_they_have",
            a_token_gives_no_one_more_than_they_have,
        ),
        (
            "only_the_owner_role_acts_as_others",
            only_the_owner_role_acts_as_others,
        ),
        (
            "a_burst_of_sessions_stays_within_the_cap",
            a_burst_of_sessions_stays_within_the_cap,
        ),
        (
            "only_a_dropped_stream_cancels_its_call",
            only_a_dropped_stream_cancels_its_call,
        ),
        (
            "an_mcp_session_belongs_to_its_token",
            an_mcp_session_belongs_to_its_token,
        ),
        (
            "headers_the_gate_decides_by_come_once",
            headers_the_gate_decides_by_come_once,
        ),
        (
            "a_listed_origin_can_call_from_a_browser",
            a_listed_origin_can_call_from_a_browser,
        ),
        (
            "leaving_an_mcp_session_call_cancels_it",
            leaving_an_mcp_session_call_cancels_it,
        ),
        ("mcp_takes_post_and_delete", mcp_takes_post_and_delete),
        (
            "expiring_token_cannot_forward_credential_minting_tools",
            expiring_token_cannot_forward_credential_minting_tools,
        ),
        (
            "ui_assets_are_served_with_security_headers_and_guarded_api",
            ui_assets_are_served_with_security_headers_and_guarded_api,
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
///
/// `--http-helper wait <marker>`: writes the marker and waits a minute, a command a test can see
/// start before it does what it means to test to a running call.
fn helper(args: &[String]) -> ExitCode {
    if let [mode, marker] = args
        && mode == "wait"
    {
        std::fs::write(marker, b"started").unwrap();
        std::thread::sleep(Duration::from_secs(60));
        return ExitCode::SUCCESS;
    }
    let [mode, legion_home, user_home] = args else {
        eprintln!("the helper takes rpc, a LEGION_HOME and a home, or wait and a marker");
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
        let admin = make_token(&home, &["admin", "admin", "allow_owner=true"]);
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
    /// Says when the stderr reader has read everything there is.
    read_all: mpsc::Receiver<()>,
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
        let (finished, read_all) = mpsc::channel();
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
            let _ = finished.send(());
        });
        let port = port
            .recv_timeout(PATIENCE)
            .expect("legion serve did not say where it listens");
        Server {
            child,
            port,
            stderr: seen,
            read_all,
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
                self.read_all
                    .recv_timeout(PATIENCE)
                    .expect("stderr did not end with the server");
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

    /// The next event's name and data, or `None` once the stream has ended. Comments, which keep
    /// a quiet stream alive, are skipped.
    fn next(&mut self) -> Option<(String, Value)> {
        loop {
            let event = self.next_block()?;
            if event.lines().all(|line| line.starts_with(':')) {
                continue;
            }
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
    }

    /// The next block of the stream as it was sent, comments included.
    fn next_block(&mut self) -> Option<String> {
        loop {
            if let Some(end) = self.buffered.find("\n\n") {
                return Some(self.buffered.drain(..end + 2).collect());
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

/// Starts a call that runs until cancelled, through `exec.run` with a token whose role may use it,
/// and returns its connection once the command is seen running.
fn running_call(server: &Server, token: &str, home: &Home) -> TcpStream {
    let marker = home.home().join("call-started");
    let _ = std::fs::remove_file(&marker);
    let argv = [
        std::env::current_exe()
            .unwrap()
            .to_string_lossy()
            .into_owned(),
        HELPER_FLAG.to_owned(),
        "wait".to_owned(),
        marker.to_string_lossy().into_owned(),
    ];
    let stream = send(
        server.port,
        "POST",
        "/v1/call",
        &auth(Some(token)),
        json!({"tool": "exec.run", "args": {"argv": argv}})
            .to_string()
            .as_bytes(),
    );
    let deadline = Instant::now() + PATIENCE;
    while !marker.exists() {
        assert!(Instant::now() < deadline, "the command never started");
        std::thread::sleep(Duration::from_millis(20));
    }
    stream
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
            drop(running_call(&server, &fixture.admin, &fixture.home));
        }
        let line = audited(&fixture.home, |line| {
            (line["tool"] == json!("test.sleep") || line["tool"] == json!("exec.run"))
                && line["outcome"] == json!("cancelled")
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
    assert_eq!(
        names,
        [
            "legion_call",
            "legion_describe",
            "legion_read",
            "legion_tools"
        ]
    );
    let called = compact.call("legion_call", json!({"tool": "meta.whoami"}));
    let text: Value = serde_json::from_str(called["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(text["principal"], json!("phone"));
    let read = compact.call("legion_read", json!({"tool": "meta.whoami"}));
    assert_eq!(read["isError"], json!(false), "{read}");
    let text: Value = serde_json::from_str(read["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(text["principal"], json!("phone"));
    assert_eq!(text["door"], json!("http"));
    let refused = compact.call(
        "legion_read",
        json!({"tool": "test.write_path", "args": {"path": "~/projects/x"}}),
    );
    assert_eq!(refused["isError"], json!(true), "{refused}");
    let text: Value =
        serde_json::from_str(refused["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(text["code"], json!("denied"));
    assert_eq!(text["details"]["effect"], json!("write"));
    assert!(
        text["hint"].as_str().unwrap().contains("legion_call"),
        "{text}"
    );
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
    assert_eq!(tools.as_array().unwrap().len(), 4, "{tools}");
    let read = tools
        .as_array()
        .unwrap()
        .iter()
        .find(|tool| tool["name"] == json!("legion_read"))
        .unwrap();
    assert_eq!(read["annotations"]["readOnlyHint"], json!(true), "{read}");
    assert_eq!(
        read["annotations"]["destructiveHint"],
        json!(false),
        "{read}"
    );

    let line = audited(&fixture.home, |line| line["tool"] == json!("test.fail"));
    assert_eq!(line["door"], json!("http"));
    assert_eq!(line["principal"], json!("phone"));
    // legion_read refused test.write_path before anything reached the node.
    assert!(
        !fixture
            .home
            .audit()
            .iter()
            .any(|line| line["tool"] == json!("test.write_path"))
    );
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
    let stream = running_call(&server, &fixture.admin, &fixture.home);
    let started = Instant::now();
    assert_eq!(server.stop(), if cfg!(unix) { 0 } else { 1 });
    assert!(started.elapsed() < Duration::from_secs(15));
    drop(stream);
    if cfg!(unix) {
        let line = audited(&fixture.home, |line| line["tool"] == json!("exec.run"));
        assert_eq!(line["outcome"], json!("cancelled"));
        assert!(server.stderr().contains("legion serve: stopped"));
    }
}

fn a_token_gives_no_one_more_than_they_have() {
    let home = Home::new();
    home.write_config(
        "policy.toml",
        &format!("{POLICY}\n[principals.keeper]\nrole = \"keeper\"\n\n[roles.keeper]\nallow = [\"effect:admin\"]\n"),
    );
    let refused = |line: &[&str], says: &str| {
        let mut line = line.to_vec();
        line.push("--json");
        let run = home.run(&line);
        assert_eq!(run.code, 1, "{line:?}: {}", run.stdout);
        let error = run.json();
        assert_eq!(error["code"], json!("denied"), "{line:?}: {error}");
        let message = error["message"].as_str().unwrap();
        assert!(message.contains(says), "{line:?}: {message}");
    };
    // A role that may only administer cannot mint its way to more.
    refused(
        &[
            "token",
            "add",
            "x",
            "owner",
            "allow_owner=true",
            "--as",
            "keeper",
        ],
        "only a caller with the owner role",
    );
    refused(
        &["token", "add", "x", "phone", "--as", "keeper"],
        "unless the caller has the owner role",
    );
    // `keeper` has `effect:admin` (owner-strength), so neither `keeper` itself nor an owner
    // without `allow_owner=true` may mint a token for it.
    refused(
        &["token", "add", "keeper", "keeper", "--as", "keeper"],
        "this role can change who may do what, which makes it as strong as owner",
    );
    refused(
        &[
            "token",
            "add",
            "keeper",
            "keeper",
            "allow_owner=true",
            "--as",
            "keeper",
        ],
        "this role can change who may do what, which makes it as strong as owner",
    );
    refused(
        &["token", "add", "keeper", "keeper"],
        "this role can change who may do what, which makes it as strong as owner",
    );
    let keeper = make_token(&home, &["keeper", "keeper", "allow_owner=true"]);
    make_token(&home, &["phone", "phone", "--as", "admin"]);
    make_token(&home, &["root", "owner", "allow_owner=true"]);

    let names = |line: &[&str]| -> Vec<String> {
        let mut line = line.to_vec();
        line.push("--json");
        home.run(&line).json()["tokens"]
            .as_array()
            .unwrap()
            .iter()
            .map(|token| token["name"].as_str().unwrap().to_owned())
            .collect()
    };
    assert_eq!(names(&["token", "list", "--as", "keeper"]), ["keeper"]);
    assert_eq!(
        names(&["token", "list", "--as", "admin"]),
        ["keeper", "phone", "root"]
    );
    assert_eq!(names(&["token", "list"]), ["keeper", "phone", "root"]);
    for (name, caller) in [("phone", "keeper"), ("root", "keeper")] {
        let removed = home.run(&["token", "remove", name, "--as", caller, "--json"]);
        assert_eq!(
            removed.json()["code"],
            json!("not_found"),
            "{name} as {caller}"
        );
    }

    // The way it was found: over HTTP, with the keeper's own token.
    let server = Server::start(&home);
    let minted = server.call(
        &keeper,
        json!({"tool": "token.add", "args": {"name": "x", "principal": "owner", "allow_owner": true}}),
    );
    assert_eq!(minted.status, 403, "{}", minted.text());
    assert!(!minted.text().contains("lgn_"));
    assert_eq!(names(&["token", "list"]), ["keeper", "phone", "root"]);
}

/// Opens an MCP session of the revision with a handshake, and returns its id.
fn open_session(server: &Server, token: &str) -> String {
    let mut mcp = Mcp {
        server,
        token,
        path: "/mcp",
        session: None,
        inline: false,
        next_id: 0,
    };
    mcp.request(
        "initialize",
        json!({
            "protocolVersion": "2025-11-25",
            "capabilities": {},
            "clientInfo": {"name": "test", "version": "0"}
        }),
    );
    let session = mcp.session.clone().expect("no session header");
    let initialized = mcp.post(
        &json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
        "notifications/initialized",
        None,
    );
    assert_eq!(initialized.status, 202);
    session
}

/// What `token` gets for `tools/list` (or, with `DELETE`, for ending the session) in `session`.
fn in_session(server: &Server, token: &str, session: &str, method: &str) -> Reply {
    let mcp = Mcp {
        server,
        token,
        path: "/mcp",
        session: Some(session.to_owned()),
        inline: false,
        next_id: 0,
    };
    let headers = mcp.headers("tools/list", None);
    let body = json!({"jsonrpc": "2.0", "id": 7, "method": "tools/list", "params": {}});
    match method {
        "DELETE" => http(server.port, "DELETE", "/mcp", &headers, b""),
        _ => http(
            server.port,
            "POST",
            "/mcp",
            &headers,
            body.to_string().as_bytes(),
        ),
    }
}

fn an_mcp_session_belongs_to_its_token() {
    let fixture = Fixture::new();
    let server = Server::start(&fixture.home);
    let admins = open_session(&server, &fixture.admin);

    // Another token gets what a session that does not exist gets, whatever it tries.
    let unknown = in_session(&server, &fixture.phone, "no-such-session", "POST");
    assert_eq!(unknown.status, 404);
    for method in ["POST", "DELETE"] {
        let reply = in_session(&server, &fixture.phone, &admins, method);
        assert_eq!(reply.status, 404, "{method}");
        assert_eq!(reply.body, unknown.body, "{method}");
        assert!(!reply.text().contains("exec_run"), "{method}");
    }
    let still = in_session(&server, &fixture.admin, &admins, "POST");
    assert_eq!(still.status, 200, "{}", still.text());
    assert!(still.text().contains("exec_run"));

    // A token holds only so many sessions; its oldest makes way.
    let first = open_session(&server, &fixture.phone);
    for _ in 0..16 {
        open_session(&server, &fixture.phone);
    }
    assert_eq!(
        in_session(&server, &fixture.phone, &first, "POST").status,
        404
    );
    assert_eq!(
        in_session(&server, &fixture.admin, &admins, "POST").status,
        200
    );

    // Removing a token ends its sessions, even when a new token takes its name.
    let tablet = make_token(&fixture.home, &["tablet", "phone"]);
    let tablets = open_session(&server, &tablet);
    assert_eq!(in_session(&server, &tablet, &tablets, "POST").status, 200);
    fixture.home.run(&["token", "remove", "tablet"]);
    assert_eq!(in_session(&server, &tablet, &tablets, "POST").status, 401);
    let tablet = make_token(&fixture.home, &["tablet", "phone"]);
    assert_eq!(in_session(&server, &tablet, &tablets, "POST").status, 404);

    // A session ended by its own token is gone.
    assert!((200..300).contains(&in_session(&server, &fixture.admin, &admins, "DELETE").status));
    assert_eq!(
        in_session(&server, &fixture.admin, &admins, "POST").status,
        404
    );
}

/// The status a request written byte for byte gets.
fn raw_status(port: u16, request: &[u8]) -> u16 {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
    stream.set_read_timeout(Some(PATIENCE)).unwrap();
    stream.write_all(request).unwrap();
    read_head(&mut BufReader::new(stream)).0
}

fn headers_the_gate_decides_by_come_once() {
    let fixture = Fixture::new();
    fixture.home.write_config(
        "node.toml",
        "name = \"laptop\"\n\n[serve]\norigins = [\"https://app.example.net\"]\n",
    );
    let server = Server::start(&fixture.home);
    let port = server.port;
    let host = format!("Host: 127.0.0.1:{port}\r\n");
    let good = format!("Authorization: Bearer {}\r\n", fixture.phone);
    let get = |lines: &str| {
        raw_status(
            port,
            format!("GET /v1/tools HTTP/1.1\r\nConnection: close\r\n{lines}\r\n").as_bytes(),
        )
    };
    assert_eq!(get(&format!("{host}{good}")), 200);
    for (what, lines) in [
        (
            "a second Host",
            format!("{host}Host: evil.example\r\n{good}"),
        ),
        (
            "a second Authorization",
            format!("{host}{good}Authorization: Bearer lgn_x\r\n"),
        ),
        (
            "a second Origin",
            format!(
                "{host}{good}Origin: https://app.example.net\r\nOrigin: https://evil.example\r\n"
            ),
        ),
        ("no Host", good.clone()),
    ] {
        assert_eq!(get(&lines), 400, "{what}");
    }
    let mut binary =
        format!("GET /v1/tools HTTP/1.1\r\nConnection: close\r\n{good}Host: 127.0.0.1:{port}")
            .into_bytes();
    binary.extend_from_slice(b"\xff\r\n\r\n");
    assert_eq!(raw_status(port, &binary), 400);
    let elsewhere = format!(
        "GET http://evil.example:{port}/v1/tools HTTP/1.1\r\nConnection: close\r\n{host}{good}\r\n"
    );
    assert_eq!(raw_status(port, elsewhere.as_bytes()), 400);
    let here = format!(
        "GET http://127.0.0.1:{port}/v1/tools HTTP/1.1\r\nConnection: close\r\n{host}{good}\r\n"
    );
    assert_eq!(raw_status(port, here.as_bytes()), 200);
}

fn a_listed_origin_can_call_from_a_browser() {
    let fixture = Fixture::new();
    fixture.home.write_config(
        "node.toml",
        "name = \"laptop\"\n\n[serve]\norigins = [\"https://app.example.net\"]\n",
    );
    let server = Server::start(&fixture.home);
    let page = "https://app.example.net".to_owned();
    let asking = |origin: &str| {
        vec![
            ("Origin", origin.to_owned()),
            ("Access-Control-Request-Method", "POST".to_owned()),
            (
                "Access-Control-Request-Headers",
                "authorization, content-type, mcp-session-id".to_owned(),
            ),
        ]
    };

    // The preflight carries no token.
    let preflight = http(server.port, "OPTIONS", "/v1/call", &asking(&page), b"");
    assert_eq!(preflight.status, 204);
    assert_eq!(
        preflight.header("access-control-allow-origin"),
        Some(page.as_str())
    );
    let allowed = preflight
        .header("access-control-allow-headers")
        .unwrap()
        .to_ascii_lowercase();
    for header in [
        "authorization",
        "content-type",
        "accept",
        "mcp-session-id",
        "mcp-protocol-version",
    ] {
        assert!(allowed.contains(header), "{allowed}");
    }
    assert!(
        preflight
            .header("access-control-allow-methods")
            .is_some_and(|methods| methods.contains("POST"))
    );
    assert_eq!(preflight.header("access-control-allow-credentials"), None);
    assert!(
        preflight
            .header("vary")
            .is_some_and(|vary| vary.contains("Origin"))
    );
    let mcp = http(server.port, "OPTIONS", "/mcp", &asking(&page), b"");
    assert_eq!(mcp.status, 204);

    // The call itself, and a refusal, both name the page so it can read them.
    let mut headers = auth(Some(&fixture.phone));
    headers.push(("Origin", page.clone()));
    let called = http(
        server.port,
        "POST",
        "/v1/call",
        &headers,
        json!({"tool": "meta.version"}).to_string().as_bytes(),
    );
    assert_eq!(called.status, 200);
    assert_eq!(
        called.header("access-control-allow-origin"),
        Some(page.as_str())
    );
    assert!(
        called
            .header("vary")
            .is_some_and(|vary| vary.contains("Origin"))
    );
    let refused = http(
        server.port,
        "GET",
        "/v1/tools",
        &[("Origin", page.clone())],
        b"",
    );
    assert_eq!(refused.status, 401);
    assert_eq!(
        refused.header("access-control-allow-origin"),
        Some(page.as_str())
    );

    // Errors too, whatever their status, so the page can read what went wrong.
    for body in [
        json!({"tool": "test.sleep", "args": {"secs": 1}}),
        json!({"tool": "fs.frobnicate"}),
        json!({"tool": "test.fail", "args": {"code": "busy"}}),
    ] {
        let failed = http(
            server.port,
            "POST",
            "/v1/call",
            &headers,
            body.to_string().as_bytes(),
        );
        assert!(failed.status >= 400, "{body}");
        assert_eq!(
            failed.header("access-control-allow-origin"),
            Some(page.as_str()),
            "{} {body}",
            failed.status
        );
    }
    let mut malformed = headers.clone();
    malformed.push(("Content-Type", "application/json".to_owned()));
    let not_json = http(server.port, "POST", "/v1/call", &malformed, b"not json");
    assert_eq!(not_json.status, 400);
    assert_eq!(
        not_json.header("access-control-allow-origin"),
        Some(page.as_str())
    );

    // Any other page gets nothing it could use.
    let other = http(
        server.port,
        "OPTIONS",
        "/v1/call",
        &asking("https://evil.example"),
        b"",
    );
    assert_eq!(other.status, 403);
    assert_eq!(other.header("access-control-allow-origin"), None);
    // Without an Origin an OPTIONS is no preflight, and needs a token like anything else.
    assert_eq!(
        http(server.port, "OPTIONS", "/v1/call", &[], b"").status,
        401
    );
}

fn leaving_an_mcp_session_call_cancels_it() {
    let fixture = Fixture::new();
    let server = Server::start(&fixture.home);
    let session = open_session(&server, &fixture.phone);
    let mcp = Mcp {
        server: &server,
        token: &fixture.phone,
        path: "/mcp",
        session: Some(session),
        inline: false,
        next_id: 0,
    };
    let headers = mcp.headers("tools/call", Some("test_sleep"));
    let call = json!({
        "jsonrpc": "2.0",
        "id": 9,
        "method": "tools/call",
        "params": {"_meta": {"progressToken": "p"}, "name": "test_sleep", "arguments": {"seconds": 60}}
    });
    let stream = send(
        server.port,
        "POST",
        "/mcp",
        &headers,
        call.to_string().as_bytes(),
    );
    let mut reader = BufReader::new(stream);
    let (status, _) = read_head(&mut reader);
    assert_eq!(status, 200);
    // The call is running once it reports progress; then the client goes.
    let mut line = String::new();
    while !line.contains("notifications/progress") {
        line.clear();
        assert!(
            reader.read_line(&mut line).unwrap() > 0,
            "the stream ended first"
        );
    }
    let started = Instant::now();
    drop(reader);
    let line = audited(&fixture.home, |line| line["tool"] == json!("test.sleep"));
    assert_eq!(line["outcome"], json!("cancelled"));
    assert_eq!(line["door"], json!("http"));
    assert!(started.elapsed() < Duration::from_secs(20));
}

fn mcp_takes_post_and_delete() {
    let fixture = Fixture::new();
    let server = Server::start(&fixture.home);
    for method in ["GET", "PUT", "PATCH", "HEAD", "OPTIONS", "TRACE"] {
        let reply = http(
            server.port,
            method,
            "/mcp",
            &auth(Some(&fixture.phone)),
            b"",
        );
        assert_eq!(reply.status, 405, "{method}");
        assert_eq!(reply.header("allow"), Some("POST, DELETE"), "{method}");
    }
}

fn only_the_owner_role_acts_as_others() {
    let home = Home::new();
    let roles = "\n[principals.boss]\nrole = \"owner\"\n\n[principals.deputy]\nrole = \"deputy\"\n\n[roles.deputy]\nallow = [\"*\"]\n";
    home.write_config("policy.toml", &format!("{POLICY}{roles}"));
    let run = |line: &[&str]| {
        let mut line = line.to_vec();
        line.push("--json");
        home.run(&line)
    };
    // `deputy` allows `*` (owner-strength), so minting a token for it requires `allow_owner=true`.
    let deputy_no_flag = run(&["token", "add", "deputy", "deputy"]);
    assert_eq!(
        deputy_no_flag.json()["code"],
        json!("denied"),
        "{}",
        deputy_no_flag.stdout
    );
    assert!(
        deputy_no_flag.stdout.contains("deputy")
            && deputy_no_flag.stdout.contains(
                "this role can change who may do what, which makes it as strong as owner"
            ),
        "{}",
        deputy_no_flag.stdout
    );
    let deputy = make_token(&home, &["deputy", "deputy", "allow_owner=true"]);

    // A principal the file gives the owner role is as much the owner as owner is.
    let boss = run(&["token", "add", "b", "boss"]);
    assert_eq!(boss.json()["code"], json!("denied"), "{}", boss.stdout);
    assert!(boss.stdout.contains("owner role"), "{}", boss.stdout);
    let agent = run(&["agent.add", "agent=claude", "as=boss", "dry_run=true"]);
    assert_eq!(agent.json()["code"], json!("denied"), "{}", agent.stdout);
    let agent = run(&["agent.add", "agent=claude", "as=phone", "dry_run=true"]);
    assert_eq!(agent.code, 0, "{}", agent.stdout);
    let refused = run(&[
        "token",
        "add",
        "b",
        "boss",
        "allow_owner=true",
        "--as",
        "deputy",
    ]);
    assert_eq!(refused.json()["code"], json!("denied"));

    // A token made through an expiring token expires no later.
    let lasting = make_token(
        &home,
        &["lasting", "boss", "allow_owner=true", "expires_days=30"],
    );
    let server = Server::start(&home);
    for (days, status) in [(None, 403), (Some(60), 403), (Some(10), 200)] {
        let mut args = json!({"name": format!("p{}", days.unwrap_or(0)), "principal": "phone"});
        if let Some(days) = days {
            args["expires_days"] = json!(days);
        }
        let made = server.call(&lasting, json!({"tool": "token.add", "args": args}));
        assert_eq!(made.status, status, "{days:?}: {}", made.text());
    }

    // Acting as another principal is the owner role's alone: a file that gives it to a role of
    // its own is refused, so the deputy cannot become owner by asking.
    home.write_config("policy.toml", &format!("{POLICY}{roles}delegate = true\n"));
    let refused = run(&["meta.whoami"]);
    assert_eq!(refused.code, 1);
    assert!(
        refused.stdout.contains("the owner's alone"),
        "{}",
        refused.stdout
    );
    let minted = server.call(
        &deputy,
        json!({"tool": "token.add", "as": "owner", "args": {"name": "x", "principal": "owner", "allow_owner": true}}),
    );
    assert_ne!(minted.status, 200, "{}", minted.text());
    assert!(!minted.text().contains("lgn_"));
}

fn a_burst_of_sessions_stays_within_the_cap() {
    let fixture = Fixture::new();
    let server = Server::start(&fixture.home);
    let port = server.port;
    let burst = 24;
    let start = Arc::new(std::sync::Barrier::new(burst));
    let initialize = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": "2025-11-25",
            "capabilities": {},
            "clientInfo": {"name": "test", "version": "0"}
        }
    })
    .to_string();
    let threads: Vec<_> = (0..burst)
        .map(|_| {
            let start = Arc::clone(&start);
            let token = fixture.phone.clone();
            let body = initialize.clone();
            std::thread::spawn(move || {
                let headers = vec![
                    ("Authorization", format!("Bearer {token}")),
                    ("Accept", "application/json, text/event-stream".to_owned()),
                    ("Content-Type", "application/json".to_owned()),
                ];
                start.wait();
                let reply = http(port, "POST", "/mcp", &headers, body.as_bytes());
                (
                    reply.status,
                    reply.header("mcp-session-id").map(str::to_owned),
                )
            })
        })
        .collect();
    let opened: Vec<(u16, Option<String>)> = threads
        .into_iter()
        .map(|thread| thread.join().unwrap())
        .collect();
    for (status, _) in &opened {
        assert!(*status == 200 || *status == 503, "{status}");
    }
    let usable = opened
        .iter()
        .filter_map(|(_, session)| session.as_deref())
        .filter(|session| in_session(&server, &fixture.phone, session, "POST").status == 200)
        .count();
    assert!(usable <= 16, "{usable} sessions are open for one token");
    assert!(usable > 0);

    // Ending a session with a body still forgets it, so it no longer counts.
    let session = open_session(&server, &fixture.admin);
    let mcp = Mcp {
        server: &server,
        token: &fixture.admin,
        path: "/mcp",
        session: Some(session.clone()),
        inline: false,
        next_id: 0,
    };
    let headers = mcp.headers("tools/list", None);
    let body = json!({"jsonrpc": "2.0", "id": 3, "method": "tools/list", "params": {}});
    let ended = http(
        port,
        "DELETE",
        "/mcp",
        &headers,
        body.to_string().as_bytes(),
    );
    assert!((200..300).contains(&ended.status), "{}", ended.status);
    assert_eq!(
        in_session(&server, &fixture.admin, &session, "POST").status,
        404
    );
}

fn only_a_dropped_stream_cancels_its_call() {
    let fixture = Fixture::new();
    let server = Server::start(&fixture.home);
    let session = open_session(&server, &fixture.phone);
    let mcp = Mcp {
        server: &server,
        token: &fixture.phone,
        path: "/mcp",
        session: Some(session),
        inline: false,
        next_id: 0,
    };
    let headers = mcp.headers("tools/call", Some("test_sleep"));
    let call = json!({
        "jsonrpc": "2.0",
        "id": 9,
        "method": "tools/call",
        "params": {"_meta": {"progressToken": "p"}, "name": "test_sleep", "arguments": {"seconds": 60}}
    });
    let stream = send(
        server.port,
        "POST",
        "/mcp",
        &headers,
        call.to_string().as_bytes(),
    );
    let mut reader = BufReader::new(stream);
    assert_eq!(read_head(&mut reader).0, 200);
    let mut progress = || loop {
        let mut line = String::new();
        assert!(
            reader.read_line(&mut line).unwrap() > 0,
            "the call's stream ended"
        );
        assert!(!line.contains("\"result\""), "the call ended: {line}");
        if line.contains("notifications/progress") {
            return;
        }
    };
    progress();

    // A request that reuses the running call's id, refused for asking for JSON alone.
    let mut refused = headers.clone();
    refused.retain(|(name, _)| *name != "Accept");
    refused.push(("Accept", "application/json".to_owned()));
    let reply = http(
        server.port,
        "POST",
        "/mcp",
        &refused,
        call.to_string().as_bytes(),
    );
    assert_eq!(reply.status, 406, "{}", reply.text());
    // The call goes on: two seconds more of progress, and no result.
    for _ in 0..40 {
        progress();
    }
    drop(reader);
    let line = audited(&fixture.home, |line| line["tool"] == json!("test.sleep"));
    assert_eq!(line["outcome"], json!("cancelled"));

    // The JSON API's stream says something every few seconds while its call is quiet, so a
    // client that vanished is noticed.
    let marker = fixture.home.home().join("quiet-started");
    let argv = [
        std::env::current_exe()
            .unwrap()
            .to_string_lossy()
            .into_owned(),
        HELPER_FLAG.to_owned(),
        "wait".to_owned(),
        marker.to_string_lossy().into_owned(),
    ];
    let mut events = Events::open(
        server.port,
        &fixture.admin,
        &json!({"tool": "exec.run", "args": {"argv": argv}}),
    );
    assert_eq!(events.status, 200);
    let block = events.next_block().expect("the quiet stream said nothing");
    assert!(block.starts_with(':'), "{block}");
}

fn expiring_token_cannot_forward_credential_minting_tools() {
    let fixture = Fixture::new();
    let pi = Home::new();
    let pi_auth_keys = pi.home().join("pi_authorized_keys");
    let pi_host_key = pi.home().join("ssh_host_ed25519_key.pub");
    std::fs::write(
        &pi_host_key,
        "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAI0000000000000000000000000000000000000000000 root@pi\n",
    )
    .unwrap();
    let auth_literal = format!(
        "'{}'",
        pi_auth_keys.display().to_string().replace('\'', "''")
    );
    let host_literal = format!(
        "'{}'",
        pi_host_key.display().to_string().replace('\'', "''")
    );
    pi.write_config(
        "node.toml",
        &format!(
            "name = \"pi\"\n\n[devices]\nauthorized_keys = {auth_literal}\nhost_keys = [{host_literal}]\n"
        ),
    );
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
    fixture.home.write_config(
        "fleet.toml",
        &format!(
            "[machines.pi]\ncommand = {}\n",
            serde_json::to_string(&helper).unwrap()
        ),
    );

    let expiring_admin = make_token(
        &fixture.home,
        &["temp-admin", "admin", "allow_owner=true", "expires_days=7"],
    );
    let server = Server::start(&fixture.home);

    // Sanity check: ordinary tools forward cleanly with the expiring token.
    let ver = server.call(&expiring_admin, json!({"tool": "meta.version", "on": "pi"}));
    assert_eq!(ver.status, 200, "{}", ver.text());
    assert_eq!(ver.json()["node"], json!("pi"));

    // Forwarding device.invite to `pi` must be refused by `laptop` without touching `pi`.
    let inv = server.call(
        &expiring_admin,
        json!({
            "tool": "device.invite",
            "on": "pi",
            "args": {"principal": "phone", "host": "127.0.0.1", "user": "me"}
        }),
    );
    assert_eq!(inv.status, 403, "{}", inv.text());
    assert_eq!(inv.json()["code"], json!("denied"));
    assert_eq!(inv.json()["machine"], json!("laptop"));
    assert!(!pi_auth_keys.exists());

    // Forwarding token.add to `pi` (both non-expiring and short-lived) must be refused by `laptop`.
    for args in [
        json!({"name": "forever", "principal": "phone"}),
        json!({"name": "short", "principal": "phone", "expires_days": 1}),
    ] {
        let reply = server.call(
            &expiring_admin,
            json!({"tool": "token.add", "on": "pi", "args": args}),
        );
        assert_eq!(reply.status, 403, "{}", reply.text());
        assert_eq!(reply.json()["code"], json!("denied"));
        assert_eq!(reply.json()["machine"], json!("laptop"));
    }

    // Forwarding device.set, config.write (devices.toml / policy.toml), and fleet.sync push/pull
    // must also be refused by `laptop` when the caller arrived with an expiring credential.
    for (tool, args) in [
        (
            "device.set",
            json!({"name": "phone", "role": "owner", "allow_owner": true}),
        ),
        ("device.set", json!({"name": "phone", "rename": "phone2"})),
        (
            "config.write",
            json!({"document": "devices.toml", "content": ""}),
        ),
        (
            "config.write",
            json!({"document": "policy.toml", "content": POLICY}),
        ),
        (
            "fleet.sync",
            json!({"direction": "push", "documents": ["devices.toml"]}),
        ),
        (
            "fleet.sync",
            json!({"direction": "pull", "from": "laptop", "documents": ["devices.toml"]}),
        ),
    ] {
        let reply = server.call(
            &expiring_admin,
            json!({"tool": tool, "on": "pi", "args": args}),
        );
        assert_eq!(reply.status, 403, "{tool} {args}: {}", reply.text());
        assert_eq!(reply.json()["code"], json!("denied"), "{tool} {args}");
        assert_eq!(reply.json()["machine"], json!("laptop"), "{tool} {args}");
    }

    // Read-only fleet.sync status and non-credential config.write (projects.toml) are not blocked.
    let status_reply = server.call(
        &expiring_admin,
        json!({"tool": "fleet.sync", "on": "pi", "args": {"direction": "status"}}),
    );
    assert_eq!(status_reply.status, 200, "{}", status_reply.text());
    let proj_reply = server.call(
        &expiring_admin,
        json!({"tool": "config.write", "on": "pi", "args": {"document": "projects.toml", "content": "[projects]\n"}}),
    );
    assert_eq!(proj_reply.status, 200, "{}", proj_reply.text());
    let pi_tokens = pi
        .state()
        .parent()
        .unwrap()
        .join("config")
        .join("tokens.toml");
    assert!(!pi_tokens.exists());

    // Fan-out across local + remote refuses the remote leg while still allowing the local shorter token.
    let fan = server.call(
        &expiring_admin,
        json!({
            "tool": "token.add",
            "on": ["laptop", "pi"],
            "args": {"name": "fan-short", "principal": "phone", "expires_days": 1}
        }),
    );
    assert_eq!(fan.status, 200, "{}", fan.text());
    let results = &fan.json()["results"];
    assert_eq!(results["laptop"]["ok"]["name"], json!("fan-short"));
    assert_eq!(results["pi"]["error"]["code"], json!("denied"));
    assert_eq!(results["pi"]["error"]["machine"], json!("laptop"));
    assert!(!pi_tokens.exists());

    // A non-expiring admin token can still forward token.add to `pi`.
    let allowed = server.call(
        &fixture.admin,
        json!({
            "tool": "token.add",
            "on": "pi",
            "args": {"name": "remote-phone", "principal": "phone"}
        }),
    );
    assert_eq!(allowed.status, 200, "{}", allowed.text());
    assert_eq!(allowed.json()["name"], json!("remote-phone"));

    // S5: Both sides of tombstone-only propagation with an expiring credential.
    // Seed `laptop` and `pi` with two shared live devices (`drop-fwd` and `drop-local`) plus one
    // laptop-only live device (`stay-local`) that must NOT leak to `pi` via an expiring token.
    let laptop_auth = fixture.home.home().join("laptop_authorized_keys");
    let laptop_auth_literal = format!(
        "'{}'",
        laptop_auth.display().to_string().replace('\'', "''")
    );
    fixture.home.write_config(
        "node.toml",
        &format!("name = \"laptop\"\n\n[devices]\nauthorized_keys = {laptop_auth_literal}\n"),
    );

    let id_fwd = "aa".repeat(16);
    let id_local = "bb".repeat(16);
    let id_stay = "cc".repeat(16);
    let now = legion_tools::device::registry::next_updated_timestamp(None).unwrap();
    let key_fwd = legion_tools::device::keys::ed25519_public_key_from_seed(&[0xaa; 32]);
    let key_local = legion_tools::device::keys::ed25519_public_key_from_seed(&[0xbb; 32]);
    let key_stay = legion_tools::device::keys::ed25519_public_key_from_seed(&[0xcc; 32]);

    let make_dev =
        |name: &str, key: &str, principal: &str| legion_tools::device::registry::FleetDevice {
            name: name.to_owned(),
            public_key: key.to_owned(),
            principal: principal.to_owned(),
            role: None,
            paired_at: now.clone(),
            paired_on: "laptop".to_owned(),
            paired_by: None,
            confirmed_by: None,
            confirmed_at: None,
            updated_at: now.clone(),
            updated_on: "laptop".to_owned(),
            name_updated_at: now.clone(),
            name_updated_on: "laptop".to_owned(),
            role_updated_at: now.clone(),
            role_updated_on: "laptop".to_owned(),
            conflict: false,
            role_conflict: false,
        };
    let mut pi_devices = legion_tools::device::registry::DevicesFile::default();
    pi_devices
        .devices
        .insert(id_fwd.clone(), make_dev("drop-fwd", &key_fwd, "phone"));
    pi_devices.devices.insert(
        id_local.clone(),
        make_dev("drop-local", &key_local, "phone"),
    );
    pi.write_config("devices.toml", &pi_devices.to_toml_string());

    let mut laptop_devices = pi_devices.clone();
    laptop_devices
        .devices
        .insert(id_stay.clone(), make_dev("stay-local", &key_stay, "phone"));
    fixture
        .home
        .write_config("devices.toml", &laptop_devices.to_toml_string());

    // Side 1: Forwarding a tombstone-only `config.write devices.toml` with the expiring token is
    // allowed and revokes `drop-fwd` on `pi`, while forwarding one with a live entry is refused.
    let mut fwd_tomb = legion_tools::device::registry::DevicesFile::default();
    fwd_tomb.removed.insert(
        id_fwd.clone(),
        legion_tools::device::registry::DeviceTombstone {
            name: "drop-fwd".to_owned(),
            principal: "phone".to_owned(),
            fingerprint: None,
            paired_at: Some(now.clone()),
            paired_on: Some("laptop".to_owned()),
            paired_by: None,
            removed_by: None,
            removed_at: now.clone(),
            removed_on: "laptop".to_owned(),
        },
    );
    let fwd_ok = server.call(
        &expiring_admin,
        json!({
            "tool": "config.write",
            "on": "pi",
            "args": {"document": "devices.toml", "content": fwd_tomb.to_toml_string()}
        }),
    );
    assert_eq!(
        fwd_ok.status,
        200,
        "tombstone-only forward failed: {}",
        fwd_ok.text()
    );

    let mut fwd_with_live = fwd_tomb.clone();
    fwd_with_live
        .devices
        .insert(id_stay.clone(), make_dev("stay-local", &key_stay, "phone"));
    let fwd_live_denied = server.call(
        &expiring_admin,
        json!({
            "tool": "config.write",
            "on": "pi",
            "args": {"document": "devices.toml", "content": fwd_with_live.to_toml_string()}
        }),
    );
    assert_eq!(fwd_live_denied.status, 403, "{}", fwd_live_denied.text());
    assert_eq!(fwd_live_denied.json()["code"], json!("denied"));

    // Side 2: Local `device.remove` on `laptop` with the expiring token pushes tombstone-only to
    // `pi` (`partial` because `stay-local` was not pushed under the expiring credential, with empty
    // `still_trusted_by` since the removal landed) without pushing `stay-local` to `pi`.
    let rem_local = server.call(
        &expiring_admin,
        json!({
            "tool": "device.remove",
            "args": {"id": id_local}
        }),
    );
    assert_eq!(
        rem_local.status,
        200,
        "local device.remove failed: {}",
        rem_local.text()
    );
    let rem_json = rem_local.json();
    assert_eq!(rem_json["removed"], json!(true), "{rem_json}");
    assert_eq!(
        rem_json["fleet"]["pi"]["status"],
        json!("partial"),
        "expiring-credential removal with unsynced live device must report partial on pi: {rem_json}"
    );
    assert!(
        rem_json["fleet"]["pi"]["reason"]
            .as_str()
            .is_some_and(|r| r.contains("Removals delivered") && r.contains("expires")),
        "partial reason must explain removals delivered and credential expiry: {rem_json}"
    );
    assert!(
        rem_json.get("still_trusted_by").is_none(),
        "still_trusted_by must be empty after tombstone delivery: {rem_json}"
    );

    let pi_devices_path = pi_home.join("config").join("devices.toml");
    let pi_devices_after: legion_tools::device::registry::DevicesFile =
        legion_core::config::parse(&pi_devices_path, &std::fs::read(&pi_devices_path).unwrap())
            .unwrap();
    assert!(pi_devices_after.removed.contains_key(&id_fwd));
    assert!(pi_devices_after.removed.contains_key(&id_local));
    assert!(
        !pi_devices_after.devices.contains_key(&id_stay),
        "laptop-only live device must NOT be pushed to pi by an expiring credential"
    );
}

fn ui_assets_are_served_with_security_headers_and_guarded_api() {
    const EXPECTED_CSP: &str = "default-src 'none'; script-src 'self'; style-src 'self'; img-src 'self' data:; connect-src 'self'; manifest-src 'self'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'";

    let fixture = Fixture::new();
    let server = Server::start(&fixture.home);

    // Public UI assets are served without a token with strict security headers and ETag.
    for (path, expected_type) in [
        ("/", "text/html; charset=utf-8"),
        ("/styles.css", "text/css; charset=utf-8"),
        ("/app.js", "text/javascript; charset=utf-8"),
        (
            "/manifest.webmanifest",
            "application/manifest+json; charset=utf-8",
        ),
        ("/icons/legion.svg", "image/svg+xml"),
    ] {
        let reply = server.get(path, None);
        assert_eq!(reply.status, 200, "{path}: {}", reply.text());
        assert_eq!(reply.header("content-type"), Some(expected_type), "{path}");
        assert_eq!(reply.header("cache-control"), Some("no-cache"), "{path}");
        assert_eq!(
            reply.header("content-security-policy"),
            Some(EXPECTED_CSP),
            "{path}"
        );
        assert_eq!(
            reply.header("x-content-type-options"),
            Some("nosniff"),
            "{path}"
        );
        assert_eq!(
            reply.header("referrer-policy"),
            Some("no-referrer"),
            "{path}"
        );
        let etag = reply.header("etag").expect("missing ETag").to_owned();
        assert!(etag.starts_with('"') && etag.ends_with('"'), "{etag}");

        let cached = http(server.port, "GET", path, &[("If-None-Match", etag)], b"");
        assert_eq!(cached.status, 304, "{path}");
    }

    // API paths still require a token even while UI assets are public.
    assert_eq!(server.get("/v1/tools", None).status, 401);
    assert_eq!(
        server
            .post("/v1/call", None, &json!({"tool": "meta.whoami"}))
            .status,
        401
    );
    assert_eq!(server.post("/mcp", None, &json!({})).status, 401);

    // `?all=1` lists unavailable tools for the UI while plain `/v1/tools` remains filtered.
    let all_tools = server.get("/v1/tools?all=1", Some(&fixture.phone));
    assert_eq!(all_tools.status, 200);
    let all_json = all_tools.json();
    let all_names: Vec<&str> = all_json["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert!(all_names.contains(&"exec.run"), "{all_names:?}");
    assert!(all_names.contains(&"power.restart"), "{all_names:?}");

    // Same-origin browser calls from the served UI succeed, while cross-origin calls stay 403.
    let mut same_headers = auth(Some(&fixture.phone));
    same_headers.push(("Content-Type", "application/json".to_owned()));
    same_headers.push(("Origin", format!("http://127.0.0.1:{}", server.port)));
    let same = http(
        server.port,
        "POST",
        "/v1/call",
        &same_headers,
        br#"{"tool":"meta.whoami","args":{}}"#,
    );
    assert_eq!(same.status, 200, "{}", same.text());

    let mut evil_headers = auth(Some(&fixture.phone));
    evil_headers.push(("Content-Type", "application/json".to_owned()));
    evil_headers.push(("Origin", "https://evil.example".to_owned()));
    let evil = http(
        server.port,
        "POST",
        "/v1/call",
        &evil_headers,
        br#"{"tool":"meta.whoami","args":{}}"#,
    );
    assert_eq!(evil.status, 403, "{}", evil.text());

    // Traversal, directories, and excluded reference/test files are never served.
    for path in [
        "/../node.toml",
        "/%2e%2e/node.toml",
        "/icons/../../node.toml",
        "/design/prototype.html",
        "/lib/",
        "/lib/test/format.test.mjs",
        "/vendor/README.md",
    ] {
        let with_token = server.get(path, Some(&fixture.phone));
        assert_eq!(with_token.status, 404, "{path}");
        let without_token = server.get(path, None);
        assert_ne!(without_token.status, 200, "{path}");
    }

    // Turning the interface off in node.toml disables `/` and assets immediately.
    fixture
        .home
        .write_config("node.toml", "name = \"laptop\"\n\n[serve]\nui = false\n");
    assert_eq!(server.get("/", Some(&fixture.phone)).status, 404);
    assert_eq!(server.get("/app.js", Some(&fixture.phone)).status, 404);
    assert_eq!(server.get("/", None).status, 401);
    assert_eq!(server.get("/v1/health", None).status, 200);
    assert_eq!(server.get("/v1/tools", Some(&fixture.phone)).status, 200);
}
