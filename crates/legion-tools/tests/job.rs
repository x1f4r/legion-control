//! The `job` tool family, called through a node the way every door calls it.
//!
//! Uses self-re-exec (`harness = false`): the compiled test binary plays both the detached
//! `job-supervise` supervisor (invoking the exact `legion_tools::job::try_supervise_cli` entry
//! point used by `legion-cli`) and cross-platform helper children so tests never depend on `sh`,
//! `sleep`, or `/tmp`.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, missing_docs)]

mod support;

use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::time::{Duration, Instant};

use legion_core::proto::ErrorCode;
use legion_core::{Call, CancellationToken, Door, Node, Paths};
use legion_os::detach::{DetachHandle, stop_detached};
use legion_os::procid;
use legion_tools::job::exclusive_running_jobs;
use serde_json::{Value, json};
use support::{
    Events, Fixture, HELPER, Test, alive, gone, read_pids, spawn_helper, this_binary, wait_for,
};

const JOB_HELPER: &str = "LEGION_JOB_HELPER";
const JOB_STARTER: &str = "LEGION_JOB_STARTER";

fn main() -> ExitCode {
    // Order matters:
    // 1. Standard support helper children (e.g. `sleep`, `args`, `env`, `cwd`).
    if env::var(HELPER).is_ok() {
        return support::main(&[]);
    }
    // 2. Job-specific helper children (trickle output, process family).
    if let Ok(mode) = env::var(JOB_HELPER) {
        return job_helper(&mode, env::args().skip(1).collect());
    }
    // 3. Real supervisor entry point when `job.start` re-executes this binary.
    if let Some(code) = legion_tools::job::try_supervise_cli(env::args_os().skip(1)) {
        return code;
    }
    // 4. Separate starter process that starts a job and immediately exits.
    if env::var(JOB_STARTER).is_ok() {
        return run_starter_child(env::args().skip(1).collect());
    }

    let tests: &[Test] = &[
        ("job_outlives_the_process_that_started_it", || {
            Box::pin(job_outlives_the_process_that_started_it())
        }),
        ("output_is_captured_and_readable_incrementally", || {
            Box::pin(output_is_captured_and_readable_incrementally())
        }),
        ("exit_code_env_secrecy_and_wait_are_reported", || {
            Box::pin(exit_code_env_secrecy_and_wait_are_reported())
        }),
        ("cancel_stops_the_whole_tree_and_reports_cancelled", || {
            Box::pin(cancel_stops_the_whole_tree_and_reports_cancelled())
        }),
        (
            "forced_cancel_works_when_supervisor_was_killed_first",
            || Box::pin(forced_cancel_works_when_supervisor_was_killed_first()),
        ),
        ("killed_supervisor_and_reboot_yield_interrupted", || {
            Box::pin(killed_supervisor_and_reboot_yield_interrupted())
        }),
        (
            "idempotent_key_deduplicates_conflicts_and_concurrent_starts",
            || Box::pin(idempotent_key_deduplicates_conflicts_and_concurrent_starts()),
        ),
        ("retention_prunes_old_and_excess_finished_jobs_only", || {
            Box::pin(retention_prunes_old_and_excess_finished_jobs_only())
        }),
        ("a_role_without_exec_is_denied_and_cwd_is_confined", || {
            Box::pin(a_role_without_exec_is_denied_and_cwd_is_confined())
        }),
        ("reading_state_never_writes", || {
            Box::pin(reading_state_never_writes())
        }),
        ("exclusive_jobs_and_wait_timeout_or_cancel", || {
            Box::pin(exclusive_jobs_and_wait_timeout_or_cancel())
        }),
    ];
    support::main(tests)
}

fn job_helper(mode: &str, args: Vec<String>) -> ExitCode {
    let mut stdout = std::io::stdout();
    match mode {
        // Prints a first chunk (including multi-byte UTF-8), waits until the gate file appears,
        // then prints a second chunk and a stderr line and exits 0.
        "trickle" => {
            let gate = Path::new(&args[0]);
            let _ = writeln!(stdout, "chunk-1: αβγ");
            let _ = stdout.flush();
            let deadline = Instant::now() + Duration::from_secs(20);
            while !gate.exists() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(20));
            }
            let _ = writeln!(stdout, "chunk-2: €😀");
            let _ = stdout.flush();
            let _ = writeln!(std::io::stderr(), "stderr-line");
            let _ = std::io::stderr().flush();
            ExitCode::SUCCESS
        }
        // Starts a sleeping grandchild, writes "<own pid> <grandchild pid>" atomically, prints
        // "ready", then waits for the grandchild.
        "family" => {
            let mut grandchild = spawn_helper("sleep", &["60"]);
            let line = format!("{} {}", std::process::id(), grandchild.id());
            let target = Path::new(&args[0]);
            let partial = target.with_extension("partial");
            fs::write(&partial, &line).unwrap();
            fs::rename(&partial, target).unwrap();
            let _ = writeln!(stdout, "ready");
            let _ = stdout.flush();
            let _ = grandchild.wait();
            ExitCode::SUCCESS
        }
        other => {
            let _ = writeln!(std::io::stderr(), "unknown job helper {other}");
            ExitCode::from(101)
        }
    }
}

/// Runs `job.start` inside a short-lived child process that exits immediately afterward.
fn run_starter_child(args: Vec<String>) -> ExitCode {
    let legion_home = PathBuf::from(&args[0]);
    let home_dir = PathBuf::from(&args[1]);
    let payload: Value = serde_json::from_str(&args[2]).unwrap();

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    let result = runtime.block_on(async move {
        let node = Node::new(
            Paths::under(&legion_home).with_home(&home_dir),
            legion_tools::registry(),
        );
        node.call(Call::new(Door::Cli, "job.start", payload)).await
    });
    match result {
        Ok(value) => {
            println!("{value}");
            ExitCode::SUCCESS
        }
        Err(error) => {
            println!("{}", serde_json::to_string(&error).unwrap());
            ExitCode::from(1)
        }
    }
}

/// Ensures every detached job and helper PID started by a test is terminated even if the test
/// panics. Declared after `Fixture` so it drops while the temporary directory still exists.
struct Cleanup {
    state_dir: PathBuf,
    jobs: Vec<String>,
    pids: Vec<u32>,
}

impl Cleanup {
    fn new(fixture: &Fixture) -> Self {
        Self {
            state_dir: fixture.node.paths().state_dir().to_path_buf(),
            jobs: Vec::new(),
            pids: Vec::new(),
        }
    }

    fn track_job(&mut self, id: &str) {
        self.jobs.push(id.to_owned());
    }

    fn track_pids(&mut self, pids: &[u32]) {
        self.pids.extend_from_slice(pids);
    }
}

impl Drop for Cleanup {
    fn drop(&mut self) {
        if self.jobs.is_empty() && self.pids.is_empty() {
            return;
        }
        let state_dir = self.state_dir.clone();
        let jobs = std::mem::take(&mut self.jobs);
        let pids = std::mem::take(&mut self.pids);
        // Run cleanup on a fresh OS thread so dropping inside a Tokio worker or during a panic
        // unwind never hits "cannot start a runtime from within a runtime".
        let _ = std::thread::spawn(move || {
            let Ok(rt) = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            else {
                return;
            };
            rt.block_on(async {
                for id in &jobs {
                    let job_json = state_dir.join("jobs").join(id).join("job.json");
                    let Ok(bytes) = fs::read(&job_json) else {
                        continue;
                    };
                    let Ok(value) = serde_json::from_slice::<Value>(&bytes) else {
                        continue;
                    };
                    if let Some(detach_val) = value.get("detach")
                        && let Ok(handle) =
                            serde_json::from_value::<DetachHandle>(detach_val.clone())
                    {
                        let _ = stop_detached(&handle, Duration::ZERO).await;
                    }
                }
                for &pid in &pids {
                    if let Ok(Some(proc_id)) = procid::of_pid(pid) {
                        let _ = procid::kill_tree(&proc_id, Duration::ZERO).await;
                    }
                }
            });
        })
        .join();
    }
}

fn support_helper_args(mode: &str, args: &[&str]) -> Value {
    let mut argv = vec![this_binary()];
    argv.extend(args.iter().map(|arg| (*arg).to_owned()));
    json!({"argv": argv, "env": {HELPER: mode}})
}

fn job_helper_args(mode: &str, args: &[&str]) -> Value {
    let mut argv = vec![this_binary()];
    argv.extend(args.iter().map(|arg| (*arg).to_owned()));
    json!({"argv": argv, "env": {JOB_HELPER: mode}})
}

fn merge_args(mut base: Value, extra: Value) -> Value {
    for (key, value) in extra.as_object().unwrap() {
        if key == "env" {
            let env = base["env"].as_object_mut().unwrap();
            env.extend(value.as_object().unwrap().clone());
        } else {
            base[key] = value.clone();
        }
    }
    base
}

async fn job_outlives_the_process_that_started_it() {
    let fixture = Fixture::new();
    let mut cleanup = Cleanup::new(&fixture);
    let pids_file = fixture.dir.path().join("outlive.pids");
    let payload = merge_args(
        job_helper_args("family", &[&pids_file.to_string_lossy()]),
        json!({"name": "outlive-job"}),
    );

    // Start the job from a separate child process that immediately exits.
    let output = Command::new(this_binary())
        .env(JOB_STARTER, "1")
        .arg(fixture.dir.path())
        .arg(fixture.home())
        .arg(payload.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "starter child failed: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let started: Value = serde_json::from_slice(&output.stdout).unwrap();
    let id = started["id"].as_str().unwrap().to_owned();
    cleanup.track_job(&id);
    assert_eq!(started["name"], json!("outlive-job"));
    assert_eq!(started["state"], json!("running"));
    assert_eq!(started["existing"], json!(false));
    assert!(started["warnings"].is_array());

    // The starter process has already exited; verify the detached job and its family are still
    // alive and observable from this completely separate process.
    let family = read_pids(&pids_file).await;
    cleanup.track_pids(&family);
    assert_eq!(family.len(), 2);
    for &pid in &family {
        assert!(alive(pid), "process {pid} did not outlive starter process");
    }

    let status = fixture.call("job.status", json!({"id": id})).await.unwrap();
    assert_eq!(status["state"], json!("running"), "{status:#}");
    assert!(status["output_bytes"].as_u64().unwrap() > 0);

    let cancelled = fixture.call("job.cancel", json!({"id": id})).await.unwrap();
    assert_eq!(cancelled["state"], json!("cancelled"));
    assert_eq!(cancelled["survivors"], json!([]));
    for pid in family {
        assert!(gone(pid).await, "process {pid} survived cancel");
    }
}

async fn output_is_captured_and_readable_incrementally() {
    let fixture = Fixture::new();
    let mut cleanup = Cleanup::new(&fixture);
    let gate = fixture.dir.path().join("gate");

    let started = fixture
        .call(
            "job.start",
            job_helper_args("trickle", &[&gate.to_string_lossy()]),
        )
        .await
        .unwrap();
    let id = started["id"].as_str().unwrap().to_owned();
    cleanup.track_job(&id);

    // Wait for the first chunk to appear via incremental `offset: 0` polling.
    let first = wait_for(Duration::from_secs(10), || {
        let rt_res = fs::read_to_string(
            fixture
                .node
                .paths()
                .state_dir()
                .join("jobs")
                .join(&id)
                .join("output.log"),
        )
        .ok()?;
        rt_res.contains("chunk-1: αβγ\n").then_some(())
    })
    .await;
    assert!(first.is_some(), "first chunk never arrived");

    // Read in small chunks (5 bytes) to exercise UTF-8 boundary handling across incremental polls.
    let mut offset = 0u64;
    let mut collected = String::new();
    loop {
        let page = fixture
            .call("job.logs", json!({"id": id, "offset": offset, "limit": 5}))
            .await
            .unwrap();
        assert_eq!(page["offset"], json!(offset));
        let piece = page["content"].as_str().unwrap();
        assert!(!piece.contains('\u{fffd}'), "split UTF-8: {page:#}");
        collected.push_str(piece);
        offset = page["next_offset"].as_u64().unwrap();
        if page["eof"] == json!(true) {
            break;
        }
    }
    assert_eq!(collected, "chunk-1: αβγ\n");

    // Release the helper so chunk-2 and stderr arrive, then wait for completion.
    fs::write(&gate, b"go").unwrap();
    let waited = fixture
        .call("job.wait", json!({"id": id, "timeout_s": 15}))
        .await
        .unwrap();
    assert_eq!(waited["state"], json!("succeeded"), "{waited:#}");
    assert_eq!(waited["timed_out"], json!(false));

    // Poll from the previous `next_offset` to get only the newly appended bytes.
    let rest = fixture
        .call("job.logs", json!({"id": id, "offset": offset}))
        .await
        .unwrap();
    assert_eq!(rest["state"], json!("succeeded"));
    assert_eq!(rest["eof"], json!(true));
    assert_eq!(rest["truncated"], json!(false));
    let tail_text = rest["content"].as_str().unwrap();
    assert!(tail_text.contains("chunk-2: €😀"), "{tail_text:?}");
    assert!(tail_text.contains("stderr-line"), "{tail_text:?}");

    // Default tail mode returns the final lines with offset/next_offset.
    let tail_one = fixture
        .call("job.logs", json!({"id": id, "tail": 1}))
        .await
        .unwrap();
    assert_eq!(tail_one["eof"], json!(true));
    assert_eq!(tail_one["truncated"], json!(true));
    assert_eq!(tail_one["content"].as_str().unwrap().trim(), "stderr-line");

    // Combining `offset` and `tail` is invalid.
    let err = fixture
        .call("job.logs", json!({"id": id, "offset": 0, "tail": 5}))
        .await
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::InvalidArgs);
}

async fn exit_code_env_secrecy_and_wait_are_reported() {
    let fixture = Fixture::new();
    let mut cleanup = Cleanup::new(&fixture);

    // Non-zero exit code + secret environment variable.
    let secret = "super-secret-value-999";
    let started = fixture
        .call(
            "job.start",
            merge_args(
                support_helper_args("args", &["hello"]),
                json!({
                    "name": "exit-check",
                    "env": {
                        "LEGION_TEST_EXIT": "7",
                        "MY_API_SECRET": secret
                    }
                }),
            ),
        )
        .await
        .unwrap();
    let id = started["id"].as_str().unwrap().to_owned();
    cleanup.track_job(&id);
    assert!(!started.to_string().contains(secret));

    let waited = fixture
        .call("job.wait", json!({"id": id, "timeout_s": 15}))
        .await
        .unwrap();
    assert_eq!(waited["state"], json!("failed"), "{waited:#}");
    assert_eq!(waited["exit_code"], json!(7));
    assert_eq!(waited["timed_out"], json!(false));
    assert!(waited["finished"].is_string());
    assert!(waited["duration_s"].is_number());
    assert!(!waited.to_string().contains(secret));
    assert_eq!(
        waited["env_names"],
        json!(["LEGION_TEST_EXIT", "LEGION_TEST_HELPER", "MY_API_SECRET"])
    );

    // Confirm `job.json` never holds the secret value while private `env.json` does, and verify
    // private permissions on Unix.
    let job_dir = fixture.node.paths().state_dir().join("jobs").join(&id);
    let record_text = fs::read_to_string(job_dir.join("job.json")).unwrap();
    assert!(!record_text.contains(secret), "{record_text}");
    let env_text = fs::read_to_string(job_dir.join("env.json")).unwrap();
    assert!(env_text.contains(secret));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&fixture.node.paths().state_dir().join("jobs")), 0o700);
        assert_eq!(mode(&job_dir), 0o700);
        assert_eq!(mode(&job_dir.join("job.json")), 0o600);
        assert_eq!(mode(&job_dir.join("env.json")), 0o600);
        assert_eq!(mode(&job_dir.join("exit.json")), 0o600);
        assert_eq!(mode(&job_dir.join("output.log")), 0o600);
    }

    // Confirm `cmd` works through the platform shell and a missing executable records `failed`
    // with a clear detail.
    let shell_job = fixture
        .call("job.start", json!({"cmd": "echo shell-job-ok"}))
        .await
        .unwrap();
    let shell_id = shell_job["id"].as_str().unwrap().to_owned();
    cleanup.track_job(&shell_id);
    let shell_done = fixture
        .call("job.wait", json!({"id": shell_id, "timeout_s": 15}))
        .await
        .unwrap();
    assert_eq!(shell_done["state"], json!("succeeded"), "{shell_done:#}");
    let shell_logs = fixture
        .call("job.logs", json!({"id": shell_id}))
        .await
        .unwrap();
    assert_eq!(
        shell_logs["content"].as_str().unwrap().trim(),
        "shell-job-ok"
    );

    let missing_bin = fixture
        .call(
            "job.start",
            json!({"argv": ["legion-no-such-job-program-7b2e"]}),
        )
        .await
        .unwrap();
    let missing_id = missing_bin["id"].as_str().unwrap().to_owned();
    cleanup.track_job(&missing_id);
    let missing_done = fixture
        .call("job.wait", json!({"id": missing_id, "timeout_s": 15}))
        .await
        .unwrap();
    assert_eq!(missing_done["state"], json!("failed"), "{missing_done:#}");
    assert!(
        missing_done["detail"]
            .as_str()
            .unwrap()
            .contains("legion-no-such-job-program-7b2e")
    );

    // Confirm `job.list` filters by state, sorts newest first, and reports truncation.
    let failed_only = fixture
        .call("job.list", json!({"state": "failed"}))
        .await
        .unwrap();
    let failed_jobs = failed_only["jobs"].as_array().unwrap();
    assert_eq!(failed_jobs.len(), 2);
    assert_eq!(failed_jobs[0]["id"], json!(missing_id));
    assert_eq!(failed_jobs[1]["id"], json!(id));

    let limited = fixture.call("job.list", json!({"limit": 1})).await.unwrap();
    assert_eq!(limited["jobs"].as_array().unwrap().len(), 1);
    assert_eq!(limited["truncated"], json!(true));

    // Confirm the command actually received the env value.
    let env_job = fixture
        .call(
            "job.start",
            merge_args(
                support_helper_args("env", &["LEGION_CHECK_VAL"]),
                json!({"env": {"LEGION_CHECK_VAL": "passed-through"}}),
            ),
        )
        .await
        .unwrap();
    let env_id = env_job["id"].as_str().unwrap().to_owned();
    cleanup.track_job(&env_id);
    let env_done = fixture
        .call("job.wait", json!({"id": env_id, "timeout_s": 15}))
        .await
        .unwrap();
    assert_eq!(env_done["state"], json!("succeeded"));
    assert_eq!(env_done["exit_code"], json!(0));
    let env_logs = fixture
        .call("job.logs", json!({"id": env_id, "offset": 0}))
        .await
        .unwrap();
    assert_eq!(env_logs["content"], json!("passed-through"));

    // Cancelling a finished job is not an error and returns its final state.
    let cancel_finished = fixture.call("job.cancel", json!({"id": id})).await.unwrap();
    assert_eq!(cancel_finished["state"], json!("failed"));
    assert_eq!(cancel_finished["survivors"], json!([]));

    // Unknown id returns `not_found` with a hint mentioning `job.list`.
    let missing = fixture
        .call("job.status", json!({"id": "no-such-job"}))
        .await
        .unwrap_err();
    assert_eq!(missing.code, ErrorCode::NotFound);
    assert!(missing.hint.as_deref().unwrap().contains("job.list"));
}

async fn cancel_stops_the_whole_tree_and_reports_cancelled() {
    let fixture = Fixture::new();
    let mut cleanup = Cleanup::new(&fixture);
    let pids_file = fixture.dir.path().join("cancel.pids");

    let started = fixture
        .call(
            "job.start",
            job_helper_args("family", &[&pids_file.to_string_lossy()]),
        )
        .await
        .unwrap();
    let id = started["id"].as_str().unwrap().to_owned();
    cleanup.track_job(&id);

    let family = read_pids(&pids_file).await;
    cleanup.track_pids(&family);
    assert_eq!(family.len(), 2);

    let cancelled = fixture.call("job.cancel", json!({"id": id})).await.unwrap();
    assert_eq!(cancelled["id"], json!(id));
    assert_eq!(cancelled["state"], json!("cancelled"), "{cancelled:#}");
    assert_eq!(cancelled["survivors"], json!([]));

    for pid in family {
        assert!(gone(pid).await, "process {pid} survived job.cancel");
    }

    let status = fixture.call("job.status", json!({"id": id})).await.unwrap();
    assert_eq!(status["state"], json!("cancelled"));
    assert!(status["finished"].is_string());
}

async fn forced_cancel_works_when_supervisor_was_killed_first() {
    let fixture = Fixture::new();
    let mut cleanup = Cleanup::new(&fixture);
    let pids_file = fixture.dir.path().join("force-cancel.pids");

    let started = fixture
        .call(
            "job.start",
            job_helper_args("family", &[&pids_file.to_string_lossy()]),
        )
        .await
        .unwrap();
    let id = started["id"].as_str().unwrap().to_owned();
    cleanup.track_job(&id);

    let family = read_pids(&pids_file).await;
    cleanup.track_pids(&family);

    let status_before = fixture.call("job.status", json!({"id": id})).await.unwrap();
    let sup_pid = status_before["detach"]["process"]["pid"].as_u64().unwrap() as u32;
    let has_systemd_unit = status_before["detach"]["systemd_unit"].is_string();

    // Kill ONLY the supervisor process abruptly, simulating a supervisor crash.
    let killed = fixture
        .call(
            "sys.kill",
            json!({"pid": sup_pid, "tree": false, "force": true}),
        )
        .await
        .unwrap();
    assert_eq!(killed["stopped"], json!(true), "{killed:#}");
    assert!(gone(sup_pid).await, "supervisor {sup_pid} stayed alive");

    // With the supervisor gone and no exit record, state is `interrupted`.
    let mid = fixture.call("job.status", json!({"id": id})).await.unwrap();
    assert_eq!(mid["state"], json!("interrupted"), "{mid:#}");

    // Forced cancel stops the detached handle and records the job as cancelled & forced.
    let cancelled = fixture
        .call("job.cancel", json!({"id": id, "force": true}))
        .await
        .unwrap();
    assert_eq!(cancelled["state"], json!("cancelled"), "{cancelled:#}");
    assert_eq!(cancelled["survivors"], json!([]));

    let after = fixture.call("job.status", json!({"id": id})).await.unwrap();
    assert_eq!(after["state"], json!("cancelled"), "{after:#}");

    let exit_json: Value = serde_json::from_slice(
        &fs::read(
            fixture
                .node
                .paths()
                .state_dir()
                .join("jobs")
                .join(&id)
                .join("exit.json"),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(exit_json["forced"], json!(true));

    // When Linux systemd scope containment is active (or on Windows where closing the Job Object
    // kills descendants), verify the orphaned command tree is also gone.
    if has_systemd_unit || cfg!(windows) {
        for pid in family {
            assert!(
                gone(pid).await,
                "descendant {pid} survived forced cancel of systemd scope"
            );
        }
    }
}

async fn killed_supervisor_and_reboot_yield_interrupted() {
    let fixture = Fixture::new();
    let mut cleanup = Cleanup::new(&fixture);
    let pids_file = fixture.dir.path().join("interrupted.pids");

    let started = fixture
        .call(
            "job.start",
            job_helper_args("family", &[&pids_file.to_string_lossy()]),
        )
        .await
        .unwrap();
    let id = started["id"].as_str().unwrap().to_owned();
    cleanup.track_job(&id);

    let family = read_pids(&pids_file).await;
    cleanup.track_pids(&family);

    let running = fixture.call("job.status", json!({"id": id})).await.unwrap();
    let sup_pid = running["detach"]["process"]["pid"].as_u64().unwrap() as u32;

    fixture
        .call(
            "sys.kill",
            json!({"pid": sup_pid, "tree": false, "force": true}),
        )
        .await
        .unwrap();
    assert!(gone(sup_pid).await);

    let interrupted = fixture.call("job.status", json!({"id": id})).await.unwrap();
    assert_eq!(
        interrupted["state"],
        json!("interrupted"),
        "{interrupted:#}"
    );
    let detail = interrupted["detail"].as_str().unwrap();
    assert!(
        detail.contains("supervisor") && detail.contains(&sup_pid.to_string()),
        "{detail}"
    );
    assert!(
        detail.contains('s'),
        "detail should mention duration: {detail}"
    );

    // Also simulate a machine reboot by replacing `boot_id` in `job.json`.
    let job_json_path = fixture
        .node
        .paths()
        .state_dir()
        .join("jobs")
        .join(&id)
        .join("job.json");
    let mut rec: Value = serde_json::from_slice(&fs::read(&job_json_path).unwrap()).unwrap();
    rec["boot_id"] = json!("00000000-0000-0000-0000-000000000000");
    fs::write(&job_json_path, serde_json::to_vec(&rec).unwrap()).unwrap();

    let rebooted = fixture.call("job.status", json!({"id": id})).await.unwrap();
    assert_eq!(rebooted["state"], json!("interrupted"));
    assert!(
        rebooted["detail"].as_str().unwrap().contains("rebooted"),
        "{rebooted:#}"
    );
}

async fn idempotent_key_deduplicates_conflicts_and_concurrent_starts() {
    let fixture = Fixture::new();
    let mut cleanup = Cleanup::new(&fixture);
    let base_args = merge_args(
        support_helper_args("args", &["idempotent-ok"]),
        json!({"key": "deploy-42", "env": {"EXTRA": "1"}}),
    );

    let first = fixture.call("job.start", base_args.clone()).await.unwrap();
    let id = first["id"].as_str().unwrap().to_owned();
    cleanup.track_job(&id);
    assert_eq!(first["existing"], json!(false));

    // Wait for completion, then retry with the same key and identical command/cwd/env names (even
    // with a different env value: only env variable names are part of the identity).
    fixture
        .call("job.wait", json!({"id": id, "timeout_s": 15}))
        .await
        .unwrap();

    let retry_args = merge_args(
        support_helper_args("args", &["idempotent-ok"]),
        json!({"key": "deploy-42", "env": {"EXTRA": "different-value"}}),
    );
    let second = fixture.call("job.start", retry_args).await.unwrap();
    assert_eq!(second["id"], json!(id));
    assert_eq!(second["existing"], json!(true));
    assert_eq!(second["state"], json!("succeeded"));

    // Different command with the same key must fail with `conflict` and a hint.
    let diff_cmd = merge_args(
        support_helper_args("args", &["something-else"]),
        json!({"key": "deploy-42", "env": {"EXTRA": "1"}}),
    );
    let conflict = fixture.call("job.start", diff_cmd).await.unwrap_err();
    assert_eq!(conflict.code, ErrorCode::Conflict, "{conflict}");
    assert!(conflict.hint.is_some());

    // Different env names with the same key must also conflict.
    let diff_env = merge_args(
        support_helper_args("args", &["idempotent-ok"]),
        json!({"key": "deploy-42", "env": {"OTHER": "1"}}),
    );
    let conflict_env = fixture.call("job.start", diff_env).await.unwrap_err();
    assert_eq!(conflict_env.code, ErrorCode::Conflict);

    // Two concurrent separate processes starting the same key must create only one job.
    let concurrent_payload = merge_args(
        support_helper_args("sleep", &["30"]),
        json!({"key": "race-key"}),
    )
    .to_string();
    let spawn_starter = || {
        Command::new(this_binary())
            .env(JOB_STARTER, "1")
            .arg(fixture.dir.path())
            .arg(fixture.home())
            .arg(&concurrent_payload)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap()
    };
    let c1 = spawn_starter();
    let c2 = spawn_starter();
    let out1 = c1.wait_with_output().unwrap();
    let out2 = c2.wait_with_output().unwrap();
    assert!(
        out1.status.success(),
        "{}",
        String::from_utf8_lossy(&out1.stderr)
    );
    assert!(
        out2.status.success(),
        "{}",
        String::from_utf8_lossy(&out2.stderr)
    );

    let r1: Value = serde_json::from_slice(&out1.stdout).unwrap();
    let r2: Value = serde_json::from_slice(&out2.stdout).unwrap();
    let race_id = r1["id"].as_str().unwrap().to_owned();
    cleanup.track_job(&race_id);
    assert_eq!(r1["id"], r2["id"], "two jobs created: {r1} vs {r2}");
    assert_ne!(
        r1["existing"], r2["existing"],
        "exactly one call should have existing=true: {r1} vs {r2}"
    );

    let listed = fixture.call("job.list", json!({})).await.unwrap();
    let race_count = listed["jobs"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|j| j["id"] == json!(race_id))
        .count();
    assert_eq!(race_count, 1);

    fixture
        .call("job.cancel", json!({"id": race_id, "force": true}))
        .await
        .unwrap();
}

async fn retention_prunes_old_and_excess_finished_jobs_only() {
    let fixture = Fixture::new();
    let mut cleanup = Cleanup::new(&fixture);

    // Start one genuinely running job first; retention must NEVER delete it.
    let running = fixture
        .call("job.start", support_helper_args("sleep", &["30"]))
        .await
        .unwrap();
    let running_id = running["id"].as_str().unwrap().to_owned();
    cleanup.track_job(&running_id);

    let jobs_root = fixture.node.paths().state_dir().join("jobs");
    let boot = procid::boot_id().unwrap();
    let now = jiff::Timestamp::now();

    // Seed 203 recent finished jobs + 1 finished job older than 30 days.
    let mut seeded_recent = Vec::new();
    for idx in 0..203i64 {
        let ts =
            jiff::Timestamp::from_millisecond(now.as_millisecond() - (205 - idx) * 1000).unwrap();
        let ts_str = ts.strftime("%Y-%m-%dT%H:%M:%S%.3fZ").to_string();
        let id = format!("seed-{idx:04}");
        write_synthetic_finished_job(&jobs_root, &id, &ts_str, &ts_str, &boot);
        seeded_recent.push(id);
    }

    let old_ts =
        jiff::Timestamp::from_millisecond(now.as_millisecond() - 31 * 24 * 60 * 60 * 1000).unwrap();
    let old_str = old_ts.strftime("%Y-%m-%dT%H:%M:%S%.3fZ").to_string();
    write_synthetic_finished_job(&jobs_root, "expired-old", &old_str, &old_str, &boot);

    // Starting a new job triggers retention pruning.
    let fresh = fixture
        .call("job.start", support_helper_args("args", &["trigger"]))
        .await
        .unwrap();
    let fresh_id = fresh["id"].as_str().unwrap().to_owned();
    cleanup.track_job(&fresh_id);
    fixture
        .call("job.wait", json!({"id": fresh_id, "timeout_s": 15}))
        .await
        .unwrap();

    // The 31-day-old job and the 3 oldest excess finished jobs are gone; the newest 200 survive.
    assert!(!jobs_root.join("expired-old").exists());
    for oldest in &seeded_recent[..3] {
        assert!(!jobs_root.join(oldest).exists(), "{oldest} was not pruned");
    }
    for kept in &seeded_recent[3..] {
        assert!(jobs_root.join(kept).exists(), "{kept} was wrongly pruned");
    }
    // The running job is untouched.
    assert!(jobs_root.join(&running_id).exists());
    let still_running = fixture
        .call("job.status", json!({"id": running_id}))
        .await
        .unwrap();
    assert_eq!(still_running["state"], json!("running"));

    fixture
        .call("job.cancel", json!({"id": running_id, "force": true}))
        .await
        .unwrap();
}

fn write_synthetic_finished_job(
    jobs_root: &Path,
    id: &str,
    created: &str,
    finished: &str,
    boot_id: &str,
) {
    let dir = jobs_root.join(id);
    fs::create_dir_all(&dir).unwrap();
    let record = json!({
        "id": id,
        "name": id,
        "cmd": "echo done",
        "env_names": [],
        "principal": "owner",
        "created": created,
        "boot_id": boot_id,
        "exclusive": false
    });
    let exit = json!({
        "outcome": "succeeded",
        "exit_code": 0,
        "finished": finished,
        "forced": false
    });
    fs::write(dir.join("job.json"), serde_json::to_vec(&record).unwrap()).unwrap();
    fs::write(dir.join("exit.json"), serde_json::to_vec(&exit).unwrap()).unwrap();
}

async fn a_role_without_exec_is_denied_and_cwd_is_confined() {
    let fixture = Fixture::new();
    let mut cleanup = Cleanup::new(&fixture);
    fixture.write_config(
        "policy.toml",
        concat!(
            "[principals.observer]\nrole = \"observer\"\n",
            "[principals.\"agent:claude\"]\nrole = \"builder\"\n",
            "[roles.observer]\nallow = [\"effect:read\"]\n",
            "[roles.builder]\nallow = [\"effect:read\", \"effect:exec\"]\npaths = [\"~/projects/**\"]\n",
        ),
    );

    // Observer has read-only access: `job.list` works, `job.start` and `job.cancel` are denied.
    let listed = fixture
        .call_as("observer", "job.list", json!({}))
        .await
        .unwrap();
    assert_eq!(listed["jobs"], json!([]));

    let denied_start = fixture
        .call_as("observer", "job.start", support_helper_args("args", &[]))
        .await
        .unwrap_err();
    assert_eq!(denied_start.code, ErrorCode::Denied);

    let denied_cancel = fixture
        .call_as("observer", "job.cancel", json!({"id": "any-id"}))
        .await
        .unwrap_err();
    assert_eq!(denied_cancel.code, ErrorCode::Denied);

    // Builder is confined to `~/projects/**`: inside succeeds, outside is denied.
    let inside = fixture.home().join("projects").join("app");
    fs::create_dir_all(&inside).unwrap();
    let ok = fixture
        .call_as(
            "agent:claude",
            "job.start",
            merge_args(
                support_helper_args("cwd", &[]),
                json!({"cwd": "~/projects/app"}),
            ),
        )
        .await
        .unwrap();
    let ok_id = ok["id"].as_str().unwrap().to_owned();
    cleanup.track_job(&ok_id);
    let done = fixture
        .call_as(
            "observer",
            "job.wait",
            json!({"id": ok_id, "timeout_s": 15}),
        )
        .await
        .unwrap();
    assert_eq!(done["state"], json!("succeeded"));

    let denied_cwd = fixture
        .call_as(
            "agent:claude",
            "job.start",
            merge_args(support_helper_args("cwd", &[]), json!({"cwd": "~"})),
        )
        .await
        .unwrap_err();
    assert_eq!(denied_cwd.code, ErrorCode::Denied);
}

async fn reading_state_never_writes() {
    let fixture = Fixture::new();
    let mut cleanup = Cleanup::new(&fixture);
    let jobs_root = fixture.node.paths().state_dir().join("jobs");
    let jobs_lock = fixture.node.paths().state_dir().join("jobs.lock");

    // On a fresh node, `job.list` and `exclusive_running_jobs` must not create `jobs/` or
    // `jobs.lock`.
    let empty = fixture.call("job.list", json!({})).await.unwrap();
    assert_eq!(empty["jobs"], json!([]));
    assert!(!jobs_root.exists(), "job.list created jobs directory");
    assert!(!jobs_lock.exists(), "job.list created jobs.lock");
    assert!(
        exclusive_running_jobs(fixture.node.paths())
            .unwrap()
            .is_empty()
    );
    assert!(!jobs_root.exists());

    // Create a finished job and an interrupted job (without a `lock` file!), snapshot every entry
    // under `jobs/`, then run `job.list`, `job.status`, `job.logs`, and `exclusive_running_jobs`
    // and assert the tree is completely identical.
    let finished = fixture
        .call("job.start", support_helper_args("args", &["read-only"]))
        .await
        .unwrap();
    let fin_id = finished["id"].as_str().unwrap().to_owned();
    cleanup.track_job(&fin_id);
    fixture
        .call("job.wait", json!({"id": fin_id, "timeout_s": 15}))
        .await
        .unwrap();

    // Seed a job without `lock` or `exit.json` (old boot id -> interrupted) to verify `probe_lock`
    // does not create a `lock` file on read.
    let no_lock_dir = jobs_root.join("no-lock-interrupted");
    fs::create_dir_all(&no_lock_dir).unwrap();
    fs::write(
        no_lock_dir.join("job.json"),
        serde_json::to_vec(&json!({
            "id": "no-lock-interrupted",
            "name": "rebooted",
            "cmd": "echo x",
            "env_names": [],
            "principal": "owner",
            "created": "2026-01-01T00:00:00.000Z",
            "boot_id": "00000000-0000-0000-0000-000000000000",
            "exclusive": false
        }))
        .unwrap(),
    )
    .unwrap();

    let before = snapshot_tree(&jobs_root);
    let list = fixture.call("job.list", json!({})).await.unwrap();
    assert_eq!(list["jobs"].as_array().unwrap().len(), 2);
    let st1 = fixture
        .call("job.status", json!({"id": fin_id}))
        .await
        .unwrap();
    assert_eq!(st1["state"], json!("succeeded"));
    let st2 = fixture
        .call("job.status", json!({"id": "no-lock-interrupted"}))
        .await
        .unwrap();
    assert_eq!(st2["state"], json!("interrupted"));
    let _ = fixture
        .call("job.logs", json!({"id": fin_id}))
        .await
        .unwrap();
    let _ = fixture
        .call("job.logs", json!({"id": "no-lock-interrupted"}))
        .await
        .unwrap();
    let _ = exclusive_running_jobs(fixture.node.paths()).unwrap();
    let after = snapshot_tree(&jobs_root);

    assert_eq!(before, after, "read operations modified jobs directory");
}

fn snapshot_tree(root: &Path) -> BTreeMap<PathBuf, (bool, u64, Option<std::time::SystemTime>)> {
    let mut out = BTreeMap::new();
    fn walk(
        base: &Path,
        dir: &Path,
        out: &mut BTreeMap<PathBuf, (bool, u64, Option<std::time::SystemTime>)>,
    ) {
        let Ok(entries) = fs::read_dir(dir) else {
            return;
        };
        for entry in entries {
            let entry = entry.unwrap();
            let path = entry.path();
            let meta = fs::symlink_metadata(&path).unwrap();
            let rel = path.strip_prefix(base).unwrap().to_path_buf();
            let is_dir = meta.is_dir();
            out.insert(
                rel,
                (
                    is_dir,
                    if is_dir { 0 } else { meta.len() },
                    meta.modified().ok(),
                ),
            );
            if is_dir {
                walk(base, &path, out);
            }
        }
    }
    walk(root, root, &mut out);
    out
}

async fn exclusive_jobs_and_wait_timeout_or_cancel() {
    let fixture = Fixture::new();
    let mut cleanup = Cleanup::new(&fixture);

    let normal = fixture
        .call(
            "job.start",
            merge_args(support_helper_args("sleep", &["30"]), json!({"name": "bg"})),
        )
        .await
        .unwrap();
    let normal_id = normal["id"].as_str().unwrap().to_owned();
    cleanup.track_job(&normal_id);

    let excl = fixture
        .call(
            "job.start",
            merge_args(
                support_helper_args("sleep", &["30"]),
                json!({"name": "train", "exclusive": true}),
            ),
        )
        .await
        .unwrap();
    let excl_id = excl["id"].as_str().unwrap().to_owned();
    cleanup.track_job(&excl_id);

    // Only the exclusive running job appears in `exclusive_running_jobs`.
    let busy = exclusive_running_jobs(fixture.node.paths()).unwrap();
    assert_eq!(busy.len(), 1, "{busy:?}");
    assert_eq!(busy[0].id, excl_id);
    assert_eq!(busy[0].name, "train");
    assert_eq!(busy[0].since, excl["created"].as_str().unwrap());

    // `job.wait` returns `timed_out: true` when `timeout_s` elapses while running.
    let timed = fixture
        .call("job.wait", json!({"id": excl_id, "timeout_s": 0.15}))
        .await
        .unwrap();
    assert_eq!(timed["timed_out"], json!(true));
    assert_eq!(timed["state"], json!("running"));

    // `job.wait` stops immediately when cancelled.
    let token = CancellationToken::new();
    let events = Events::default();
    let waiter = fixture.call_with(
        "job.wait",
        json!({"id": excl_id, "timeout_s": 30}),
        &events,
        token.clone(),
    );
    let canceler = async {
        tokio::time::sleep(Duration::from_millis(80)).await;
        token.cancel();
    };
    let (wait_res, ()) = tokio::join!(waiter, canceler);
    assert_eq!(wait_res.unwrap_err().code, ErrorCode::Cancelled);

    // Once cancelled, the exclusive job is no longer reported as busy.
    fixture
        .call("job.cancel", json!({"id": excl_id}))
        .await
        .unwrap();
    fixture
        .call("job.cancel", json!({"id": normal_id}))
        .await
        .unwrap();
    assert!(
        exclusive_running_jobs(fixture.node.paths())
            .unwrap()
            .is_empty()
    );
}
