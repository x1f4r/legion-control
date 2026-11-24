//! Cross-platform integration tests for `legion-os`.
//!
//! Uses `harness = false` so the compiled test binary can re-exec itself with
//! `LEGION_OS_HELPER_MODE` set, avoiding any dependency on external Unix or Windows tools
//! (`sh`, `sleep`, `/tmp`) and keeping helper stdout/stderr completely free of test-harness noise.

#![deny(unsafe_code)]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, missing_docs)]

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
use legion_os::detach::{
    DETACH_MARKER_ENV, DetachHandle, DetachOptions, ScopeProbeOverride, detached_active,
    spawn_detached, spawn_detached_for_test, stop_detached,
};
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
        ("env_remove_drops_inherited_vars_in_run_and_detach", || {
            Box::pin(test_env_remove_drops_inherited_vars_in_run_and_detach())
        }),
        ("run_shell_string_works_on_platform_shell", || {
            Box::pin(test_run_shell_string_works_on_platform_shell())
        }),
        ("run_shell_string_round_trips_non_ascii_stdin", || {
            Box::pin(test_run_shell_string_round_trips_non_ascii_stdin())
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
        ("run_delivers_single_sigterm_for_graceful_cleanup", || {
            Box::pin(test_run_delivers_single_sigterm_for_graceful_cleanup())
        }),
        ("run_dropped_future_kills_grandchildren", || {
            Box::pin(test_run_dropped_future_kills_grandchildren())
        }),
        (
            "run_normal_exit_kills_background_grandchild_without_timeout",
            || Box::pin(test_run_normal_exit_kills_background_grandchild_without_timeout()),
        ),
        (
            "run_does_not_hang_when_escaped_grandchild_keeps_pipes_open",
            || Box::pin(test_run_does_not_hang_when_escaped_grandchild_keeps_pipes_open()),
        ),
        (
            "detach_survives_parent_exit_appends_logs_and_kill_tree_stops_tree",
            || Box::pin(test_detach_survives_parent_exit_appends_logs_and_kill_tree_stops_tree()),
        ),
        (
            "detach_duplicate_names_get_distinct_scopes_and_isolate_stop",
            || Box::pin(test_detach_duplicate_names_get_distinct_scopes_and_isolate_stop()),
        ),
        ("detach_preserves_dollar_signs_in_argv_and_shell", || {
            Box::pin(test_detach_preserves_dollar_signs_in_argv_and_shell())
        }),
        (
            "detach_does_not_inherit_extra_open_file_descriptors",
            || Box::pin(test_detach_does_not_inherit_extra_open_file_descriptors()),
        ),
        (
            "detach_active_tracks_root_and_surviving_descendants",
            || Box::pin(test_detach_active_tracks_root_and_surviving_descendants()),
        ),
        (
            "detach_orphan_proof_ignores_unmarked_and_mismatched_processes",
            || Box::pin(test_detach_orphan_proof_ignores_unmarked_and_mismatched_processes()),
        ),
        (
            "detach_stop_without_scope_stops_reparented_setsid_grandchild",
            || Box::pin(test_detach_stop_without_scope_stops_reparented_setsid_grandchild()),
        ),
        (
            "procid_kill_tree_leaves_process_untouched_on_mismatched_start_time",
            || Box::pin(test_procid_kill_tree_leaves_process_untouched_on_mismatched_start_time()),
        ),
        (
            "procid_kill_tree_reports_unverified_orphans_after_root_exits",
            || Box::pin(test_procid_kill_tree_reports_unverified_orphans_after_root_exits()),
        ),
        (
            "procid_kill_process_stops_only_verified_target_and_respects_force",
            || Box::pin(test_procid_kill_process_stops_only_verified_target_and_respects_force()),
        ),
        ("atomic_write_interrupted_leaves_no_partial_file", || {
            Box::pin(test_atomic_write_interrupted_leaves_no_partial_file())
        }),
        ("lock_contention_and_crash_release_across_processes", || {
            Box::pin(test_lock_contention_and_crash_release_across_processes())
        }),
        #[cfg(target_os = "macos")]
        (
            "macos_is_alive_stays_true_until_killed_child_releases_flock",
            || Box::pin(test_macos_is_alive_stays_true_until_killed_child_releases_flock()),
        ),
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

fn wait_for_path_sync(path: &Path, timeout: Duration, what: &str) {
    let deadline = Instant::now() + timeout;
    while !path.exists() {
        assert!(
            Instant::now() < deadline,
            "timed out after {timeout:?} waiting for {what} ({})",
            path.display()
        );
        std::thread::sleep(Duration::from_millis(15));
    }
}

fn run_helper(mode: &str) -> ExitCode {
    let args: Vec<String> = env::args().skip(1).collect();
    match mode {
        "echo" => helper_echo(&args),
        "flood" => helper_flood(&args),
        "sleep" => helper_sleep(&args),
        "spawn_tree" => helper_spawn_tree(&args),
        "spawn_and_exit" => helper_spawn_and_exit(&args),
        "escape_pipes" => helper_escape_pipes(&args),
        "detach_parent" => helper_detach_parent(&args),
        "detach_worker" => helper_detach_worker(&args),
        "lock_holder" => helper_lock_holder(&args),
        "count_sigterm" => helper_count_sigterm(),
        "orphan_root_exit" => helper_orphan_root_exit(&args),
        "detach_fd_parent" => helper_detach_fd_parent(&args),
        "report_open_fds" => helper_report_open_fds(&args),
        "env_remove_driver" => helper_env_remove_driver(&args),
        "dump_env_vars" => helper_dump_env_vars(&args),
        "dump_argv" => helper_dump_argv(&args),
        "setsid_reparent_root" => helper_setsid_reparent_root(&args),
        "setsid_reparent_mid" => helper_setsid_reparent_mid(&args),
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
    wait_for_path_sync(
        Path::new(grandchild_id_file),
        Duration::from_secs(20),
        "grandchild PID file in spawn_tree",
    );

    println!("READY");
    let _ = io::stdout().flush();

    let _ = child.wait();
    ExitCode::SUCCESS
}

fn helper_spawn_and_exit(args: &[String]) -> ExitCode {
    let grandchild_id_file = &args[0];

    // Spawn a long-lived background grandchild that keeps stdout/stderr open, then exit immediately
    // without waiting for it (equivalent to `sleep 999 & echo done`).
    #[allow(clippy::zombie_processes)]
    let _bg = Command::new(current_exe())
        .env(HELPER_ENV, "sleep")
        .args(["30000", grandchild_id_file])
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("failed to spawn background grandchild");

    wait_for_path_sync(
        Path::new(grandchild_id_file),
        Duration::from_secs(20),
        "background grandchild PID file in spawn_and_exit",
    );

    println!("DONE");
    let _ = io::stdout().flush();
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

    wait_for_path_sync(
        Path::new(escaped_id_file),
        Duration::from_secs(20),
        "escaped grandchild PID file in escape_pipes",
    );

    println!("READY");
    let _ = io::stdout().flush();

    #[cfg(unix)]
    std::thread::sleep(Duration::from_secs(30));
    #[cfg(not(unix))]
    let _ = child.wait();
    ExitCode::SUCCESS
}

fn helper_detach_parent(args: &[String]) -> ExitCode {
    let handle_file = PathBuf::from(&args[0]);
    let subchild_id_file = args[1].clone();
    let setsid_id_file = args[2].clone();
    let stdout_log = PathBuf::from(&args[3]);
    let stderr_log = PathBuf::from(&args[4]);
    let job_name = args[5].clone();
    let me = procid::current().expect("failed to query detach_parent ProcessId");
    let parent_id_arg = format!("{}:{}", me.pid, me.start_time);

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("helper tokio runtime");

    let handle = rt
        .block_on(spawn_detached(
            DetachOptions::new(CommandSpec::argv(
                current_exe(),
                [subchild_id_file, setsid_id_file, parent_id_arg],
            ))
            .name(job_name)
            .env(HELPER_ENV, "detach_worker")
            .env("LEGION_DETACH_MARK", "alive-after-parent")
            .stdout_path(stdout_log)
            .stderr_path(stderr_log),
        ))
        .expect("spawn_detached failed inside detach_parent");

    let serialized = serde_json::to_string(&handle).expect("failed to serialize DetachHandle");
    write_atomic(&handle_file, serialized.as_bytes(), WriteOptions::default())
        .expect("failed to write DetachHandle");

    // Exit immediately so the test can verify the detached worker outlives its parent.
    ExitCode::SUCCESS
}

fn helper_detach_worker(args: &[String]) -> ExitCode {
    let subchild_id_file = &args[0];
    let setsid_id_file = &args[1];
    let parent_id = parse_procid(&args[2]);

    // Wait for the parent process to actually exit so log writes deterministically happen after
    // parent exit without relying on a fixed sleep.
    let deadline = Instant::now() + Duration::from_secs(20);
    while procid::is_alive(&parent_id) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(15));
    }

    let mark = env::var("LEGION_DETACH_MARK").unwrap_or_default();
    println!("worker-stdout:{mark}");
    let _ = io::stdout().flush();
    eprintln!("worker-stderr:{mark}");
    let _ = io::stderr().flush();

    // Spawn a regular child so stop_detached/kill_tree verifies tree termination.
    let mut sub = Command::new(current_exe())
        .env(HELPER_ENV, "sleep")
        .args(["30000", subchild_id_file])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn subchild of detached worker");

    // On Unix, also spawn a grandchild in its own session (`setsid`) to verify that a Linux
    // systemd user scope still contains and terminates `setsid` descendants via stop_detached.
    #[cfg(unix)]
    {
        use process_wrap::std::{CommandWrap, ProcessSession};
        let mut escaped = CommandWrap::with_new(current_exe(), |cmd| {
            cmd.env(HELPER_ENV, "sleep")
                .args(["30000", setsid_id_file])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
        });
        escaped.wrap(ProcessSession);
        let _escaped_child = escaped
            .spawn()
            .expect("failed to spawn setsid grandchild of detached worker");
    }
    #[cfg(not(unix))]
    let _ = setsid_id_file;

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

fn helper_count_sigterm() -> ExitCode {
    #[cfg(unix)]
    {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("helper tokio runtime");
        rt.block_on(async {
            let mut sig = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                .expect("failed to install SIGTERM handler");
            println!("READY");
            let _ = io::stdout().flush();

            if sig.recv().await.is_some() {
                let mut count = 1u32;
                let deadline = tokio::time::Instant::now() + Duration::from_millis(80);
                while tokio::time::timeout_at(deadline, sig.recv()).await.is_ok() {
                    count += 1;
                }
                println!("sigterm_count={count}");
                let _ = io::stdout().flush();
            }
        });
    }
    ExitCode::SUCCESS
}

fn helper_orphan_root_exit(args: &[String]) -> ExitCode {
    let root_id_file = Path::new(&args[0]);
    let child_id_file = &args[1];

    write_current_procid(root_id_file);

    // Spawn a long-lived child and exit immediately while the child stays alive in our session
    // (on Unix) or with our PID recorded as its parent_pid (on Windows).
    #[allow(clippy::zombie_processes)]
    let _child = Command::new(current_exe())
        .env(HELPER_ENV, "sleep")
        .args(["30000", child_id_file])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn surviving child");

    wait_for_path_sync(
        Path::new(child_id_file),
        Duration::from_secs(20),
        "surviving child PID file in orphan_root_exit",
    );

    ExitCode::SUCCESS
}

fn helper_detach_fd_parent(args: &[String]) -> ExitCode {
    let handle_file = PathBuf::from(&args[0]);
    let report_file = args[1].clone();
    let canary_file = PathBuf::from(&args[2]);
    let scope_override = match args.get(3).map(String::as_str) {
        Some("--no-systemd") => ScopeProbeOverride::ForceNoSystemd,
        Some("--no-linger") => ScopeProbeOverride::ForceLingerDisabled,
        _ => ScopeProbeOverride::Auto,
    };

    // Verify fd 3 really is open and points to `canary_file` in the parent calling spawn_detached.
    #[cfg(target_os = "linux")]
    {
        let fd3_target = fs::read_link("/proc/self/fd/3")
            .expect("expected inheritable fd 3 to be open in parent");
        assert_eq!(
            fs::canonicalize(&fd3_target).unwrap(),
            fs::canonicalize(&canary_file).unwrap(),
            "fd 3 in parent did not point to canary file"
        );
    }
    #[cfg(not(target_os = "linux"))]
    let _ = canary_file;

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("helper tokio runtime");

    let opts = DetachOptions::new(CommandSpec::argv(current_exe(), [report_file]))
        .name("fd-leak-check")
        .env(HELPER_ENV, "report_open_fds");

    let handle = rt
        .block_on(spawn_detached_for_test(opts, scope_override))
        .expect("spawn_detached failed inside detach_fd_parent");

    let serialized = serde_json::to_string(&handle).expect("failed to serialize DetachHandle");
    write_atomic(&handle_file, serialized.as_bytes(), WriteOptions::default())
        .expect("failed to write DetachHandle");
    ExitCode::SUCCESS
}

fn helper_report_open_fds(args: &[String]) -> ExitCode {
    let report_file = Path::new(&args[0]);
    let mut lines: Vec<String> = Vec::new();
    #[cfg(target_os = "linux")]
    {
        if let Ok(entries) = fs::read_dir("/proc/self/fd") {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().into_owned();
                let target = fs::read_link(entry.path())
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|_| "<unreadable>".to_owned());
                lines.push(format!("{name}->{target}"));
            }
        }
    }
    lines.sort();
    write_atomic(
        report_file,
        (lines.join("\n") + "\n").as_bytes(),
        WriteOptions::default(),
    )
    .expect("failed to write open fd report");
    std::thread::sleep(Duration::from_secs(30));
    ExitCode::SUCCESS
}

fn helper_env_remove_driver(args: &[String]) -> ExitCode {
    let detach_out = PathBuf::from(&args[0]);
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("helper tokio runtime");

    // 1. Verify RunOptions::env_remove drops inherited vars and lets explicit `env` win.
    let (spec, helper_env) = helper_spec("dump_env_vars", &[]);
    let run_out = rt
        .block_on(run(RunOptions::new(spec)
            .envs(helper_env)
            .env_remove("LEGION_DROP_ME")
            .env_removes(["LEGION_DROP_TOO", "LEGION_RESET_ME"])
            .env("LEGION_RESET_ME", "fresh-value")
            .timeout(Duration::from_secs(10))))
        .expect("run with env_remove failed");
    assert!(run_out.success());
    let text = run_out.stdout.text;
    assert!(text.contains("LEGION_DROP_ME=<unset>"), "got: {text}");
    assert!(text.contains("LEGION_DROP_TOO=<unset>"), "got: {text}");
    assert!(text.contains("LEGION_KEEP_ME=keep-val"), "got: {text}");
    assert!(text.contains("LEGION_RESET_ME=fresh-value"), "got: {text}");

    // 2. Verify DetachOptions::env_remove drops inherited vars, lets explicit `env` win, and still
    //    stamps a fresh `LEGION_DETACH_ID` matching `handle.marker` even when `LEGION_DETACH_ID`
    //    is listed in `env_remove`.
    let (dspec, denv) = helper_spec("dump_env_vars", &[]);
    let handle = rt
        .block_on(spawn_detached(
            DetachOptions::new(dspec)
                .name("env-remove")
                .envs(denv)
                .env_remove("LEGION_DROP_ME")
                .env_removes(["LEGION_DROP_TOO", "LEGION_RESET_ME", DETACH_MARKER_ENV])
                .env("LEGION_RESET_ME", "detached-fresh")
                .stdout_path(&detach_out),
        ))
        .expect("spawn_detached with env_remove failed");
    let expected_marker = handle
        .marker
        .as_deref()
        .expect("spawn_detached should stamp DetachHandle.marker");

    let detach_text = rt.block_on(wait_for_detached_file(
        &detach_out,
        Duration::from_secs(20),
        &handle,
        &[&detach_out],
    ));
    assert!(
        detach_text.contains("LEGION_DROP_ME=<unset>"),
        "got: {detach_text}"
    );
    assert!(
        detach_text.contains("LEGION_DROP_TOO=<unset>"),
        "got: {detach_text}"
    );
    assert!(
        detach_text.contains("LEGION_KEEP_ME=keep-val"),
        "got: {detach_text}"
    );
    assert!(
        detach_text.contains("LEGION_RESET_ME=detached-fresh"),
        "got: {detach_text}"
    );
    assert!(
        detach_text.contains(&format!("{DETACH_MARKER_ENV}={expected_marker}")),
        "expected fresh detach marker in child env, got: {detach_text}"
    );
    ExitCode::SUCCESS
}

fn helper_dump_env_vars(_args: &[String]) -> ExitCode {
    for key in [
        "LEGION_DROP_ME",
        "LEGION_DROP_TOO",
        "LEGION_KEEP_ME",
        "LEGION_RESET_ME",
        DETACH_MARKER_ENV,
    ] {
        let val = env::var(key).unwrap_or_else(|_| "<unset>".to_owned());
        println!("{key}={val}");
    }
    let _ = io::stdout().flush();
    ExitCode::SUCCESS
}

fn helper_dump_argv(args: &[String]) -> ExitCode {
    let out_file = Path::new(&args[0]);
    let joined = args[1..].join("\n") + "\n";
    write_atomic(out_file, joined.as_bytes(), WriteOptions::default())
        .expect("failed to write argv dump");
    ExitCode::SUCCESS
}

/// Detached root that spawns an intermediate child which in turn starts a `setsid` grandchild and
/// exits immediately, leaving the grandchild reparented outside the root's parent/group/session
/// while the root itself keeps running.
fn helper_setsid_reparent_root(args: &[String]) -> ExitCode {
    let grandchild_id_file = &args[0];
    let reparented_ready_file = Path::new(&args[1]);
    let ignore_term = args.iter().any(|a| a == "--ignore-term");

    let mut cmd = Command::new(current_exe());
    cmd.env(HELPER_ENV, "setsid_reparent_mid")
        .arg(grandchild_id_file)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if ignore_term {
        cmd.arg("--ignore-term");
    }
    let status = cmd.status().expect("failed to run setsid_reparent_mid");
    assert!(status.success());

    // Signal to the test only after `setsid_reparent_mid` has completely exited so the grandchild
    // is genuinely reparented before `stop_detached` runs.
    write_atomic(reparented_ready_file, b"ready\n", WriteOptions::default())
        .expect("failed to write reparented ready marker");

    // Keep the root alive so `stop_detached` runs while both root and the reparented `setsid`
    // grandchild are live simultaneously.
    std::thread::sleep(Duration::from_secs(60));
    ExitCode::SUCCESS
}

fn helper_setsid_reparent_mid(args: &[String]) -> ExitCode {
    let grandchild_id_file = &args[0];
    let ignore_term = args.iter().any(|a| a == "--ignore-term");

    #[cfg(unix)]
    {
        use process_wrap::std::{CommandWrap, ProcessSession};
        let mut wrapped = CommandWrap::with_new(current_exe(), |cmd| {
            cmd.env(HELPER_ENV, "sleep")
                .args(["60000", grandchild_id_file])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            if ignore_term {
                cmd.arg("--ignore-term");
            }
        });
        wrapped.wrap(ProcessSession);
        let _child = wrapped.spawn().expect("failed to spawn setsid grandchild");
    }
    #[cfg(not(unix))]
    {
        let mut cmd = Command::new(current_exe());
        cmd.env(HELPER_ENV, "sleep")
            .args(["60000", grandchild_id_file])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        if ignore_term {
            cmd.arg("--ignore-term");
        }
        #[allow(clippy::zombie_processes)]
        let _child = cmd.spawn().expect("failed to spawn grandchild");
    }

    wait_for_path_sync(
        Path::new(grandchild_id_file),
        Duration::from_secs(20),
        "grandchild PID file in setsid_reparent_mid",
    );
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

async fn wait_for_detached_file(
    path: &Path,
    timeout: Duration,
    handle: &DetachHandle,
    log_paths: &[&Path],
) -> String {
    let deadline = Instant::now() + timeout;
    loop {
        if let Ok(content) = fs::read_to_string(path)
            && !content.trim().is_empty()
        {
            return content;
        }
        if Instant::now() >= deadline {
            let root_alive = procid::is_alive(&handle.process);
            let active = detached_active(handle).await;
            let mut logs_summary = String::new();
            for log_path in log_paths {
                let tail = match fs::read_to_string(log_path) {
                    Ok(text) => {
                        let mut start = text.len().saturating_sub(2048);
                        while start < text.len() && !text.is_char_boundary(start) {
                            start += 1;
                        }
                        text[start..].to_owned()
                    }
                    Err(err) => format!("<unreadable: {err}>"),
                };
                logs_summary.push_str(&format!("\n--- log {} ---\n{tail}", log_path.display()));
            }
            panic!(
                "timed out waiting for file {}\nhandle: {handle:?}\nwarnings: {:?}\nroot_alive: {root_alive}\ndetached_active: {active:?}{logs_summary}",
                path.display(),
                handle.warnings
            );
        }
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

async fn test_env_remove_drops_inherited_vars_in_run_and_detach() {
    let dir = tempfile::tempdir().unwrap();
    let detach_out = dir.path().join("detach-env.txt");
    let detach_out_str = detach_out.to_string_lossy().into_owned();

    // Launch a helper process with inherited LEGION_* variables so we never mutate this test
    // runner's own environment.
    let (spec, helper_env) = helper_spec("env_remove_driver", &[&detach_out_str]);
    let res = run(RunOptions::new(spec)
        .envs(helper_env)
        .env("LEGION_DROP_ME", "secret-1")
        .env("LEGION_DROP_TOO", "secret-2")
        .env("LEGION_KEEP_ME", "keep-val")
        .env("LEGION_RESET_ME", "old-val")
        .timeout(Duration::from_secs(60)))
    .await
    .unwrap();
    assert!(
        res.success(),
        "env_remove_driver failed: stdout={} stderr={}",
        res.stdout.text,
        res.stderr.text
    );
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

/// What a command reads on stdin through the platform shell arrives as the UTF-8 that was sent.
/// Windows PowerShell decoded it in the OEM code page until the wrapper set the input encoding.
async fn test_run_shell_string_round_trips_non_ascii_stdin() {
    let copy = if cfg!(windows) {
        "[Console]::Out.Write([Console]::In.ReadToEnd())"
    } else {
        "cat"
    };
    let text = "caf\u{e9} \u{2713} Stra\u{df}e \u{65e5}\u{672c}\u{8a9e} \u{1f600}\nsecond line\n";
    let out = run(RunOptions::new(CommandSpec::shell(copy))
        .stdin(text.as_bytes().to_vec())
        .timeout(Duration::from_secs(30)))
    .await
    .unwrap();
    assert!(out.success(), "shell command failed: {out:?}");
    assert_eq!(out.stdout.text, text);
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

    let timeout = if cfg!(windows) {
        Duration::from_secs(4)
    } else {
        Duration::from_millis(1200)
    };
    let (spec, helper_env) = helper_spec("spawn_tree", &[&grandchild_str]);
    let res = run(RunOptions::new(spec)
        .envs(helper_env)
        .timeout(timeout)
        .kill_grace(Duration::from_millis(200)))
    .await
    .unwrap();

    assert!(res.timed_out, "expected timeout, got {res:?}");
    assert!(!res.cancelled);
    assert!(!res.success());

    let grandchild_id =
        parse_procid(&wait_for_file(&grandchild_file, Duration::from_secs(10)).await);
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
        .timeout(Duration::from_secs(30))
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
    assert!(
        res.duration < Duration::from_secs(15),
        "cancellation took unexpectedly long: {:?}",
        res.duration
    );

    let grandchild_id =
        parse_procid(&wait_for_file(&grandchild_file, Duration::from_secs(10)).await);
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
        .timeout(Duration::from_secs(30))
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
        parse_procid(&wait_for_file(&grandchild_file, Duration::from_secs(10)).await);
    assert!(
        !procid::is_alive(&grandchild_id),
        "SIGTERM-ignoring grandchild {grandchild_id} survived SIGKILL escalation"
    );
}

async fn test_run_delivers_single_sigterm_for_graceful_cleanup() {
    #[cfg(unix)]
    {
        let token = CancellationToken::new();
        let trigger = token.clone();
        let (spec, helper_env) = helper_spec("count_sigterm", &[]);
        let res = run(RunOptions::new(spec)
            .envs(helper_env)
            .timeout(Duration::from_secs(10))
            .kill_grace(Duration::from_millis(500))
            .cancel(token)
            .on_output(Arc::new(move |chunk| {
                if String::from_utf8_lossy(&chunk.bytes).contains("READY") {
                    trigger.cancel();
                }
            })))
        .await
        .unwrap();

        assert!(res.cancelled);
        assert_eq!(res.exit_code, Some(0));
        assert!(
            res.stdout.text.contains("sigterm_count=1"),
            "expected exactly one SIGTERM, got stdout: {:?}",
            res.stdout.text
        );
    }
}

async fn test_run_dropped_future_kills_grandchildren() {
    let dir = tempfile::tempdir().unwrap();
    let grandchild_file = dir.path().join("grandchild_drop.pid");
    let grandchild_str = grandchild_file.to_string_lossy().into_owned();

    let ready = Arc::new(tokio::sync::Notify::new());
    let notify_ready = Arc::clone(&ready);
    let (spec, helper_env) = helper_spec("spawn_tree", &[&grandchild_str]);
    let handle = tokio::spawn(run(RunOptions::new(spec)
        .envs(helper_env)
        .timeout(Duration::from_secs(30))
        .on_output(Arc::new(move |chunk| {
            if String::from_utf8_lossy(&chunk.bytes).contains("READY") {
                notify_ready.notify_one();
            }
        }))));

    tokio::time::timeout(Duration::from_secs(20), ready.notified())
        .await
        .expect("timed out waiting 20s for spawn_tree READY");

    let grandchild_id =
        parse_procid(&wait_for_file(&grandchild_file, Duration::from_secs(10)).await);
    assert!(procid::is_alive(&grandchild_id));

    // Abort the task running `run(...)`, dropping its future mid-flight.
    handle.abort();
    let _ = handle.await;

    let deadline = Instant::now() + Duration::from_secs(10);
    while procid::is_alive(&grandchild_id) && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(
        !procid::is_alive(&grandchild_id),
        "grandchild {grandchild_id} leaked after dropping run() future"
    );
}

async fn test_run_normal_exit_kills_background_grandchild_without_timeout() {
    let dir = tempfile::tempdir().unwrap();
    let grandchild_file = dir.path().join("grandchild_bg.pid");
    let grandchild_str = grandchild_file.to_string_lossy().into_owned();

    // No timeout set: once the direct child exits, run() must stop the background grandchild
    // holding stdout open and return promptly instead of hanging or leaking it.
    let (spec, helper_env) = helper_spec("spawn_and_exit", &[&grandchild_str]);
    let res = tokio::time::timeout(
        Duration::from_secs(20),
        run(RunOptions::new(spec)
            .envs(helper_env)
            .kill_grace(Duration::from_millis(150))),
    )
    .await
    .expect("run() hung for 20s waiting on background grandchild's stdout pipe")
    .unwrap();

    assert!(res.success(), "expected clean exit, got {res:?}");
    assert!(!res.timed_out);
    assert!(!res.cancelled);
    assert_eq!(res.stdout.text.trim(), "DONE");

    let grandchild_id =
        parse_procid(&wait_for_file(&grandchild_file, Duration::from_secs(10)).await);
    assert!(
        !procid::is_alive(&grandchild_id),
        "background grandchild {grandchild_id} survived after direct child exited"
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
        .timeout(Duration::from_secs(30))
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
        res.duration < Duration::from_secs(15),
        "run() hung waiting on an escaped grandchild's pipes: {:?}",
        res.duration
    );

    let escaped_id = parse_procid(&wait_for_file(&escaped_file, Duration::from_secs(10)).await);
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
    let handle_file = dir.path().join("worker.handle.json");
    let subchild_id_file = dir.path().join("subchild.pid");
    let setsid_id_file = dir.path().join("setsid.pid");
    let stdout_log = dir.path().join("stdout.log");
    let stderr_log = dir.path().join("stderr.log");
    let job_name = format!("it-{}", std::process::id());

    // Pre-populate logs so we verify append semantics (existing lines are preserved).
    write_atomic(&stdout_log, b"existing-stdout\n", WriteOptions::default()).unwrap();
    write_atomic(&stderr_log, b"existing-stderr\n", WriteOptions::default()).unwrap();

    let handle_str = handle_file.to_string_lossy().into_owned();
    let subchild_id_str = subchild_id_file.to_string_lossy().into_owned();
    let setsid_id_str = setsid_id_file.to_string_lossy().into_owned();
    let stdout_log_str = stdout_log.to_string_lossy().into_owned();
    let stderr_log_str = stderr_log.to_string_lossy().into_owned();

    let (spec, helper_env) = helper_spec(
        "detach_parent",
        &[
            &handle_str,
            &subchild_id_str,
            &setsid_id_str,
            &stdout_log_str,
            &stderr_log_str,
            &job_name,
        ],
    );
    let parent_out = run(RunOptions::new(spec)
        .envs(helper_env)
        .timeout(Duration::from_secs(60)))
    .await
    .unwrap();
    assert!(
        parent_out.success(),
        "detach_parent failed: stdout={} stderr={}",
        parent_out.stdout.text,
        parent_out.stderr.text
    );

    // Parent has now completely exited; deserialize the persisted DetachHandle and verify the
    // detached worker is alive and finished writing to its logs.
    let handle_json = wait_for_file(&handle_file, Duration::from_secs(10)).await;
    let handle: DetachHandle =
        serde_json::from_str(&handle_json).expect("failed to deserialize DetachHandle");
    assert!(
        procid::is_alive(&handle.process),
        "detached worker {} did not survive its parent exiting",
        handle.process
    );

    let subchild_id = parse_procid(
        &wait_for_detached_file(
            &subchild_id_file,
            Duration::from_secs(20),
            &handle,
            &[&stdout_log, &stderr_log],
        )
        .await,
    );
    assert!(procid::is_alive(&handle.process));
    assert!(procid::is_alive(&subchild_id));

    #[cfg(unix)]
    let setsid_id = parse_procid(
        &wait_for_detached_file(
            &setsid_id_file,
            Duration::from_secs(20),
            &handle,
            &[&stdout_log, &stderr_log],
        )
        .await,
    );

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

    // Stop the entire detached process tree via stop_detached(&handle, grace).
    let report = stop_detached(&handle, Duration::from_millis(300))
        .await
        .unwrap();
    assert!(report.all_gone, "detached tree left survivors: {report:?}");
    assert!(report.survivors.is_empty());
    assert!(!procid::is_alive(&handle.process));
    assert!(!procid::is_alive(&subchild_id));

    #[cfg(unix)]
    {
        if let Some(ref unit) = handle.systemd_unit {
            assert!(
                unit.starts_with(&format!("legion-job-{job_name}-")) && unit.ends_with(".scope"),
                "unexpected systemd scope unit name: {unit}"
            );
        }
        assert!(
            !procid::is_alive(&setsid_id),
            "setsid grandchild {setsid_id} survived stop_detached (systemd_unit={:?})",
            handle.systemd_unit
        );
    }
}

async fn test_detach_duplicate_names_get_distinct_scopes_and_isolate_stop() {
    let dir = tempfile::tempdir().unwrap();
    let id1_file = dir.path().join("dup1.pid");
    let id2_file = dir.path().join("dup2.pid");
    let id1_str = id1_file.to_string_lossy().into_owned();
    let id2_str = id2_file.to_string_lossy().into_owned();

    let (spec1, env1) = helper_spec("sleep", &["30000", &id1_str]);
    let (spec2, env2) = helper_spec("sleep", &["30000", &id2_str]);

    let h1 = spawn_detached(DetachOptions::new(spec1).name("dup").envs(env1))
        .await
        .unwrap();
    let h2 = spawn_detached(DetachOptions::new(spec2).name("dup").envs(env2))
        .await
        .unwrap();

    let _ = wait_for_detached_file(&id1_file, Duration::from_secs(20), &h1, &[]).await;
    let _ = wait_for_detached_file(&id2_file, Duration::from_secs(20), &h2, &[]).await;
    assert!(procid::is_alive(&h1.process));
    assert!(procid::is_alive(&h2.process));

    if let (Some(u1), Some(u2)) = (&h1.systemd_unit, &h2.systemd_unit) {
        assert_ne!(u1, u2, "duplicate caller names collided on scope {u1}");
        assert!(u1.starts_with("legion-job-dup-") && u1.ends_with(".scope"));
        assert!(u2.starts_with("legion-job-dup-") && u2.ends_with(".scope"));
    }

    // Also confirm fast-exiting commands succeed cleanly under systemd scope confirmation.
    let (fast_spec, fast_env) = helper_spec("echo", &["0"]);
    let fast = spawn_detached(DetachOptions::new(fast_spec).name("dup").envs(fast_env))
        .await
        .unwrap();
    assert_ne!(fast.process, h1.process);

    // Exercise the linger-disabled `setsid` fallback branch in the same test run.
    #[cfg(target_os = "linux")]
    {
        let linger_id_file = dir.path().join("dup-no-linger.pid");
        let linger_id_str = linger_id_file.to_string_lossy().into_owned();
        let (linger_spec, linger_env) = helper_spec("sleep", &["30000", &linger_id_str]);
        let h_no_linger = spawn_detached_for_test(
            DetachOptions::new(linger_spec).name("dup").envs(linger_env),
            ScopeProbeOverride::ForceLingerDisabled,
        )
        .await
        .unwrap();
        assert!(h_no_linger.systemd_unit.is_none());
        if h1.systemd_unit.is_some() {
            assert!(
                h_no_linger
                    .warnings
                    .iter()
                    .any(|w| w.contains("loginctl enable-linger")),
                "expected linger warning when user manager is reachable, got: {:?}",
                h_no_linger.warnings
            );
        }
        let _ = wait_for_detached_file(&linger_id_file, Duration::from_secs(5), &h_no_linger, &[])
            .await;
        let rl = stop_detached(&h_no_linger, Duration::from_millis(300))
            .await
            .unwrap();
        assert!(rl.all_gone);
        assert!(!procid::is_alive(&h_no_linger.process));
    }

    // Stopping the second handle must not stop the first.
    let r2 = stop_detached(&h2, Duration::from_millis(300))
        .await
        .unwrap();
    assert!(r2.all_gone);
    assert!(!procid::is_alive(&h2.process));
    assert!(
        procid::is_alive(&h1.process),
        "stopping second handle unexpectedly stopped first handle"
    );

    let r1 = stop_detached(&h1, Duration::from_millis(300))
        .await
        .unwrap();
    assert!(r1.all_gone);
    assert!(!procid::is_alive(&h1.process));
}

async fn test_detach_preserves_dollar_signs_in_argv_and_shell() {
    let dir = tempfile::tempdir().unwrap();
    let argv_out = dir.path().join("argv-dollars.txt");
    let argv_out_str = argv_out.to_string_lossy().into_owned();

    // 1. Exact argv with `$$`, `$HOME`, `${X}`, and a lone `$`.
    let (spec, env_vars) = helper_spec("dump_argv", &[&argv_out_str, "$$", "$HOME", "${X}", "$"]);
    let handle = spawn_detached(
        DetachOptions::new(spec)
            .name("dollar-argv")
            .envs(env_vars)
            .env("X", "must-not-expand"),
    )
    .await
    .unwrap();

    let got = wait_for_detached_file(&argv_out, Duration::from_secs(20), &handle, &[]).await;
    let lines: Vec<&str> = got.lines().collect();
    assert_eq!(
        lines,
        vec!["$$", "$HOME", "${X}", "$"],
        "systemd-run or launcher rewrote `$` arguments: {lines:?}"
    );

    // 2. Shell command containing `$$`, `$HOME`, `${X}`, and a lone `$` in single quotes.
    #[cfg(unix)]
    {
        let shell_out = dir.path().join("shell-dollars.txt");
        let shell_handle = spawn_detached(
            DetachOptions::new(CommandSpec::shell(
                "printf '%s|%s|%s|%s\\n' '$$' '$HOME' '${X}' '$'",
            ))
            .name("dollar-shell")
            .env("X", "must-not-expand")
            .stdout_path(&shell_out),
        )
        .await
        .unwrap();

        let shell_text = wait_for_detached_file(
            &shell_out,
            Duration::from_secs(5),
            &shell_handle,
            &[&shell_out],
        )
        .await;
        assert_eq!(
            shell_text.trim(),
            "$$|$HOME|${X}|$",
            "systemd-run corrupted `$` inside shell command string"
        );
    }
}

async fn test_detach_does_not_inherit_extra_open_file_descriptors() {
    #[cfg(target_os = "linux")]
    {
        let dir = tempfile::tempdir().unwrap();
        let canary = dir.path().join("leaked-canary-fd3.txt");
        let canary_str = canary.to_string_lossy().into_owned();
        let exe_str = current_exe().to_string_lossy().into_owned();

        // Exercise every Linux branch in one run: default (`systemd-run --user --scope` when
        // available, else `setsid`), direct `setsid` (`--no-systemd`), and linger-disabled
        // fallback (`--no-linger`).
        for (label, extra_flag) in [
            ("default", ""),
            ("no-systemd", "--no-systemd"),
            ("no-linger", "--no-linger"),
        ] {
            let handle_file = dir.path().join(format!("fd-handle-{label}.json"));
            let report_file = dir.path().join(format!("fd-report-{label}.txt"));
            let handle_str = handle_file.to_string_lossy().into_owned();
            let report_str = report_file.to_string_lossy().into_owned();

            // `exec 3>"$1"` opens `canary` on fd 3 WITHOUT O_CLOEXEC, matching `cmd ... 3>/file`.
            let mut cmd = Command::new("sh");
            cmd.args([
                "-c",
                "exec 3>\"$1\"; shift; exec \"$@\"",
                "_",
                &canary_str,
                &exe_str,
                &handle_str,
                &report_str,
                &canary_str,
            ]);
            if !extra_flag.is_empty() {
                cmd.arg(extra_flag);
            }
            let status = cmd
                .env(HELPER_ENV, "detach_fd_parent")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::inherit())
                .status()
                .unwrap();
            assert!(status.success(), "detach_fd_parent ({label}) failed");

            let handle_json = wait_for_file(&handle_file, Duration::from_secs(5)).await;
            let handle: DetachHandle = serde_json::from_str(&handle_json).unwrap();
            let fd_report =
                wait_for_detached_file(&report_file, Duration::from_secs(5), &handle, &[]).await;

            assert!(
                !fd_report.contains("leaked-canary-fd3.txt"),
                "detached child ({label}) inherited caller's fd 3 canary file:\n{fd_report}"
            );

            let stop = stop_detached(&handle, Duration::from_millis(300))
                .await
                .unwrap();
            assert!(stop.all_gone);
            assert!(!procid::is_alive(&handle.process));
        }
    }
}

async fn test_detach_active_tracks_root_and_surviving_descendants() {
    let dir = tempfile::tempdir().unwrap();

    // Test every Linux branch in the same run (systemd scope when available + both setsid
    // fallbacks), and the default mode on other platforms.
    #[cfg(target_os = "linux")]
    let modes: &[(&str, ScopeProbeOverride)] = &[
        ("default", ScopeProbeOverride::Auto),
        ("no-systemd", ScopeProbeOverride::ForceNoSystemd),
        ("no-linger", ScopeProbeOverride::ForceLingerDisabled),
    ];
    #[cfg(not(target_os = "linux"))]
    let modes: &[(&str, ScopeProbeOverride)] = &[("default", ScopeProbeOverride::Auto)];

    for &(label, scope_override) in modes {
        let root_id_file = dir.path().join(format!("active-root-{label}.pid"));
        let child_id_file = dir.path().join(format!("active-child-{label}.pid"));
        let root_str = root_id_file.to_string_lossy().into_owned();
        let child_str = child_id_file.to_string_lossy().into_owned();

        let (spec, env_vars) = helper_spec("orphan_root_exit", &[&root_str, &child_str]);
        let opts = DetachOptions::new(spec)
            .name(format!("active-{label}"))
            .envs(env_vars);

        let handle = spawn_detached_for_test(opts, scope_override).await.unwrap();
        if scope_override != ScopeProbeOverride::Auto {
            assert!(handle.systemd_unit.is_none());
        }

        let child_id = parse_procid(
            &wait_for_detached_file(&child_id_file, Duration::from_secs(20), &handle, &[]).await,
        );

        // Wait for the root process to finish exiting while its child keeps running.
        let deadline = Instant::now() + Duration::from_secs(15);
        while procid::is_alive(&handle.process) && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(
            !procid::is_alive(&handle.process),
            "root {} ({label}) should have exited",
            handle.process
        );
        assert!(
            procid::is_alive(&child_id),
            "surviving child {child_id} ({label}) should still be alive"
        );

        // `detached_active` must still report true because a descendant under the handle is alive.
        assert!(
            detached_active(&handle).await.unwrap(),
            "detached_active ({label}) returned false while descendant {child_id} was still running"
        );

        let report = stop_detached(&handle, Duration::from_millis(300))
            .await
            .unwrap();
        assert!(
            report.all_gone,
            "stop_detached ({label}) left survivors: {report:?}"
        );
        assert!(!procid::is_alive(&child_id));
        assert!(
            !detached_active(&handle).await.unwrap(),
            "detached_active ({label}) still returned true after tree stopped"
        );
    }
}

async fn test_detach_orphan_proof_ignores_unmarked_and_mismatched_processes() {
    let dir = tempfile::tempdir().unwrap();
    let root_id_file = dir.path().join("proof-root.pid");
    let child_id_file = dir.path().join("proof-child.pid");
    let root_str = root_id_file.to_string_lossy().into_owned();
    let child_str = child_id_file.to_string_lossy().into_owned();

    let (spec, env_vars) = helper_spec("orphan_root_exit", &[&root_str, &child_str]);
    let opts = DetachOptions::new(spec).name("orphan-proof").envs(env_vars);
    let handle = spawn_detached_for_test(opts, ScopeProbeOverride::ForceNoSystemd)
        .await
        .unwrap();
    assert!(handle.systemd_unit.is_none());
    assert!(
        handle.marker.is_some(),
        "detached handle must carry a unique env marker"
    );

    let child_id = parse_procid(
        &wait_for_detached_file(&child_id_file, Duration::from_secs(20), &handle, &[]).await,
    );

    let deadline = Instant::now() + Duration::from_secs(15);
    while procid::is_alive(&handle.process) && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(!procid::is_alive(&handle.process));
    assert!(procid::is_alive(&child_id));

    // A handle referencing the same exited root PID with a DIFFERENT marker must NOT treat this
    // orphan as active and must NOT kill it (simulating an unrelated process reusing the PID).
    let wrong_marker = DetachHandle {
        process: handle.process,
        systemd_unit: None,
        windows_job: None,
        marker: Some("unrelated-job-marker-0000".into()),
        warnings: Vec::new(),
    };
    assert!(
        !detached_active(&wrong_marker).await.unwrap(),
        "detached_active matched an orphan carrying a different marker"
    );
    let wrong_stop = stop_detached(&wrong_marker, Duration::from_millis(150))
        .await
        .unwrap();
    assert!(wrong_stop.all_gone, "unexpected report: {wrong_stop:?}");
    assert!(
        procid::is_alive(&child_id),
        "stop_detached with a mismatched marker killed an unrelated orphan"
    );

    // A legacy handle with no marker, scope, or job must NOT treat an exited root's orphan as
    // active and must report it in `unverified_survivors` without killing it.
    let legacy_handle = DetachHandle {
        process: handle.process,
        systemd_unit: None,
        windows_job: None,
        marker: None,
        warnings: Vec::new(),
    };
    assert!(
        !detached_active(&legacy_handle).await.unwrap(),
        "detached_active with no marker should return false after root exits"
    );
    let legacy_stop = stop_detached(&legacy_handle, Duration::from_millis(150))
        .await
        .unwrap();
    assert!(!legacy_stop.all_gone);
    assert!(legacy_stop.survivors.is_empty());
    assert!(
        legacy_stop.unverified_survivors.contains(&child_id),
        "expected {child_id} in unverified_survivors, got {legacy_stop:?}"
    );
    assert!(
        procid::is_alive(&child_id),
        "legacy stop_detached without marker must not kill unverified orphan"
    );

    // The real handle carrying the matching marker proves ownership and stops the orphan cleanly.
    assert!(detached_active(&handle).await.unwrap());
    let real_stop = stop_detached(&handle, Duration::from_millis(300))
        .await
        .unwrap();
    assert!(
        real_stop.all_gone,
        "real stop left survivors: {real_stop:?}"
    );
    assert!(real_stop.survivors.is_empty());
    assert!(real_stop.unverified_survivors.is_empty());
    assert!(!procid::is_alive(&child_id));
    assert!(!detached_active(&handle).await.unwrap());
}

async fn test_detach_stop_without_scope_stops_reparented_setsid_grandchild() {
    let dir = tempfile::tempdir().unwrap();

    for (label, extra) in [("normal", None), ("ignore-term", Some("--ignore-term"))] {
        let grandchild_file = dir.path().join(format!("reparented-setsid-{label}.pid"));
        let ready_file = dir.path().join(format!("reparented-ready-{label}.txt"));
        let grandchild_str = grandchild_file.to_string_lossy().into_owned();
        let ready_str = ready_file.to_string_lossy().into_owned();
        let args: Vec<&str> = match extra {
            Some(flag) => vec![&grandchild_str, &ready_str, flag],
            None => vec![&grandchild_str, &ready_str],
        };

        let (spec, env_vars) = helper_spec("setsid_reparent_root", &args);
        let opts = DetachOptions::new(spec)
            .name(format!("setsid-noscope-{label}"))
            .envs(env_vars);
        let handle = spawn_detached_for_test(opts, ScopeProbeOverride::ForceNoSystemd)
            .await
            .unwrap();
        assert!(handle.systemd_unit.is_none());
        assert!(handle.marker.is_some());

        let grandchild_id = parse_procid(
            &wait_for_detached_file(&grandchild_file, Duration::from_secs(20), &handle, &[]).await,
        );
        // Wait for the intermediate child to finish exiting so the grandchild is reparented.
        let _ = wait_for_detached_file(&ready_file, Duration::from_secs(20), &handle, &[]).await;
        assert!(procid::is_alive(&handle.process));
        assert!(procid::is_alive(&grandchild_id));
        assert!(detached_active(&handle).await.unwrap());

        let report = stop_detached(&handle, Duration::from_millis(200))
            .await
            .unwrap();
        assert!(
            report.all_gone,
            "stop_detached without scope ({label}) left survivors (root={}, grandchild={grandchild_id}): {report:?}",
            handle.process
        );
        assert!(report.survivors.is_empty());
        assert!(report.unverified_survivors.is_empty());
        assert!(!procid::is_alive(&handle.process));
        assert!(
            !procid::is_alive(&grandchild_id),
            "reparented setsid grandchild {grandchild_id} ({label}) survived stop_detached without scope"
        );
        assert!(
            !detached_active(&handle).await.unwrap(),
            "detached_active still returned true after stop_detached ({label})"
        );
    }
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
    assert!(report.survivors.is_empty());
    assert!(report.unverified_survivors.is_empty());
    assert!(
        procid::is_alive(&real_id),
        "kill_tree killed a live process whose start_time did not match"
    );

    let _ = child.kill();
    let _ = child.wait();
    assert!(!procid::is_alive(&real_id));
}

async fn test_procid_kill_tree_reports_unverified_orphans_after_root_exits() {
    let dir = tempfile::tempdir().unwrap();
    let root_id_file = dir.path().join("orphan_root.pid");
    let child_id_file = dir.path().join("orphan_child.pid");
    let root_id_str = root_id_file.to_string_lossy().into_owned();
    let child_id_str = child_id_file.to_string_lossy().into_owned();

    #[cfg(unix)]
    {
        use process_wrap::std::{CommandWrap, ProcessSession};
        let mut wrapped = CommandWrap::with_new(current_exe(), |cmd| {
            cmd.env(HELPER_ENV, "orphan_root_exit")
                .args([&root_id_str, &child_id_str])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
        });
        wrapped.wrap(ProcessSession);
        let mut child = wrapped.spawn().unwrap();
        let status = child.wait().unwrap();
        assert!(status.success());
    }
    #[cfg(not(unix))]
    {
        let status = Command::new(current_exe())
            .env(HELPER_ENV, "orphan_root_exit")
            .args([&root_id_str, &child_id_str])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap();
        assert!(status.success());
    }

    let root_id = parse_procid(&wait_for_file(&root_id_file, Duration::from_secs(5)).await);
    let child_id = parse_procid(&wait_for_file(&child_id_file, Duration::from_secs(5)).await);

    // Confirm root is completely gone while its child is still running.
    assert!(
        !procid::is_alive(&root_id),
        "root {root_id} should have exited before kill_tree"
    );
    assert!(
        procid::is_alive(&child_id),
        "child {child_id} should still be running after root exited"
    );

    // Plain `procid::kill_tree` without a detach marker must NOT kill orphans by session/group or
    // parent_pid alone once the root has exited; it reports them in `unverified_survivors`.
    let report = procid::kill_tree(&root_id, Duration::from_millis(200))
        .await
        .unwrap();
    assert!(
        !report.all_gone,
        "expected all_gone == false when unverified orphan candidates exist: {report:?}"
    );
    assert!(report.survivors.is_empty());
    assert!(
        report.unverified_survivors.contains(&child_id),
        "expected surviving child {child_id} in unverified_survivors, got {report:?}"
    );
    assert!(
        procid::is_alive(&child_id),
        "plain kill_tree killed unverified orphan {child_id} after root exited"
    );

    // Clean up the test helper directly by its own verified identity.
    let gone = procid::kill_process(&child_id, true, Duration::from_millis(200))
        .await
        .unwrap();
    assert!(gone);
    assert!(!procid::is_alive(&child_id));
}

async fn test_procid_kill_process_stops_only_verified_target_and_respects_force() {
    let dir = tempfile::tempdir().unwrap();
    let grandchild_file = dir.path().join("single_grandchild.pid");
    let grandchild_str = grandchild_file.to_string_lossy().into_owned();

    // 1. Verify kill_process kills only the target PID (leaving its child running) and ignores
    //    mismatched start times.
    let mut parent = Command::new(current_exe())
        .env(HELPER_ENV, "spawn_tree")
        .arg(&grandchild_str)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();

    let mut ready_line = String::new();
    let mut reader = BufReader::new(parent.stdout.take().unwrap());
    reader.read_line(&mut ready_line).unwrap();
    assert_eq!(ready_line.trim(), "READY");

    let parent_id = procid::of_pid(parent.id()).unwrap().unwrap();
    let grandchild_id =
        parse_procid(&wait_for_file(&grandchild_file, Duration::from_secs(5)).await);
    assert!(procid::is_alive(&parent_id));
    assert!(procid::is_alive(&grandchild_id));
    assert!(
        procid::is_protected(&parent_id).is_none(),
        "ordinary child process {parent_id} must not be marked protected"
    );
    assert!(
        procid::is_protected(&grandchild_id).is_none(),
        "ordinary grandchild process {grandchild_id} must not be marked protected"
    );

    let mismatched = ProcessId::new(parent_id.pid, parent_id.start_time.wrapping_add(7));
    assert!(
        procid::kill_process(&mismatched, true, Duration::from_millis(50))
            .await
            .unwrap()
    );
    assert!(
        procid::is_alive(&parent_id),
        "kill_process killed a process with mismatched start_time"
    );

    // Reap the direct child in a background thread so wait() does not leave a zombie blocking
    // is_alive checks if needed (of_pid already treats zombies as not alive).
    let gone = procid::kill_process(&parent_id, false, Duration::from_millis(300))
        .await
        .unwrap();
    let _ = parent.wait();
    assert!(gone);
    assert!(!procid::is_alive(&parent_id));
    assert!(
        procid::is_alive(&grandchild_id),
        "kill_process unexpectedly killed child process {grandchild_id}"
    );

    // Clean up the surviving grandchild via kill_process.
    assert!(
        procid::kill_process(&grandchild_id, true, Duration::from_millis(100))
            .await
            .unwrap()
    );
    assert!(!procid::is_alive(&grandchild_id));

    // 2. Verify force = true kills a SIGTERM-ignoring process immediately without waiting grace.
    let stubborn_file = dir.path().join("stubborn.pid");
    let stubborn_str = stubborn_file.to_string_lossy().into_owned();
    let mut stubborn = Command::new(current_exe())
        .env(HELPER_ENV, "sleep")
        .args(["30000", &stubborn_str, "--ignore-term"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();

    let stubborn_id = parse_procid(&wait_for_file(&stubborn_file, Duration::from_secs(5)).await);
    let started = Instant::now();
    let gone = procid::kill_process(&stubborn_id, true, Duration::from_secs(10))
        .await
        .unwrap();
    let elapsed = started.elapsed();
    let _ = stubborn.wait();
    assert!(gone);
    assert!(!procid::is_alive(&stubborn_id));
    assert!(
        elapsed < Duration::from_secs(5),
        "force = true waited for grace instead of killing immediately ({elapsed:?})"
    );
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

    let taken = FileLock::lock_exclusive_timeout(&lock_path, Duration::from_secs(10)).unwrap();
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
    let exclusive = FileLock::lock_exclusive_timeout(&lock_path, Duration::from_secs(10)).unwrap();
    drop(exclusive);
    assert!(lock_path.exists());
}

#[cfg(target_os = "macos")]
async fn test_macos_is_alive_stays_true_until_killed_child_releases_flock() {
    let dir = tempfile::tempdir().unwrap();
    let lock_path = dir.path().join("exit-race.lock");
    let lock_str = lock_path.to_string_lossy().into_owned();

    // Repeat tight kill -> poll `is_alive` -> immediate `try_lock_exclusive` checks BEFORE
    // reaping the child so the `P_WEXIT` window (where `proc_pidinfo` fails with `ESRCH` while
    // `flock` is still held) is exercised directly.
    for trial in 0..25 {
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

        let holder_id = procid::of_pid(holder.id())
            .unwrap()
            .expect("lock_holder should have a live ProcessId");
        assert!(matches!(
            FileLock::try_lock_exclusive(&lock_path),
            Err(OsError::WouldBlock { .. })
        ));

        holder.kill().unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while procid::is_alive(&holder_id) {
            assert!(
                Instant::now() < deadline,
                "trial {trial}: killed lock holder {holder_id} stayed alive past timeout"
            );
            std::hint::spin_loop();
        }

        // Before reaping `holder`, the lock MUST already be free as soon as `is_alive` is false.
        let acquired = FileLock::try_lock_exclusive(&lock_path).unwrap_or_else(|err| {
            let _ = holder.wait();
            panic!(
                "trial {trial}: flock was still held after procid::is_alive({holder_id}) returned false: {err}"
            );
        });
        drop(acquired);
        let _ = holder.wait();
    }
}
