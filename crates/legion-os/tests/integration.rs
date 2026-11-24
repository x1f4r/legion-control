//! Cross-platform integration tests for `legion-os`.
//!
//! Uses `harness = false` so the compiled test binary can re-exec itself with
//! `LEGION_OS_HELPER_MODE` set, avoiding any dependency on external Unix or Windows tools
//! (`sh`, `sleep`, `/tmp`) and keeping helper stdout/stderr completely free of test-harness noise.

#![deny(unsafe_code)]

use std::env;
use std::fs;
use std::future::Future;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::process::{Command, ExitCode, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use legion_os::OsError;
use legion_os::atomic::{WriteOptions, write_atomic, write_atomic_from_reader};
use legion_os::detach::{DetachOptions, spawn_detached};
use legion_os::lock::{FileLock, LockMode};
use legion_os::procid::{self, ProcessId};
use legion_os::run::{OutputStream, RunOptions, run};
use legion_os::shell::CommandSpec;
use tokio_util::sync::CancellationToken;

const HELPER_ENV: &str = "LEGION_OS_HELPER_MODE";

fn main() -> ExitCode {
    if let Ok(mode) = env::var(HELPER_ENV) {
        return run_helper(&mode);
    }

    let filter = env::args().skip(1).find(|arg| !arg.starts_with('-'));

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("failed to build tokio test runtime");

    type TestFn = fn() -> Pin<Box<dyn Future<Output = ()>>>;
    let tests: Vec<(&str, TestFn)> = vec![
        ("run_argv_echo_env_cwd_stdin_and_exit_code", || {
            Box::pin(test_run_argv_echo_env_cwd_stdin_and_exit_code())
        }),
        ("run_shell_string_works_on_platform_shell", || {
            Box::pin(test_run_shell_string_works_on_platform_shell())
        }),
        ("run_live_streaming_and_head_tail_truncation", || {
            Box::pin(test_run_live_streaming_and_head_tail_truncation())
        }),
        ("run_missing_program_returns_typed_error", || {
            Box::pin(test_run_missing_program_returns_typed_error())
        }),
        ("run_timeout_kills_direct_child_and_grandchildren", || {
            Box::pin(test_run_timeout_kills_direct_child_and_grandchildren())
        }),
        (
            "run_cancellation_kills_direct_child_and_grandchildren",
            || Box::pin(test_run_cancellation_kills_direct_child_and_grandchildren()),
        ),
        ("run_escalation_kills_term_ignoring_grandchild", || {
            Box::pin(test_run_escalation_kills_term_ignoring_grandchild())
        }),
        (
            "run_does_not_hang_when_escaped_grandchild_keeps_pipes_open",
            || Box::pin(test_run_does_not_hang_when_escaped_grandchild_keeps_pipes_open()),
        ),
        (
            "detach_survives_parent_exit_appends_logs_and_kill_tree_stops_tree",
            || Box::pin(test_detach_survives_parent_exit_appends_logs_and_kill_tree_stops_tree()),
        ),
        (
            "procid_kill_tree_leaves_process_untouched_on_mismatched_start_time",
            || Box::pin(test_procid_kill_tree_leaves_process_untouched_on_mismatched_start_time()),
        ),
        ("atomic_write_interrupted_leaves_no_partial_file", || {
            Box::pin(test_atomic_write_interrupted_leaves_no_partial_file())
        }),
        ("lock_contention_and_crash_release_across_processes", || {
            Box::pin(test_lock_contention_and_crash_release_across_processes())
        }),
    ];

    let selected: Vec<(&str, TestFn)> = tests
        .into_iter()
        .filter(|(name, _)| filter.as_deref().is_none_or(|f| name.contains(f)))
        .collect();

    println!("running {} integration tests", selected.len());
    let mut failures = Vec::new();
    for (name, test_fn) in selected {
        let start = Instant::now();
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            runtime.block_on(test_fn());
        }));
        match outcome {
            Ok(()) => println!("test {name} ... ok ({:?})", start.elapsed()),
            Err(_) => {
                println!("test {name} ... FAILED ({:?})", start.elapsed());
                failures.push(name);
            }
        }
    }

    if failures.is_empty() {
        println!("integration test result: ok");
        ExitCode::SUCCESS
    } else {
        eprintln!("failed integration tests: {}", failures.join(", "));
        ExitCode::FAILURE
    }
}

// ---------------------------------------------------------------------------
// Self-re-exec helper modes (zero external OS binary dependencies)
// ---------------------------------------------------------------------------

fn current_exe() -> PathBuf {
    env::current_exe().expect("failed to resolve current test executable")
}

fn helper_spec(mode: &str, args: &[&str]) -> (CommandSpec, Vec<(String, String)>) {
    let spec = CommandSpec::argv(current_exe(), args.iter().copied());
    let env_vars = vec![(HELPER_ENV.to_owned(), mode.to_owned())];
    (spec, env_vars)
}

fn parse_procid(text: &str) -> ProcessId {
    let trimmed = text.trim();
    let (pid_str, start_str) = trimmed
        .split_once(':')
        .unwrap_or_else(|| panic!("invalid procid file contents: {trimmed:?}"));
    ProcessId::new(
        pid_str.parse().expect("invalid pid"),
        start_str.parse().expect("invalid start_time"),
    )
}

fn write_current_procid(path: &Path) {
    let me = procid::current().expect("failed to query helper ProcessId");
    write_atomic(
        path,
        format!("{}:{}\n", me.pid, me.start_time).as_bytes(),
        WriteOptions::default(),
    )
    .expect("failed to write helper ProcessId");
}

fn run_helper(mode: &str) -> ExitCode {
    let args: Vec<String> = env::args().skip(1).collect();
    match mode {
        "echo" => helper_echo(&args),
        "flood" => helper_flood(&args),
        "sleep" => helper_sleep(&args),
        "spawn_tree" => helper_spawn_tree(&args),
        "escape_pipes" => helper_escape_pipes(&args),
        "detach_parent" => helper_detach_parent(&args),
        "detach_worker" => helper_detach_worker(&args),
        "lock_holder" => helper_lock_holder(&args),
        other => {
            eprintln!("unknown helper mode: {other}");
            ExitCode::from(2)
        }
    }
}

fn helper_echo(args: &[String]) -> ExitCode {
    let mut stdin_text = String::new();
    let _ = io::stdin().read_to_string(&mut stdin_text);
    let cwd = env::current_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_default();
    let custom = env::var("LEGION_TEST_VAR").unwrap_or_default();
    let lc_all = env::var("LC_ALL").unwrap_or_default();
    let lang = env::var("LANG").unwrap_or_default();
    let exit_code: u8 = args.first().and_then(|s| s.parse().ok()).unwrap_or(0);

    println!("cwd={cwd}");
    println!("var={custom}");
    println!("locale={lc_all}/{lang}");
    println!("stdin={stdin_text}");
    eprintln!("err=sample-stderr");
    ExitCode::from(exit_code)
}

fn helper_flood(args: &[String]) -> ExitCode {
    let out_len: usize = args.first().and_then(|s| s.parse().ok()).unwrap_or(4000);
    let err_len: usize = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(2000);

    let write_pattern = |mut writer: Box<dyn Write>, head: &str, tail: &str, total: usize| {
        let mid_len = total.saturating_sub(head.len() + tail.len());
        let mut payload = Vec::with_capacity(total);
        payload.extend_from_slice(head.as_bytes());
        payload.extend(std::iter::repeat_n(b'm', mid_len));
        payload.extend_from_slice(tail.as_bytes());
        for chunk in payload.chunks(257) {
            let _ = writer.write_all(chunk);
            let _ = writer.flush();
        }
    };

    write_pattern(Box::new(io::stdout()), "HEAD_OUT:", ":TAIL_OUT", out_len);
    write_pattern(Box::new(io::stderr()), "HEAD_ERR:", ":TAIL_ERR", err_len);
    ExitCode::SUCCESS
}

fn helper_sleep(args: &[String]) -> ExitCode {
    let ms: u64 = args.first().and_then(|s| s.parse().ok()).unwrap_or(30_000);
    let id_file = args.get(1).filter(|s| !s.is_empty());
    let ignore_term = args.iter().any(|arg| arg == "--ignore-term");

    if ignore_term {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("helper tokio runtime");
        rt.block_on(async move {
            #[cfg(unix)]
            let _sig = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                .expect("failed to install SIGTERM handler");
            if let Some(path) = id_file {
                write_current_procid(Path::new(path));
            }
            tokio::time::sleep(Duration::from_millis(ms)).await;
        });
        return ExitCode::SUCCESS;
    }

    if let Some(path) = id_file {
        write_current_procid(Path::new(path));
    }
    std::thread::sleep(Duration::from_millis(ms));
    ExitCode::SUCCESS
}

fn helper_spawn_tree(args: &[String]) -> ExitCode {
    let grandchild_id_file = &args[0];
    let ignore_term = args.iter().any(|arg| arg == "--ignore-term");

    let mut cmd = Command::new(current_exe());
    cmd.env(HELPER_ENV, "sleep")
        .arg("30000")
        .arg(grandchild_id_file)
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    if ignore_term {
        cmd.arg("--ignore-term");
    }

    let mut child = cmd.spawn().expect("failed to spawn grandchild");
    let deadline = Instant::now() + Duration::from_secs(5);
    while !Path::new(grandchild_id_file).exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(15));
    }

    println!("READY");
    let _ = io::stdout().flush();

    let _ = child.wait();
    ExitCode::SUCCESS
}

fn helper_escape_pipes(args: &[String]) -> ExitCode {
    let escaped_id_file = &args[0];

    #[cfg(unix)]
    {
        use process_wrap::std::{CommandWrap, ProcessSession};
        let mut wrapped = CommandWrap::with_new(current_exe(), |cmd| {
            cmd.env(HELPER_ENV, "sleep")
                .args(["30000", escaped_id_file])
                .stdin(Stdio::null())
                .stdout(Stdio::inherit())
                .stderr(Stdio::inherit());
        });
        // Starting the grandchild in its own session moves it out of our process group while
        // keeping the inherited stdout/stderr pipes open.
        wrapped.wrap(ProcessSession);
        let _child = wrapped.spawn().expect("failed to spawn escaped grandchild");
    }
    #[cfg(not(unix))]
    let mut child = Command::new(current_exe())
        .env(HELPER_ENV, "sleep")
        .args(["30000", escaped_id_file])
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("failed to spawn grandchild");

    let deadline = Instant::now() + Duration::from_secs(5);
    while !Path::new(escaped_id_file).exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(15));
    }

    println!("READY");
    let _ = io::stdout().flush();

    #[cfg(unix)]
    std::thread::sleep(Duration::from_secs(30));
    #[cfg(not(unix))]
    let _ = child.wait();
    ExitCode::SUCCESS
}

fn helper_detach_parent(args: &[String]) -> ExitCode {
    let worker_id_file = PathBuf::from(&args[0]);
    let subchild_id_file = args[1].clone();
    let stdout_log = PathBuf::from(&args[2]);
    let stderr_log = PathBuf::from(&args[3]);

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("helper tokio runtime");

    let worker_id = rt
        .block_on(spawn_detached(
            DetachOptions::new(CommandSpec::argv(current_exe(), [subchild_id_file]))
                .env(HELPER_ENV, "detach_worker")
                .env("LEGION_DETACH_MARK", "alive-after-parent")
                .stdout_path(stdout_log)
                .stderr_path(stderr_log),
        ))
        .expect("spawn_detached failed inside detach_parent");

    write_atomic(
        &worker_id_file,
        format!("{}:{}\n", worker_id.pid, worker_id.start_time).as_bytes(),
        WriteOptions::default(),
    )
    .expect("failed to write worker_id");

    // Exit immediately so the test can verify the detached worker outlives its parent.
    ExitCode::SUCCESS
}

fn helper_detach_worker(args: &[String]) -> ExitCode {
    let subchild_id_file = &args[0];

    // Give the parent a moment to exit first so log writes happen after parent exit.
    std::thread::sleep(Duration::from_millis(250));

    let mark = env::var("LEGION_DETACH_MARK").unwrap_or_default();
    println!("worker-stdout:{mark}");
    let _ = io::stdout().flush();
    eprintln!("worker-stderr:{mark}");
    let _ = io::stderr().flush();

    // Also spawn a child of our own so the test verifies kill_tree terminates the whole detached tree.
    let mut sub = Command::new(current_exe())
        .env(HELPER_ENV, "sleep")
        .args(["30000", subchild_id_file])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn subchild of detached worker");

    let _ = sub.wait();
    ExitCode::SUCCESS
}

fn helper_lock_holder(args: &[String]) -> ExitCode {
    let lock_path = Path::new(&args[0]);
    let mode = match args.get(1).map(String::as_str) {
        Some("shared") => LockMode::Shared,
        _ => LockMode::Exclusive,
    };

    let _guard = FileLock::try_acquire(lock_path, mode).expect("helper failed to acquire lock");
    println!("LOCKED");
    let _ = io::stdout().flush();

    // Hold the lock until stdin closes or the parent kills us.
    let mut buf = Vec::new();
    let _ = io::stdin().read_to_end(&mut buf);
    ExitCode::SUCCESS
}

// ---------------------------------------------------------------------------
// Integration tests
// ---------------------------------------------------------------------------

async fn wait_for_file(path: &Path, timeout: Duration) -> String {
    let deadline = Instant::now() + timeout;
    loop {
        if let Ok(content) = fs::read_to_string(path)
            && !content.trim().is_empty()
        {
            return content;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for file {}",
            path.display()
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

async fn test_run_argv_echo_env_cwd_stdin_and_exit_code() {
    let dir = tempfile::tempdir().unwrap();
    let (spec, helper_env) = helper_spec("echo", &["0"]);
    let out = run(RunOptions::new(spec)
        .cwd(dir.path())
        .envs(helper_env)
        .env("LEGION_TEST_VAR", "from-test")
        .c_locale()
        .stdin(b"hello stdin".to_vec())
        .timeout(Duration::from_secs(10)))
    .await
    .unwrap();

    assert!(out.success());
    assert_eq!(out.exit_code, Some(0));
    assert_eq!(out.signal, None);
    assert!(!out.timed_out);
    assert!(!out.cancelled);
    let expected_cwd = fs::canonicalize(dir.path()).unwrap();
    let reported_cwd = out
        .stdout
        .text
        .lines()
        .find_map(|line| line.strip_prefix("cwd="))
        .expect("missing cwd line");
    assert_eq!(
        fs::canonicalize(reported_cwd).unwrap(),
        expected_cwd,
        "child did not run in requested cwd"
    );
    assert!(out.stdout.text.contains("var=from-test"));
    assert!(out.stdout.text.contains("locale=C/C"));
    assert!(out.stdout.text.contains("stdin=hello stdin"));
    assert!(out.stderr.text.contains("err=sample-stderr"));

    let (spec_fail, helper_env_fail) = helper_spec("echo", &["42"]);
    let fail = run(RunOptions::new(spec_fail)
        .envs(helper_env_fail)
        .timeout(Duration::from_secs(10)))
    .await
    .unwrap();
    assert!(!fail.success());
    assert_eq!(fail.exit_code, Some(42));
}

async fn test_run_shell_string_works_on_platform_shell() {
    let out = run(RunOptions::new(CommandSpec::shell("echo hello-from-shell"))
        .c_locale()
        .timeout(Duration::from_secs(15)))
    .await
    .unwrap();
    assert!(out.success(), "shell command failed: {out:?}");
    assert_eq!(out.stdout.text.trim(), "hello-from-shell");
}

async fn test_run_live_streaming_and_head_tail_truncation() {
    let streamed_out = Arc::new(Mutex::new(Vec::<u8>::new()));
    let streamed_err = Arc::new(Mutex::new(Vec::<u8>::new()));
    let out_sink = Arc::clone(&streamed_out);
    let err_sink = Arc::clone(&streamed_err);

    let (spec, helper_env) = helper_spec("flood", &["4000", "2000"]);
    let res = run(RunOptions::new(spec)
        .envs(helper_env)
        .capture_limit(128)
        .on_output(Arc::new(move |chunk| match chunk.stream {
            OutputStream::Stdout => out_sink.lock().unwrap().extend_from_slice(&chunk.bytes),
            OutputStream::Stderr => err_sink.lock().unwrap().extend_from_slice(&chunk.bytes),
        }))
        .timeout(Duration::from_secs(10)))
    .await
    .unwrap();

    assert!(res.success());
    assert!(res.stdout.truncated);
    assert_eq!(res.stdout.total_bytes, 4000);
    assert!(res.stdout.text.starts_with("HEAD_OUT:"));
    assert!(res.stdout.text.ends_with(":TAIL_OUT"));

    assert!(res.stderr.truncated);
    assert_eq!(res.stderr.total_bytes, 2000);
    assert!(res.stderr.text.starts_with("HEAD_ERR:"));
    assert!(res.stderr.text.ends_with(":TAIL_ERR"));

    assert_eq!(streamed_out.lock().unwrap().len(), 4000);
    assert_eq!(streamed_err.lock().unwrap().len(), 2000);
}

async fn test_run_missing_program_returns_typed_error() {
    let missing = "legion-nonexistent-binary-integration-test";
    let err = run(RunOptions::new(CommandSpec::argv(
        missing,
        Vec::<&str>::new(),
    )))
    .await
    .unwrap_err();
    match err {
        OsError::ProgramNotFound { program } => assert_eq!(program, missing),
        other => panic!("expected ProgramNotFound, got {other:?}"),
    }
}

async fn test_run_timeout_kills_direct_child_and_grandchildren() {
    let dir = tempfile::tempdir().unwrap();
    let grandchild_file = dir.path().join("grandchild.pid");
    let grandchild_str = grandchild_file.to_string_lossy().into_owned();

    let (spec, helper_env) = helper_spec("spawn_tree", &[&grandchild_str]);
    let res = run(RunOptions::new(spec)
        .envs(helper_env)
        .timeout(Duration::from_millis(1200))
        .kill_grace(Duration::from_millis(200)))
    .await
    .unwrap();

    assert!(res.timed_out, "expected timeout, got {res:?}");
    assert!(!res.cancelled);
    assert!(!res.success());

    let grandchild_id =
        parse_procid(&wait_for_file(&grandchild_file, Duration::from_secs(2)).await);
    assert!(
        !procid::is_alive(&grandchild_id),
        "grandchild {grandchild_id} survived timeout"
    );
}

async fn test_run_cancellation_kills_direct_child_and_grandchildren() {
    let dir = tempfile::tempdir().unwrap();
    let grandchild_file = dir.path().join("grandchild_cancel.pid");
    let grandchild_str = grandchild_file.to_string_lossy().into_owned();

    let token = CancellationToken::new();
    let trigger = token.clone();
    let (spec, helper_env) = helper_spec("spawn_tree", &[&grandchild_str]);
    let res = run(RunOptions::new(spec)
        .envs(helper_env)
        .timeout(Duration::from_secs(20))
        .kill_grace(Duration::from_millis(200))
        .cancel(token)
        .on_output(Arc::new(move |chunk| {
            if String::from_utf8_lossy(&chunk.bytes).contains("READY") {
                trigger.cancel();
            }
        })))
    .await
    .unwrap();

    assert!(res.cancelled, "expected cancellation, got {res:?}");
    assert!(!res.timed_out);
    assert!(!res.success());
    assert!(res.duration < Duration::from_secs(5));

    let grandchild_id =
        parse_procid(&wait_for_file(&grandchild_file, Duration::from_secs(2)).await);
    assert!(
        !procid::is_alive(&grandchild_id),
        "grandchild {grandchild_id} survived cancellation"
    );
}

async fn test_run_escalation_kills_term_ignoring_grandchild() {
    let dir = tempfile::tempdir().unwrap();
    let grandchild_file = dir.path().join("grandchild_ignore_term.pid");
    let grandchild_str = grandchild_file.to_string_lossy().into_owned();

    let token = CancellationToken::new();
    let trigger = token.clone();
    let (spec, helper_env) = helper_spec("spawn_tree", &[&grandchild_str, "--ignore-term"]);
    let res = run(RunOptions::new(spec)
        .envs(helper_env)
        .timeout(Duration::from_secs(20))
        .kill_grace(Duration::from_millis(150))
        .cancel(token)
        .on_output(Arc::new(move |chunk| {
            if String::from_utf8_lossy(&chunk.bytes).contains("READY") {
                trigger.cancel();
            }
        })))
    .await
    .unwrap();

    assert!(res.cancelled);
    let grandchild_id =
        parse_procid(&wait_for_file(&grandchild_file, Duration::from_secs(2)).await);
    assert!(
        !procid::is_alive(&grandchild_id),
        "SIGTERM-ignoring grandchild {grandchild_id} survived SIGKILL escalation"
    );
}

async fn test_run_does_not_hang_when_escaped_grandchild_keeps_pipes_open() {
    let dir = tempfile::tempdir().unwrap();
    let escaped_file = dir.path().join("escaped.pid");
    let escaped_str = escaped_file.to_string_lossy().into_owned();

    let token = CancellationToken::new();
    let trigger = token.clone();
    let (spec, helper_env) = helper_spec("escape_pipes", &[&escaped_str]);
    let res = run(RunOptions::new(spec)
        .envs(helper_env)
        .timeout(Duration::from_secs(20))
        .kill_grace(Duration::from_millis(100))
        .cancel(token)
        .on_output(Arc::new(move |chunk| {
            if String::from_utf8_lossy(&chunk.bytes).contains("READY") {
                trigger.cancel();
            }
        })))
    .await
    .unwrap();

    assert!(res.cancelled);
    assert!(
        res.duration < Duration::from_secs(5),
        "run() hung waiting on an escaped grandchild's pipes: {:?}",
        res.duration
    );

    let escaped_id = parse_procid(&wait_for_file(&escaped_file, Duration::from_secs(2)).await);
    // Clean up the escaped grandchild via kill_tree if it survived (expected on Unix).
    if procid::is_alive(&escaped_id) {
        let report = procid::kill_tree(&escaped_id, Duration::from_millis(200))
            .await
            .unwrap();
        assert!(
            report.all_gone,
            "failed to stop escaped grandchild: {report:?}"
        );
    }
    assert!(!procid::is_alive(&escaped_id));
}

async fn test_detach_survives_parent_exit_appends_logs_and_kill_tree_stops_tree() {
    let dir = tempfile::tempdir().unwrap();
    let worker_id_file = dir.path().join("worker.pid");
    let subchild_id_file = dir.path().join("subchild.pid");
    let stdout_log = dir.path().join("stdout.log");
    let stderr_log = dir.path().join("stderr.log");

    // Pre-populate logs so we verify append semantics (existing lines are preserved).
    write_atomic(&stdout_log, b"existing-stdout\n", WriteOptions::default()).unwrap();
    write_atomic(&stderr_log, b"existing-stderr\n", WriteOptions::default()).unwrap();

    let worker_id_str = worker_id_file.to_string_lossy().into_owned();
    let subchild_id_str = subchild_id_file.to_string_lossy().into_owned();
    let stdout_log_str = stdout_log.to_string_lossy().into_owned();
    let stderr_log_str = stderr_log.to_string_lossy().into_owned();

    let (spec, helper_env) = helper_spec(
        "detach_parent",
        &[
            &worker_id_str,
            &subchild_id_str,
            &stdout_log_str,
            &stderr_log_str,
        ],
    );
    let parent_out = run(RunOptions::new(spec)
        .envs(helper_env)
        .timeout(Duration::from_secs(20)))
    .await
    .unwrap();
    assert!(
        parent_out.success(),
        "detach_parent failed: stdout={} stderr={}",
        parent_out.stdout.text,
        parent_out.stderr.text
    );

    // Parent has now completely exited; verify the detached worker is alive and finishes its writes.
    let worker_id = parse_procid(&wait_for_file(&worker_id_file, Duration::from_secs(5)).await);
    assert!(
        procid::is_alive(&worker_id),
        "detached worker {worker_id} did not survive its parent exiting"
    );

    let subchild_id =
        parse_procid(&wait_for_file(&subchild_id_file, Duration::from_secs(10)).await);
    assert!(procid::is_alive(&worker_id));
    assert!(procid::is_alive(&subchild_id));

    let out_log = fs::read_to_string(&stdout_log).unwrap();
    let err_log = fs::read_to_string(&stderr_log).unwrap();
    assert!(
        out_log.contains("existing-stdout") && out_log.contains("worker-stdout:alive-after-parent"),
        "unexpected stdout log: {out_log:?}"
    );
    assert!(
        err_log.contains("existing-stderr") && err_log.contains("worker-stderr:alive-after-parent"),
        "unexpected stderr log: {err_log:?}"
    );

    // Stop the entire detached process tree (worker + its subchild) using procid::kill_tree.
    let report = procid::kill_tree(&worker_id, Duration::from_millis(300))
        .await
        .unwrap();
    assert!(report.all_gone, "detached tree left survivors: {report:?}");
    assert!(report.survivors.is_empty());
    assert!(!procid::is_alive(&worker_id));
    assert!(!procid::is_alive(&subchild_id));
}

async fn test_procid_kill_tree_leaves_process_untouched_on_mismatched_start_time() {
    let dir = tempfile::tempdir().unwrap();
    let id_file = dir.path().join("live.pid");
    let id_str = id_file.to_string_lossy().into_owned();

    let mut child = Command::new(current_exe())
        .env(HELPER_ENV, "sleep")
        .args(["30000", &id_str])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();

    let real_id = parse_procid(&wait_for_file(&id_file, Duration::from_secs(5)).await);
    assert!(procid::is_alive(&real_id));

    let recycled = ProcessId::new(real_id.pid, real_id.start_time.wrapping_add(999));
    let report = procid::kill_tree(&recycled, Duration::from_millis(50))
        .await
        .unwrap();
    assert!(report.all_gone);
    assert!(
        procid::is_alive(&real_id),
        "kill_tree killed a live process whose start_time did not match"
    );

    let _ = child.kill();
    let _ = child.wait();
    assert!(!procid::is_alive(&real_id));
}

async fn test_atomic_write_interrupted_leaves_no_partial_file() {
    struct InterruptedReader(bool);
    impl Read for InterruptedReader {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            if !self.0 {
                self.0 = true;
                let part = b"partial-prefix";
                let n = part.len().min(buf.len());
                buf[..n].copy_from_slice(&part[..n]);
                return Ok(n);
            }
            Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "aborted mid-stream",
            ))
        }
    }

    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("important.toml");
    write_atomic(&target, b"original=true\n", WriteOptions::default()).unwrap();

    let mut reader = InterruptedReader(false);
    let err = write_atomic_from_reader(&target, &mut reader, WriteOptions::default()).unwrap_err();
    assert!(matches!(err, OsError::Io { .. }));
    assert_eq!(fs::read(&target).unwrap(), b"original=true\n");

    let entries: Vec<_> = fs::read_dir(dir.path()).unwrap().collect();
    assert_eq!(entries.len(), 1, "stray temporary file was left behind");
}

async fn test_lock_contention_and_crash_release_across_processes() {
    let dir = tempfile::tempdir().unwrap();
    let lock_path = dir.path().join("machine.lock");
    let lock_str = lock_path.to_string_lossy().into_owned();

    // 1. Exclusive contention across processes + kernel release when holder is killed abruptly.
    let mut holder = Command::new(current_exe())
        .env(HELPER_ENV, "lock_holder")
        .args([&lock_str, "exclusive"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap();

    let mut line = String::new();
    let mut reader = BufReader::new(holder.stdout.take().unwrap());
    reader.read_line(&mut line).unwrap();
    assert_eq!(line.trim(), "LOCKED");

    assert!(matches!(
        FileLock::try_lock_exclusive(&lock_path),
        Err(OsError::WouldBlock { .. })
    ));
    assert!(matches!(
        FileLock::try_lock_shared(&lock_path),
        Err(OsError::WouldBlock { .. })
    ));
    assert!(matches!(
        FileLock::lock_exclusive_timeout(&lock_path, Duration::from_millis(60)),
        Err(OsError::TimedOut { .. })
    ));

    // Kill the holder abruptly; the kernel must release the lock immediately.
    holder.kill().unwrap();
    let _ = holder.wait();

    let taken = FileLock::lock_exclusive_timeout(&lock_path, Duration::from_secs(2)).unwrap();
    drop(taken);
    assert!(lock_path.exists(), "lock file must never be deleted");

    // 2. Shared lock compatibility across processes.
    let mut shared_holder = Command::new(current_exe())
        .env(HELPER_ENV, "lock_holder")
        .args([&lock_str, "shared"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap();

    let mut line2 = String::new();
    let mut reader2 = BufReader::new(shared_holder.stdout.take().unwrap());
    reader2.read_line(&mut line2).unwrap();
    assert_eq!(line2.trim(), "LOCKED");

    let our_shared = FileLock::try_lock_shared(&lock_path).unwrap();
    assert!(matches!(
        FileLock::try_lock_exclusive(&lock_path),
        Err(OsError::WouldBlock { .. })
    ));
    drop(our_shared);

    // Close stdin so shared_holder exits cleanly, then exclusive lock succeeds.
    drop(shared_holder.stdin.take());
    let _ = shared_holder.wait();
    let exclusive = FileLock::lock_exclusive_timeout(&lock_path, Duration::from_secs(2)).unwrap();
    drop(exclusive);
    assert!(lock_path.exists());
}
