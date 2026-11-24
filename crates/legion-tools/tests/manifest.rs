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
