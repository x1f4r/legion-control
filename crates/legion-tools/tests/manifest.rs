//! `fs.manifest`, called through a node: the listing a transfer compares, bounded for a reply.

#![allow(clippy::unwrap_used)]

use std::fs;

use legion_core::proto::{ErrorBody, ErrorCode};
use legion_core::{Call, Door, Node, Paths};
use serde_json::{Value, json};

struct Fixture {
    dir: tempfile::TempDir,
    node: Node,
}

impl Fixture {
    fn new() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("home/projects")).unwrap();
        let node = Node::new(
            Paths::under(dir.path().join("legion")).with_home(dir.path().join("home")),
            legion_tools::registry(),
        );
        Fixture { dir, node }
    }

    async fn call(&self, args: Value, principal: Option<&str>) -> Result<Value, ErrorBody> {
        let mut call = Call::new(Door::Cli, "fs.manifest", args);
        call.as_principal = principal.map(str::to_owned);
        self.node.call(call).await
    }
}

#[tokio::test]
async fn lists_a_tree_the_way_a_transfer_compares_it() {
    let f = Fixture::new();
    let root = f.dir.path().join("home/projects/app");
    fs::create_dir_all(root.join("src/empty")).unwrap();
    fs::write(root.join("src/main.rs"), b"fn main() {}").unwrap();
    fs::write(root.join("debug.log"), b"log").unwrap();
    fs::write(root.join("Cargo.toml"), b"[package]").unwrap();

    let listed = f
        .call(
            json!({"path": "~/projects/app", "checksum": true, "exclude": ["*.log"]}),
            None,
        )
        .await
        .unwrap();
    let paths: Vec<&str> = listed["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| entry["path"].as_str().unwrap())
        .collect();
    assert_eq!(paths, ["Cargo.toml", "src", "src/empty", "src/main.rs"]);
    assert_eq!(listed["total"], 4);
    assert_eq!(listed["truncated"], false);
    let main = &listed["entries"][3];
    assert_eq!(main["kind"], "file");
    assert_eq!(main["size"], 12);
    assert!(main["hash"].as_str().unwrap().starts_with("blake3:"));
    assert!(main["mtime"]["secs"].is_i64());

    let limited = f
        .call(json!({"path": "~/projects/app", "limit": 2}), None)
        .await
        .unwrap();
    assert_eq!(limited["entries"].as_array().unwrap().len(), 2);
    assert_eq!(limited["total"], 5);
    assert_eq!(limited["truncated"], true);
    assert!(limited["entries"][0].get("hash").is_none());

    let one = f
        .call(json!({"path": "~/projects/app/Cargo.toml"}), None)
        .await
        .unwrap();
    assert_eq!(one["entries"][0]["path"], "Cargo.toml");

    for limit in [0, 10_001] {
        let error = f
            .call(json!({"path": "~/projects/app", "limit": limit}), None)
            .await
            .unwrap_err();
        assert_eq!(error.code, ErrorCode::InvalidArgs, "{limit}");
    }
    let error = f.call(json!({"path": "~/none"}), None).await.unwrap_err();
    assert_eq!(error.code, ErrorCode::NotFound);
}

#[tokio::test]
async fn a_confined_role_lists_only_its_own_paths() {
    let f = Fixture::new();
    let config = f.dir.path().join("legion/config");
    fs::create_dir_all(&config).unwrap();
    fs::write(
        config.join("policy.toml"),
        "[principals.\"agent:x\"]\nrole = \"worker\"\n[roles.worker]\nallow = [\"effect:read\"]\npaths = [\"~/projects\"]\n",
    )
    .unwrap();
    fs::write(f.dir.path().join("home/notes.txt"), b"n").unwrap();
    f.call(json!({"path": "~/projects"}), Some("agent:x"))
        .await
        .unwrap();
    let error = f
        .call(json!({"path": "~"}), Some("agent:x"))
        .await
        .unwrap_err();
    assert_eq!(error.code, ErrorCode::Denied);
}

/// Whether this process can read a directory it has taken every permission from, as root can, in
/// which case nothing is unreadable to it.
#[cfg(unix)]
fn reads_anything(dir: &std::path::Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    let probe = dir.join("probe");
    fs::create_dir(&probe).unwrap();
    fs::set_permissions(&probe, fs::Permissions::from_mode(0o000)).unwrap();
    let readable = fs::read_dir(&probe).is_ok();
    fs::set_permissions(&probe, fs::Permissions::from_mode(0o755)).unwrap();
    fs::remove_dir(&probe).unwrap();
    readable
}

#[cfg(unix)]
#[tokio::test]
async fn what_could_not_be_read_is_bounded_like_the_entries() {
    use std::os::unix::fs::PermissionsExt;
    let f = Fixture::new();
    let root = f.dir.path().join("home/projects/locked");
    fs::create_dir_all(&root).unwrap();
    if reads_anything(&root) {
        return;
    }
    for at in 0..150 {
        let dir = root.join(format!("d{at:03}"));
        fs::create_dir(&dir).unwrap();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o000)).unwrap();
    }
    let listed = f
        .call(json!({"path": "~/projects/locked", "limit": 5}), None)
        .await
        .unwrap();
    for at in 0..150 {
        let dir = root.join(format!("d{at:03}"));
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
    }
    assert_eq!(listed["skipped"].as_array().unwrap().len(), 5, "{listed}");
    assert_eq!(listed["skipped_total"], 150);
    assert_eq!(listed["truncated"], true);
}

#[cfg(unix)]
#[tokio::test]
async fn a_role_without_admin_never_looks_inside_legions_own_directories() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    // Legion's files live under the home, as they do by default.
    let legion = home.join(".legion");
    let config = legion.join("config");
    fs::create_dir_all(&config).unwrap();
    if reads_anything(&config) {
        return;
    }
    fs::write(
        config.join("policy.toml"),
        "[principals.\"agent:x\"]\nrole = \"reader\"\n[roles.reader]\nallow = [\"effect:read\"]\n",
    )
    .unwrap();
    // Something in it that a walk would trip over and name, were it to look.
    let hidden = config.join("hidden-name");
    fs::create_dir(&hidden).unwrap();
    fs::set_permissions(&hidden, fs::Permissions::from_mode(0o000)).unwrap();
    fs::write(home.join("notes.txt"), b"n").unwrap();
    let node = Node::new(
        Paths::under(&legion).with_home(&home),
        legion_tools::registry(),
    );
    let mut call = Call::new(
        Door::Cli,
        "fs.manifest",
        json!({"path": "~", "checksum": true}),
    );
    call.as_principal = Some("agent:x".to_owned());
    let listed = node.call(call).await;
    fs::set_permissions(&hidden, fs::Permissions::from_mode(0o755)).unwrap();
    let listed = listed.unwrap();
    let text = listed.to_string();
    assert!(!text.contains("hidden-name"), "{text}");
    assert!(!text.contains("policy.toml"), "{text}");
    assert!(text.contains("notes.txt"), "{text}");
    assert!(text.contains("effect:admin"), "{text}");
}
