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
        ("procs_marks_self_and_groups_by_program", || {
            Box::pin(procs_marks_self_and_groups_by_program())
        }),
        ("kill_targets_reports_per_target_outcomes", || {
            Box::pin(kill_targets_reports_per_target_outcomes())
        }),
        ("kill_refuses_thread_ids_of_self_and_child", || {
            Box::pin(kill_refuses_thread_ids_of_self_and_child())
        }),
        ("kill_reports_unreaped_zombie_child_as_gone", || {
            Box::pin(kill_reports_unreaped_zombie_child_as_gone())
        }),
        ("kill_targets_merges_repeated_entries", || {
            Box::pin(kill_targets_merges_repeated_entries())
        }),
    ];
    support::main(tests)
}

async fn info_describes_the_machine() {
    let fixture = Fixture::new();
    let started = Instant::now();
    let info = fixture.call("sys.info", json!({})).await.unwrap();
    assert!(
        // Generous: on a cold Windows runner the boot id alone starts PowerShell.
        started.elapsed() < Duration::from_secs(30),
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
    wait_for(Duration::from_secs(30), || alive(pid).then_some(()))
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
    // Sleeping through a CPU sample on the runtime would hold it for 250 ms. Windows timers tick
    // every 15 ms and its runners stall now and then, so there only a gross stall counts.
    let limit = if std::env::consts::OS == "windows" {
        Duration::from_secs(1)
    } else {
        Duration::from_millis(200)
    };
    assert!(worst < limit, "the runtime was held for {worst:?}");
}

async fn procs_hides_secrets_in_command_lines() {
    let fixture = Fixture::new();
    let mut child = spawn_helper(
        "sleep",
        &["60", "--password", "hunter2-7d1c", "--token=s3cr3t-7d1c"],
    );
    let pid = child.id();
    wait_for(Duration::from_secs(30), || alive(pid).then_some(()))
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

async fn procs_marks_self_and_groups_by_program() {
    let fixture = Fixture::new();
    let own = std::process::id();
    let exe = std::env::current_exe().unwrap();
    let name = exe.file_stem().unwrap().to_string_lossy().into_owned();

    let listed = fixture
        .call(
            "sys.procs",
            json!({"filter": name, "sort": "gpu_memory", "limit": 500}),
        )
        .await
        .unwrap();
    assert_no_nulls(&listed);
    let me = listed["procs"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["pid"] == json!(own))
        .unwrap_or_else(|| panic!("own pid {own} not in {listed:#}"));
    assert_eq!(me["self"], json!(true), "{me}");

    let mut c1 = spawn_helper("sleep", &["60"]);
    let mut c2 = spawn_helper("sleep", &["60"]);
    wait_for(Duration::from_secs(30), || {
        (alive(c1.id()) && alive(c2.id())).then_some(())
    })
    .await
    .unwrap();

    let grouped = fixture
        .call(
            "sys.procs",
            json!({"filter": name, "group": true, "limit": 100}),
        )
        .await
        .unwrap();
    assert_no_nulls(&grouped);
    let rows = grouped["procs"].as_array().unwrap();
    assert!(!rows.is_empty(), "{grouped:#}");
    let row = rows
        .iter()
        .find(|r| {
            r["members"]
                .as_array()
                .is_some_and(|m| m.iter().any(|x| x["pid"] == json!(c1.id())))
        })
        .unwrap_or_else(|| panic!("grouped row for helper children not in {grouped:#}"));
    assert!(row["count"].as_u64().unwrap() >= 3, "{row}");
    assert_eq!(row["more"], json!(0), "{row}");
    assert_eq!(row["self"], json!(true), "{row}");

    let _ = c1.kill();
    let _ = c2.kill();
    let _ = c1.wait();
    let _ = c2.wait();
}

async fn kill_targets_reports_per_target_outcomes() {
    let fixture = Fixture::new();

    // 1. A normal child that exits cleanly on SIGTERM.
    let mut normal = spawn_helper("sleep", &["60"]);
    // 2. A child that exits immediately (so it is `gone`).
    let mut exited = spawn_helper("args", &["done"]);
    exited.wait().unwrap();
    // 3. A child whose start_time we intentionally mismatch (`reused`).
    let mut reused_child = spawn_helper("sleep", &["60"]);
    // 4. On Unix, a stubborn child that ignores SIGTERM until `force: true`.
    #[cfg(unix)]
    let mut stubborn = spawn_helper("stubborn", &[]);

    wait_for(Duration::from_secs(30), || {
        let base_ok = alive(normal.id()) && alive(reused_child.id());
        #[cfg(unix)]
        let ok = base_ok && alive(stubborn.id());
        #[cfg(not(unix))]
        let ok = base_ok;
        ok.then_some(())
    })
    .await
    .unwrap();

    // Give stubborn child a moment to install its SIGTERM handler before signaling.
    #[cfg(unix)]
    tokio::time::sleep(Duration::from_millis(150)).await;

    let normal_start = start_time_of(&fixture, normal.id()).await;
    #[cfg(unix)]
    let stubborn_start = start_time_of(&fixture, stubborn.id()).await;

    let own = std::process::id();
    let parent = parent_of(own);
    let mut targets = vec![
        json!({"pid": normal.id(), "start_time": normal_start}),
        json!({"pid": exited.id(), "start_time": "2020-01-01T00:00:00Z"}),
        json!({"pid": reused_child.id(), "start_time": "2001-01-01T00:00:00Z"}),
        json!({"pid": 0, "start_time": "2020-01-01T00:00:00Z"}),
        json!({"pid": 1, "start_time": "2020-01-01T00:00:00Z"}),
        json!({"pid": own, "start_time": "2020-01-01T00:00:00Z"}),
    ];
    if let Some(ppid) = parent
        && ppid > 1
    {
        targets.push(json!({"pid": ppid, "start_time": "2020-01-01T00:00:00Z"}));
    }
    #[cfg(unix)]
    targets.push(json!({"pid": stubborn.id(), "start_time": stubborn_start}));

    let res = fixture
        .call("sys.kill", json!({"targets": targets, "force": false}))
        .await
        .unwrap();
    assert_no_nulls(&res);
    let results = res["results"].as_array().unwrap();
    assert_eq!(results.len(), targets.len(), "{res:#}");
    #[cfg(unix)]
    assert_eq!(results[0]["status"], json!("stopped"), "{res:#}");
    #[cfg(windows)]
    {
        // Windowless helper outside its own console group cannot be asked gracefully on Windows:
        // reports `still_running` immediately with a sentence saying it must be forced.
        assert_eq!(results[0]["status"], json!("still_running"), "{res:#}");
        assert!(results[0]["message"].is_string(), "{res:#}");
        assert!(
            alive(normal.id()),
            "normal child must not be terminated without force"
        );
        let forced_normal = fixture
            .call(
                "sys.kill",
                json!({
                    "targets": [{"pid": normal.id(), "start_time": normal_start}],
                    "force": true
                }),
            )
            .await
            .unwrap();
        assert_eq!(
            forced_normal["results"][0]["status"],
            json!("stopped"),
            "{forced_normal:#}"
        );
    }
    assert_eq!(results[1]["status"], json!("gone"), "{res:#}");
    assert_eq!(results[2]["status"], json!("reused"), "{res:#}");
    assert!(alive(reused_child.id()), "reused pid must not be stopped");

    // Refusals: pid 0, pid 1, self, and parent/ancestor.
    assert_eq!(results[3]["status"], json!("refused"), "{res:#}");
    assert_eq!(results[3]["code"], json!("denied"), "{res:#}");
    assert!(results[3]["message"].is_string(), "{res:#}");
    assert_eq!(results[4]["status"], json!("refused"), "{res:#}");
    assert_eq!(results[4]["code"], json!("denied"), "{res:#}");
    assert_eq!(results[5]["status"], json!("refused"), "{res:#}");
    assert_eq!(results[5]["code"], json!("denied"), "{res:#}");
    if parent.is_some_and(|p| p > 1) {
        assert_eq!(results[6]["status"], json!("refused"), "{res:#}");
        assert_eq!(results[6]["code"], json!("denied"), "{res:#}");
    }

    #[cfg(unix)]
    {
        let last = results.last().unwrap();
        assert_eq!(last["pid"], json!(stubborn.id()), "{res:#}");
        assert_eq!(last["status"], json!("still_running"), "{res:#}");
        assert!(
            alive(stubborn.id()),
            "stubborn process should still be alive"
        );

        // Now force stop the stubborn process.
        let forced = fixture
            .call(
                "sys.kill",
                json!({
                    "targets": [{"pid": stubborn.id(), "start_time": stubborn_start}],
                    "force": true
                }),
            )
            .await
            .unwrap();
        assert_eq!(
            forced["results"][0]["status"],
            json!("stopped"),
            "{forced:#}"
        );
        let _ = stubborn.wait();
        assert!(gone(stubborn.id()).await);
    }

    // Validation errors.
    for bad in [
        json!({}),
        json!({"targets": []}),
        json!({"pid": 123, "targets": [{"pid": 123, "start_time": "2020-01-01T00:00:00Z"}]}),
        json!({"targets": [{"pid": 123, "start_time": "not-a-time"}]}),
    ] {
        let err = fixture.call("sys.kill", bad.clone()).await.unwrap_err();
        assert_eq!(err.code, ErrorCode::InvalidArgs, "{bad}");
    }

    let _ = normal.wait();
    let _ = reused_child.kill();
    let _ = reused_child.wait();
}

async fn kill_refuses_thread_ids_of_self_and_child() {
    #[cfg(target_os = "linux")]
    {
        let fixture = Fixture::new();
        let own = std::process::id();
        let self_start = start_time_of(&fixture, own).await;

        // 1. Spawn a thread in the current process and verify both `pid` and `targets` refuse its
        // TID without killing the test process.
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let (stop_tx, stop_rx) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            let _ = ready_tx.send(());
            let _ = stop_rx.recv();
        });
        ready_rx.recv().unwrap();
        let self_tid = support::find_other_linux_tid(own).unwrap();
        assert_ne!(self_tid, own);

        let err_single = fixture
            .call("sys.kill", json!({"pid": self_tid, "force": true}))
            .await
            .unwrap_err();
        assert_eq!(
            err_single.code,
            ErrorCode::InvalidArgs,
            "single-pid form must refuse self TID: {}",
            err_single.message
        );
        assert!(
            err_single.message.to_lowercase().contains("not a process"),
            "{}",
            err_single.message
        );

        let res_targets = fixture
            .call(
                "sys.kill",
                json!({
                    "targets": [{"pid": self_tid, "start_time": self_start}],
                    "force": true
                }),
            )
            .await
            .unwrap();
        assert_eq!(
            res_targets["results"][0]["status"],
            json!("refused"),
            "{res_targets:#}"
        );
        assert_eq!(
            res_targets["results"][0]["code"],
            json!("invalid_args"),
            "{res_targets:#}"
        );
        assert!(
            res_targets["results"][0]["message"]
                .as_str()
                .unwrap_or_default()
                .to_lowercase()
                .contains("not a process"),
            "{res_targets:#}"
        );

        let _ = stop_tx.send(());
        worker.join().unwrap();

        // 2. Spawn a multi-threaded child process and verify both `pid` and `targets` refuse its
        // worker TID while the child survives.
        let pid_file = fixture.dir.path().join("threaded.pids");
        let mut child = spawn_helper("threaded", &[pid_file.to_str().unwrap()]);
        let ids = read_pids(&pid_file).await;
        assert_eq!(ids.len(), 2, "{ids:?}");
        let child_pid = ids[0];
        let child_tid = ids[1];
        assert_eq!(child_pid, child.id());
        assert_ne!(
            child_tid, child_pid,
            "child worker TID must differ from child PID"
        );
        let child_start = start_time_of(&fixture, child_pid).await;

        let err_child_single = fixture
            .call("sys.kill", json!({"pid": child_tid, "force": true}))
            .await
            .unwrap_err();
        assert_eq!(
            err_child_single.code,
            ErrorCode::InvalidArgs,
            "{}",
            err_child_single.message
        );
        assert!(
            alive(child_pid),
            "child process must survive refusal of its worker TID"
        );

        let res_child_targets = fixture
            .call(
                "sys.kill",
                json!({
                    "targets": [{"pid": child_tid, "start_time": child_start}],
                    "force": true
                }),
            )
            .await
            .unwrap();
        assert_eq!(
            res_child_targets["results"][0]["status"],
            json!("refused"),
            "{res_child_targets:#}"
        );
        assert_eq!(
            res_child_targets["results"][0]["code"],
            json!("invalid_args"),
            "{res_child_targets:#}"
        );
        assert!(
            alive(child_pid),
            "child process must survive targets refusal of its worker TID"
        );

        let _ = child.kill();
        let _ = child.wait();
    }
}

async fn kill_reports_unreaped_zombie_child_as_gone() {
    #[cfg(unix)]
    {
        let fixture = Fixture::new();
        // Start a child that exits immediately and deliberately do NOT wait() on it yet so it
        // remains an unreaped zombie in the kernel process table.
        let mut child = spawn_helper("args", &["done"]);
        let pid = child.id();

        wait_for(Duration::from_secs(10), || (!alive(pid)).then_some(()))
            .await
            .unwrap();

        let err_single = fixture
            .call("sys.kill", json!({"pid": pid}))
            .await
            .unwrap_err();
        assert_eq!(
            err_single.code,
            ErrorCode::NotFound,
            "unreaped zombie must be NotFound, not Denied: {}",
            err_single.message
        );

        let res_targets = fixture
            .call(
                "sys.kill",
                json!({
                    "targets": [{"pid": pid, "start_time": "2020-01-01T00:00:00Z"}],
                    "force": false
                }),
            )
            .await
            .unwrap();
        assert_eq!(
            res_targets["results"][0]["status"],
            json!("gone"),
            "unreaped zombie must be gone, not refused: {res_targets:#}"
        );

        let _ = child.wait();
    }
}

async fn kill_targets_merges_repeated_entries() {
    let fixture = Fixture::new();
    let mut child = spawn_helper("sleep", &["60"]);
    wait_for(Duration::from_secs(30), || alive(child.id()).then_some(()))
        .await
        .unwrap();
    let start_z = start_time_of(&fixture, child.id()).await;
    let start_offset = start_z.replace('Z', "+00:00");

    let res = fixture
        .call(
            "sys.kill",
            json!({
                "targets": [
                    {"pid": child.id(), "start_time": start_z},
                    {"pid": child.id(), "start_time": start_z},
                    {"pid": child.id(), "start_time": start_offset},
                ],
                "force": true
            }),
        )
        .await
        .unwrap();
    let results = res["results"].as_array().unwrap();
    assert_eq!(
        results.len(),
        1,
        "repeated (pid, start_time) entries must merge into one outcome: {res:#}"
    );
    assert_eq!(results[0]["status"], json!("stopped"), "{res:#}");

    let _ = child.wait();
}
