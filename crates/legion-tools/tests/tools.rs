//! The built-in tools, called through a node the way every door calls them.

#![allow(clippy::unwrap_used)]

use legion_core::proto::{ErrorBody, ErrorCode};
use legion_core::{BUILD, Call, Door, Node, Paths, VERSION};
use serde_json::{Value, json};

struct Fixture {
    dir: tempfile::TempDir,
    node: Node,
}

impl Fixture {
    fn new() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let node = Node::new(
            Paths::under(dir.path()).with_home(dir.path().join("home")),
            legion_tools::registry(),
        );
        Fixture { dir, node }
    }

    fn write_config(&self, name: &str, text: &str) {
        let config = self.dir.path().join("config");
        std::fs::create_dir_all(&config).unwrap();
        legion_core::config::write_atomically(&config.join(name), text.as_bytes()).unwrap();
    }

    async fn call(&self, tool: &str, args: Value) -> Result<Value, ErrorBody> {
        self.node.call(Call::new(Door::Cli, tool, args)).await
    }

    async fn call_as(&self, tool: &str, principal: &str) -> Result<Value, ErrorBody> {
        let mut call = Call::new(Door::Mcp, tool, json!({}));
        call.as_principal = Some(principal.to_owned());
        self.node.call(call).await
    }
}

#[test]
fn every_built_in_tool_has_a_closed_object_schema_and_a_description() {
    let registry = legion_tools::registry();
    let names: Vec<&str> = registry.names().collect();
    for expected in ["audit.tail", "meta.doctor", "meta.version", "meta.whoami"] {
        assert!(
            names.contains(&expected),
            "{expected} is missing from {names:?}"
        );
    }
    // The test family, which a workspace test run builds in, echoes whatever it is given on purpose.
    let released = registry
        .specs()
        .into_iter()
        .filter(|spec| !spec.name.starts_with("test."));
    for spec in released {
        assert_eq!(spec.input_schema["type"], json!("object"), "{}", spec.name);
        assert_eq!(
            spec.input_schema["additionalProperties"],
            json!(false),
            "{}",
            spec.name
        );
        assert!(spec.description.ends_with('.'), "{}", spec.name);
    }
}

#[tokio::test]
async fn version_says_which_build_runs() {
    let fixture = Fixture::new();
    let version = fixture.call("meta.version", json!({})).await.unwrap();
    assert_eq!(version["name"], json!("legion"));
    assert_eq!(version["version"], json!(VERSION));
    assert_eq!(version.get("build").and_then(Value::as_str), BUILD);
    assert_eq!(version["protocol"], json!(1));
    assert_eq!(version["os"], json!(std::env::consts::OS));
    assert_eq!(version["node"], json!(fixture.node.name()));
    let error = fixture
        .call("meta.version", json!({"verbose": true}))
        .await
        .unwrap_err();
    assert_eq!(error.code, ErrorCode::InvalidArgs);
}

#[tokio::test]
async fn whoami_lists_what_the_caller_may_use() {
    let fixture = Fixture::new();
    let owner = fixture.call("meta.whoami", json!({})).await.unwrap();
    assert_eq!(owner["principal"], json!("owner"));
    assert_eq!(owner["role"], json!("owner"));
    assert_eq!(owner["door"], json!("cli"));
    assert_eq!(owner["delegate"], json!(true));
    assert_eq!(owner["paths"], Value::Null);
    let all: Vec<Value> = legion_tools::registry()
        .names()
        .map(|name| json!(name))
        .collect();
    assert_eq!(owner["tools"], json!(all));

    fixture.write_config(
        "policy.toml",
        "[principals.phone]\nrole = \"narrow\"\n[roles.narrow]\nallow = [\"meta.*\"]\ndeny = [\"meta.doctor\"]\npaths = [\"~/projects/**\"]\n",
    );
    let phone = fixture.call_as("meta.whoami", "phone").await.unwrap();
    assert_eq!(phone["role"], json!("narrow"));
    assert_eq!(phone["door"], json!("mcp"));
    assert_eq!(phone["delegate"], json!(false));
    assert_eq!(phone["paths"], json!(["~/projects"]));
    assert_eq!(
        phone["tools"],
        json!(["meta.init", "meta.update", "meta.version", "meta.whoami"])
    );
}

#[tokio::test]
async fn doctor_checks_the_directories_and_every_configuration_file() {
    let fixture = Fixture::new();
    let healthy = fixture.call("meta.doctor", json!({})).await.unwrap();
    assert_eq!(healthy["ok"], json!(true), "{healthy:#}");
    let names: Vec<&str> = healthy["checks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        &names[..7],
        &[
            "configuration directory",
            "state directory",
            "node.toml",
            "policy.toml",
            "projects.toml",
            "tokens.toml",
            "tools.d"
        ]
    );
    assert!(names.contains(&"executable on PATH"), "{names:?}");
    assert!(names.contains(&"ssh"), "{names:?}");
    assert!(names.contains(&"policy exec roles"), "{names:?}");
    #[cfg(target_os = "linux")]
    assert!(names.contains(&"systemd lingering"), "{names:?}");
    assert!(
        healthy["checks"][0]["detail"]
            .as_str()
            .unwrap()
            .contains("does not exist")
    );

    fixture.write_config("node.toml", "name = 3\n");
    let broken = fixture.call("meta.doctor", json!({})).await.unwrap();
    assert_eq!(broken["ok"], json!(false));
    let node = &broken["checks"][2];
    assert_eq!(node["ok"], json!(false));
    assert!(
        node["detail"].as_str().unwrap().contains("line 1"),
        "{node}"
    );
    assert!(node["hint"].is_string());
    assert_eq!(
        broken["checks"][0]["detail"]
            .as_str()
            .map(|d| d.contains("readable")),
        Some(true)
    );

    // A policy that does not parse fails the doctor's own call, as it fails every call: the error
    // is the report.
    fixture.write_config("policy.toml", "default = 3\n");
    let error = fixture.call("meta.doctor", json!({})).await.unwrap_err();
    assert!(
        error
            .message
            .contains("policy.toml does not parse at line 1"),
        "{}",
        error.message
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn the_doctor_writes_only_to_the_state_directory_and_leaves_nothing_there() {
    let fixture = Fixture::new();
    fixture.write_config("node.toml", "name = \"tower\"\n");
    let config = fixture.dir.path().join("config");
    let state = fixture.dir.path().join("state");
    let before = |dir: &std::path::Path| {
        let mut names: Vec<_> = std::fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        names.sort();
        names
    };
    let config_before = before(&config);
    let calls: Vec<_> = (0..16)
        .map(|_| {
            let node = fixture.node.clone();
            tokio::spawn(async move {
                node.call(Call::new(Door::Cli, "meta.doctor", json!({})))
                    .await
            })
        })
        .collect();
    for call in calls {
        let report = call.await.unwrap().unwrap();
        assert_eq!(report["ok"], json!(true), "{report}");
    }
    assert_eq!(before(&config), config_before);
    let left: Vec<_> = before(&state)
        .into_iter()
        .filter(|name| name.to_string_lossy().contains("doctor"))
        .collect();
    assert!(left.is_empty(), "{left:?}");
}

#[cfg(unix)]
#[tokio::test]
async fn a_configuration_directory_kept_read_only_is_healthy() {
    use std::os::unix::fs::PermissionsExt;
    let fixture = Fixture::new();
    fixture.write_config("node.toml", "name = \"tower\"\n");
    let config = fixture.dir.path().join("config");
    std::fs::set_permissions(&config, std::fs::Permissions::from_mode(0o500)).unwrap();
    let report = fixture.call("meta.doctor", json!({})).await.unwrap();
    std::fs::set_permissions(&config, std::fs::Permissions::from_mode(0o700)).unwrap();
    let check = &report["checks"][0];
    assert_eq!(check["name"], json!("configuration directory"));
    assert_eq!(check["ok"], json!(true), "{check}");
    assert!(
        check["detail"].as_str().unwrap().contains("readable"),
        "{check}"
    );
}

#[tokio::test]
async fn audit_tail_shows_the_calls_and_filters_them() {
    let fixture = Fixture::new();
    fixture.call("meta.version", json!({})).await.unwrap();
    fixture
        .call_as("meta.version", "stranger")
        .await
        .unwrap_err();
    fixture.call("meta.whoami", json!({})).await.unwrap();

    let tail = fixture.call("audit.tail", json!({})).await.unwrap();
    let tools: Vec<&str> = tail["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["tool"].as_str().unwrap())
        .collect();
    assert_eq!(tools, ["meta.version", "meta.version", "meta.whoami"]);
    assert_eq!(tail["truncated"], json!(false));

    let denied = fixture
        .call("audit.tail", json!({"outcome": "denied"}))
        .await
        .unwrap();
    assert_eq!(denied["entries"].as_array().unwrap().len(), 1);
    assert_eq!(denied["entries"][0]["principal"], json!("stranger"));

    let last = fixture
        .call("audit.tail", json!({"limit": 1, "tool": "meta.*"}))
        .await
        .unwrap();
    assert_eq!(last["entries"].as_array().unwrap().len(), 1);
    assert_eq!(last["truncated"], json!(true));

    for wrong in [
        json!({"limit": 0}),
        json!({"limit": 1001}),
        json!({"outcome": "fine"}),
    ] {
        let error = fixture.call("audit.tail", wrong.clone()).await.unwrap_err();
        assert_eq!(error.code, ErrorCode::InvalidArgs, "{wrong}");
    }
}
