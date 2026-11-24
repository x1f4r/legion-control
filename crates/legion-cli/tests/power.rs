//! The power tools end to end: the real `legion` binary on several local nodes reached through the
//! `command` connector, each with its own `LEGION_HOME`.
//!
//! Nothing here can sleep, restart or wake anything. The binary under test is built with the
//! `testing` feature, whose power tools act through a recording fake that appends what it was asked
//! to do to `actions.jsonl` in the directory `LEGION_TEST_POWER` names. A machine is simulated by a
//! state file saying which of its systems is up, read by the helper this test binary runs as the
//! connector command, and a boot id file every system of the machine reports from
//! (`LEGION_TEST_BOOT_ID_FILE`): the test "restarts" a machine by writing a new boot id and
//! bringing up a system, and the helper drops the link of a system that went down.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitCode, Stdio};
use std::time::{Duration, Instant};

use common::{Home, PATIENCE, Run};
use serde_json::{Value, json};

const LEGION_BIN: &str = env!("CARGO_BIN_EXE_legion");
const HELPER_FLAG: &str = "--power-helper";

type Test = (&'static str, fn());

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some(HELPER_FLAG) {
        return run_helper(&args[1..]);
    }
    let filter = args
        .iter()
        .find(|arg| !arg.starts_with('-'))
        .map(String::as_str);
    let tests: &[Test] = &[
        (
            "local_tools_gate_dry_run_policy_and_exclusive_jobs",
            local_tools_gate_dry_run_policy_and_exclusive_jobs,
        ),
        (
            "one_power_action_at_a_time_on_a_long_lived_node",
            one_power_action_at_a_time_on_a_long_lived_node,
        ),
        (
            "cancel_calls_off_a_pending_restart_on_another_machine",
            cancel_calls_off_a_pending_restart_on_another_machine,
        ),
        (
            "wake_asks_the_first_helper_that_answers_and_waits_for_the_machine",
            wake_asks_the_first_helper_that_answers_and_waits_for_the_machine,
        ),
        (
            "switch_confirms_a_new_boot_in_the_expected_system",
            switch_confirms_a_new_boot_in_the_expected_system,
        ),
        (
            "switch_reaches_a_machine_through_a_relay",
            switch_reaches_a_machine_through_a_relay,
        ),
        (
            "switch_looks_at_every_system_however_short_the_wait",
            switch_looks_at_every_system_however_short_the_wait,
        ),
    ];
    let mut failed = Vec::new();
    let mut ran = 0;
    for (name, test) in tests {
        if filter.is_some_and(|f| !name.contains(f)) {
            continue;
        }
        ran += 1;
        let started = Instant::now();
        let outcome = std::panic::catch_unwind(test);
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

/// The modes this binary runs in when a node's fleet.toml or a job starts it.
fn run_helper(args: &[String]) -> ExitCode {
    match args.split_first() {
        // system <legion_home> <user_home> <power_dir> <machine_dir> <system>
        // system <legion_home> <user_home> <power_dir> <machine_dir> <system> [hang]
        Some((mode, rest)) if mode == "system" && (rest.len() == 5 || rest.len() == 6) => {
            run_system(
                Path::new(&rest[0]),
                Path::new(&rest[1]),
                Path::new(&rest[2]),
                Path::new(&rest[3]),
                &rest[4],
                rest.get(5).map(String::as_str) == Some("hang"),
            )
        }
        // sleep <seconds>
        Some((mode, rest)) if mode == "sleep" && rest.len() == 1 => {
            std::thread::sleep(Duration::from_secs(rest[0].parse().unwrap()));
            ExitCode::SUCCESS
        }
        _ => {
            eprintln!("unknown helper mode: {args:?}");
            ExitCode::from(2)
        }
    }
}

/// One system of a simulated machine: unreachable unless the machine's state says it is the one
/// up, and its link dropped as soon as the state moves on.
fn run_system(
    legion_home: &Path,
    user_home: &Path,
    power_dir: &Path,
    machine_dir: &Path,
    system: &str,
    hang: bool,
) -> ExitCode {
    let started_as = read_state(machine_dir);
    if started_as.up != system {
        if hang {
            // A system half way down: the connection opens and nothing ever answers, until the
            // caller gives up and closes it.
            let (done, gone) = std::sync::mpsc::channel();
            std::thread::spawn(move || {
                let _ = std::io::copy(&mut std::io::stdin(), &mut std::io::sink());
                let _ = done.send(());
            });
            let _ = gone.recv_timeout(PATIENCE);
        } else {
            eprintln!("ssh: connect to host {system} port 22: No route to host");
        }
        return ExitCode::from(255);
    }
    let mut child = Command::new(LEGION_BIN)
        .arg("rpc")
        .env("LEGION_HOME", legion_home)
        .env("HOME", user_home)
        .env("USERPROFILE", user_home)
        .env("LEGION_TEST_POWER", power_dir)
        .env("LEGION_TEST_BOOT_ID_FILE", machine_dir.join("boot_id"))
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("XDG_STATE_HOME")
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap();
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            return ExitCode::from(u8::try_from(status.code().unwrap_or(1)).unwrap_or(1));
        }
        let now = read_state(machine_dir);
        if now.up != system || now.generation != started_as.generation {
            let _ = child.kill();
            let _ = child.wait();
            return ExitCode::from(255);
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[derive(PartialEq)]
struct State {
    up: String,
    generation: u64,
}

fn read_state(machine_dir: &Path) -> State {
    let text = std::fs::read_to_string(machine_dir.join("state.json")).unwrap();
    let value: Value = serde_json::from_str(&text).unwrap();
    State {
        up: value["up"].as_str().unwrap().to_owned(),
        generation: value["generation"].as_u64().unwrap(),
    }
}

/// A simulated machine: which system is up, and the boot id its systems report.
struct Machine {
    dir: tempfile::TempDir,
}

impl Machine {
    fn new(up: &str, boot_id: &str) -> Machine {
        let machine = Machine {
            dir: tempfile::tempdir().unwrap(),
        };
        machine.set(up, 1, boot_id);
        machine
    }

    fn path(&self) -> &Path {
        self.dir.path()
    }

    /// Writes the boot id first and the state after, as a restarted machine would come back.
    fn set(&self, up: &str, generation: u64, boot_id: &str) {
        legion_core::config::write_atomically(&self.path().join("boot_id"), boot_id.as_bytes())
            .unwrap();
        let state = json!({"up": up, "generation": generation}).to_string();
        legion_core::config::write_atomically(&self.path().join("state.json"), state.as_bytes())
            .unwrap();
    }
}

/// One system of a machine: a node with its own home and the directory its fake records into.
struct System {
    home: Home,
    power: tempfile::TempDir,
}

impl System {
    fn new(node_toml: &str) -> System {
        let system = System {
            home: Home::new(),
            power: tempfile::tempdir().unwrap(),
        };
        system.home.write_config("node.toml", node_toml);
        system
    }

    /// The connector command that reaches this system of `machine` as `name`.
    fn command(&self, machine: &Machine, name: &str) -> String {
        self.connector(machine, name, false)
    }

    /// The same, for a system that hangs rather than refuses while it is down.
    fn hanging_command(&self, machine: &Machine, name: &str) -> String {
        self.connector(machine, name, true)
    }

    fn connector(&self, machine: &Machine, name: &str, hang: bool) -> String {
        let mut argv = vec![
            std::env::current_exe()
                .unwrap()
                .to_string_lossy()
                .into_owned(),
            HELPER_FLAG.to_owned(),
            "system".to_owned(),
            legion_home(&self.home),
            self.home.home().to_string_lossy().into_owned(),
            self.power.path().to_string_lossy().into_owned(),
            machine.path().to_string_lossy().into_owned(),
            name.to_owned(),
        ];
        if hang {
            argv.push("hang".to_owned());
        }
        let quoted: Vec<String> = argv
            .iter()
            .map(|part| serde_json::to_string(part).unwrap())
            .collect();
        format!("[{}]", quoted.join(", "))
    }

    /// Runs `legion` here as a person would, with this system's fake and boot id.
    fn legion(&self, machine: &Machine, args: &[&str]) -> Run {
        let output = self
            .cli(machine, args)
            .stdin(Stdio::null())
            .output()
            .unwrap();
        Run {
            code: output.status.code().unwrap_or(-1),
            stdout: String::from_utf8(output.stdout).unwrap(),
            stderr: String::from_utf8(output.stderr).unwrap(),
        }
    }

    /// Starts `legion` here without waiting for it.
    fn spawn(&self, machine: &Machine, args: &[&str]) -> Child {
        self.cli(machine, args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap()
    }

    fn cli(&self, machine: &Machine, args: &[&str]) -> Command {
        let mut command = self.home.command(args);
        command
            .env("LEGION_TEST_POWER", self.power.path())
            .env("LEGION_TEST_BOOT_ID_FILE", machine.path().join("boot_id"));
        command
    }

    /// What this system's fake was asked to do, one object per line.
    fn actions(&self) -> Vec<Value> {
        std::fs::read_to_string(self.power.path().join("actions.jsonl"))
            .unwrap_or_default()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    /// The actions that would have restarted or powered the machine down: handed to the delayed
    /// runner on Linux and macOS, or run on Windows, where `shutdown` waits by itself.
    fn power_actions(&self) -> Vec<Value> {
        self.actions()
            .into_iter()
            .filter(|action| {
                action["kind"] == json!("schedule")
                    || (action["kind"] == json!("run") && action["argv"][0] == json!("shutdown"))
            })
            .collect()
    }

    fn datagrams(&self) -> Vec<String> {
        self.actions()
            .into_iter()
            .filter(|action| action["kind"] == json!("datagram"))
            .map(|action| action["hex"].as_str().unwrap().to_owned())
            .collect()
    }
}

fn legion_home(home: &Home) -> String {
    home.state()
        .parent()
        .unwrap()
        .to_string_lossy()
        .into_owned()
}

/// The magic packet for `mac`, as the fake records it.
fn packet_hex(mac: &str) -> String {
    let bytes: String = mac.split(':').collect();
    format!("{}{}", "ff".repeat(6), bytes.repeat(16))
}

/// Waits until `ready` holds, failing the test after [`PATIENCE`].
fn wait_until(what: &str, ready: impl Fn() -> bool) {
    let deadline = Instant::now() + PATIENCE;
    while !ready() {
        assert!(Instant::now() < deadline, "gave up waiting until {what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// The JSON a finished background `legion` printed, and its exit code.
fn finish(child: Child) -> (i32, Value) {
    let output = child.wait_with_output().unwrap();
    let stdout = String::from_utf8(output.stdout).unwrap();
    let value = serde_json::from_str(stdout.trim()).unwrap_or_else(|error| {
        panic!(
            "not JSON ({error}): {stdout}\nstderr: {}",
            String::from_utf8_lossy(&output.stderr)
        )
    });
    (output.status.code().unwrap_or(-1), value)
}

fn this_exe() -> PathBuf {
    std::env::current_exe().unwrap()
}

fn local_tools_gate_dry_run_policy_and_exclusive_jobs() {
    let machine = Machine::new("laptop", "boot-1");
    let laptop = System::new("name = \"laptop\"\n");

    // Nothing in node.toml says what keeps the machine busy: refused, and nothing was done.
    let status = laptop.legion(&machine, &["power.status"]);
    assert_eq!(status.code, 0, "{}", status.stderr);
    assert_eq!(status.json()["busy"]["state"], json!("unmonitored"));
    assert_eq!(status.json()["boot_id"], json!("boot-1"));
    let refused = laptop.legion(&machine, &["power.sleep"]);
    assert_eq!(refused.code, 1);
    assert_eq!(refused.json()["code"], json!("busy"));
    assert!(
        refused.json()["hint"]
            .as_str()
            .unwrap()
            .contains("busy = \"none\"")
    );
    assert!(laptop.actions().is_empty(), "{:?}", laptop.actions());

    laptop.home.write_config(
        "node.toml",
        "name = \"laptop\"\nsystem = \"linux\"\nbusy = \"none\"\n[power]\nsudo = true\n",
    );
    let dry = laptop.legion(&machine, &["power.restart", "dry_run=true", "delay_s=5"]);
    assert_eq!(dry.code, 0, "{}", dry.stderr);
    let dry = dry.json();
    assert_eq!(dry["scheduled"], json!(false));
    assert_eq!(dry["in_s"], json!(5));
    let command = dry["commands"].as_array().unwrap().last().unwrap().clone();
    assert!(laptop.power_actions().is_empty(), "{:?}", laptop.actions());

    let restart = laptop.legion(&machine, &["power.restart", "delay_s=5"]);
    assert_eq!(restart.code, 0, "{}", restart.stderr);
    assert_eq!(restart.json()["scheduled"], json!(true));
    assert_eq!(restart.json()["boot_id"], json!("boot-1"));
    let handed = laptop.power_actions();
    assert_eq!(handed.len(), 1, "{handed:?}");
    assert_eq!(handed[0]["argv"], command);
    // The fake restarts nothing, so the machine comes back as the real one would: on a new boot.
    // Until then the restart is pending, and every later call would hear about that first (on
    // Windows, which times the restart itself, until its time and a margin are up).
    machine.set("laptop", 2, "boot-2");
    let after = laptop.legion(&machine, &["power.status"]).json();
    assert_eq!(after.get("pending"), None, "{after}");
    assert_eq!(after["last"]["action"], json!("restart"), "{after}");

    // An exclusive job keeps the machine busy for as long as it runs.
    let args = json!({
        "argv": [this_exe(), HELPER_FLAG, "sleep", "60"],
        "exclusive": true,
        "name": "render-frames",
    })
    .to_string();
    let started = laptop.legion(&machine, &["job.start", "--args", &args]);
    assert_eq!(started.code, 0, "{}", started.stderr);
    let job = started.json()["id"].as_str().unwrap().to_owned();
    let busy = laptop.legion(&machine, &["power.status"]).json();
    assert_eq!(busy["busy"]["state"], json!("busy"), "{busy}");
    assert_eq!(busy["busy"]["reasons"][0]["source"], json!("job"));
    assert_eq!(busy["busy"]["reasons"][0]["name"], json!("render-frames"));
    let held = laptop.legion(&machine, &["power.shutdown"]);
    assert_eq!(held.json()["code"], json!("busy"), "{}", held.stdout);
    let cancelled = laptop.legion(&machine, &["job.cancel", &format!("id={job}")]);
    assert_eq!(cancelled.code, 0, "{}", cancelled.stderr);
    let idle = laptop.legion(&machine, &["power.status"]).json();
    assert_eq!(idle["busy"]["state"], json!("idle"), "{idle}");
    assert_eq!(laptop.power_actions().len(), 1);

    // A role without the power effect may look but not act.
    laptop.home.write_config(
        "policy.toml",
        "[principals.\"agent:test\"]\nrole = \"observer\"\n\n[roles.observer]\nallow = [\"effect:read\"]\n",
    );
    let denied = laptop.legion(
        &machine,
        &["power.restart", "force=true", "--as", "agent:test"],
    );
    assert_eq!(denied.json()["code"], json!("denied"), "{}", denied.stdout);
    let looked = laptop.legion(&machine, &["power.status", "--as", "agent:test"]);
    assert_eq!(looked.code, 0, "{}", looked.stderr);
    assert_eq!(laptop.power_actions().len(), 1);
}

/// `legion rpc` with a system's fake, answering one call at a time.
struct Rpc {
    child: Child,
    lines: std::io::Lines<std::io::BufReader<std::process::ChildStdout>>,
    next: u64,
}

impl Rpc {
    fn start(system: &System, machine: &Machine) -> Rpc {
        let mut child = system
            .cli(machine, &["rpc"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let stdout = child.stdout.take().unwrap();
        Rpc {
            child,
            lines: std::io::BufRead::lines(std::io::BufReader::new(stdout)),
            next: 0,
        }
    }

    /// Calls `tool` and returns the reply's `result` or `error`.
    fn call(&mut self, tool: &str, args: Value) -> Value {
        self.send(json!({"tool": tool, "args": args}))
    }

    /// Calls `tool` on the machine `on`, through this node's own session to it.
    fn call_on(&mut self, tool: &str, args: Value, on: &str) -> Value {
        self.send(json!({"tool": tool, "args": args, "on": on}))
    }

    fn send(&mut self, params: Value) -> Value {
        self.next += 1;
        let request =
            json!({"jsonrpc": "2.0", "id": self.next, "method": "call", "params": params});
        let stdin = self.child.stdin.as_mut().unwrap();
        std::io::Write::write_all(stdin, format!("{request}\n").as_bytes()).unwrap();
        loop {
            let line = self.lines.next().unwrap().unwrap();
            let Ok(reply) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            if reply["id"] == json!(self.next) {
                return reply
                    .get("result")
                    .cloned()
                    .unwrap_or_else(|| reply["error"]["data"].clone());
            }
        }
    }
}

impl Drop for Rpc {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn cancel_calls_off_a_pending_restart_on_another_machine() {
    let laptop_machine = Machine::new("laptop", "boot-1");
    let laptop = System::new("name = \"laptop\"\nbusy = \"none\"\n[power]\nsudo = true\n");
    let desk_machine = Machine::new("desk", "boot-d");
    let desk = System::new("name = \"desk\"\n");
    desk.home.write_config(
        "fleet.toml",
        &format!(
            "[machines.laptop]\ncommand = {}\n",
            laptop.command(&laptop_machine, "laptop")
        ),
    );
    // One long-lived session from desk to laptop, as an agent's MCP door keeps one: laptop's node
    // and the runner its fake stands in for live as long as it does.
    let mut desk_node = Rpc::start(&desk, &desk_machine);
    let restart = desk_node.call_on("power.restart", json!({"delay_s": 600}), "laptop");
    assert_eq!(restart["scheduled"], json!(true), "{restart}");
    let pending = desk_node.call_on("power.status", json!({}), "laptop");
    assert_eq!(pending["pending"]["action"], json!("restart"), "{pending}");

    let cancelled = desk_node.call_on("power.cancel", json!({}), "laptop");
    assert_eq!(cancelled["cancelled"], json!(true), "{cancelled}");
    assert_eq!(cancelled["id"], pending["pending"]["id"]);
    let after = desk_node.call_on("power.status", json!({}), "laptop");
    assert_eq!(after.get("pending"), None, "{after}");
    assert_eq!(after["last"]["fate"], json!("cancelled"), "{after}");
    assert_eq!(after["last"]["cancelled_by"], json!("owner"), "{after}");
    let again = desk_node.call_on("power.cancel", json!({}), "laptop");
    assert_eq!(again["cancelled"], json!(false), "{again}");

    // Laptop's fake called it off the platform's way; desk itself did nothing.
    let called_off = laptop.actions().iter().any(|action| {
        action["kind"] == json!("stop")
            || (action["kind"] == json!("run") && action["argv"] == json!(["shutdown", "/a"]))
    });
    assert!(called_off, "{:?}", laptop.actions());
    assert!(desk.actions().is_empty(), "{:?}", desk.actions());
}

fn one_power_action_at_a_time_on_a_long_lived_node() {
    let machine = Machine::new("laptop", "boot-1");
    let laptop = System::new(
        "name = \"laptop\"\nbusy = \"none\"\n[power]\nsudo = true\nsleep = [\"go-to-sleep\"]\n",
    );
    let mut node = Rpc::start(&laptop, &machine);
    let first = node.call("power.sleep", json!({}));
    assert_eq!(first["scheduled"], json!(true), "{first}");
    // The runner the fake stands in for has not run: a second sleep would put the machine back
    // to sleep as it woke.
    let second = node.call("power.sleep", json!({}));
    assert_eq!(second["code"], json!("conflict"), "{second}");
    let status = node.call("power.status", json!({}));
    assert_eq!(status["pending"]["action"], json!("sleep"), "{status}");
    assert_eq!(laptop.power_actions().len(), 1);
}

fn wake_asks_the_first_helper_that_answers_and_waits_for_the_machine() {
    let laptop_machine = Machine::new("laptop", "boot-l");
    let laptop = System::new("name = \"laptop\"\n");
    let tower_machine = Machine::new("asleep", "boot-t");
    let tower = System::new("name = \"tower\"\n");
    let shed_machine = Machine::new("asleep", "boot-s");
    let shed = System::new("name = \"shed\"\n");
    let pi_machine = Machine::new("pi", "boot-p");
    let pi = System::new("name = \"pi\"\n");
    let box_machine = Machine::new("asleep", "boot-b");
    let sleeper = System::new("name = \"box\"\n");

    laptop.home.write_config(
        "fleet.toml",
        &format!(
            "[machines.tower]\nmac = \"02:00:00:00:00:01\"\nwake_via = [\"shed\", \"pi\"]\ncommand = {}\n\n\
             [machines.shed]\ncommand = {}\n\n\
             [machines.pi]\ncommand = {}\n\n\
             [machines.box]\nmac = \"02:00:00:00:00:03\"\ncommand = {}\n\n\
             [machines.attic]\nmac = \"02:00:00:00:00:04\"\nwake_via = [\"shed\"]\ncommand = {}\n",
            tower.command(&tower_machine, "tower"),
            shed.command(&shed_machine, "shed"),
            pi.command(&pi_machine, "pi"),
            sleeper.command(&box_machine, "box"),
            sleeper.command(&box_machine, "box"),
        ),
    );
    // The helper knows tower by another address; that one, its own, is what it sends.
    pi.home.write_config(
        "fleet.toml",
        "[machines.tower]\nmac = \"02:00:00:00:00:02\"\ncommand = [\"unused\"]\n",
    );

    let woken = laptop.legion(&laptop_machine, &["power.wake", "machine=tower"]);
    assert_eq!(woken.code, 0, "{}\n{}", woken.stdout, woken.stderr);
    let woken = woken.json();
    assert_eq!(woken["sent_by"], json!("pi"), "{woken}");
    assert_eq!(woken["awake"], json!(false));
    assert!(woken["packets"].as_u64().unwrap() >= 3, "{woken}");
    assert_eq!(woken["skipped"][0]["helper"], json!("shed"));
    let sent = pi.datagrams();
    assert!(!sent.is_empty());
    assert!(
        sent.iter()
            .all(|hex| *hex == packet_hex("02:00:00:00:00:02"))
    );
    // The helper that did not answer was passed over and never woken; the caller sent nothing.
    assert!(shed.actions().is_empty());
    assert!(laptop.datagrams().is_empty());

    // A dry run picks the same sender and says where its packets would go, sending none.
    let dry = laptop
        .legion(
            &laptop_machine,
            &["power.wake", "machine=tower", "dry_run=true"],
        )
        .json();
    assert_eq!(dry["sent_by"], json!("pi"), "{dry}");
    assert_eq!(dry["packets"], json!(0));
    assert_eq!(dry["dry_run"], json!(true));
    assert_eq!(dry["would_send"]["mac"], json!("02:00:00:00:00:02"));
    assert_eq!(pi.datagrams().len(), sent.len());

    // A helper takes the address from its own fleet.toml, never from the call.
    let forged = laptop.legion(
        &laptop_machine,
        &[
            "power.wol",
            "machine=tower",
            "mac=02:00:00:00:00:09",
            "--on",
            "pi",
        ],
    );
    assert_eq!(
        forged.json()["code"],
        json!("invalid_args"),
        "{}",
        forged.stdout
    );

    // Waiting: awake only once the machine itself answers.
    let sent_before = pi.datagrams().len();
    let waiting = laptop.spawn(
        &laptop_machine,
        &["power.wake", "machine=tower", "wait_s=60"],
    );
    wait_until("pi sends again", || pi.datagrams().len() > sent_before);
    std::thread::sleep(Duration::from_millis(300));
    tower_machine.set("tower", 2, "boot-t");
    let (code, waited) = finish(waiting);
    assert_eq!(code, 0, "{waited}");
    assert_eq!(waited["awake"], json!(true), "{waited}");
    assert!(waited["waited_s"].as_f64().unwrap() > 0.0, "{waited}");

    // Awake already: nothing is sent.
    let before = pi.datagrams().len();
    let again = laptop
        .legion(&laptop_machine, &["power.wake", "machine=tower"])
        .json();
    assert_eq!(again["awake"], json!(true), "{again}");
    assert_eq!(again["sent_by"], json!(null));
    assert_eq!(pi.datagrams().len(), before);

    // No helpers: this node sends it itself, from its own fleet.toml.
    let direct = laptop
        .legion(&laptop_machine, &["power.wake", "machine=box"])
        .json();
    assert_eq!(direct["sent_by"], json!("laptop"), "{direct}");
    assert!(
        laptop
            .datagrams()
            .iter()
            .all(|hex| *hex == packet_hex("02:00:00:00:00:03"))
    );

    // Every helper asleep: unreachable, saying which were tried.
    let stuck = laptop.legion(&laptop_machine, &["power.wake", "machine=attic"]);
    assert_eq!(stuck.code, 1);
    assert_eq!(
        stuck.json()["code"],
        json!("unreachable"),
        "{}",
        stuck.stdout
    );
    assert!(
        stuck.json()["message"].as_str().unwrap().contains("shed"),
        "{}",
        stuck.stdout
    );
    assert!(shed.actions().is_empty());
}

fn switch_confirms_a_new_boot_in_the_expected_system() {
    let laptop_machine = Machine::new("laptop", "boot-l");
    let laptop = System::new("name = \"laptop\"\n");
    let tower = Machine::new("linux", "boot-1");
    let linux = System::new(
        "name = \"tower\"\nsystem = \"linux\"\nbusy = \"none\"\n[power]\nsudo = true\n[boot.windows]\narm = [\"arm-windows\"]\nverify = [\"verify-windows\"]\n",
    );
    let windows = System::new(
        "name = \"tower\"\nsystem = \"windows\"\nbusy = \"none\"\n[power]\nsudo = true\n[boot.linux]\narm = [\"arm-linux\"]\nverify = [\"verify-linux\"]\n",
    );
    laptop.home.write_config(
        "fleet.toml",
        &format!(
            "[machines.tower]\n\n[[machines.tower.systems]]\nname = \"linux\"\ncommand = {}\n\n[[machines.tower.systems]]\nname = \"windows\"\ncommand = {}\n",
            linux.command(&tower, "linux"),
            windows.command(&tower, "windows"),
        ),
    );

    // A dry run asks the machine for its own dry run, and nothing is armed or restarted.
    let dry = laptop.legion(
        &laptop_machine,
        &[
            "power.switch",
            "machine=tower",
            "into=windows",
            "dry_run=true",
        ],
    );
    assert_eq!(dry.code, 0, "{}\n{}", dry.stdout, dry.stderr);
    let dry = dry.json();
    assert_eq!(dry["dry_run"], json!(true), "{dry}");
    assert_eq!(dry["restarted"], json!(false));
    assert_eq!(dry["from_system"], json!("linux"));
    assert_eq!(dry["would_run"]["commands"][0], json!(["arm-windows"]));
    assert!(linux.power_actions().is_empty());
    assert!(
        !linux
            .actions()
            .iter()
            .any(|action| action["argv"] == json!(["arm-windows"]))
    );

    // It restarts, the link drops, and windows answers with a new boot id.
    let switching = laptop.spawn(
        &laptop_machine,
        &["power.switch", "machine=tower", "into=windows", "wait_s=60"],
    );
    wait_until("tower's linux hands over the restart", || {
        linux.power_actions().len() == 1
    });
    tower.set("windows", 2, "boot-2");
    let (code, switched) = finish(switching);
    assert_eq!(code, 0, "{switched}");
    assert_eq!(
        switched,
        json!({
            "machine": "tower",
            "from_system": "linux",
            "system": "windows",
            "restarted": true,
            "waited_s": switched["waited_s"],
        })
    );
    // Armed and read back before the restart was handed over.
    let argv: Vec<Value> = linux.actions().iter().map(|a| a["argv"].clone()).collect();
    let at = |wanted: Value| argv.iter().position(|a| *a == wanted).unwrap();
    assert!(at(json!(["arm-windows"])) < at(json!(["verify-windows"])));
    assert!(at(json!(["verify-windows"])) < at(linux.power_actions()[0]["argv"].clone()));

    // It restarts, but linux comes back: the call says so and fails.
    tower.set("linux", 3, "boot-3");
    let switching = laptop.spawn(
        &laptop_machine,
        &["power.switch", "machine=tower", "into=windows", "wait_s=60"],
    );
    wait_until("tower's linux hands over the second restart", || {
        linux.power_actions().len() == 2
    });
    tower.set("linux", 4, "boot-4");
    let (code, wrong) = finish(switching);
    assert_eq!(code, 1, "{wrong}");
    assert_eq!(wrong["code"], json!("failed"), "{wrong}");
    assert!(
        wrong["message"]
            .as_str()
            .unwrap()
            .contains("came back up in linux, not windows"),
        "{wrong}"
    );
    assert_eq!(wrong["details"]["restarted"], json!(true));
    assert_eq!(wrong["details"]["system"], json!("linux"));

    // It never goes down: a timeout, saying it still runs the boot it had.
    let stuck = laptop.legion(
        &laptop_machine,
        &["power.switch", "machine=tower", "wait_s=3"],
    );
    assert_eq!(stuck.code, 1, "{}", stuck.stdout);
    let stuck = stuck.json();
    assert_eq!(stuck["code"], json!("timeout"), "{stuck}");
    assert!(
        stuck["hint"]
            .as_str()
            .unwrap()
            .contains("still running the boot"),
        "{stuck}"
    );
    assert_eq!(stuck["details"]["boot_id"], json!("boot-4"));
    assert_eq!(linux.power_actions().len(), 3);
    assert!(windows.power_actions().is_empty());
}

const TOWER_LINUX: &str = "name = \"tower\"\nsystem = \"linux\"\nbusy = \"none\"\n[power]\nsudo = true\n[boot.windows]\narm = [\"arm-windows\"]\nverify = [\"verify-windows\"]\n";
const TOWER_WINDOWS: &str = "name = \"tower\"\nsystem = \"windows\"\nbusy = \"none\"\n[power]\nsudo = true\n[boot.linux]\narm = [\"arm-linux\"]\nverify = [\"verify-linux\"]\n";

fn switch_reaches_a_machine_through_a_relay() {
    let laptop_machine = Machine::new("laptop", "boot-l");
    let laptop = System::new("name = \"laptop\"\n");
    let pi_machine = Machine::new("pi", "boot-p");
    let pi = System::new("name = \"pi\"\n");
    let tower = Machine::new("linux", "boot-1");
    let linux = System::new(TOWER_LINUX);
    let windows = System::new(TOWER_WINDOWS);
    // Only the relay can reach tower.
    pi.home.write_config(
        "fleet.toml",
        &format!(
            "[machines.tower]\n\n[[machines.tower.systems]]\nname = \"linux\"\ncommand = {}\n\n[[machines.tower.systems]]\nname = \"windows\"\ncommand = {}\n",
            linux.command(&tower, "linux"),
            windows.command(&tower, "windows"),
        ),
    );
    laptop.home.write_config(
        "fleet.toml",
        &format!(
            "[machines.pi]\ncommand = {}\n\n[machines.tower]\nvia = \"pi\"\n",
            pi.command(&pi_machine, "pi"),
        ),
    );

    let switching = laptop.spawn(
        &laptop_machine,
        &["power.switch", "machine=tower", "into=windows", "wait_s=60"],
    );
    wait_until("tower's linux hands over the restart", || {
        linux.power_actions().len() == 1
    });
    tower.set("windows", 2, "boot-2");
    let (code, switched) = finish(switching);
    assert_eq!(code, 0, "{switched}");
    assert_eq!(switched["restarted"], json!(true), "{switched}");
    assert_eq!(switched["from_system"], json!("linux"));
    assert_eq!(switched["system"], json!("windows"));

    // A refusal on the way keeps its own code: the relay does not know this principal.
    laptop.home.write_config(
        "policy.toml",
        "[principals.\"agent:test\"]\nrole = \"operator\"\n\n[roles.operator]\nallow = [\"effect:read\", \"power.*\"]\n",
    );
    let refused = laptop.legion(
        &laptop_machine,
        &["power.switch", "machine=tower", "--as", "agent:test"],
    );
    assert_eq!(refused.code, 1);
    assert_eq!(
        refused.json()["code"],
        json!("denied"),
        "{}",
        refused.stdout
    );
    assert_eq!(windows.power_actions().len(), 0);
}

fn switch_looks_at_every_system_however_short_the_wait() {
    let laptop_machine = Machine::new("laptop", "boot-l");
    let laptop = System::new("name = \"laptop\"\n");
    let tower = Machine::new("linux", "boot-1");
    let linux = System::new(TOWER_LINUX);
    let windows = System::new(TOWER_WINDOWS);
    let fleet = |linux_command: String| {
        format!(
            "[machines.tower]\n\n[[machines.tower.systems]]\nname = \"linux\"\ncommand = {linux_command}\n\n[[machines.tower.systems]]\nname = \"windows\"\ncommand = {}\n",
            windows.command(&tower, "windows"),
        )
    };
    let switch = |wait: &str| {
        let switching = laptop.spawn(
            &laptop_machine,
            &["power.switch", "machine=tower", "into=windows", wait],
        );
        let handed = linux.power_actions().len() + 1;
        wait_until("tower's linux hands over the restart", || {
            linux.power_actions().len() == handed
        });
        switching
    };

    // A wait shorter than the time between looks still gets one look, at its end.
    laptop
        .home
        .write_config("fleet.toml", &fleet(linux.command(&tower, "linux")));
    let switching = switch("wait_s=1");
    tower.set("windows", 2, "boot-2");
    let (code, switched) = finish(switching);
    assert_eq!(code, 0, "{switched}");
    assert_eq!(switched["restarted"], json!(true), "{switched}");

    // linux, tried first, hangs on its way down; windows still gets its share of each look.
    tower.set("linux", 3, "boot-3");
    laptop
        .home
        .write_config("fleet.toml", &fleet(linux.hanging_command(&tower, "linux")));
    let switching = switch("wait_s=5");
    tower.set("windows", 4, "boot-4");
    let (code, switched) = finish(switching);
    assert_eq!(code, 0, "{switched}");
    assert_eq!(switched["system"], json!("windows"), "{switched}");
}
