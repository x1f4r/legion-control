//! What the `sys` and `exec` tests share: a node under its own temporary `LEGION_HOME`, a runner
//! for tests without the standard harness, and the child processes the tests need.
//!
//! The tests need children that print, read, sleep and start grandchildren, and CI runs them on
//! systems with different programs (no `sh` or `sleep` on Windows, no PowerShell on Linux). So the
//! test binary is its own child: started with `LEGION_TEST_HELPER` set, it plays the part named
//! there instead of running tests. That is also why these tests have no standard harness, which
//! would write its own lines to stdout before a helper could print anything.

#![allow(
    dead_code,
    reason = "each test binary uses the part of the support it needs"
)]

use std::env;
use std::future::Future;
use std::io::{Read, Write};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::process::{Command, ExitCode, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use legion_core::proto::{ErrorBody, EventKind};
use legion_core::{Call, CancellationToken, Door, EventSink, Node, Paths};
use serde_json::Value;

/// The variable that turns the test binary into a helper child.
pub const HELPER: &str = "LEGION_TEST_HELPER";

/// One test: its name and its body.
pub type Test = (
    &'static str,
    fn() -> Pin<Box<dyn Future<Output = ()> + Send>>,
);

/// Runs the helper named in `LEGION_TEST_HELPER` when it is set, and otherwise every test whose
/// name contains the first argument that is not an option, each on a fresh runtime.
pub fn main(tests: &[Test]) -> ExitCode {
    if let Ok(mode) = env::var(HELPER) {
        return helper(&mode, env::args().skip(1).collect());
    }
    let filter = env::args().skip(1).find(|arg| !arg.starts_with('-'));
    let mut failed = Vec::new();
    let mut ran = 0;
    for (name, test) in tests {
        if filter
            .as_deref()
            .is_some_and(|filter| !name.contains(filter))
        {
            continue;
        }
        ran += 1;
        let started = Instant::now();
        let outcome = catch_unwind(AssertUnwindSafe(|| {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .unwrap();
            runtime.block_on(test());
        }));
        let verdict = if outcome.is_ok() { "ok" } else { "FAILED" };
        println!(
            "test {name} ... {verdict} ({:.1}s)",
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

/// The parts a helper child can play.
fn helper(mode: &str, args: Vec<String>) -> ExitCode {
    let mut stdout = std::io::stdout();
    match mode {
        // Prints its arguments, one per line, and a line on stderr; exits with LEGION_TEST_EXIT.
        "args" => {
            for arg in &args {
                let _ = writeln!(stdout, "{arg}");
            }
            let _ = writeln!(std::io::stderr(), "to stderr");
            let code = env::var("LEGION_TEST_EXIT")
                .ok()
                .and_then(|code| code.parse::<u8>().ok())
                .unwrap_or(0);
            let _ = stdout.flush();
            return ExitCode::from(code);
        }
        // Copies stdin to stdout.
        "cat" => {
            let mut input = Vec::new();
            let _ = std::io::stdin().read_to_end(&mut input);
            let _ = stdout.write_all(&input);
        }
        // Prints the directory it runs in.
        "cwd" => {
            let cwd = env::current_dir().unwrap();
            let _ = write!(stdout, "{}", cwd.display());
        }
        // Prints the named variable's value.
        "env" => {
            let value = env::var(&args[0]).unwrap_or_default();
            let _ = write!(stdout, "{value}");
        }
        // Writes the given number of bytes to stdout: a start marker, `x`s, an end marker.
        "flood" => {
            let size: usize = args[0].parse().unwrap();
            let middle = size.saturating_sub(10);
            let _ = stdout.write_all(b"START");
            let _ = stdout.write_all(&vec![b'x'; middle]);
            let _ = stdout.write_all(b"END!!");
        }
        // Prints a line, then sleeps the given number of seconds.
        "sleep" => {
            let _ = writeln!(stdout, "sleeping");
            let _ = stdout.flush();
            std::thread::sleep(Duration::from_secs_f64(args[0].parse().unwrap()));
        }
        // Sleeps, ignoring the polite request to stop where there is one (SIGTERM).
        "stubborn" => {
            let _ignoring = ignore_terminate();
            let _ = writeln!(stdout, "sleeping");
            let _ = stdout.flush();
            std::thread::sleep(Duration::from_secs(60));
        }
        // Starts a sleeping grandchild, writes "<own pid> <grandchild pid>" to the given file,
        // then waits for it. With a second argument `stubborn`, both ignore SIGTERM.
        "family" => {
            let stubborn = args.get(1).is_some_and(|mode| mode == "stubborn");
            let _ignoring = stubborn.then(ignore_terminate);
            let mode = if stubborn { "stubborn" } else { "sleep" };
            let mut grandchild = spawn_helper(mode, &["60"]);
            let line = format!("{} {}", std::process::id(), grandchild.id());
            write_atomically(Path::new(&args[0]), &line);
            let _ = writeln!(stdout, "started");
            let _ = stdout.flush();
            let _ = grandchild.wait();
        }
        other => {
            let _ = writeln!(std::io::stderr(), "no helper called {other}");
            return ExitCode::from(101);
        }
    }
    let _ = stdout.flush();
    ExitCode::SUCCESS
}

/// Makes SIGTERM do nothing to this process, for as long as the returned runtime lives. Windows
/// has no polite request to ignore.
#[cfg(unix)]
fn ignore_terminate() -> Option<tokio::runtime::Runtime> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .ok()?;
    let signal = runtime.block_on(async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
    });
    // Once registered, tokio's handler replaces the default for the life of the process.
    drop(signal);
    Some(runtime)
}

#[cfg(not(unix))]
fn ignore_terminate() -> Option<tokio::runtime::Runtime> {
    None
}

/// The test binary itself, which every helper child runs.
pub fn this_binary() -> String {
    env::current_exe().unwrap().to_string_lossy().into_owned()
}

/// Starts a helper child directly, for tests that need a process they did not start through a
/// tool.
pub fn spawn_helper(mode: &str, args: &[&str]) -> std::process::Child {
    Command::new(this_binary())
        .args(args)
        .env(HELPER, mode)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap()
}

fn write_atomically(path: &Path, text: &str) {
    let partial = path.with_extension("partial");
    std::fs::write(&partial, text).unwrap();
    std::fs::rename(&partial, path).unwrap();
}

/// Waits until `path` exists and returns the process ids written in it.
pub async fn read_pids(path: &Path) -> Vec<u32> {
    let text = wait_for(Duration::from_secs(20), || {
        std::fs::read_to_string(path).ok()
    })
    .await
    .unwrap_or_else(|| panic!("{} was never written", path.display()));
    text.split_whitespace()
        .map(|pid| pid.parse().unwrap())
        .collect()
}

/// Polls `check` until it returns something or `limit` passes.
pub async fn wait_for<T>(limit: Duration, mut check: impl FnMut() -> Option<T>) -> Option<T> {
    let deadline = Instant::now() + limit;
    loop {
        if let Some(found) = check() {
            return Some(found);
        }
        if Instant::now() >= deadline {
            return None;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

/// Whether a process with this id is running (a zombie is not).
pub fn alive(pid: u32) -> bool {
    matches!(legion_os::procid::of_pid(pid), Ok(Some(_)))
}

/// Waits a while for a process to be gone.
pub async fn gone(pid: u32) -> bool {
    wait_for(Duration::from_secs(10), || (!alive(pid)).then_some(()))
        .await
        .is_some()
}

/// The events a call delivered, in order.
#[derive(Clone, Default)]
pub struct Events(Arc<Mutex<Vec<(EventKind, Value)>>>);

impl Events {
    /// A sink that records into this.
    pub fn sink(&self) -> EventSink {
        let events = self.0.clone();
        EventSink::new(move |kind, data| events.lock().unwrap().push((kind, data)))
    }

    /// The text of every event of one kind, joined.
    pub fn text(&self, kind: EventKind) -> String {
        self.0
            .lock()
            .unwrap()
            .iter()
            .filter(|(seen, _)| *seen == kind)
            .filter_map(|(_, data)| data.as_str())
            .collect()
    }
}

/// A node whose configuration and state live in a temporary directory, with `home` under it.
pub struct Fixture {
    /// The temporary directory, removed when the fixture is dropped.
    pub dir: tempfile::TempDir,
    /// The node.
    pub node: Node,
}

impl Fixture {
    /// A node with every built-in tool and no policy file.
    pub fn new() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let node = Node::new(
            Paths::under(dir.path()).with_home(dir.path().join("home")),
            legion_tools::registry(),
        );
        std::fs::create_dir_all(dir.path().join("home")).unwrap();
        Fixture { dir, node }
    }

    /// The home directory `~` stands for.
    pub fn home(&self) -> PathBuf {
        self.dir.path().join("home")
    }

    /// Writes a configuration file.
    pub fn write_config(&self, name: &str, text: &str) {
        let config = self.dir.path().join("config");
        std::fs::create_dir_all(&config).unwrap();
        std::fs::write(config.join(name), text).unwrap();
    }

    /// Calls a tool as the owner.
    pub async fn call(&self, tool: &str, args: Value) -> Result<Value, ErrorBody> {
        self.node.call(Call::new(Door::Cli, tool, args)).await
    }

    /// Calls a tool as a principal.
    pub async fn call_as(
        &self,
        principal: &str,
        tool: &str,
        args: Value,
    ) -> Result<Value, ErrorBody> {
        let mut call = Call::new(Door::Mcp, tool, args);
        call.as_principal = Some(principal.to_owned());
        self.node.call(call).await
    }

    /// Calls a tool as the owner, recording its events and cancelled by `cancel`.
    pub async fn call_with(
        &self,
        tool: &str,
        args: Value,
        events: &Events,
        cancel: CancellationToken,
    ) -> Result<Value, ErrorBody> {
        let mut call = Call::new(Door::Rpc, tool, args);
        call.events = events.sink();
        call.cancel = cancel;
        self.node.call(call).await
    }
}
