//! Integration tests for the `svc` tool family, called through a real `Node`.

#![allow(clippy::unwrap_used)]

use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

use legion_core::proto::{Effect, ErrorBody, ErrorCode};
use legion_core::{Call, Door, Node, Paths};
use serde_json::{Value, json};

struct Fixture {
    dir: tempfile::TempDir,
    node: Node,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        let node = Node::new(
            Paths::under(dir.path()).with_home(home),
            legion_tools::registry(),
        );
        Self { dir, node }
    }

    fn write_config(&self, name: &str, text: &str) {
        let config = self.dir.path().join("config");
        std::fs::create_dir_all(&config).unwrap();
        std::fs::write(config.join(name), text).unwrap();
    }

    async fn call(&self, tool: &str, args: Value) -> Result<Value, ErrorBody> {
        self.node.call(Call::new(Door::Cli, tool, args)).await
    }

    async fn call_as(&self, principal: &str, tool: &str, args: Value) -> Result<Value, ErrorBody> {
        let mut call = Call::new(Door::Mcp, tool, args);
        call.as_principal = Some(principal.to_owned());
        self.node.call(call).await
    }
}

#[test]
fn svc_tools_are_registered_with_expected_effects_and_closed_schemas() {
    let registry = legion_tools::registry();
    let expected = [
        ("svc.list", Effect::Read),
        ("svc.status", Effect::Read),
        ("svc.start", Effect::Exec),
        ("svc.stop", Effect::Exec),
        ("svc.restart", Effect::Exec),
        ("svc.logs", Effect::Read),
    ];
    for (name, effect) in expected {
        let spec = registry
            .get(name)
            .unwrap_or_else(|| panic!("{name} is missing from registry"))
            .spec();
        assert_eq!(spec.effect, effect, "{name}");
        assert!(spec.description.ends_with('.'), "{name}");
        assert_eq!(spec.input_schema["type"], json!("object"), "{name}");
        assert_eq!(
            spec.input_schema["additionalProperties"],
            json!(false),
            "{name}"
        );
    }
}

#[tokio::test]
async fn svc_tools_validate_arguments_and_service_names() {
    let fixture = Fixture::new();

    for bad_list in [
        json!({"limit": 0}),
        json!({"limit": 1001}),
        json!({"scope": "global"}),
        json!({"state": "paused"}),
        json!({"unknown": true}),
    ] {
        let err = fixture
            .call("svc.list", bad_list.clone())
            .await
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::InvalidArgs, "{bad_list}");
    }

    let too_long = "a".repeat(257);
    for bad_name in [
        "",
        "   ",
        "-bad",
        "legion-review-*",
        "worker-?.service",
        "worker-[0].service",
        "foo/bar",
        "foo\\bar",
        "bad\nname",
        "bad\0name",
        &too_long,
    ] {
        for tool in [
            "svc.status",
            "svc.start",
            "svc.stop",
            "svc.restart",
            "svc.logs",
        ] {
            let err = fixture
                .call(tool, json!({"name": bad_name}))
                .await
                .unwrap_err();
            assert_eq!(err.code, ErrorCode::InvalidArgs, "{tool} {bad_name:?}");
        }
    }

    for bad_logs in [
        json!({"name": "demo", "lines": 0}),
        json!({"name": "demo", "lines": 2001}),
        json!({"name": "demo", "since": 0}),
        json!({"name": "demo", "since": -10}),
        json!({"name": "demo", "since": "not-a-timestamp"}),
    ] {
        let err = fixture
            .call("svc.logs", bad_logs.clone())
            .await
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::InvalidArgs, "{bad_logs}");
    }
}

#[tokio::test]
async fn svc_policy_and_node_toml_are_enforced() {
    let fixture = Fixture::new();
    fixture.write_config(
        "policy.toml",
        "[principals.observer]\nrole = \"observer\"\n[roles.observer]\nallow = [\"effect:read\"]\n",
    );
    for tool in ["svc.start", "svc.stop", "svc.restart"] {
        let err = fixture
            .call_as("observer", tool, json!({"name": "demo"}))
            .await
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::Denied, "{tool}");
    }

    // Strict parsing of [svc] in node.toml fails control calls when a typo is present.
    fixture.write_config("node.toml", "[svc]\nsudo_mode = true\n");
    let err = fixture
        .call("svc.start", json!({"name": "demo"}))
        .await
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::Failed);
    assert!(
        err.message.contains("node.toml") && err.message.contains("line 2"),
        "{}",
        err.message
    );
}

/// Ensures a transient user systemd unit created by a test is always stopped and cleaned up,
/// even if an assertion panics.
struct TransientUnitGuard {
    unit: String,
}

impl Drop for TransientUnitGuard {
    fn drop(&mut self) {
        let _ = Command::new("systemctl")
            .args(["--user", "stop", "--no-pager", "--", &self.unit])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        let _ = Command::new("systemctl")
            .args(["--user", "reset-failed", "--no-pager", "--", &self.unit])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

fn linux_user_systemd_available() -> bool {
    if std::env::consts::OS != "linux" {
        return false;
    }
    let mgr_ok = Command::new("systemctl")
        .args(["--user", "show-environment"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success());
    let run_ok = Command::new("systemd-run")
        .args(["--user", "--version"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success());
    mgr_ok && run_ok
}

#[tokio::test]
async fn linux_user_systemd_transient_unit_list_status_logs_and_stop() {
    if !linux_user_systemd_available() {
        return;
    }

    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let base = format!("legion-svc-test-{}-{nanos}", std::process::id());
    let unit = format!("{base}.service");
    let _guard = TransientUnitGuard { unit: unit.clone() };

    let started = Command::new("systemd-run")
        .args([
            "--user",
            &format!("--unit={base}"),
            "--description=Legion svc integration test",
            "/bin/sh",
            "-c",
            "printf 'legion-svc-started\\n'; exec sleep 60",
        ])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(
        started.status.success(),
        "systemd-run failed: {}",
        String::from_utf8_lossy(&started.stderr)
    );

    let fixture = Fixture::new();

    // 1. svc.list finds the running transient user service.
    let listed = fixture
        .call(
            "svc.list",
            json!({"scope": "user", "filter": base, "state": "running"}),
        )
        .await
        .unwrap();
    assert_eq!(listed["backend"], json!("systemd"));
    let services = listed["services"].as_array().unwrap();
    assert!(
        services.iter().any(|s| s["name"] == json!(unit)
            && s["scope"] == json!("user")
            && s["state"] == json!("running")),
        "expected {unit} in {listed:#}"
    );

    // 2. svc.status with omitted scope resolves to "user" and reports PID, since and description.
    let status = fixture
        .call("svc.status", json!({"name": unit}))
        .await
        .unwrap();
    assert_eq!(status["name"], json!(unit));
    assert_eq!(status["scope"], json!("user"));
    assert_eq!(status["backend"], json!("systemd"));
    assert_eq!(status["state"], json!("running"));
    assert!(status["pid"].as_u64().is_some_and(|p| p > 0), "{status:#}");
    assert!(
        status["since"]
            .as_str()
            .is_some_and(|s| s.ends_with('Z') && s.contains('T')),
        "{status:#}"
    );
    assert_eq!(status["description"], json!("Legion svc integration test"));

    // 3. svc.logs succeeds for the transient unit.
    let logs = fixture
        .call(
            "svc.logs",
            json!({"name": unit, "scope": "user", "lines": 20, "since": 60}),
        )
        .await
        .unwrap();
    assert_eq!(logs["name"], json!(unit));
    assert_eq!(logs["scope"], json!("user"));
    assert!(logs["lines"].is_array());

    // 4. svc.stop with omitted scope resolves to "user", stops the unit, and confirms stopped.
    let stopped = fixture
        .call("svc.stop", json!({"name": unit}))
        .await
        .unwrap();
    assert_eq!(stopped["name"], json!(unit));
    assert_eq!(stopped["scope"], json!("user"));
    assert_eq!(stopped["previous_state"], json!("running"));
    assert_eq!(stopped["state"], json!("stopped"));

    // 5. After stop, the transient unit is either unloaded (not_found) or stopped.
    match fixture
        .call("svc.status", json!({"name": unit, "scope": "user"}))
        .await
    {
        Err(err) => assert_eq!(err.code, ErrorCode::NotFound),
        Ok(after) => assert_eq!(after["state"], json!("stopped")),
    }

    // 6. Unknown service reports not_found.
    let missing = fixture
        .call(
            "svc.status",
            json!({"name": "legion-definitely-nonexistent-unit-98765.service"}),
        )
        .await
        .unwrap_err();
    assert_eq!(missing.code, ErrorCode::NotFound);
}

#[tokio::test]
async fn linux_rejects_wildcards_and_non_service_units() {
    if !linux_user_systemd_available() {
        return;
    }

    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let prefix = format!("legion-svc-glob-{}-{nanos}", std::process::id());
    let unit_a = format!("{prefix}-a.service");
    let unit_b = format!("{prefix}-b.service");
    let _guard_a = TransientUnitGuard {
        unit: unit_a.clone(),
    };
    let _guard_b = TransientUnitGuard {
        unit: unit_b.clone(),
    };

    for unit in [&unit_a, &unit_b] {
        let out = Command::new("systemd-run")
            .args(["--user", &format!("--unit={unit}"), "/bin/sleep", "60"])
            .stdin(Stdio::null())
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    let fixture = Fixture::new();
    let glob = format!("{prefix}-*");
    for tool in ["svc.status", "svc.stop"] {
        let err = fixture.call(tool, json!({"name": glob})).await.unwrap_err();
        assert_eq!(err.code, ErrorCode::InvalidArgs, "{tool}");
    }
    // Neither unit was affected by the rejected wildcard stop.
    for unit in [&unit_a, &unit_b] {
        let st = fixture
            .call("svc.status", json!({"name": unit}))
            .await
            .unwrap();
        assert_eq!(st["state"], json!("running"));
    }

    // Non-service control targets like exit.target and reboot.target are refused.
    for target in ["exit.target", "reboot.target", "backup.timer"] {
        let err = fixture
            .call("svc.start", json!({"name": target}))
            .await
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::InvalidArgs, "{target}");
    }
}

#[tokio::test]
async fn linux_start_catches_quick_crash_and_restart_proves_new_pid() {
    if !linux_user_systemd_available() {
        return;
    }

    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let crash_unit = format!("legion-svc-crash-{}-{nanos}.service", std::process::id());
    let live_unit = format!("legion-svc-live-{}-{nanos}.service", std::process::id());
    let _guard_crash = TransientUnitGuard {
        unit: crash_unit.clone(),
    };
    let _guard_live = TransientUnitGuard {
        unit: live_unit.clone(),
    };

    // 1. A unit that exits with code 3 after 0.4 s stays loaded in `failed` state; restarting it
    // shows `running` briefly and must be caught as `failed` after the settle window.
    let out = Command::new("systemd-run")
        .args([
            "--user",
            &format!("--unit={crash_unit}"),
            "/bin/sh",
            "-c",
            "sleep 0.4; exit 3",
        ])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    tokio::time::sleep(std::time::Duration::from_millis(700)).await;

    let fixture = Fixture::new();
    let err = fixture
        .call("svc.restart", json!({"name": crash_unit, "scope": "user"}))
        .await
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::Failed, "{err}");

    // 2. Restarting a healthy unit confirms a brand-new PID.
    let out = Command::new("systemd-run")
        .args(["--user", &format!("--unit={live_unit}"), "/bin/sleep", "60"])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    let before = fixture
        .call("svc.status", json!({"name": live_unit, "scope": "user"}))
        .await
        .unwrap();
    let old_pid = before["pid"].as_u64().unwrap();

    let restarted = fixture
        .call("svc.restart", json!({"name": live_unit, "scope": "user"}))
        .await
        .unwrap();
    assert_eq!(restarted["previous_state"], json!("running"));
    assert_eq!(restarted["state"], json!("running"));

    let after = fixture
        .call("svc.status", json!({"name": live_unit, "scope": "user"}))
        .await
        .unwrap();
    assert_ne!(after["pid"].as_u64().unwrap(), old_pid);
}

#[tokio::test]
async fn linux_stop_on_failed_unit_reports_failed_immediately() {
    if !linux_user_systemd_available() {
        return;
    }

    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let unit = format!("legion-svc-stopfail-{}-{nanos}.service", std::process::id());
    let _guard = TransientUnitGuard { unit: unit.clone() };

    let out = Command::new("systemd-run")
        .args([
            "--user",
            &format!("--unit={unit}"),
            "/bin/sh",
            "-c",
            "exit 3",
        ])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let fixture = Fixture::new();
    let fail_deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let status = fixture
            .call("svc.status", json!({"name": unit, "scope": "user"}))
            .await
            .unwrap();
        if status["state"] == json!("failed") {
            break;
        }
        assert!(
            std::time::Instant::now() < fail_deadline,
            "timed out waiting for transient unit {unit} to enter failed state; last status={status}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }

    let started = std::time::Instant::now();
    let res = fixture
        .call("svc.stop", json!({"name": unit, "scope": "user"}))
        .await
        .unwrap();
    assert!(
        started.elapsed() < std::time::Duration::from_secs(7),
        "svc.stop on failed unit took too long (should not wait full 10s policy): {:?}",
        started.elapsed()
    );
    assert_eq!(res["previous_state"], json!("failed"));
    assert_eq!(res["state"], json!("failed"));
}

#[tokio::test]
async fn linux_oneshot_service_start_reports_ran_and_exited() {
    if !linux_user_systemd_available() {
        return;
    }

    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let unit = format!("legion-svc-oneshot-{}-{nanos}.service", std::process::id());
    let _guard = TransientUnitGuard { unit: unit.clone() };

    let out = Command::new("systemd-run")
        .args([
            "--user",
            &format!("--unit={unit}"),
            "--property=Type=oneshot",
            "--remain-after-exit",
            "/bin/true",
        ])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    let fixture = Fixture::new();
    let restarted = fixture
        .call("svc.restart", json!({"name": unit, "scope": "user"}))
        .await
        .unwrap();
    assert!(
        restarted["note"]
            .as_str()
            .is_some_and(|n| n.contains("ran and exited")),
        "{restarted:#}"
    );
}
