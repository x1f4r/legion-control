//! The `sys` tools, called through a node the way every door calls them.

#![allow(clippy::unwrap_used, clippy::panic, missing_docs)]

mod support;

use std::process::ExitCode;
use std::time::{Duration, Instant};

use legion_core::proto::ErrorCode;
use serde_json::{Value, json};
use support::{Fixture, Test, alive, gone, read_pids, spawn_helper, wait_for};

fn main() -> ExitCode {
    let tests: &[Test] = &[
        ("info_describes_the_machine", || {
            Box::pin(info_describes_the_machine())
        }),
        ("procs_finds_this_process_and_respects_its_limits", || {
            Box::pin(procs_finds_this_process_and_respects_its_limits())
        }),
        ("kill_stops_a_child_and_only_that_child", || {
            Box::pin(kill_stops_a_child_and_only_that_child())
        }),
        ("kill_tree_stops_the_descendants_too", || {
            Box::pin(kill_tree_stops_the_descendants_too())
        }),
        ("kill_refuses_a_reused_pid_and_protected_processes", || {
            Box::pin(kill_refuses_a_reused_pid_and_protected_processes())
        }),
        ("the_runtime_stays_free_while_the_system_is_read", || {
            Box::pin(the_runtime_stays_free_while_the_system_is_read())
        }),
        ("procs_hides_secrets_in_command_lines", || {
            Box::pin(procs_hides_secrets_in_command_lines())
        }),
        ("a_role_without_the_effect_is_denied", || {
            Box::pin(a_role_without_the_effect_is_denied())
        }),
    ];
    support::main(tests)
}

async fn info_describes_the_machine() {
    let fixture = Fixture::new();
    let started = Instant::now();
    let info = fixture.call("sys.info", json!({})).await.unwrap();
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "took {:?}",
        started.elapsed()
    );
    assert_eq!(info["node"], json!(fixture.node.name()));
    assert_eq!(info["os"]["family"], json!(std::env::consts::OS));
    assert_eq!(info["os"]["arch"], json!(std::env::consts::ARCH));
    assert!(info["uptime_s"].as_u64().unwrap() > 0, "{info:#}");
    assert!(info["boot_id"].is_string(), "{info:#}");
    let cpu = &info["cpu"];
    assert!(cpu["cores"].as_u64().unwrap() >= 1, "{cpu}");
    let usage = cpu["usage_pct"].as_f64().unwrap();
    assert!((0.0..=100.0).contains(&usage), "{cpu}");
    assert_eq!(
        cpu.get("load").is_some(),
        std::env::consts::OS != "windows",
        "{cpu}"
    );
    let memory = &info["memory"];
    let total = memory["total"].as_u64().unwrap();
    assert!(total > 0, "{memory}");
    assert!(memory["used"].as_u64().unwrap() <= total, "{memory}");
    assert!(memory["available"].as_u64().unwrap() <= total, "{memory}");
    for disk in info["disks"].as_array().unwrap() {
        assert!(
            disk["mount"].is_string() && disk["fs"].is_string(),
            "{disk}"
        );
        assert!(disk["available"].as_u64().unwrap() <= disk["total"].as_u64().unwrap());
        assert_ne!(disk["fs"], json!("tmpfs"));
    }
    assert!(info["gpus"].is_array());

    assert_no_nulls(&info);

    let error = fixture
        .call("sys.info", json!({"verbose": true}))
        .await
        .unwrap_err();
    assert_eq!(error.code, ErrorCode::InvalidArgs);
}

async fn procs_finds_this_process_and_respects_its_limits() {
    let fixture = Fixture::new();
    let own = std::process::id();
    let exe = std::env::current_exe().unwrap();
    let name = exe.file_stem().unwrap().to_string_lossy().to_uppercase();
    // Upper case, to show the filter ignores case.
    let found = fixture
        .call(
            "sys.procs",
            json!({"filter": name, "sort": "memory", "limit": 500}),
        )
        .await
        .unwrap();
    let procs = found["procs"].as_array().unwrap();
    let me = procs
        .iter()
        .find(|proc| proc["pid"] == json!(own))
        .unwrap_or_else(|| panic!("{own} is not among {found:#}"));
    assert!(me["memory"].as_u64().unwrap() > 0, "{me}");
    assert!(me["cpu_pct"].as_f64().unwrap() >= 0.0, "{me}");
    assert!(me["cmd"].as_str().unwrap().chars().count() <= 301, "{me}");
    let start: jiff::Timestamp = me["start_time"].as_str().unwrap().parse().unwrap();
    assert!(start <= jiff::Timestamp::now());

    assert_no_nulls(&found);

    let busiest = fixture
        .call("sys.procs", json!({"limit": 1}))
        .await
        .unwrap();
    assert_eq!(busiest["procs"].as_array().unwrap().len(), 1);
    assert!(busiest["total"].as_u64().unwrap() > 1);
    assert_eq!(busiest["truncated"], json!(true));

    let none = fixture
        .call(
            "sys.procs",
            json!({"filter": "legion-no-such-process-7c1e"}),
        )
        .await
        .unwrap();
    assert_eq!(none, json!({"procs": [], "total": 0, "truncated": false}));

    for wrong in [
        json!({"limit": 0}),
        json!({"limit": 501}),
        json!({"sort": "name"}),
    ] {
        let error = fixture.call("sys.procs", wrong.clone()).await.unwrap_err();
        assert_eq!(error.code, ErrorCode::InvalidArgs, "{wrong}");
    }
}

/// The start time sys.procs reports for a helper child, found by the test binary's name, which
/// its command line holds.
async fn start_time_of(fixture: &Fixture, pid: u32) -> String {
    let exe = std::env::current_exe().unwrap();
    let name = exe.file_stem().unwrap().to_string_lossy().into_owned();
    let procs = fixture
        .call("sys.procs", json!({"filter": name, "limit": 500}))
        .await
        .unwrap();
    let listed = procs["procs"]
        .as_array()
        .unwrap()
        .iter()
        .find(|proc| proc["pid"] == json!(pid))
        .unwrap_or_else(|| panic!("process {pid} is not among {procs:#}"));
    listed["start_time"].as_str().unwrap().to_owned()
}

async fn kill_stops_a_child_and_only_that_child() {
    let fixture = Fixture::new();
    let pids = fixture.dir.path().join("pids");
    let mut child = spawn_helper("family", &[&pids.to_string_lossy()]);
    let family = read_pids(&pids).await;
    let (pid, grandchild) = (family[0], family[1]);
    assert_eq!(pid, child.id());
    let start_time = start_time_of(&fixture, pid).await;
    let killed = fixture
        .call("sys.kill", json!({"pid": pid, "start_time": start_time}))
        .await
        .unwrap();
    assert_eq!(
        killed,
        json!({"pid": pid, "stopped": true, "survivors": []})
    );
    let status = child.wait().unwrap();
    assert!(!status.success());
    // Without `tree`, what the process started is left running.
    assert!(alive(grandchild), "the grandchild was stopped too");

    let error = fixture
        .call("sys.kill", json!({"pid": pid}))
        .await
        .unwrap_err();
    assert_eq!(error.code, ErrorCode::NotFound, "{}", error.message);

    let killed = fixture
        .call("sys.kill", json!({"pid": grandchild, "force": true}))
        .await
        .unwrap();
    assert_eq!(killed["stopped"], json!(true), "{killed}");
    assert!(gone(grandchild).await);
}

async fn kill_tree_stops_the_descendants_too() {
    let fixture = Fixture::new();
    let pids = fixture.dir.path().join("pids");
    let mut child = spawn_helper("family", &[&pids.to_string_lossy()]);
    let family = read_pids(&pids).await;
    assert_eq!(family[0], child.id());
    let killed = fixture
        .call(
            "sys.kill",
            json!({"pid": child.id(), "tree": true, "force": true}),
        )
        .await
        .unwrap();
    assert_eq!(killed["stopped"], json!(true), "{killed}");
    child.wait().unwrap();
    for pid in family {
        assert!(gone(pid).await, "process {pid} outlived sys.kill");
    }
}

async fn kill_refuses_a_reused_pid_and_protected_processes() {
    let fixture = Fixture::new();
    let mut child = spawn_helper("sleep", &["60"]);
    let pid = child.id();
    wait_for(Duration::from_secs(10), || alive(pid).then_some(()))
        .await
        .unwrap();
    let error = fixture
        .call(
            "sys.kill",
            json!({"pid": pid, "start_time": "2001-01-01T00:00:00Z"}),
        )
        .await
        .unwrap_err();
    assert_eq!(error.code, ErrorCode::Conflict, "{}", error.message);
    assert!(
        alive(pid),
        "a mismatched start time still stopped the process"
    );
    let error = fixture
        .call(
            "sys.kill",
            json!({"pid": pid, "start_time": "last tuesday"}),
        )
        .await
        .unwrap_err();
    assert_eq!(error.code, ErrorCode::InvalidArgs);
    child.kill().unwrap();
    child.wait().unwrap();

    // Legion itself, the test runner that started it, and the system's own.
    let parent = parent_of(std::process::id());
    let mut protected = vec![0, std::process::id()];
    protected.extend(parent);
    if std::env::consts::OS == "windows" {
        protected.push(4);
    } else {
        protected.push(1);
    }
    for pid in protected {
        let error = fixture
            .call("sys.kill", json!({"pid": pid}))
            .await
            .unwrap_err();
        assert_eq!(error.code, ErrorCode::Denied, "{pid}: {}", error.message);
        assert!(error.details.unwrap()["reason"].is_string());
    }
}

async fn a_role_without_the_effect_is_denied() {
    let fixture = Fixture::new();
    fixture.write_config(
        "policy.toml",
        "[principals.phone]\nrole = \"observer\"\n[roles.observer]\nallow = [\"effect:read\"]\n",
    );
    let info = fixture
        .call_as("phone", "sys.info", json!({}))
        .await
        .unwrap();
    assert!(info["memory"]["total"].as_u64().unwrap() > 0);
    let error = fixture
        .call_as("phone", "sys.kill", json!({"pid": std::process::id()}))
        .await
        .unwrap_err();
    assert_eq!(error.code, ErrorCode::Denied);
    assert!(error.message.contains("sys.kill"), "{}", error.message);
    let _: Value = error.details.unwrap();
}

/// The parent of a process, as sys.procs reports it.
fn parent_of(pid: u32) -> Option<u32> {
    let mut system = sysinfo::System::new().ok()?;
    let target = sysinfo::Pid::from_u32(pid);
    system.refresh_processes(sysinfo::ProcessesToUpdate::Some(&[target]), true);
    system.process(target)?.parent().map(sysinfo::Pid::as_u32)
}

/// The sys tools read the system on the blocking pool: on a single-threaded runtime, a task that
/// wakes every few milliseconds keeps waking while they run.
async fn the_runtime_stays_free_while_the_system_is_read() {
    let worst = tokio::task::spawn_blocking(|| {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let fixture = Fixture::new();
            let done = std::sync::atomic::AtomicBool::new(false);
            let ticker = async {
                let mut worst = Duration::ZERO;
                let mut last = Instant::now();
                while !done.load(std::sync::atomic::Ordering::Relaxed) {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                    worst = worst.max(last.elapsed());
                    last = Instant::now();
                }
                worst
            };
            let work = async {
                fixture.call("sys.info", json!({})).await.unwrap();
                fixture
                    .call("sys.procs", json!({"limit": 500}))
                    .await
                    .unwrap();
                let mut child = spawn_helper("sleep", &["60"]);
                let pid = child.id();
                fixture.call("sys.kill", json!({"pid": pid})).await.unwrap();
                child.wait().unwrap();
                done.store(true, std::sync::atomic::Ordering::Relaxed);
            };
            tokio::join!(ticker, work).0
        })
    })
    .await
    .unwrap();
    assert!(
        worst < Duration::from_millis(250),
        "the runtime was held for {worst:?}"
    );
}

async fn procs_hides_secrets_in_command_lines() {
    let fixture = Fixture::new();
    let mut child = spawn_helper(
        "sleep",
        &["60", "--password", "hunter2-7d1c", "--token=s3cr3t-7d1c"],
    );
    let pid = child.id();
    wait_for(Duration::from_secs(10), || alive(pid).then_some(()))
        .await
        .unwrap();
    let exe = std::env::current_exe().unwrap();
    let name = exe.file_stem().unwrap().to_string_lossy().into_owned();
    let listed = fixture
        .call("sys.procs", json!({"filter": name, "limit": 500}))
        .await
        .unwrap();
    let proc = listed["procs"]
        .as_array()
        .unwrap()
        .iter()
        .find(|proc| proc["pid"] == json!(pid))
        .cloned()
        .unwrap_or_else(|| panic!("process {pid} is not among {listed:#}"));
    let cmd = proc["cmd"].as_str().unwrap();
    assert!(
        cmd.ends_with("60 --password [redacted] --token=[redacted]"),
        "{cmd}"
    );
    // A filter cannot find a process by its secret either.
    let probed = fixture
        .call("sys.procs", json!({"filter": "hunter2-7d1c"}))
        .await
        .unwrap();
    assert_eq!(probed["total"], json!(0), "{probed:#}");
    child.kill().unwrap();
    child.wait().unwrap();
}

/// Optional fields are left out, never `null`.
fn assert_no_nulls(value: &Value) {
    match value {
        Value::Null => panic!("a result holds null"),
        Value::Array(items) => items.iter().for_each(assert_no_nulls),
        Value::Object(map) => {
            for (key, value) in map {
                assert!(!value.is_null(), "`{key}` is null");
                assert_no_nulls(value);
            }
        }
        _ => {}
    }
}
