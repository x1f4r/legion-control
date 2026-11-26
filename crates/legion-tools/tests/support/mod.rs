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

/// Runs the helper named in `LEGION_TEST_HELPER` when it is set, and otherwise the tests, reading
/// the arguments libtest takes so that `cargo test` and CI drive these binaries like any other.
pub fn main(tests: &[Test]) -> ExitCode {
    if let Ok(mode) = env::var(HELPER) {
        return helper(&mode, env::args().skip(1).collect());
    }
    let options = match Options::parse(env::args().skip(1)) {
        Ok(options) => options,
        Err(problem) => {
            eprintln!("error: {problem}");
            return ExitCode::from(101);
        }
    };
    let chosen: Vec<&Test> = tests
        .iter()
        .filter(|(name, _)| options.runs(name))
        .collect();
    if options.list {
        for (name, _) in &chosen {
            println!("{name}: test");
        }
        println!("\n{} tests, 0 benchmarks", chosen.len());
        return ExitCode::SUCCESS;
    }
    println!("\nrunning {} tests", chosen.len());
    let mut failed = Vec::new();
    for (name, test) in &chosen {
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
        chosen.len() - failed.len(),
        failed.len()
    );
    if failed.is_empty() {
        ExitCode::SUCCESS
    } else {
        println!("failures: {}", failed.join(", "));
        ExitCode::FAILURE
    }
}

/// What the arguments a test binary is given ask for, as libtest reads them.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Options {
    /// Name the tests instead of running them.
    pub list: bool,
    /// Only tests whose name contains one of these, or equals one with `exact`.
    pub filters: Vec<String>,
    /// Leave out tests whose name contains one of these.
    pub skip: Vec<String>,
    /// Filters must match a whole name.
    pub exact: bool,
    /// Only ignored tests were asked for, and none of these is.
    pub only_ignored: bool,
}

impl Options {
    /// Reads libtest's arguments. Options that change only how results are shown are accepted and
    /// have no effect; one this runner does not know is an error, so a misread never passes as a
    /// run of no tests.
    pub fn parse(args: impl IntoIterator<Item = String>) -> Result<Options, String> {
        const SHOWN: [&str; 9] = [
            "--nocapture",
            "--show-output",
            "-q",
            "--quiet",
            "--include-ignored",
            "--report-time",
            "--ensure-time",
            "--test",
            "--no-capture",
        ];
        const WITH_VALUE: [&str; 6] = [
            "--format",
            "--test-threads",
            "--logfile",
            "--color",
            "-Z",
            "--shuffle-seed",
        ];
        let mut options = Options::default();
        let mut args = args.into_iter();
        while let Some(arg) = args.next() {
            let (flag, inline) = match arg.split_once('=') {
                Some((flag, value)) if arg.starts_with("--") => {
                    (flag.to_owned(), Some(value.to_owned()))
                }
                _ => (arg.clone(), None),
            };
            match flag.as_str() {
                "--list" => options.list = true,
                "--exact" => options.exact = true,
                "--ignored" => options.only_ignored = true,
                "--skip" => {
                    let value = inline
                        .or_else(|| args.next())
                        .ok_or("--skip needs a value")?;
                    options.skip.push(value);
                }
                flag if SHOWN.contains(&flag) || flag == "--shuffle" => {}
                flag if WITH_VALUE.contains(&flag) => {
                    if inline.is_none() && args.next().is_none() {
                        return Err(format!("{flag} needs a value"));
                    }
                }
                flag if flag.starts_with('-') => return Err(format!("unknown option {flag}")),
                _ => options.filters.push(arg),
            }
        }
        Ok(options)
    }

    /// Whether the test called `name` is to run.
    pub fn runs(&self, name: &str) -> bool {
        let matches = |filter: &String| {
            if self.exact {
                name == filter
            } else {
                name.contains(filter.as_str())
            }
        };
        !self.only_ignored
            && (self.filters.is_empty() || self.filters.iter().any(matches))
            && !self.skip.iter().any(|skip| name.contains(skip.as_str()))
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
        // Runs a node in this process, whose environment the test set, and prints what a command
        // exec.run starts sees of two variables: `<LEGION_TEST_INHERITED>|<TEST_INHERITED>`.
        "exec-env" => {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            let seen = runtime.block_on(async {
                let fixture = Fixture::new();
                let ran = fixture
                    .call(
                        "exec.run",
                        serde_json::json!({
                            "argv": [this_binary(), "LEGION_TEST_INHERITED", "TEST_INHERITED"],
                            "env": {HELPER: "env-pair"},
                        }),
                    )
                    .await
                    .unwrap();
                ran["stdout"].as_str().unwrap_or_default().to_owned()
            });
            let _ = write!(stdout, "{seen}");
        }
        // Prints the values of two variables, `|` between them.
        "env-pair" => {
            let value = |name: &str| env::var(name).unwrap_or_default();
            let _ = write!(stdout, "{}|{}", value(&args[0]), value(&args[1]));
        }
        // Writes `ok` and the first byte of a two-byte character it never finishes.
        "cut-short" => {
            let _ = stdout.write_all(b"ok\xc3");
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
            if let Some(ready_path) = args.iter().find_map(|a| a.strip_prefix("--ready=")) {
                write_atomically(Path::new(ready_path), "ready");
            }
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
        // Spawns a worker thread, writes "<pid> <worker_tid>" to the given file (on Linux), and
        // sleeps so tests can verify thread IDs are refused without harming the process.
        "threaded" => {
            let own = std::process::id();
            let (ready_tx, ready_rx) = std::sync::mpsc::channel();
            let worker = std::thread::spawn(move || {
                let _ = ready_tx.send(());
                std::thread::sleep(Duration::from_secs(60));
            });
            let _ = ready_rx.recv();
            let worker_tid = find_other_linux_tid(own).unwrap_or(own);
            let line = format!("{own} {worker_tid}");
            write_atomically(Path::new(&args[0]), &line);
            let _ = writeln!(stdout, "started");
            let _ = stdout.flush();
            std::thread::sleep(Duration::from_secs(60));
            let _ = worker.join();
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

/// Find a non-leader Linux kernel thread id in `/proc/self/task` for `own_pid`.
pub fn find_other_linux_tid(own_pid: u32) -> Option<u32> {
    let entries = std::fs::read_dir("/proc/self/task").ok()?;
    for entry in entries.flatten() {
        if let Some(tid) = entry
            .file_name()
            .to_str()
            .and_then(|s| s.parse::<u32>().ok())
            && tid != own_pid
        {
            return Some(tid);
        }
    }
    None
}

fn write_atomically(path: &Path, text: &str) {
    let partial = path.with_extension("partial");
    std::fs::write(&partial, text).unwrap();
    std::fs::rename(&partial, path).unwrap();
}

/// Waits until `path` exists and returns the process ids written in it.
pub async fn read_pids(path: &Path) -> Vec<u32> {
    let text = wait_for(Duration::from_secs(60), || {
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
    wait_for(Duration::from_secs(30), || (!alive(pid)).then_some(()))
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
