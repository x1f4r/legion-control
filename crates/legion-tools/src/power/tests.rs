//! The local power tools called through a real node, acting through the recording fake.
//!
//! The commands differ per platform, so the expected ones are built with the same pure builders
//! the tools use (which `plan` tests for every platform) and the assertions hold on any host.

use std::sync::Arc;

use legion_core::proto::{ErrorBody, ErrorCode};
use legion_core::{Call, Door, Node, Paths, Registry};
use serde_json::{Value, json};

use super::fake::{Record, RecordingActuator, Reply};
use super::plan::{Action, Platform, Timing, action_plan, strings};
use super::*;

struct Fixture {
    dir: tempfile::TempDir,
    node: Node,
    fake: Arc<RecordingActuator>,
}

impl Fixture {
    fn new(fake: RecordingActuator) -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let fake = Arc::new(fake);
        let mut registry = Registry::new();
        register_with(&mut registry, Arc::clone(&fake) as Arc<dyn Actuator>);
        let node = Node::new(
            Paths::under(dir.path()).with_home(dir.path().join("home")),
            registry,
        );
        Fixture { dir, node, fake }
    }

    fn with_node_toml(fake: RecordingActuator, text: &str) -> Fixture {
        let fixture = Fixture::new(fake);
        fixture.write("node.toml", text);
        fixture
    }

    fn write(&self, name: &str, text: &str) {
        let config = self.dir.path().join("config");
        std::fs::create_dir_all(&config).unwrap();
        std::fs::write(config.join(name), text).unwrap();
    }

    async fn call(&self, tool: &str, args: Value) -> Result<Value, ErrorBody> {
        self.node.call(Call::new(Door::Cli, tool, args)).await
    }

    /// Whether the fake was asked to carry out `argv`: scheduled on Unix, run on Windows.
    fn carried_out(&self, argv: &[String]) -> bool {
        self.fake.records().iter().any(|record| match record {
            Record::Schedule { argv: done, .. } | Record::Run { argv: done } => done == argv,
            Record::Datagram { .. } => false,
        })
    }
}

fn here() -> Platform {
    Platform::of(std::env::consts::OS).unwrap()
}

/// The command this host would carry `action` out with, with `sudo` and nothing found missing.
fn expected(action: Action, delay_s: u32, sudo: bool) -> (Vec<String>, Timing) {
    let plan = action_plan(here(), action, delay_s, sudo, None, &|name| {
        Some(std::path::PathBuf::from(name))
    })
    .unwrap();
    (plan.argv, plan.timing)
}

const IDLE_WITH_SUDO: &str = "busy = \"none\"\n[power]\nsudo = true\n";

#[test]
fn the_tools_are_registered_with_their_effects_and_closed_schemas() {
    let mut registry = Registry::new();
    register_with(&mut registry, Arc::new(RecordingActuator::new()));
    for (name, effect) in [
        ("power.status", Effect::Read),
        ("power.sleep", Effect::Power),
        ("power.restart", Effect::Power),
        ("power.shutdown", Effect::Power),
    ] {
        let spec = registry.get(name).unwrap().spec();
        assert_eq!(spec.effect, effect, "{name}");
        assert_eq!(
            spec.input_schema["additionalProperties"],
            json!(false),
            "{name}"
        );
        assert!(spec.description.ends_with('.'), "{name}");
        if effect == Effect::Power {
            assert!(spec.description.contains("dry_run"), "{name}");
            assert!(spec.description.contains("force: true"), "{name}");
            assert!(spec.description.contains("unmonitored"), "{name}");
        }
    }
    let restart = registry.get("power.restart").unwrap().spec();
    assert_eq!(
        restart.input_schema["properties"]["delay_s"]["maximum"],
        json!(60)
    );
}

#[tokio::test]
async fn status_reports_the_busy_state_targets_system_and_boot() {
    let fixture =
        Fixture::new(RecordingActuator::new().reply(Reply::new(&["is-rendering"], 1, "")));
    let bare = fixture.call("power.status", json!({})).await.unwrap();
    assert_eq!(bare["busy"], json!({"state": "unmonitored", "reasons": []}));
    assert_eq!(bare.get("system"), None);
    assert_eq!(bare["targets"], json!([]));
    assert_eq!(bare["sudo"], json!(false));
    assert_eq!(bare["boot_id"], json!(fixture.node.boot_id()));

    fixture.write(
        "node.toml",
        "system = \"linux\"\n[[busy.probes]]\nname = \"render\"\nrun = [\"is-rendering\"]\n[power]\nsudo = true\n[boot.windows]\narm = [\"a\"]\nverify = [\"v\"]\n",
    );
    let idle = fixture.call("power.status", json!({})).await.unwrap();
    assert_eq!(idle["busy"]["state"], json!("idle"), "{idle}");
    assert_eq!(idle["system"], json!("linux"));
    assert_eq!(
        idle["targets"],
        json!([{"name": "windows", "how": "the arm and verify commands in node.toml"}])
    );
    assert_eq!(idle["sudo"], json!(here() != Platform::Windows));
    // Reading the status runs the probes and nothing else.
    assert_eq!(fixture.fake.runs(), [strings(&["is-rendering"])]);
}

#[tokio::test]
async fn an_unmonitored_or_busy_machine_is_not_interrupted_without_force() {
    let fixture = Fixture::new(RecordingActuator::new());
    let error = fixture.call("power.sleep", json!({})).await.unwrap_err();
    assert_eq!(error.code, ErrorCode::Busy);
    assert!(error.hint.as_deref().unwrap().contains("busy = \"none\""));
    assert!(fixture.fake.records().is_empty());

    let fixture_busy = Fixture::with_node_toml(
        RecordingActuator::new().reply(Reply::new(&["is-rendering"], 0, "blender\n")),
        "[[busy.probes]]\nname = \"render\"\nrun = [\"is-rendering\"]\n[power]\nsudo = true\n",
    );
    let error = fixture_busy
        .call("power.restart", json!({}))
        .await
        .unwrap_err();
    assert_eq!(error.code, ErrorCode::Busy);
    assert!(
        error.message.contains("probe render: busy: blender"),
        "{}",
        error.message
    );
    assert_eq!(fixture_busy.fake.runs(), [strings(&["is-rendering"])]);

    let forced = fixture_busy
        .call("power.restart", json!({"force": true}))
        .await
        .unwrap();
    assert_eq!(forced["scheduled"], json!(true));
    let (argv, _) = expected(Action::Restart, 3, true);
    assert!(fixture_busy.carried_out(&argv));
    // Forced, the probe is not even asked again.
    assert_eq!(
        fixture_busy
            .fake
            .runs()
            .iter()
            .filter(|run| run[0] == "is-rendering")
            .count(),
        1
    );
}

#[tokio::test]
async fn the_state_is_checked_again_just_before_acting() {
    let fixture = Fixture::with_node_toml(
        RecordingActuator::new()
            .reply(Reply::new(&["is-rendering"], 1, ""))
            .reply(Reply::new(&["is-rendering"], 0, "started meanwhile\n")),
        "[[busy.probes]]\nname = \"render\"\nrun = [\"is-rendering\"]\n[power]\nsudo = true\n[boot.windows]\narm = [\"arm-windows\"]\nverify = [\"verify-windows\"]\n",
    );
    let error = fixture
        .call("power.restart", json!({"into": "windows"}))
        .await
        .unwrap_err();
    assert_eq!(error.code, ErrorCode::Busy);
    assert!(
        error.message.contains("started meanwhile"),
        "{}",
        error.message
    );
    let runs = fixture.fake.runs();
    assert_eq!(runs.first(), Some(&strings(&["is-rendering"])));
    assert_eq!(runs.last(), Some(&strings(&["is-rendering"])));
    assert!(!runs.contains(&strings(&["arm-windows"])));
    assert!(fixture.fake.schedules().is_empty());
}

#[tokio::test]
async fn a_dry_run_returns_the_commands_and_changes_nothing() {
    let fixture = Fixture::with_node_toml(
        RecordingActuator::new(),
        "busy = \"none\"\n[power]\nsudo = true\n[boot.windows]\narm = [\"arm-windows\"]\nverify = [\"verify-windows\"]\n",
    );
    let dry = fixture
        .call(
            "power.restart",
            json!({"into": "windows", "delay_s": 10, "dry_run": true}),
        )
        .await
        .unwrap();
    let (argv, _) = expected(Action::Restart, 10, true);
    assert_eq!(dry["scheduled"], json!(false));
    assert_eq!(dry["dry_run"], json!(true));
    assert_eq!(dry["in_s"], json!(10));
    assert_eq!(dry["into"], json!("windows"));
    assert_eq!(dry["commands"], json!([["arm-windows"], argv]));
    let runs = fixture.fake.runs();
    assert!(!runs.contains(&strings(&["arm-windows"])));
    assert!(!runs.contains(&strings(&["verify-windows"])));
    assert!(!fixture.carried_out(&argv));
    assert!(fixture.fake.schedules().is_empty());
}

#[tokio::test]
async fn a_restart_into_another_system_arms_reads_back_and_then_schedules() {
    let fixture = Fixture::with_node_toml(
        RecordingActuator::new(),
        "busy = \"none\"\n[power]\nsudo = true\n[boot.windows]\narm = [\"arm-windows\"]\nverify = [\"verify-windows\"]\n",
    );
    let done = fixture
        .call("power.restart", json!({"into": "windows"}))
        .await
        .unwrap();
    let (argv, timing) = expected(Action::Restart, 3, true);
    assert_eq!(done["action"], json!("restart"));
    assert_eq!(done["scheduled"], json!(true));
    assert_eq!(done["in_s"], json!(3));
    assert_eq!(done["into"], json!("windows"));
    assert_eq!(done["boot_id"], json!(fixture.node.boot_id()));
    assert_eq!(done["commands"], json!([["arm-windows"], argv.clone()]));
    let records = fixture.fake.records();
    let position = |wanted: &Record| records.iter().position(|record| record == wanted);
    let armed = position(&Record::Run {
        argv: strings(&["arm-windows"]),
    })
    .unwrap();
    let verified = position(&Record::Run {
        argv: strings(&["verify-windows"]),
    })
    .unwrap();
    let carried = match timing {
        Timing::Delayed => position(&Record::Schedule {
            argv: argv.clone(),
            delay_s: 3,
        }),
        Timing::SelfTimed => position(&Record::Run { argv: argv.clone() }),
    }
    .unwrap();
    assert!(armed < verified && verified < carried, "{records:?}");
}

#[tokio::test]
async fn a_read_back_that_fails_restarts_nothing() {
    let fixture = Fixture::with_node_toml(
        RecordingActuator::new().reply(Reply::new(&["verify-windows"], 1, "")),
        "busy = \"none\"\n[power]\nsudo = true\n[boot.windows]\narm = [\"arm-windows\"]\nverify = [\"verify-windows\"]\n",
    );
    let error = fixture
        .call("power.restart", json!({"into": "windows"}))
        .await
        .unwrap_err();
    assert_eq!(error.code, ErrorCode::Failed);
    assert!(
        error.message.contains("Nothing was restarted"),
        "{}",
        error.message
    );
    let (argv, _) = expected(Action::Restart, 3, true);
    assert!(!fixture.carried_out(&argv));
}

#[tokio::test]
async fn an_unknown_target_lists_the_configured_ones() {
    let fixture = Fixture::with_node_toml(
        RecordingActuator::new(),
        "busy = \"none\"\n[boot.windows]\nefi_label = \"Windows Boot Manager\"\n[boot.other]\narm = [\"a\"]\nverify = [\"v\"]\n",
    );
    let error = fixture
        .call("power.restart", json!({"into": "macos"}))
        .await
        .unwrap_err();
    assert_eq!(error.code, ErrorCode::NotFound);
    assert_eq!(
        error.hint.as_deref(),
        Some("The boot targets in node.toml are other, windows.")
    );
    assert!(fixture.fake.records().is_empty());
}

#[tokio::test]
async fn a_refusal_without_sudo_is_denied_and_names_the_setting() {
    let (reply, expected_argv) = match here() {
        Platform::Linux => (
            Reply::new(&["busctl"], 0, "s \"challenge\"\n"),
            strings(&["sudo", "-n", "systemctl", "reboot"]),
        ),
        Platform::Macos => (
            Reply::new(&["id", "-u"], 0, "501\n"),
            strings(&["sudo", "-n", "shutdown", "-r", "now"]),
        ),
        Platform::Windows => (
            Reply {
                stderr: "Access is denied.(5)\r\n".to_owned(),
                ..Reply::new(&["shutdown"], 5, "")
            },
            strings(&["shutdown", "/r", "/t", "3", "/d", "p:0:0"]),
        ),
    };
    let fixture =
        Fixture::with_node_toml(RecordingActuator::new().reply(reply), "busy = \"none\"\n");
    let error = fixture.call("power.restart", json!({})).await.unwrap_err();
    assert_eq!(error.code, ErrorCode::Denied, "{error:?}");
    assert_eq!(
        error.details.as_ref().unwrap()["argv"],
        json!(expected_argv)
    );
    if here() != Platform::Windows {
        assert!(
            error
                .hint
                .as_deref()
                .unwrap()
                .contains("sudo = true under [power]")
        );
        // Asked, refused, and nothing was scheduled.
        assert!(fixture.fake.schedules().is_empty());
    }
}

#[tokio::test]
async fn sudo_refusing_the_command_is_denied_with_the_exact_command() {
    if here() == Platform::Windows {
        return;
    }
    let fixture = Fixture::with_node_toml(
        RecordingActuator::new().reply(Reply {
            stderr: "sudo: a password is required\n".to_owned(),
            ..Reply::new(&["sudo", "-n", "-l"], 1, "")
        }),
        IDLE_WITH_SUDO,
    );
    let error = fixture.call("power.shutdown", json!({})).await.unwrap_err();
    assert_eq!(error.code, ErrorCode::Denied);
    let (argv, _) = expected(Action::Shutdown, 3, true);
    assert_eq!(error.details.as_ref().unwrap()["argv"], json!(argv));
    assert!(error.hint.as_deref().unwrap().contains("NOPASSWD"));
    assert!(fixture.fake.schedules().is_empty());
}

#[tokio::test]
async fn the_action_is_handed_over_to_happen_after_the_reply() {
    let fixture = Fixture::with_node_toml(RecordingActuator::new(), IDLE_WITH_SUDO);
    let done = fixture
        .call("power.shutdown", json!({"delay_s": 7}))
        .await
        .unwrap();
    assert_eq!(done["in_s"], json!(7));
    let (argv, timing) = expected(Action::Shutdown, 7, true);
    match timing {
        Timing::Delayed => assert_eq!(fixture.fake.schedules(), [(argv, 7)]),
        Timing::SelfTimed => {
            assert_eq!(fixture.fake.runs().last(), Some(&argv));
            assert!(argv.contains(&"7".to_owned()));
        }
    }
}

#[tokio::test]
async fn before_sleep_runs_first_and_a_failure_stops_the_sleep() {
    let node_toml = "busy = \"none\"\n[power]\nsudo = true\nsleep = [\"go-to-sleep\"]\nbefore_sleep = [[\"stop-keepawake\"], [\"sync\"]]\n";
    let fixture = Fixture::with_node_toml(RecordingActuator::new(), node_toml);
    let done = fixture.call("power.sleep", json!({})).await.unwrap();
    assert_eq!(
        done["commands"],
        json!([["stop-keepawake"], ["sync"], ["go-to-sleep"]])
    );
    assert_eq!(
        fixture.fake.records(),
        [
            Record::Run {
                argv: strings(&["stop-keepawake"])
            },
            Record::Run {
                argv: strings(&["sync"])
            },
            Record::Schedule {
                argv: strings(&["go-to-sleep"]),
                delay_s: 3
            },
        ]
    );

    let failing = Fixture::with_node_toml(
        RecordingActuator::new().reply(Reply::new(&["stop-keepawake"], 3, "still held\n")),
        node_toml,
    );
    let error = failing.call("power.sleep", json!({})).await.unwrap_err();
    assert_eq!(error.code, ErrorCode::Failed);
    assert!(
        error.message.contains("stop-keepawake"),
        "{}",
        error.message
    );
    assert!(failing.fake.schedules().is_empty());
    assert!(!failing.fake.runs().contains(&strings(&["sync"])));
}

#[tokio::test]
async fn arguments_are_checked() {
    let fixture = Fixture::with_node_toml(RecordingActuator::new(), IDLE_WITH_SUDO);
    for (tool, args) in [
        ("power.restart", json!({"delay_s": 0})),
        ("power.restart", json!({"delay_s": 61})),
        ("power.shutdown", json!({"into": "windows"})),
        ("power.sleep", json!({"delay_s": 3})),
        ("power.status", json!({"force": true})),
    ] {
        let error = fixture.call(tool, args.clone()).await.unwrap_err();
        assert_eq!(error.code, ErrorCode::InvalidArgs, "{tool} {args}");
    }
    assert!(fixture.fake.records().is_empty());
}

#[tokio::test]
async fn a_role_without_the_power_effect_is_denied() {
    let fixture = Fixture::with_node_toml(RecordingActuator::new(), IDLE_WITH_SUDO);
    fixture.write(
        "policy.toml",
        "[principals.\"agent:test\"]\nrole = \"observer\"\n\n[roles.observer]\nallow = [\"effect:read\"]\n",
    );
    let as_agent = |tool: &str| {
        let mut call = Call::new(Door::Mcp, tool, json!({}));
        call.as_principal = Some("agent:test".to_owned());
        fixture.node.call(call)
    };
    for tool in ["power.sleep", "power.restart", "power.shutdown"] {
        let error = as_agent(tool).await.unwrap_err();
        assert_eq!(error.code, ErrorCode::Denied, "{tool}");
    }
    assert!(as_agent("power.status").await.is_ok());
    assert!(fixture.fake.records().is_empty());
}
