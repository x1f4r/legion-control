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
            Record::Datagram { .. } | Record::Stop { .. } => false,
        })
    }
}

fn here() -> Platform {
    Platform::of(std::env::consts::OS).unwrap()
}

/// The command this host would carry `action` out with, with `sudo` and nothing found missing.
fn expected(action: Action, delay_s: u32, sudo: bool) -> (Vec<String>, Timing) {
    let plan = action_plan(here(), action, delay_s, sudo, false, None, &|name| {
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
        ("power.cancel", Effect::Power),
    ] {
        let spec = registry.get(name).unwrap().spec();
        assert_eq!(spec.effect, effect, "{name}");
        assert_eq!(
            spec.input_schema["additionalProperties"],
            json!(false),
            "{name}"
        );
        assert!(spec.description.ends_with('.'), "{name}");
        if effect == Effect::Power && name != "power.cancel" {
            assert!(spec.description.contains("dry_run"), "{name}");
            assert!(spec.description.contains("force: true"), "{name}");
            assert!(spec.description.contains("unmonitored"), "{name}");
        }
    }
    let restart = registry.get("power.restart").unwrap().spec();
    assert_eq!(
        restart.input_schema["properties"]["delay_s"]["maximum"],
        json!(600)
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
async fn a_restart_that_cannot_be_handed_over_says_the_firmware_stays_armed() {
    // Handing over fails: the delayed runner cannot start, or Windows' shutdown refuses.
    let fixture = Fixture::with_node_toml(
        RecordingActuator::new().refusing_schedules().reply(Reply {
            stderr: "The parameter is incorrect.(87)\r\n".to_owned(),
            ..Reply::new(&["shutdown"], 87, "")
        }),
        "busy = \"none\"\n[power]\nsudo = true\n[boot.linux]\narm = [\"arm-linux\"]\nverify = [\"verify-linux\"]\n",
    );
    let error = fixture
        .call("power.restart", json!({"into": "linux"}))
        .await
        .unwrap_err();
    assert_eq!(error.code, ErrorCode::Failed);
    assert!(
        error.message.ends_with(
            "The firmware stays set to start linux on the next boot, whatever causes it."
        ),
        "{}",
        error.message
    );
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
async fn a_call_that_could_never_work_says_why_before_the_busy_gate() {
    // Unmonitored, so the gate would refuse; the target that cannot be armed here is named first.
    let (node_toml, expected) = match here() {
        Platform::Macos => (
            "[boot.windows]\nefi_label = \"Windows Boot Manager\"\n",
            ("power.restart", json!({"into": "windows"})),
        ),
        Platform::Windows => ("", ("power.sleep", json!({}))),
        Platform::Linux => ("", ("power.restart", json!({"into": "windows"}))),
    };
    let fixture = Fixture::with_node_toml(
        RecordingActuator::new()
            .without_program("psshutdown64.exe")
            .without_program("psshutdown.exe"),
        node_toml,
    );
    let (tool, args) = expected;
    let error = fixture.call(tool, args).await.unwrap_err();
    let wanted = if here() == Platform::Linux {
        ErrorCode::NotFound
    } else {
        ErrorCode::Unsupported
    };
    assert_eq!(error.code, wanted, "{error:?}");
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
            strings(&["shutdown", "/r", "/t", "3"]),
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
    // The runner is told where to ask again whether the machine is busy, and that it should.
    let record = pending::read_pending(&pending::dir(fixture.node.paths())).unwrap();
    assert_eq!(
        record.config_dir.as_deref(),
        Some(fixture.node.paths().config_dir())
    );
    assert!(!record.force);
    // Windows times it itself and says the busy check will not be repeated.
    let warned = done["warnings"].as_array().is_some_and(|warnings| {
        warnings
            .iter()
            .any(|w| w.as_str().unwrap().contains("will not be checked again"))
    });
    assert_eq!(warned, here() == Platform::Windows, "{done}");
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
async fn one_power_action_is_carried_out_at_a_time() {
    let fixture = Fixture::with_node_toml(
        RecordingActuator::new(),
        "busy = \"none\"\n[power]\nsudo = true\nsleep = [\"go-to-sleep\"]\n[boot.windows]\narm = [\"arm-windows\"]\nverify = [\"verify-windows\"]\n",
    );
    let first = fixture.call("power.sleep", json!({})).await.unwrap();
    assert_eq!(first["scheduled"], json!(true));

    // Its runner has taken over and not run yet: a second sleep would put the machine back to
    // sleep as it woke, and a restart would re-arm the firmware, so both are refused.
    let again = fixture.call("power.sleep", json!({})).await.unwrap_err();
    assert_eq!(again.code, ErrorCode::Conflict);
    assert!(
        again.message.starts_with("A sleep handed over at"),
        "{}",
        again.message
    );
    let restart = fixture
        .call("power.restart", json!({"into": "windows"}))
        .await
        .unwrap_err();
    assert_eq!(restart.code, ErrorCode::Conflict);
    assert!(!fixture.fake.runs().contains(&strings(&["arm-windows"])));
    assert_eq!(fixture.fake.schedules().len(), 1);
    let status = fixture.call("power.status", json!({})).await.unwrap();
    assert_eq!(status["pending"]["action"], json!("sleep"), "{status}");
    assert_eq!(status["pending"]["argv"], json!(["go-to-sleep"]));
    // Its runner keeps the lock a while after the command, while the machine goes to sleep.
    assert_eq!(status["pending"]["hold_s"], json!(5));

    // Once it has run, the next one goes ahead.
    fixture.fake.finish_runners();
    let status = fixture.call("power.status", json!({})).await.unwrap();
    assert_eq!(status.get("pending"), None, "{status}");
    let restart = fixture
        .call("power.restart", json!({"into": "windows"}))
        .await
        .unwrap();
    assert_eq!(restart["scheduled"], json!(true));
}

#[tokio::test]
async fn a_pending_action_is_reported_before_a_busy_machine() {
    let fixture = Fixture::with_node_toml(
        RecordingActuator::new().reply(Reply::new(&["is-rendering"], 0, "blender\n")),
        "[[busy.probes]]\nname = \"render\"\nrun = [\"is-rendering\"]\n[power]\nsudo = true\nsleep = [\"go-to-sleep\"]\n",
    );
    fixture
        .call("power.sleep", json!({"force": true}))
        .await
        .unwrap();
    // Busy and something pending: the pending action is what the caller hears about.
    let second = fixture.call("power.sleep", json!({})).await.unwrap_err();
    assert_eq!(second.code, ErrorCode::Conflict, "{second:?}");
    let dry = fixture
        .call("power.sleep", json!({"dry_run": true}))
        .await
        .unwrap();
    assert_eq!(dry["would_refuse"]["code"], json!("conflict"), "{dry}");
}

#[tokio::test]
async fn a_runner_from_an_earlier_call_holds_up_the_next_one() {
    let fixture = Fixture::with_node_toml(RecordingActuator::new(), IDLE_WITH_SUDO);
    let dir = pending::dir(fixture.node.paths());
    std::fs::create_dir_all(&dir).unwrap();
    let blocker = pending::take_runner_lock(&dir, std::time::Duration::from_secs(1)).unwrap();
    let record = Pending {
        id: "earlier".to_owned(),
        action: Action::Sleep,
        into: None,
        argv: strings(&["x"]),
        delay_s: 3,
        hold_s: 0,
        since: pending::now(),
        boot_id: None,
        runner: true,
        taken: None,
        disarm: None,
        abandoned: false,
        fired: None,
        force: false,
        config_dir: None,
    };
    pending::write_pending(&dir, &record).unwrap();
    let error = fixture.call("power.shutdown", json!({})).await.unwrap_err();
    assert_eq!(error.code, ErrorCode::Conflict, "{error:?}");
    assert!(fixture.fake.records().is_empty());
    drop(blocker);
}

const UPDATER_INHIBITS: &str = r#"{"type":"a(ssssuu)","data":[[["shutdown:sleep","Updater","Installing packages","block",0,812]]]}"#;

/// A fake whose logind holds sleep back for an updater.
fn inhibited() -> RecordingActuator {
    RecordingActuator::new()
        .reply(Reply::new(
            &["busctl", "call"],
            0,
            "s \"inhibitor-blocked\"\n",
        ))
        .reply(Reply::new(&["busctl", "--json=short"], 0, UPDATER_INHIBITS))
}

#[tokio::test]
async fn an_inhibitor_holds_the_action_back_until_ignored_on_purpose() {
    if here() != Platform::Linux {
        return;
    }
    let fixture = Fixture::with_node_toml(inhibited(), IDLE_WITH_SUDO);
    for args in [json!({}), json!({"force": true})] {
        let error = fixture.call("power.sleep", args.clone()).await.unwrap_err();
        assert_eq!(error.code, ErrorCode::Busy, "{args}");
        assert!(
            error
                .message
                .contains("Updater (Installing packages, pid 812)"),
            "{}",
            error.message
        );
        assert!(
            error
                .hint
                .as_deref()
                .unwrap()
                .contains("ignore_inhibitors: true")
        );
        assert_eq!(
            error.details.as_ref().unwrap()["logind"],
            json!("inhibitor-blocked")
        );
    }
    assert!(fixture.fake.schedules().is_empty());

    let past = fixture
        .call("power.sleep", json!({"ignore_inhibitors": true}))
        .await
        .unwrap();
    assert_eq!(past["scheduled"], json!(true));
    assert_eq!(
        fixture.fake.schedules(),
        [(strings(&["sudo", "-n", "systemctl", "-i", "suspend"]), 3)]
    );

    // Without sudo this account cannot override it, and is told so before anything is handed over.
    let unprivileged = Fixture::with_node_toml(inhibited(), "busy = \"none\"\n");
    let error = unprivileged
        .call("power.sleep", json!({"ignore_inhibitors": true}))
        .await
        .unwrap_err();
    assert_eq!(error.code, ErrorCode::Denied);
    assert!(
        error
            .hint
            .as_deref()
            .unwrap()
            .contains("sudo = true under [power]")
    );
    assert!(unprivileged.fake.schedules().is_empty());
}

#[tokio::test]
async fn logind_is_asked_even_with_sudo_and_root_decides_the_rest() {
    if here() != Platform::Linux {
        return;
    }
    let fixture = Fixture::with_node_toml(
        RecordingActuator::new().reply(Reply::new(&["busctl", "call"], 0, "s \"challenge\"\n")),
        IDLE_WITH_SUDO,
    );
    let done = fixture.call("power.restart", json!({})).await.unwrap();
    assert_eq!(done["scheduled"], json!(true));
    let runs = fixture.fake.runs();
    assert_eq!(runs[0][0], "busctl");
    assert_eq!(runs[1][..4], strings(&["sudo", "-n", "-l", "-l"]));
}

#[tokio::test]
async fn a_dry_run_shows_the_commands_and_what_would_refuse_them() {
    // Unmonitored: the gate would refuse, and the dry run says so beside the commands.
    let fixture = Fixture::with_node_toml(RecordingActuator::new(), "[power]\nsudo = true\n");
    let dry = fixture
        .call("power.restart", json!({"dry_run": true}))
        .await
        .unwrap();
    let (argv, _) = expected(Action::Restart, 3, true);
    assert_eq!(dry["commands"], json!([argv]));
    assert_eq!(dry["would_refuse"]["code"], json!("busy"), "{dry}");
    assert!(
        dry["would_refuse"]["hint"]
            .as_str()
            .unwrap()
            .contains("busy = \"none\"")
    );
    assert!(fixture.fake.schedules().is_empty());

    // Nothing refusing: no would_refuse at all.
    fixture.write("node.toml", IDLE_WITH_SUDO);
    let clear = fixture
        .call("power.restart", json!({"dry_run": true}))
        .await
        .unwrap();
    assert_eq!(clear.get("would_refuse"), None, "{clear}");

    if here() == Platform::Linux {
        let held = Fixture::with_node_toml(inhibited(), IDLE_WITH_SUDO);
        let dry = held
            .call("power.sleep", json!({"dry_run": true}))
            .await
            .unwrap();
        assert_eq!(dry["would_refuse"]["code"], json!("busy"));
        assert!(
            dry["would_refuse"]["message"]
                .as_str()
                .unwrap()
                .contains("Updater")
        );
        assert_eq!(
            dry["commands"],
            json!([["sudo", "-n", "systemctl", "suspend"]])
        );
    }
}

#[tokio::test]
async fn a_sudo_rule_that_wants_a_password_refuses_before_anything_is_armed() {
    if here() == Platform::Windows {
        return;
    }
    // An unrelated NOPASSWD rule lets the listing succeed; the rule for the command wants a
    // password, which nobody can type after the reply.
    let listing = "Sudoers entry: /etc/sudoers\n    RunAsUsers: ALL\n    Commands:\n\tALL\n";
    let fixture = Fixture::with_node_toml(
        RecordingActuator::new().reply(Reply::new(&["sudo", "-n", "-l", "-l"], 0, listing)),
        "busy = \"none\"\n[power]\nsudo = true\n[boot.windows]\narm = [\"arm-windows\"]\nverify = [\"verify-windows\"]\n",
    );
    let error = fixture
        .call("power.restart", json!({"into": "windows"}))
        .await
        .unwrap_err();
    assert_eq!(error.code, ErrorCode::Denied, "{error:?}");
    assert!(
        error.message.contains("asks for a password"),
        "{}",
        error.message
    );
    let (argv, _) = expected(Action::Restart, 3, true);
    assert_eq!(error.details.as_ref().unwrap()["argv"], json!(argv));
    assert!(!fixture.fake.runs().contains(&strings(&["arm-windows"])));
    assert!(fixture.fake.schedules().is_empty());
}

#[tokio::test]
async fn status_reports_the_last_action_and_how_it_ended() {
    let fixture = Fixture::with_node_toml(RecordingActuator::new(), IDLE_WITH_SUDO);
    let outcome = pending::Outcome {
        id: "a1".to_owned(),
        action: Action::Restart,
        into: Some("windows".to_owned()),
        argv: strings(&["sudo", "-n", "systemctl", "reboot"]),
        since: "2026-11-24T10:00:00Z".to_owned(),
        ran_at: Some("2026-11-24T10:00:03Z".to_owned()),
        fate: None,
        exit_code: Some(1),
        timed_out: false,
        said: "sudo: a password is required".to_owned(),
        disarmed: Some(pending::Disarmed {
            argv: strings(&["sudo", "-n", "efibootmgr", "--delete-bootnext"]),
            exit_code: Some(0),
            said: String::new(),
        }),
        cancelled_by: None,
        cancelled_at: None,
    };
    let dir = pending::dir(fixture.node.paths());
    std::fs::create_dir_all(&dir).unwrap();
    pending::write_outcome(&dir, &outcome).unwrap();
    let status = fixture.call("power.status", json!({})).await.unwrap();
    assert_eq!(status["last"]["exit_code"], json!(1), "{status}");
    assert_eq!(status["last"]["into"], json!("windows"));
    assert_eq!(
        status["last"]["disarmed"]["argv"],
        json!(["sudo", "-n", "efibootmgr", "--delete-bootnext"])
    );
}

/// efibootmgr on a dual boot machine, before arming and after.
const EFI_LISTING: &str = "BootCurrent: 0001\nBootOrder: 0001,0000\nBoot0000* Windows Boot Manager\tHD(1,GPT,0f5b7c1e,0x800,0x100000)\nBoot0001* Linux Boot Manager\tHD(1,GPT,0f5b7c1e,0x800,0x100000)\n";

fn efi_armed() -> RecordingActuator {
    RecordingActuator::new()
        .reply(Reply::new(&["efibootmgr"], 0, EFI_LISTING))
        .reply(Reply::new(
            &["efibootmgr"],
            0,
            &format!("BootNext: 0000\n{EFI_LISTING}"),
        ))
}

#[tokio::test]
async fn a_restart_into_another_system_tells_its_runner_how_to_disarm() {
    if here() != Platform::Linux {
        return;
    }
    let node_toml = "busy = \"none\"\n[power]\nsudo = true\n[boot.windows]\nefi_label = \"Windows Boot Manager\"\n";
    let fixture = Fixture::with_node_toml(efi_armed(), node_toml);
    fixture
        .call("power.restart", json!({"into": "windows"}))
        .await
        .unwrap();
    let record = pending::read_pending(&pending::dir(fixture.node.paths())).unwrap();
    assert_eq!(
        record.disarm,
        Some(strings(&["sudo", "-n", "efibootmgr", "--delete-bootnext"]))
    );

    // Handing over fails: the call clears the arming itself and says so.
    let failing = Fixture::with_node_toml(efi_armed().refusing_schedules(), node_toml);
    let error = failing
        .call("power.restart", json!({"into": "windows"}))
        .await
        .unwrap_err();
    assert!(
        error.message.ends_with(
            "The firmware's setting to start windows next was cleared again with `sudo -n efibootmgr --delete-bootnext`."
        ),
        "{}",
        error.message
    );
    assert_eq!(
        failing.fake.runs().last(),
        Some(&strings(&["sudo", "-n", "efibootmgr", "--delete-bootnext"]))
    );
}

#[tokio::test]
async fn arguments_are_checked() {
    let fixture = Fixture::with_node_toml(RecordingActuator::new(), IDLE_WITH_SUDO);
    for (tool, args) in [
        ("power.restart", json!({"delay_s": 0})),
        ("power.restart", json!({"delay_s": 601})),
        ("power.cancel", json!({"id": "x"})),
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
    for tool in [
        "power.sleep",
        "power.restart",
        "power.shutdown",
        "power.cancel",
    ] {
        let error = as_agent(tool).await.unwrap_err();
        assert_eq!(error.code, ErrorCode::Denied, "{tool}");
    }
    assert!(as_agent("power.status").await.is_ok());
    assert!(fixture.fake.records().is_empty());
}

// --- power.cancel ---------------------------------------------------------------------------------

/// A record of a restart Windows times itself, handed over now on this node's boot, as a real
/// call on Windows writes it; `power.cancel` deals with it the same way on any host.
fn self_timed_restart(fixture: &Fixture, disarm: Option<Vec<String>>) -> Pending {
    let record = Pending {
        id: pending::new_id(),
        action: Action::Restart,
        into: disarm.as_ref().map(|_| "linux".to_owned()),
        argv: strings(&["shutdown", "/r", "/t", "600"]),
        delay_s: 600,
        hold_s: 0,
        since: pending::now(),
        boot_id: fixture.node.boot_id(),
        runner: false,
        taken: None,
        disarm,
        abandoned: false,
        fired: None,
        force: false,
        config_dir: None,
    };
    pending::write_pending(&pending::dir(fixture.node.paths()), &record).unwrap();
    record
}

#[tokio::test]
async fn cancel_calls_off_a_handed_over_restart_and_status_says_who() {
    let fixture = Fixture::with_node_toml(RecordingActuator::new(), IDLE_WITH_SUDO);
    let restart = fixture
        .call("power.restart", json!({"delay_s": 600}))
        .await
        .unwrap();
    assert_eq!(restart["scheduled"], json!(true));
    let before = fixture.call("power.status", json!({})).await.unwrap();
    assert_eq!(before["pending"]["action"], json!("restart"), "{before}");
    let id = before["pending"]["id"].clone();

    let cancelled = fixture.call("power.cancel", json!({})).await.unwrap();
    assert_eq!(cancelled["cancelled"], json!(true), "{cancelled}");
    assert_eq!(cancelled["action"], json!("restart"));
    assert_eq!(cancelled["id"], id);
    assert!(cancelled["was_due"].is_string(), "{cancelled}");
    let (_, timing) = expected(Action::Restart, 600, true);
    match timing {
        Timing::Delayed => {
            // The runner was stopped by its handle, and its lock went with it.
            assert_eq!(fixture.fake.stops(), [fake::FAKE_RUNNER_PID]);
            assert!(!pending::runner_holds(&pending::dir(fixture.node.paths())));
        }
        Timing::SelfTimed => {
            assert_eq!(
                fixture.fake.runs().last(),
                Some(&strings(&["shutdown", "/a"]))
            );
        }
    }

    let after = fixture.call("power.status", json!({})).await.unwrap();
    assert_eq!(after.get("pending"), None, "{after}");
    assert_eq!(after["last"]["id"], id, "{after}");
    assert_eq!(after["last"]["fate"], json!("cancelled"));
    assert_eq!(after["last"]["cancelled_by"], json!("owner"));
    assert!(after["last"]["cancelled_at"].is_string());

    // The power lock is free again: the next action can be handed over.
    let again = fixture
        .call("power.shutdown", json!({"delay_s": 600}))
        .await
        .unwrap();
    assert_eq!(again["scheduled"], json!(true), "{again}");
}

#[tokio::test]
async fn cancel_with_nothing_pending_or_a_second_time_says_so() {
    let fixture = Fixture::with_node_toml(RecordingActuator::new(), IDLE_WITH_SUDO);
    let nothing = fixture.call("power.cancel", json!({})).await.unwrap();
    assert_eq!(nothing["cancelled"], json!(false));
    assert_eq!(
        nothing["detail"],
        json!(
            "Nothing is pending on this machine: no sleep, restart or shutdown has been handed over."
        )
    );
    assert_eq!(nothing.get("action"), None);

    fixture
        .call("power.restart", json!({"delay_s": 600}))
        .await
        .unwrap();
    let first = fixture.call("power.cancel", json!({})).await.unwrap();
    assert_eq!(first["cancelled"], json!(true), "{first}");
    let records = fixture.fake.records().len();
    let second = fixture.call("power.cancel", json!({})).await.unwrap();
    assert_eq!(second["cancelled"], json!(false), "{second}");
    let detail = second["detail"].as_str().unwrap();
    assert!(
        detail.starts_with(
            "Nothing is pending on this machine. The last action, a restart handed over at"
        ),
        "{detail}"
    );
    assert!(detail.contains("was called off by owner at"), "{detail}");
    // Nothing was run or stopped a second time.
    assert_eq!(fixture.fake.records().len(), records);
}

#[tokio::test]
async fn a_restart_windows_times_itself_is_called_off_with_shutdown_a() {
    let fixture = Fixture::with_node_toml(RecordingActuator::new(), IDLE_WITH_SUDO);
    let record = self_timed_restart(&fixture, None);
    let cancelled = fixture.call("power.cancel", json!({})).await.unwrap();
    assert_eq!(cancelled["cancelled"], json!(true), "{cancelled}");
    assert_eq!(cancelled["id"], json!(record.id));
    assert_eq!(fixture.fake.runs(), [strings(&["shutdown", "/a"])]);
    assert!(fixture.fake.stops().is_empty());
    let status = fixture.call("power.status", json!({})).await.unwrap();
    assert_eq!(status.get("pending"), None, "{status}");
    assert_eq!(status["last"]["fate"], json!("cancelled"), "{status}");
}

#[tokio::test]
async fn windows_with_nothing_to_abort_or_refusing_is_reported_as_it_said() {
    let nothing = Fixture::with_node_toml(
        RecordingActuator::new().reply(Reply {
            stderr:
                "Unable to abort the system shutdown because no shutdown was in progress.(1116)"
                    .to_owned(),
            ..Reply::new(&["shutdown", "/a"], 1116, "")
        }),
        IDLE_WITH_SUDO,
    );
    self_timed_restart(&nothing, None);
    let answer = nothing.call("power.cancel", json!({})).await.unwrap();
    assert_eq!(answer["cancelled"], json!(false), "{answer}");
    let detail = answer["detail"].as_str().unwrap();
    assert!(
        detail.contains("no shutdown was in progress.(1116)")
            && detail.contains("already called off on the machine"),
        "{detail}"
    );
    let status = nothing.call("power.status", json!({})).await.unwrap();
    assert_eq!(status.get("pending"), None, "{status}");
    assert_eq!(status["last"]["fate"], json!("did_not_happen"), "{status}");

    let refused = Fixture::with_node_toml(
        RecordingActuator::new().reply(Reply::new(&["shutdown", "/a"], 5, "Access is denied.(5)")),
        IDLE_WITH_SUDO,
    );
    self_timed_restart(&refused, None);
    let error = refused.call("power.cancel", json!({})).await.unwrap_err();
    assert_eq!(error.code, ErrorCode::Denied, "{error:?}");
    // Still pending: nothing was recorded as called off.
    let status = refused.call("power.status", json!({})).await.unwrap();
    assert_eq!(status["pending"]["action"], json!("restart"), "{status}");
}

#[tokio::test]
async fn cancelling_a_restart_into_another_system_clears_the_firmware_again() {
    let fixture = Fixture::with_node_toml(RecordingActuator::new(), IDLE_WITH_SUDO);
    let disarm = strings(&["bcdedit", "/deletevalue", "{fwbootmgr}", "bootsequence"]);
    self_timed_restart(&fixture, Some(disarm.clone()));
    let cancelled = fixture.call("power.cancel", json!({})).await.unwrap();
    assert_eq!(cancelled["cancelled"], json!(true), "{cancelled}");
    assert!(
        cancelled["detail"]
            .as_str()
            .unwrap()
            .ends_with("The firmware's setting to start linux next was cleared again with `bcdedit /deletevalue {fwbootmgr} bootsequence`."),
        "{cancelled}"
    );
    assert_eq!(
        fixture.fake.runs(),
        [strings(&["shutdown", "/a"]), disarm.clone()]
    );
    let status = fixture.call("power.status", json!({})).await.unwrap();
    assert_eq!(
        status["last"]["disarmed"]["argv"],
        json!(disarm),
        "{status}"
    );
}

#[tokio::test]
async fn a_command_the_runner_has_started_is_too_late_to_call_off() {
    let fixture = Fixture::with_node_toml(RecordingActuator::new(), IDLE_WITH_SUDO);
    let dir = pending::dir(fixture.node.paths());
    std::fs::create_dir_all(&dir).unwrap();
    let _runner = pending::take_runner_lock(&dir, std::time::Duration::from_secs(1)).unwrap();
    let record = Pending {
        id: pending::new_id(),
        action: Action::Shutdown,
        into: None,
        argv: strings(&["sudo", "-n", "systemctl", "poweroff"]),
        delay_s: 600,
        hold_s: 0,
        since: pending::now(),
        boot_id: fixture.node.boot_id(),
        runner: true,
        taken: Some(pending::now()),
        disarm: None,
        abandoned: false,
        fired: Some(pending::now()),
        force: false,
        config_dir: None,
    };
    pending::write_pending(&dir, &record).unwrap();
    let answer = fixture.call("power.cancel", json!({})).await.unwrap();
    assert_eq!(answer["cancelled"], json!(false), "{answer}");
    assert!(
        answer["detail"]
            .as_str()
            .unwrap()
            .starts_with("Too late: the shutdown handed over at"),
        "{answer}"
    );
    assert!(fixture.fake.records().is_empty());
    // Left as it was for the runner, which is carrying it out.
    assert_eq!(pending::read_pending(&dir), Some(record));
}

#[tokio::test]
async fn a_runner_that_cannot_be_stopped_is_still_called_off() {
    let fixture =
        Fixture::with_node_toml(RecordingActuator::new().surviving_stops(), IDLE_WITH_SUDO);
    if expected(Action::Restart, 600, true).1 != Timing::Delayed {
        return;
    }
    fixture
        .call("power.restart", json!({"delay_s": 600}))
        .await
        .unwrap();
    let answer = fixture.call("power.cancel", json!({})).await.unwrap();
    assert_eq!(answer["cancelled"], json!(true), "{answer}");
    let detail = answer["detail"].as_str().unwrap();
    assert!(
        detail.contains("Its runner could not be stopped"),
        "{detail}"
    );
    assert!(
        detail.contains("find the action called off and run nothing"),
        "{detail}"
    );
    // The record says called off, so the runner will not act; its lock holds the next call up
    // until it is gone.
    let next = fixture.call("power.restart", json!({})).await.unwrap_err();
    assert_eq!(next.code, ErrorCode::Conflict, "{next:?}");
    fixture.fake.finish_runners();
    assert!(fixture.call("power.restart", json!({})).await.is_ok());
}

#[tokio::test]
async fn sudo_that_lists_only_the_command_is_asked_for_its_rules() {
    if here() == Platform::Windows {
        return;
    }
    // sudo-rs, as on Ubuntu 26.04: the listing for the command names only the command.
    let (argv, _) = expected(Action::Restart, 600, true);
    let plain: Vec<&str> = argv[2..].iter().map(String::as_str).collect();
    let mut asked = vec!["sudo", "-n", "-l", "-l"];
    asked.extend(&plain);
    let rules = "User runner may run the following commands on host:\n\nSudoers entry:\n    RunAsUsers: ALL\n    Options: !authenticate\n    Commands:\n\tALL\n";
    let sudo_rs = |rules: &str| {
        RecordingActuator::new()
            .reply(Reply::new(
                &asked,
                0,
                &format!("/usr/bin/{}\n", plain.join(" ")),
            ))
            .reply(Reply::new(&["sudo", "-n", "-l", "-l"], 0, rules))
    };
    let allowed = Fixture::with_node_toml(sudo_rs(rules), IDLE_WITH_SUDO);
    let done = allowed
        .call("power.restart", json!({"delay_s": 600}))
        .await
        .unwrap();
    assert_eq!(done["scheduled"], json!(true), "{done}");
    assert!(
        allowed
            .fake
            .runs()
            .contains(&strings(&["sudo", "-n", "-l", "-l"]))
    );

    let password = Fixture::with_node_toml(
        sudo_rs(&rules.replace("    Options: !authenticate\n", "")),
        IDLE_WITH_SUDO,
    );
    let error = password
        .call("power.restart", json!({"delay_s": 600}))
        .await
        .unwrap_err();
    assert_eq!(error.code, ErrorCode::Denied, "{error:?}");
    assert!(password.fake.schedules().is_empty());
}
