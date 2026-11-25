//! Integration tests for the `fs` tool family, exercised end to end through a real [`Node`].

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "support/audio_fixtures.rs"]
mod audio_fixtures;

use std::path::{Path, PathBuf};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use legion_core::proto::{Effect, ErrorBody, ErrorCode};
use legion_core::{Call, Door, Node, Paths};
use serde_json::{Value, json};

struct Fixture {
    dir: tempfile::TempDir,
    home: PathBuf,
    work: PathBuf,
    node: Node,
}

impl Fixture {
    fn new() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        let work = home.join("projects");
        std::fs::create_dir_all(&work).unwrap();
        let node = Node::new(
            Paths::under(dir.path()).with_home(&home),
            legion_tools::registry(),
        );
        Fixture {
            dir,
            home,
            work,
            node,
        }
    }

    fn write_config(&self, name: &str, text: &str) {
        let config = self.dir.path().join("config");
        std::fs::create_dir_all(&config).unwrap();
        std::fs::write(config.join(name), text).unwrap();
    }

    fn audit(&self) -> Vec<Value> {
        let text = std::fs::read_to_string(self.dir.path().join("state").join("audit.jsonl"))
            .unwrap_or_default();
        text.lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    fn p(&self, path: &Path) -> String {
        path.to_string_lossy().into_owned()
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
fn all_fourteen_fs_tools_are_registered_with_their_effects() {
    let registry = legion_tools::registry();
    let expected = [
        ("fs.copy", Effect::Write),
        ("fs.edit", Effect::Write),
        ("fs.find", Effect::Read),
        ("fs.grep", Effect::Read),
        ("fs.hash", Effect::Read),
        ("fs.list", Effect::Read),
        ("fs.mkdir", Effect::Write),
        ("fs.move", Effect::Write),
        ("fs.read", Effect::Read),
        ("fs.remove", Effect::Destroy),
        ("fs.stat", Effect::Read),
        ("fs.tags", Effect::Read),
        ("fs.thumb", Effect::Read),
        ("fs.write", Effect::Write),
    ];
    for (name, effect) in expected {
        let spec = registry
            .get(name)
            .unwrap_or_else(|| panic!("{name} is not registered"))
            .spec();
        assert_eq!(spec.effect, effect, "{name}");
        assert_eq!(spec.input_schema["type"], json!("object"), "{name}");
        assert_eq!(
            spec.input_schema["additionalProperties"],
            json!(false),
            "{name}"
        );
        assert!(spec.description.ends_with('.'), "{name}");
    }
}

#[tokio::test]
async fn list_orders_directories_first_handles_depth_hidden_and_truncation() {
    let f = Fixture::new();
    std::fs::create_dir_all(f.work.join("b_dir").join("inner")).unwrap();
    std::fs::create_dir_all(f.work.join("a_dir")).unwrap();
    std::fs::write(f.work.join("z.txt"), "z").unwrap();
    std::fs::write(f.work.join("a.txt"), "a").unwrap();
    std::fs::write(f.work.join(".hidden"), "secret").unwrap();
    std::fs::write(f.work.join("b_dir").join("child.txt"), "c").unwrap();
    std::fs::write(f.work.join("b_dir").join("inner").join("deep.txt"), "d").unwrap();

    // Default: depth 1, hidden included, directories first then by name.
    let listed = f
        .call("fs.list", json!({"path": "~/projects"}))
        .await
        .unwrap();
    let names: Vec<&str> = listed["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["a_dir", "b_dir", ".hidden", "a.txt", "z.txt"]);
    assert_eq!(listed["truncated"], json!(false));
    assert_eq!(listed["entries"][0]["kind"], json!("dir"));
    assert_eq!(listed["entries"][3]["kind"], json!("file"));
    assert!(
        listed["entries"][3]["mtime"]
            .as_str()
            .unwrap()
            .ends_with('Z')
    );

    // Excluding hidden entries drops dotfiles.
    let visible = f
        .call("fs.list", json!({"path": "~/projects", "hidden": false}))
        .await
        .unwrap();
    let visible_names: Vec<&str> = visible["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["name"].as_str().unwrap())
        .collect();
    assert_eq!(visible_names, ["a_dir", "b_dir", "a.txt", "z.txt"]);

    // Depth 2 includes direct children of subdirectories, not depth 3.
    let deep = f
        .call(
            "fs.list",
            json!({"path": "~/projects", "depth": 2, "hidden": false}),
        )
        .await
        .unwrap();
    let deep_names: Vec<&str> = deep["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["name"].as_str().unwrap())
        .collect();
    assert!(deep_names.contains(&"child.txt"));
    assert!(deep_names.contains(&"inner"));
    assert!(!deep_names.contains(&"deep.txt"));

    // Exact limit is not truncated; one less is truncated.
    let exact = f
        .call(
            "fs.list",
            json!({"path": "~/projects", "hidden": false, "limit": 4}),
        )
        .await
        .unwrap();
    assert_eq!(exact["entries"].as_array().unwrap().len(), 4);
    assert_eq!(exact["truncated"], json!(false));

    let cut = f
        .call(
            "fs.list",
            json!({"path": "~/projects", "hidden": false, "limit": 2}),
        )
        .await
        .unwrap();
    assert_eq!(cut["entries"].as_array().unwrap().len(), 2);
    assert_eq!(cut["truncated"], json!(true));

    // Invalid bounds are rejected cleanly.
    assert_eq!(
        f.call("fs.list", json!({"path": "~/projects", "depth": 9}))
            .await
            .unwrap_err()
            .code,
        ErrorCode::InvalidArgs
    );
    assert_eq!(
        f.call("fs.list", json!({"path": "~/projects", "limit": 5001}))
            .await
            .unwrap_err()
            .code,
        ErrorCode::InvalidArgs
    );
}

#[cfg(unix)]
#[tokio::test]
async fn list_and_stat_report_symlinks_without_following_and_include_unix_mode() {
    use std::os::unix::fs::{PermissionsExt, symlink};

    let f = Fixture::new();
    let target_dir = f.work.join("real_dir");
    std::fs::create_dir_all(&target_dir).unwrap();
    std::fs::write(target_dir.join("inside.txt"), "hello").unwrap();
    let file = f.work.join("script.sh");
    std::fs::write(&file, "#!/bin/true\n").unwrap();
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o755)).unwrap();
    symlink(&target_dir, f.work.join("dir_link")).unwrap();

    let listed = f
        .call("fs.list", json!({"path": "~/projects", "depth": 3}))
        .await
        .unwrap();
    let entries = listed["entries"].as_array().unwrap();
    let link_entry = entries
        .iter()
        .find(|e| e["name"] == json!("dir_link"))
        .unwrap();
    assert_eq!(link_entry["kind"], json!("symlink"));
    assert_eq!(link_entry["target"], json!(f.p(&target_dir)));
    // `inside.txt` appears only once (under `real_dir`), never under `dir_link`.
    let inside_count = entries
        .iter()
        .filter(|e| e["name"] == json!("inside.txt"))
        .count();
    assert_eq!(inside_count, 1);

    let file_stat = f
        .call("fs.stat", json!({"path": f.p(&file)}))
        .await
        .unwrap();
    assert_eq!(file_stat["kind"], json!("file"));
    assert_eq!(file_stat["mode"], json!("0755"));
    assert_eq!(file_stat["readonly"], json!(false));

    let link_stat = f
        .call("fs.stat", json!({"path": "~/projects/dir_link"}))
        .await
        .unwrap();
    assert_eq!(link_stat["kind"], json!("symlink"));
    assert_eq!(link_stat["target"], json!(f.p(&target_dir)));
}

#[tokio::test]
async fn stat_returns_not_found_for_missing_path() {
    let f = Fixture::new();
    let err = f
        .call("fs.stat", json!({"path": "~/projects/nope.txt"}))
        .await
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::NotFound);

    let file = f.work.join("note.txt");
    std::fs::write(&file, "12345").unwrap();
    let st = f
        .call("fs.stat", json!({"path": f.p(&file)}))
        .await
        .unwrap();
    assert_eq!(st["kind"], json!("file"));
    assert_eq!(st["size"], json!(5));
    assert!(st["mtime"].as_str().unwrap().ends_with('Z'));
    assert_eq!(st["readonly"], json!(false));
    assert!(st.get("target").is_none());
}

#[tokio::test]
async fn read_and_hash_handle_utf8_boundaries_base64_paging_and_whole_file_hash() {
    let f = Fixture::new();
    let text_file = f.work.join("unicode.txt");
    let text = "hi€🦀ok";
    std::fs::write(&text_file, text).unwrap();

    // Whole file read includes whole-file hash matching fs.hash.
    let whole = f
        .call("fs.read", json!({"path": f.p(&text_file)}))
        .await
        .unwrap();
    let hashed = f
        .call("fs.hash", json!({"path": f.p(&text_file)}))
        .await
        .unwrap();
    assert_eq!(whole["encoding"], json!("utf8"));
    assert_eq!(whole["content"], json!(text));
    assert_eq!(whole["eof"], json!(true));
    assert_eq!(whole["length"], json!(text.len() as u64));
    assert_eq!(whole["size"], json!(text.len() as u64));
    assert_eq!(whole["hash"], hashed["hash"]);
    assert!(hashed["hash"].as_str().unwrap().starts_with("blake3:"));
    assert_eq!(hashed["size"], json!(text.len() as u64));

    // Reading with a length that lands inside `€` cuts cleanly before `€` and omits `hash`.
    let page1 = f
        .call("fs.read", json!({"path": f.p(&text_file), "length": 4}))
        .await
        .unwrap();
    assert_eq!(page1["encoding"], json!("utf8"));
    assert_eq!(page1["content"], json!("hi"));
    assert_eq!(page1["length"], json!(2));
    assert_eq!(page1["eof"], json!(false));
    assert!(page1.get("hash").is_none());

    // Continuing at offset 2 with length 5 lands inside `🦀` (4 bytes), so returns just `€`.
    let page2 = f
        .call(
            "fs.read",
            json!({"path": f.p(&text_file), "offset": 2, "length": 5}),
        )
        .await
        .unwrap();
    assert_eq!(page2["encoding"], json!("utf8"));
    assert_eq!(page2["content"], json!("€"));
    assert_eq!(page2["length"], json!(3));
    assert_eq!(page2["eof"], json!(false));
    assert!(page2.get("hash").is_none());

    let page3 = f
        .call("fs.read", json!({"path": f.p(&text_file), "offset": 5}))
        .await
        .unwrap();
    assert_eq!(page3["content"], json!("🦀ok"));
    assert_eq!(page3["eof"], json!(true));
    assert!(page3.get("hash").is_none());

    // Binary file auto-detects as base64 and refuses explicit utf8.
    let bin_file = f.work.join("blob.bin");
    let raw_bytes: Vec<u8> = vec![0x00, 0xFF, 0xFE, 0x80, 0x42];
    std::fs::write(&bin_file, &raw_bytes).unwrap();
    let bin = f
        .call("fs.read", json!({"path": f.p(&bin_file)}))
        .await
        .unwrap();
    assert_eq!(bin["encoding"], json!("base64"));
    assert_eq!(
        BASE64.decode(bin["content"].as_str().unwrap()).unwrap(),
        raw_bytes
    );
    assert_eq!(bin["eof"], json!(true));
    assert!(bin["hash"].as_str().unwrap().starts_with("blake3:"));

    let bad_utf8 = f
        .call(
            "fs.read",
            json!({"path": f.p(&bin_file), "encoding": "utf8"}),
        )
        .await
        .unwrap_err();
    assert_eq!(bad_utf8.code, ErrorCode::Failed);

    // Asking for more than 1 MiB is rejected.
    let too_big = f
        .call(
            "fs.read",
            json!({"path": f.p(&text_file), "length": 1_048_577}),
        )
        .await
        .unwrap_err();
    assert_eq!(too_big.code, ErrorCode::InvalidArgs);
}

#[tokio::test]
async fn write_supports_modes_parents_base64_expect_hash_and_redacts_audit() {
    let f = Fixture::new();
    let nested = f.work.join("nested").join("file.txt");

    // Missing parent without `parents: true` fails with not_found.
    assert_eq!(
        f.call("fs.write", json!({"path": f.p(&nested), "content": "hi"}))
            .await
            .unwrap_err()
            .code,
        ErrorCode::NotFound
    );

    // `parents: true` and `mode: "create"` creates the file.
    let created = f
        .call(
            "fs.write",
            json!({
                "path": f.p(&nested),
                "content": "hello",
                "mode": "create",
                "parents": true,
            }),
        )
        .await
        .unwrap();
    assert_eq!(created["created"], json!(true));
    assert_eq!(created["size"], json!(5));
    let hash1 = created["hash"].as_str().unwrap().to_owned();

    // `mode: "create"` on an existing file fails with `exists`.
    assert_eq!(
        f.call(
            "fs.write",
            json!({"path": f.p(&nested), "content": "again", "mode": "create"})
        )
        .await
        .unwrap_err()
        .code,
        ErrorCode::Exists
    );

    // `expect_hash` mismatch fails with `conflict` and a hint.
    let conflict = f
        .call(
            "fs.write",
            json!({
                "path": f.p(&nested),
                "content": "stale",
                "expect_hash": "blake3:0000",
            }),
        )
        .await
        .unwrap_err();
    assert_eq!(conflict.code, ErrorCode::Conflict);
    assert!(conflict.hint.is_some());

    // `expect_hash` on a missing file fails with `conflict`.
    let missing_conflict = f
        .call(
            "fs.write",
            json!({
                "path": f.p(&f.work.join("absent.txt")),
                "content": "x",
                "expect_hash": hash1,
            }),
        )
        .await
        .unwrap_err();
    assert_eq!(missing_conflict.code, ErrorCode::Conflict);

    // Combining `mode: "append"` with `expect_hash` is rejected with `invalid_args`.
    assert_eq!(
        f.call(
            "fs.write",
            json!({
                "path": f.p(&nested),
                "content": "!",
                "mode": "append",
                "expect_hash": hash1,
            }),
        )
        .await
        .unwrap_err()
        .code,
        ErrorCode::InvalidArgs
    );

    // Append uses OS append mode without replacing the file.
    let appended = f
        .call(
            "fs.write",
            json!({
                "path": f.p(&nested),
                "content": " world",
                "mode": "append",
            }),
        )
        .await
        .unwrap();
    assert_eq!(appended["created"], json!(false));
    assert_eq!(appended["size"], json!(11));
    assert!(appended.get("hash").is_none());
    assert_eq!(std::fs::read_to_string(&nested).unwrap(), "hello world");

    // Base64 write round-trips raw bytes.
    let bin = f.work.join("raw.bin");
    let bytes = vec![1u8, 0, 255, 42];
    f.call(
        "fs.write",
        json!({
            "path": f.p(&bin),
            "content": BASE64.encode(&bytes),
            "encoding": "base64",
        }),
    )
    .await
    .unwrap();
    assert_eq!(std::fs::read(&bin).unwrap(), bytes);

    // Audit log redacts `content`.
    let audit = f.audit();
    let write_entry = audit
        .iter()
        .find(|line| line["tool"] == json!("fs.write") && line["outcome"] == json!("ok"))
        .unwrap();
    assert_eq!(write_entry["args"]["content"], json!({"redacted_bytes": 5}));
}

#[cfg(unix)]
#[tokio::test]
async fn write_and_edit_honor_and_preserve_unix_mode() {
    use std::os::unix::fs::PermissionsExt;

    let f = Fixture::new();
    let secret = f.work.join("secret.env");
    f.call(
        "fs.write",
        json!({
            "path": f.p(&secret),
            "content": "KEY=old\n",
            "unix_mode": "0600",
        }),
    )
    .await
    .unwrap();
    assert_eq!(
        std::fs::metadata(&secret).unwrap().permissions().mode() & 0o777,
        0o600
    );

    // `fs.edit` preserves the file's 0600 permissions.
    f.call(
        "fs.edit",
        json!({"path": f.p(&secret), "old": "KEY=old", "new": "KEY=new"}),
    )
    .await
    .unwrap();
    assert_eq!(
        std::fs::metadata(&secret).unwrap().permissions().mode() & 0o777,
        0o600
    );

    // Explicit `unix_mode` on an overwrite updates permissions.
    f.call(
        "fs.write",
        json!({
            "path": f.p(&secret),
            "content": "KEY=public\n",
            "unix_mode": "0644",
        }),
    )
    .await
    .unwrap();
    assert_eq!(
        std::fs::metadata(&secret).unwrap().permissions().mode() & 0o777,
        0o644
    );
}

#[tokio::test]
async fn edit_enforces_exact_match_conflicts_crlf_and_audit_redaction() {
    let f = Fixture::new();
    let file = f.work.join("doc.txt");
    std::fs::write(&file, "alpha\r\nbeta\r\nalpha\r\n").unwrap();

    let read = f
        .call("fs.read", json!({"path": f.p(&file)}))
        .await
        .unwrap();
    let hash = read["hash"].as_str().unwrap().to_owned();

    // Missing `old` is `not_found` with a hint.
    let missing = f
        .call(
            "fs.edit",
            json!({"path": f.p(&file), "old": "missing", "new": "x"}),
        )
        .await
        .unwrap_err();
    assert_eq!(missing.code, ErrorCode::NotFound);
    assert!(missing.hint.is_some());

    // Multiple matches without `all` is `conflict` saying how many.
    let ambig = f
        .call(
            "fs.edit",
            json!({"path": f.p(&file), "old": "alpha", "new": "ALPHA"}),
        )
        .await
        .unwrap_err();
    assert_eq!(ambig.code, ErrorCode::Conflict);
    assert!(ambig.message.contains("2 matches"), "{}", ambig.message);

    // Wrong `expect_hash` is `conflict`.
    assert_eq!(
        f.call(
            "fs.edit",
            json!({
                "path": f.p(&file),
                "old": "beta",
                "new": "BETA",
                "expect_hash": "blake3:wrong",
            }),
        )
        .await
        .unwrap_err()
        .code,
        ErrorCode::Conflict
    );

    // `all: true` with matching `expect_hash` replaces both and keeps CRLF.
    let edited = f
        .call(
            "fs.edit",
            json!({
                "path": f.p(&file),
                "old": "alpha",
                "new": "ALPHA",
                "all": true,
                "expect_hash": hash,
            }),
        )
        .await
        .unwrap();
    assert_eq!(edited["replacements"], json!(2));
    assert_eq!(
        std::fs::read_to_string(&file).unwrap(),
        "ALPHA\r\nbeta\r\nALPHA\r\n"
    );

    // Audit log redacts `old` and `new` in `fs.edit` calls.
    let audit = f.audit();
    let edit_entry = audit
        .iter()
        .rev()
        .find(|line| line["tool"] == json!("fs.edit") && line["outcome"] == json!("ok"))
        .unwrap();
    assert_eq!(edit_entry["args"]["old"], json!({"redacted_bytes": 5}));
    assert_eq!(edit_entry["args"]["new"], json!({"redacted_bytes": 5}));
}

#[tokio::test]
async fn mkdir_handles_parents_idempotence_and_conflicts() {
    let f = Fixture::new();
    let deep = f.work.join("a").join("b").join("c");

    // Without parents, missing intermediate fails with `not_found`.
    assert_eq!(
        f.call("fs.mkdir", json!({"path": f.p(&deep), "parents": false}))
            .await
            .unwrap_err()
            .code,
        ErrorCode::NotFound
    );

    // Default `parents: true` creates all and reports `created: true`, then `false`.
    let first = f
        .call("fs.mkdir", json!({"path": f.p(&deep)}))
        .await
        .unwrap();
    assert_eq!(first["created"], json!(true));
    let second = f
        .call("fs.mkdir", json!({"path": f.p(&deep)}))
        .await
        .unwrap();
    assert_eq!(second["created"], json!(false));

    // `parents: false` on an existing directory fails with `exists`.
    assert_eq!(
        f.call("fs.mkdir", json!({"path": f.p(&deep), "parents": false}))
            .await
            .unwrap_err()
            .code,
        ErrorCode::Exists
    );

    // Existing regular file at target fails with `exists` even with `parents: true`.
    let file = f.work.join("not_a_dir");
    std::fs::write(&file, "x").unwrap();
    assert_eq!(
        f.call("fs.mkdir", json!({"path": f.p(&file)}))
            .await
            .unwrap_err()
            .code,
        ErrorCode::Exists
    );
}

#[tokio::test]
async fn move_and_copy_handle_overwrite_recursion_and_errors() {
    let f = Fixture::new();
    let src = f.work.join("src.txt");
    let dst = f.work.join("dst.txt");
    std::fs::write(&src, "payload").unwrap();

    // Copy file.
    let copied = f
        .call("fs.copy", json!({"from": f.p(&src), "to": f.p(&dst)}))
        .await
        .unwrap();
    assert_eq!(copied["files"], json!(1));
    assert_eq!(copied["bytes"], json!(7));
    assert_eq!(std::fs::read_to_string(&dst).unwrap(), "payload");

    // Copy without overwrite fails with `exists`; with overwrite succeeds.
    assert_eq!(
        f.call("fs.copy", json!({"from": f.p(&src), "to": f.p(&dst)}))
            .await
            .unwrap_err()
            .code,
        ErrorCode::Exists
    );
    std::fs::write(&src, "updated").unwrap();
    f.call(
        "fs.copy",
        json!({"from": f.p(&src), "to": f.p(&dst), "overwrite": true}),
    )
    .await
    .unwrap();
    assert_eq!(std::fs::read_to_string(&dst).unwrap(), "updated");

    // Recursive directory copy.
    let tree = f.work.join("tree");
    std::fs::create_dir_all(tree.join("sub")).unwrap();
    std::fs::write(tree.join("a.txt"), "aaa").unwrap();
    std::fs::write(tree.join("sub").join("b.txt"), "bbbbb").unwrap();
    let tree_copy = f.work.join("tree_copy");
    assert_eq!(
        f.call(
            "fs.copy",
            json!({"from": f.p(&tree), "to": f.p(&tree_copy)})
        )
        .await
        .unwrap_err()
        .code,
        ErrorCode::InvalidArgs
    );
    let rec = f
        .call(
            "fs.copy",
            json!({"from": f.p(&tree), "to": f.p(&tree_copy), "recursive": true}),
        )
        .await
        .unwrap();
    assert_eq!(rec["files"], json!(2));
    assert_eq!(rec["bytes"], json!(8));
    assert_eq!(
        std::fs::read_to_string(tree_copy.join("sub").join("b.txt")).unwrap(),
        "bbbbb"
    );

    // Copying a directory into itself is rejected.
    assert_eq!(
        f.call(
            "fs.copy",
            json!({"from": f.p(&tree), "to": f.p(&tree.join("nested")), "recursive": true})
        )
        .await
        .unwrap_err()
        .code,
        ErrorCode::InvalidArgs
    );

    // Move without overwrite fails when target exists; with overwrite succeeds.
    assert_eq!(
        f.call("fs.move", json!({"from": f.p(&src), "to": f.p(&dst)}))
            .await
            .unwrap_err()
            .code,
        ErrorCode::Exists
    );
    f.call(
        "fs.move",
        json!({"from": f.p(&src), "to": f.p(&dst), "overwrite": true}),
    )
    .await
    .unwrap();
    assert!(!src.exists());
    assert_eq!(std::fs::read_to_string(&dst).unwrap(), "updated");

    // Moving a directory into itself is rejected.
    assert_eq!(
        f.call(
            "fs.move",
            json!({"from": f.p(&tree), "to": f.p(&tree.join("inside"))})
        )
        .await
        .unwrap_err()
        .code,
        ErrorCode::InvalidArgs
    );
}

#[cfg(unix)]
#[tokio::test]
async fn copy_preserves_symlinks_without_following_out_of_tree() {
    use std::os::unix::fs::symlink;

    let f = Fixture::new();
    let outside = f.home.join("outside_dir");
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::write(outside.join("secret.txt"), "top-secret").unwrap();

    let src = f.work.join("src_tree");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(src.join("local.txt"), "ok").unwrap();
    symlink(&outside, src.join("escape_link")).unwrap();

    let dst = f.work.join("dst_tree");
    let res = f
        .call(
            "fs.copy",
            json!({"from": f.p(&src), "to": f.p(&dst), "recursive": true}),
        )
        .await
        .unwrap();
    // 1 regular file + 1 symlink; `secret.txt` inside `outside` was NOT copied.
    assert_eq!(res["files"], json!(2));
    assert_eq!(res["bytes"], json!(2));
    let copied_link = dst.join("escape_link");
    assert!(
        std::fs::symlink_metadata(&copied_link)
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert_eq!(std::fs::read_link(&copied_link).unwrap(), outside);
}

#[tokio::test]
async fn remove_handles_dry_run_sample_cap_conflicts_and_protected_refusals() {
    let f = Fixture::new();

    // Protected refusals (always `denied`): filesystem root, home itself, config and state dirs,
    // and any ancestor of them.
    let fs_root = f.dir.path().ancestors().last().unwrap().to_path_buf();
    let config_dir = f.dir.path().join("config");
    let state_dir = f.dir.path().join("state");
    let home_with_slash = format!("{}/", f.p(&f.home));
    for protected in [
        f.p(&fs_root),
        "~".to_owned(),
        f.p(&f.home),
        home_with_slash,
        f.p(&config_dir),
        f.p(&state_dir),
    ] {
        let err = f
            .call(
                "fs.remove",
                json!({"path": protected, "recursive": true, "dry_run": true}),
            )
            .await
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::Denied, "should refuse {protected}");
    }
    // Recursive removal of LEGION_HOME (ancestor of config & state) is also refused.
    assert_eq!(
        f.call(
            "fs.remove",
            json!({"path": f.p(f.dir.path()), "recursive": true, "dry_run": true})
        )
        .await
        .unwrap_err()
        .code,
        ErrorCode::Denied
    );

    // Non-empty directory without `recursive` is `conflict`.
    let batch = f.work.join("batch");
    std::fs::create_dir_all(&batch).unwrap();
    for idx in 0..60 {
        std::fs::write(batch.join(format!("f_{idx:02}.txt")), "abcd").unwrap();
    }
    assert_eq!(
        f.call("fs.remove", json!({"path": f.p(&batch)}))
            .await
            .unwrap_err()
            .code,
        ErrorCode::Conflict
    );

    // `dry_run: true` counts everything, caps `sample` at 50, and deletes nothing.
    let preview = f
        .call(
            "fs.remove",
            json!({"path": f.p(&batch), "recursive": true, "dry_run": true}),
        )
        .await
        .unwrap();
    assert_eq!(preview["dry_run"], json!(true));
    assert_eq!(preview["removed"], json!(61));
    assert_eq!(preview["bytes"], json!(240));
    assert_eq!(preview["sample"].as_array().unwrap().len(), 50);
    assert!(batch.join("f_59.txt").exists());

    // Actual recursive removal deletes the tree and matches dry_run totals.
    let removed = f
        .call("fs.remove", json!({"path": f.p(&batch), "recursive": true}))
        .await
        .unwrap();
    assert_eq!(removed["dry_run"], json!(false));
    assert_eq!(removed["removed"], json!(61));
    assert_eq!(removed["bytes"], json!(240));
    assert_eq!(removed["sample"].as_array().unwrap().len(), 50);
    assert!(!batch.exists());

    // Empty directory can be removed without `recursive`.
    let empty = f.work.join("empty");
    std::fs::create_dir_all(&empty).unwrap();
    let res = f
        .call("fs.remove", json!({"path": f.p(&empty)}))
        .await
        .unwrap();
    assert_eq!(res["removed"], json!(1));
    assert!(!empty.exists());
}

#[cfg(unix)]
#[tokio::test]
async fn remove_on_symlink_deletes_only_the_link_never_its_target() {
    use std::os::unix::fs::symlink;

    let f = Fixture::new();
    let target = f.work.join("precious.txt");
    std::fs::write(&target, "keep me").unwrap();
    let link = f.work.join("alias.txt");
    symlink(&target, &link).unwrap();

    let res = f
        .call("fs.remove", json!({"path": f.p(&link)}))
        .await
        .unwrap();
    assert_eq!(res["removed"], json!(1));
    assert!(std::fs::symlink_metadata(&link).is_err());
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "keep me");
}

#[tokio::test]
async fn find_and_grep_respect_ignore_globs_binary_context_long_lines_and_limits() {
    let f = Fixture::new();
    std::fs::write(f.work.join(".gitignore"), "ignored.rs\nbuild/\n").unwrap();
    std::fs::create_dir_all(f.work.join("src")).unwrap();
    std::fs::create_dir_all(f.work.join("build")).unwrap();
    std::fs::write(f.work.join("ignored.rs"), "needle in ignored\n").unwrap();
    std::fs::write(f.work.join("build").join("out.rs"), "needle in build\n").unwrap();
    std::fs::write(f.work.join(".secret.rs"), "needle in hidden\n").unwrap();
    std::fs::write(
        f.work.join("src").join("lib.rs"),
        "line1\nline2\nNEEDLE here\nline4\nline5\n",
    )
    .unwrap();
    std::fs::write(
        f.work.join("src").join("long.txt"),
        format!("prefix needle {}\n", "x".repeat(600)),
    )
    .unwrap();
    std::fs::write(
        f.work.join("src").join("data.bin"),
        b"needle\x00binary\x00bytes\n",
    )
    .unwrap();

    // `fs.find` with `ignore: true` (default) skips gitignored & hidden entries.
    let found = f
        .call("fs.find", json!({"path": "~/projects", "glob": "**/*.rs"}))
        .await
        .unwrap();
    let found_paths: Vec<&str> = found["matches"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["path"].as_str().unwrap())
        .collect();
    assert_eq!(found_paths.len(), 1, "{found_paths:?}");
    assert!(found_paths[0].ends_with("lib.rs"));
    assert_eq!(found["truncated"], json!(false));

    // `fs.find` with `ignore: false` finds gitignored and hidden `.rs` files too.
    let all_rs = f
        .call(
            "fs.find",
            json!({"path": "~/projects", "glob": "**/*.rs", "ignore": false}),
        )
        .await
        .unwrap();
    assert_eq!(all_rs["matches"].as_array().unwrap().len(), 4);

    // `fs.find` honors `kind`, `max_depth`, and `limit` truncation.
    let dirs_only = f
        .call(
            "fs.find",
            json!({"path": "~/projects", "kind": "dir", "max_depth": 1}),
        )
        .await
        .unwrap();
    assert_eq!(dirs_only["matches"].as_array().unwrap().len(), 1);
    assert_eq!(dirs_only["matches"][0]["kind"], json!("dir"));

    let find_cut = f
        .call(
            "fs.find",
            json!({"path": "~/projects", "glob": "**/*.rs", "ignore": false, "limit": 2}),
        )
        .await
        .unwrap();
    assert_eq!(find_cut["matches"].as_array().unwrap().len(), 2);
    assert_eq!(find_cut["truncated"], json!(true));

    // `fs.grep` skips binary, gitignored and hidden files, supports case-insensitive + context,
    // and truncates >500-char lines with a marker.
    let grepped = f
        .call(
            "fs.grep",
            json!({
                "path": "~/projects",
                "pattern": "needle",
                "ignore_case": true,
                "context": 2,
            }),
        )
        .await
        .unwrap();
    assert_eq!(grepped["truncated"], json!(false));
    assert_eq!(grepped["files_searched"], json!(2));
    let matches = grepped["matches"].as_array().unwrap();
    assert_eq!(matches.len(), 2);
    let lib_match = matches
        .iter()
        .find(|m| m["path"].as_str().unwrap().ends_with("lib.rs"))
        .unwrap();
    assert_eq!(lib_match["line"], json!(3));
    assert_eq!(lib_match["text"], json!("NEEDLE here"));
    assert_eq!(lib_match["before"], json!(["line1", "line2"]));
    assert_eq!(lib_match["after"], json!(["line4", "line5"]));

    let long_match = matches
        .iter()
        .find(|m| m["path"].as_str().unwrap().ends_with("long.txt"))
        .unwrap();
    let long_text = long_match["text"].as_str().unwrap();
    assert!(long_text.ends_with("... [truncated]"), "{long_text}");
    assert!(long_text.chars().count() < 530);

    // `fs.grep` truncation with `limit: 1` keeps context on the returned match and flags truncated.
    let cut = f
        .call(
            "fs.grep",
            json!({
                "path": "~/projects",
                "pattern": "needle",
                "ignore_case": true,
                "context": 1,
                "limit": 1,
            }),
        )
        .await
        .unwrap();
    assert_eq!(cut["truncated"], json!(true));
    assert_eq!(cut["matches"].as_array().unwrap().len(), 1);
    assert!(cut["matches"][0]["after"].is_array());
}

#[tokio::test]
async fn policy_enforces_effects_and_path_confinement_across_every_fs_tool() {
    let f = Fixture::new();
    f.write_config(
        "policy.toml",
        r#"
[principals.reader]
role = "observer"

[principals.builder]
role = "builder"

[principals.jailed]
role = "jailed"

[roles.observer]
allow = ["effect:read"]

[roles.builder]
allow = ["effect:read", "effect:write"]

[roles.jailed]
allow = ["fs.*"]
paths = ["~/projects/**"]
"#,
    );

    let inside = f.work.join("inside.txt");
    let outside = f.home.join("secret.txt");
    std::fs::write(&inside, "inside-data\n").unwrap();
    std::fs::write(&outside, "outside-secret\n").unwrap();

    // Observer can read, cannot write or destroy.
    assert!(
        f.call_as("reader", "fs.read", json!({"path": f.p(&inside)}))
            .await
            .is_ok()
    );
    assert_eq!(
        f.call_as(
            "reader",
            "fs.write",
            json!({"path": f.p(&inside), "content": "nope"})
        )
        .await
        .unwrap_err()
        .code,
        ErrorCode::Denied
    );
    assert_eq!(
        f.call_as("reader", "fs.remove", json!({"path": f.p(&inside)}))
            .await
            .unwrap_err()
            .code,
        ErrorCode::Denied
    );

    // Builder can write, cannot destroy (`fs.remove` has effect `destroy`).
    assert!(
        f.call_as(
            "builder",
            "fs.write",
            json!({"path": f.p(&inside), "content": "built\n"})
        )
        .await
        .is_ok()
    );
    assert_eq!(
        f.call_as("builder", "fs.remove", json!({"path": f.p(&inside)}))
            .await
            .unwrap_err()
            .code,
        ErrorCode::Denied
    );

    // Confined role cannot read or write outside `~/projects` through ANY fs tool, directly or via `..`.
    let escape_via_dotdot = "~/projects/../secret.txt";
    let escape_dir_dotdot = "~/projects/..";
    let attempts = [
        ("fs.list", json!({"path": escape_dir_dotdot})),
        ("fs.stat", json!({"path": escape_via_dotdot})),
        ("fs.read", json!({"path": escape_via_dotdot})),
        ("fs.hash", json!({"path": escape_via_dotdot})),
        (
            "fs.write",
            json!({"path": escape_via_dotdot, "content": "pwn"}),
        ),
        (
            "fs.edit",
            json!({"path": escape_via_dotdot, "old": "outside", "new": "pwn"}),
        ),
        ("fs.mkdir", json!({"path": "~/projects/../evil_dir"})),
        (
            "fs.move",
            json!({"from": "~/projects/inside.txt", "to": "~/projects/../moved.txt"}),
        ),
        (
            "fs.move",
            json!({"from": escape_via_dotdot, "to": "~/projects/stolen.txt"}),
        ),
        (
            "fs.copy",
            json!({"from": escape_via_dotdot, "to": "~/projects/stolen.txt"}),
        ),
        (
            "fs.copy",
            json!({"from": "~/projects/inside.txt", "to": "~/projects/../pwn.txt"}),
        ),
        ("fs.remove", json!({"path": escape_via_dotdot})),
        ("fs.find", json!({"path": escape_dir_dotdot})),
        (
            "fs.grep",
            json!({"path": escape_via_dotdot, "pattern": "secret"}),
        ),
    ];
    for (tool, args) in attempts {
        let err = f.call_as("jailed", tool, args.clone()).await.unwrap_err();
        assert_eq!(
            err.code,
            ErrorCode::Denied,
            "{tool} with {args} escaped confinement: {err}"
        );
    }

    // Inside `~/projects` works for the confined role.
    assert!(
        f.call_as(
            "jailed",
            "fs.read",
            json!({"path": "~/projects/inside.txt"})
        )
        .await
        .is_ok()
    );
    assert_eq!(
        std::fs::read_to_string(&outside).unwrap(),
        "outside-secret\n"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn confined_role_cannot_escape_through_symlinks_in_any_fs_tool() {
    use std::os::unix::fs::symlink;

    let f = Fixture::new();
    f.write_config(
        "policy.toml",
        r#"
[principals.jailed]
role = "jailed"

[roles.jailed]
allow = ["effect:read", "effect:write", "effect:destroy"]
paths = ["~/projects/**"]
"#,
    );

    let outside_dir = f.home.join("vault");
    std::fs::create_dir_all(&outside_dir).unwrap();
    let outside_file = outside_dir.join("key.txt");
    std::fs::write(&outside_file, "VAULT_SECRET_VALUE\n").unwrap();

    symlink(&outside_file, f.work.join("link_file")).unwrap();
    symlink(&outside_dir, f.work.join("link_dir")).unwrap();

    for (tool, args) in [
        ("fs.stat", json!({"path": "~/projects/link_file"})),
        ("fs.read", json!({"path": "~/projects/link_file"})),
        ("fs.hash", json!({"path": "~/projects/link_file"})),
        ("fs.list", json!({"path": "~/projects/link_dir"})),
        ("fs.find", json!({"path": "~/projects/link_dir"})),
        (
            "fs.grep",
            json!({"path": "~/projects/link_file", "pattern": "VAULT"}),
        ),
        (
            "fs.write",
            json!({"path": "~/projects/link_file", "content": "pwn"}),
        ),
        (
            "fs.write",
            json!({"path": "~/projects/link_dir/pwn.txt", "content": "pwn"}),
        ),
        (
            "fs.edit",
            json!({"path": "~/projects/link_file", "old": "VAULT", "new": "PWN"}),
        ),
        ("fs.mkdir", json!({"path": "~/projects/link_dir/sub"})),
        (
            "fs.copy",
            json!({"from": "~/projects/link_file", "to": "~/projects/stolen.txt"}),
        ),
        (
            "fs.move",
            json!({"from": "~/projects/link_file", "to": "~/projects/stolen.txt"}),
        ),
        ("fs.remove", json!({"path": "~/projects/link_file"})),
    ] {
        let err = f.call_as("jailed", tool, args.clone()).await.unwrap_err();
        assert_eq!(
            err.code,
            ErrorCode::Denied,
            "{tool} with {args} escaped via symlink: {err}"
        );
    }

    // Grepping the allowed directory must skip symlinks and never leak `VAULT_SECRET_VALUE`.
    let grep = f
        .call_as(
            "jailed",
            "fs.grep",
            json!({"path": "~/projects", "pattern": "VAULT_SECRET_VALUE"}),
        )
        .await
        .unwrap();
    assert_eq!(grep["matches"].as_array().unwrap().len(), 0);

    // Recursive copy into an existing destination must not follow a symlink inside the destination
    // out of the allowed root.
    let src_dir = f.work.join("src_dir");
    let dst_dir = f.work.join("dst_dir");
    std::fs::create_dir_all(src_dir.join("sub")).unwrap();
    std::fs::write(src_dir.join("sub").join("pwn.txt"), "bad").unwrap();
    std::fs::create_dir_all(&dst_dir).unwrap();
    symlink(&outside_dir, dst_dir.join("sub")).unwrap();
    let copy_err = f
        .call_as(
            "jailed",
            "fs.copy",
            json!({
                "from": "~/projects/src_dir",
                "to": "~/projects/dst_dir",
                "recursive": true,
                "overwrite": true,
            }),
        )
        .await
        .unwrap_err();
    assert_eq!(copy_err.code, ErrorCode::Conflict);
    assert!(!outside_dir.join("pwn.txt").exists());
    assert_eq!(
        std::fs::read_to_string(&outside_file).unwrap(),
        "VAULT_SECRET_VALUE\n"
    );
}

#[tokio::test]
async fn cancelled_calls_stop_with_cancelled_error() {
    let f = Fixture::new();
    let file = f.work.join("data.txt");
    std::fs::write(&file, "hello").unwrap();

    let call = Call::new(Door::Rpc, "fs.hash", json!({"path": f.p(&file)}));
    call.cancel.cancel();
    let err = f.node.call(call).await.unwrap_err();
    assert_eq!(err.code, ErrorCode::Cancelled);
}

#[tokio::test]
async fn protected_ancestors_and_move_sources_are_refused_by_file_identity() {
    // Separate Legion's home from its config/state root so removing the parent of `home` tests
    // the home-ancestor refusal specifically (independent of config/state).
    let base = tempfile::tempdir().unwrap();
    let legion_root = base.path().join("legion");
    let home_parent = base.path().join("users");
    let home = home_parent.join("me");
    let work = home.join("projects");
    std::fs::create_dir_all(&work).unwrap();
    let node = Node::new(
        Paths::under(&legion_root).with_home(&home),
        legion_tools::registry(),
    );

    let call = |tool: &str, args: Value| {
        let node = node.clone();
        let tool = tool.to_owned();
        async move { node.call(Call::new(Door::Cli, tool, args)).await }
    };

    // Removing the parent of `home`, or the parent of `config`/`state`, is refused.
    for ancestor in [&home_parent, &legion_root] {
        let err = call(
            "fs.remove",
            json!({"path": ancestor.to_string_lossy(), "recursive": true, "dry_run": true}),
        )
        .await
        .unwrap_err();
        assert_eq!(err.code, ErrorCode::Denied, "{}", ancestor.display());
    }

    // Moving `home`, its parent, or Legion's config/state directories is refused too.
    let dst = base.path().join("relocated");
    for source in [
        home.clone(),
        home_parent.clone(),
        legion_root.join("config"),
        legion_root.join("state"),
        legion_root.clone(),
    ] {
        let err = call(
            "fs.move",
            json!({"from": source.to_string_lossy(), "to": dst.to_string_lossy()}),
        )
        .await
        .unwrap_err();
        assert_eq!(err.code, ErrorCode::Denied, "{}", source.display());
    }

    // On case-folding volumes, an alternate-case spelling of `home` is still refused.
    #[cfg(any(windows, target_os = "macos"))]
    {
        let upper_home = home_parent.join("ME");
        let err = call(
            "fs.remove",
            json!({"path": upper_home.to_string_lossy(), "recursive": true, "dry_run": true}),
        )
        .await
        .unwrap_err();
        assert_eq!(err.code, ErrorCode::Denied);
    }

    // Via a symlinked parent directory, `home` is still recognized by file identity and refused.
    #[cfg(unix)]
    {
        use std::os::unix::fs::symlink;
        let alias = base.path().join("users_alias");
        symlink(&home_parent, &alias).unwrap();
        let err = call(
            "fs.remove",
            json!({"path": alias.join("me").to_string_lossy(), "recursive": true, "dry_run": true}),
        )
        .await
        .unwrap_err();
        assert_eq!(err.code, ErrorCode::Denied);
    }
}

#[tokio::test]
async fn non_admin_walks_skip_legion_config_and_state_directories() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    let legion_dir = home.join(".legion");
    let paths = Paths::under(&legion_dir).with_home(&home);
    std::fs::create_dir_all(paths.config_dir()).unwrap();
    std::fs::write(
        paths.config_file("policy.toml"),
        r#"
[principals.agent]
role = "operator"

[roles.operator]
allow = ["effect:read", "effect:write"]
paths = ["~/**"]
"#,
    )
    .unwrap();
    std::fs::write(home.join("visible.txt"), "POLICY_MARKER\n").unwrap();
    let node = Node::new(paths, legion_tools::registry());

    let call_agent = |tool: &str, args: Value| {
        let node = node.clone();
        let tool = tool.to_owned();
        async move {
            let mut call = Call::new(Door::Mcp, tool, args);
            call.as_principal = Some("agent".to_owned());
            node.call(call).await
        }
    };

    // Direct access to policy.toml and audit.jsonl is denied by Grant::check_path.
    assert_eq!(
        call_agent("fs.read", json!({"path": "~/.legion/config/policy.toml"}))
            .await
            .unwrap_err()
            .code,
        ErrorCode::Denied
    );
    assert_eq!(
        call_agent("fs.read", json!({"path": "~/.legion/state/audit.jsonl"}))
            .await
            .unwrap_err()
            .code,
        ErrorCode::Denied
    );

    // Walking from `~` skips Legion's config and state directories so policy/audit stay hidden.
    let listed = call_agent("fs.list", json!({"path": "~", "depth": 4}))
        .await
        .unwrap();
    assert!(
        listed["entries"]
            .as_array()
            .unwrap()
            .iter()
            .all(|e| e["name"] != json!("policy.toml") && e["name"] != json!("audit.jsonl")),
        "{listed}"
    );
    let found = call_agent("fs.find", json!({"path": "~", "ignore": false}))
        .await
        .unwrap();
    assert!(
        found["matches"].as_array().unwrap().iter().all(|m| {
            let p = m["path"].as_str().unwrap();
            !p.ends_with("policy.toml") && !p.ends_with("audit.jsonl")
        }),
        "{found}"
    );
    let grepped = call_agent(
        "fs.grep",
        json!({"path": "~", "pattern": "operator", "ignore": false}),
    )
    .await
    .unwrap();
    assert_eq!(grepped["matches"].as_array().unwrap().len(), 0);
}

#[tokio::test]
async fn overwriting_an_existing_directory_requires_effect_destroy() {
    let f = Fixture::new();
    f.write_config(
        "policy.toml",
        r#"
[principals.writer]
role = "writer"

[principals.cleaner]
role = "cleaner"

[roles.writer]
allow = ["effect:read", "effect:write"]

[roles.cleaner]
allow = ["effect:read", "effect:write", "effect:destroy"]
"#,
    );

    // Writer (without `effect:destroy`) may overwrite a single file via `fs.copy` and `fs.move`.
    let f1 = f.work.join("f1.txt");
    let f2 = f.work.join("f2.txt");
    std::fs::write(&f1, "one").unwrap();
    std::fs::write(&f2, "two").unwrap();
    f.call_as(
        "writer",
        "fs.copy",
        json!({"from": f.p(&f1), "to": f.p(&f2), "overwrite": true}),
    )
    .await
    .unwrap();
    f.call_as(
        "writer",
        "fs.move",
        json!({"from": f.p(&f1), "to": f.p(&f2), "overwrite": true}),
    )
    .await
    .unwrap();

    // Writer is denied when overwriting an existing directory in `fs.copy` or `fs.move`.
    let src_dir = f.work.join("src_dir");
    let dst_dir = f.work.join("dst_dir");
    std::fs::create_dir_all(&src_dir).unwrap();
    std::fs::write(src_dir.join("a.txt"), "a").unwrap();
    std::fs::create_dir_all(&dst_dir).unwrap();

    let copy_err = f
        .call_as(
            "writer",
            "fs.copy",
            json!({
                "from": f.p(&src_dir),
                "to": f.p(&dst_dir),
                "recursive": true,
                "overwrite": true,
            }),
        )
        .await
        .unwrap_err();
    assert_eq!(copy_err.code, ErrorCode::Denied);
    assert!(
        copy_err.message.contains("effect:destroy"),
        "{}",
        copy_err.message
    );

    let move_err = f
        .call_as(
            "writer",
            "fs.move",
            json!({
                "from": f.p(&src_dir),
                "to": f.p(&dst_dir),
                "overwrite": true,
            }),
        )
        .await
        .unwrap_err();
    assert_eq!(move_err.code, ErrorCode::Denied);
    assert!(
        move_err.message.contains("effect:destroy"),
        "{}",
        move_err.message
    );

    // Role with `effect:destroy` is allowed to overwrite the existing directory.
    f.call_as(
        "cleaner",
        "fs.copy",
        json!({
            "from": f.p(&src_dir),
            "to": f.p(&dst_dir),
            "recursive": true,
            "overwrite": true,
        }),
    )
    .await
    .unwrap();
    assert_eq!(std::fs::read_to_string(dst_dir.join("a.txt")).unwrap(), "a");

    let empty_dir = f.work.join("empty_dir");
    std::fs::create_dir_all(&empty_dir).unwrap();
    f.call_as(
        "cleaner",
        "fs.move",
        json!({
            "from": f.p(&src_dir),
            "to": f.p(&empty_dir),
            "overwrite": true,
        }),
    )
    .await
    .unwrap();
    assert!(!src_dir.exists());
    assert_eq!(
        std::fs::read_to_string(empty_dir.join("a.txt")).unwrap(),
        "a"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn writing_and_editing_through_a_symlink_updates_target_and_refuses_dangling_links() {
    use std::os::unix::fs::symlink;

    let f = Fixture::new();
    let real = f.work.join("real.txt");
    let link = f.work.join("link.txt");
    std::fs::write(&real, "before\n").unwrap();
    symlink(&real, &link).unwrap();

    // `fs.edit` through the symlink preserves the link and edits `real.txt`.
    f.call(
        "fs.edit",
        json!({"path": f.p(&link), "old": "before", "new": "after"}),
    )
    .await
    .unwrap();
    assert!(
        std::fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert_eq!(std::fs::read_to_string(&real).unwrap(), "after\n");

    // `fs.write` (overwrite) through the symlink preserves the link and replaces `real.txt`.
    f.call(
        "fs.write",
        json!({"path": f.p(&link), "content": "overwritten\n"}),
    )
    .await
    .unwrap();
    assert!(
        std::fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert_eq!(std::fs::read_to_string(&real).unwrap(), "overwritten\n");

    // `fs.write` (mode: "create") on an existing symlink fails with `exists`.
    assert_eq!(
        f.call(
            "fs.write",
            json!({"path": f.p(&link), "content": "new", "mode": "create"}),
        )
        .await
        .unwrap_err()
        .code,
        ErrorCode::Exists
    );

    // Dangling symlink is refused cleanly for both `fs.write` and `fs.edit`, leaving the link
    // untouched and never creating the missing target.
    let missing = f.work.join("nowhere.txt");
    let dangling = f.work.join("dangling.txt");
    symlink(&missing, &dangling).unwrap();
    let write_err = f
        .call(
            "fs.write",
            json!({"path": f.p(&dangling), "content": "nope"}),
        )
        .await
        .unwrap_err();
    assert_eq!(write_err.code, ErrorCode::NotFound);
    assert!(
        write_err.message.contains("dangling"),
        "{}",
        write_err.message
    );
    assert!(!missing.exists());
    assert!(
        std::fs::symlink_metadata(&dangling)
            .unwrap()
            .file_type()
            .is_symlink()
    );

    let edit_err = f
        .call(
            "fs.edit",
            json!({"path": f.p(&dangling), "old": "a", "new": "b"}),
        )
        .await
        .unwrap_err();
    assert_eq!(edit_err.code, ErrorCode::NotFound);
    assert!(
        edit_err.message.contains("dangling"),
        "{}",
        edit_err.message
    );
}

#[tokio::test]
async fn append_preserves_open_file_identity_and_never_loses_concurrent_appends() {
    use std::io::Write as _;

    let f = Fixture::new();
    let log = f.work.join("service.log");
    let mut service_handle = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log)
        .unwrap();
    let before_handle = same_file::Handle::from_path(&log).unwrap();
    writeln!(service_handle, "service-1").unwrap();
    service_handle.flush().unwrap();

    // `fs.write` append must write to the same underlying file rather than renaming a new inode
    // over it, so the service's open handle keeps writing to the same file.
    f.call(
        "fs.write",
        json!({"path": f.p(&log), "content": "legion-append\n", "mode": "append"}),
    )
    .await
    .unwrap();
    writeln!(service_handle, "service-2").unwrap();
    service_handle.flush().unwrap();
    drop(service_handle);

    let after_handle = same_file::Handle::from_path(&log).unwrap();
    assert_eq!(before_handle, after_handle);
    let content = std::fs::read_to_string(&log).unwrap();
    assert_eq!(content, "service-1\nlegion-append\nservice-2\n");

    // Twenty concurrent appends must all land in the file without losing any.
    let shared = f.work.join("concurrent.log");
    let mut tasks = Vec::new();
    for idx in 0..20 {
        let node = f.node.clone();
        let path = f.p(&shared);
        tasks.push(tokio::spawn(async move {
            node.call(Call::new(
                Door::Cli,
                "fs.write",
                json!({
                    "path": path,
                    "content": format!("line-{idx:02}\n"),
                    "mode": "append",
                }),
            ))
            .await
            .unwrap()
        }));
    }
    for task in tasks {
        task.await.unwrap();
    }
    let text = std::fs::read_to_string(&shared).unwrap();
    let mut lines: Vec<&str> = text.lines().collect();
    lines.sort_unstable();
    let expected: Vec<String> = (0..20).map(|idx| format!("line-{idx:02}")).collect();
    assert_eq!(lines, expected);
}

#[tokio::test]
async fn cancelled_recursive_copy_and_remove_wait_for_worker_and_report_honest_counts() {
    let f = Fixture::new();
    let src = f.work.join("many");
    std::fs::create_dir_all(&src).unwrap();
    let blob = vec![b'a'; 16 * 1024];
    for idx in 0..200 {
        std::fs::write(src.join(format!("file_{idx:03}.bin")), &blob).unwrap();
    }

    // Cancel a recursive `fs.copy` once the destination directory has started receiving files.
    let dst = f.work.join("many_copy");
    let call = Call::new(
        Door::Rpc,
        "fs.copy",
        json!({"from": f.p(&src), "to": f.p(&dst), "recursive": true}),
    );
    let cancel = call.cancel.clone();
    let node = f.node.clone();
    let task = tokio::spawn(async move { node.call(call).await });

    // Wait until at least one file has been committed in `dst`, then cancel.
    for _ in 0..1000 {
        if let Ok(entries) = std::fs::read_dir(&dst)
            && entries
                .flatten()
                .any(|e| !e.file_name().to_string_lossy().starts_with('.'))
        {
            cancel.cancel();
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(1)).await;
    }
    cancel.cancel();

    let result = task.await.unwrap();
    if let Err(err) = result {
        assert_eq!(err.code, ErrorCode::Cancelled);
        let details = err.details.expect("cancelled copy must report counts");
        let reported_files = details["files"].as_u64().unwrap();
        let reported_bytes = details["bytes"].as_u64().unwrap();
        let count_now = std::fs::read_dir(&dst)
            .map(|rd| {
                rd.flatten()
                    .filter(|e| !e.file_name().to_string_lossy().starts_with('.'))
                    .count() as u64
            })
            .unwrap_or(0);
        assert_eq!(reported_files, count_now);
        assert_eq!(reported_bytes, count_now * (blob.len() as u64));

        // The worker must have stopped before `Node::call` returned: no further files appear and
        // no sibling temp files remain.
        tokio::time::sleep(std::time::Duration::from_millis(40)).await;
        let all_entries: Vec<String> = std::fs::read_dir(&dst)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(all_entries.len() as u64, count_now);
        assert!(
            all_entries
                .iter()
                .all(|name| !name.contains(".legion-tmp-"))
        );
    }

    // Pre-cancelled recursive `fs.remove` reports zero removed in details when cancelled early, and
    // leaves the tree untouched.
    let keep = f.work.join("keep_tree");
    std::fs::create_dir_all(&keep).unwrap();
    std::fs::write(keep.join("one.txt"), "1234").unwrap();
    let call = Call::new(
        Door::Rpc,
        "fs.remove",
        json!({"path": f.p(&keep), "recursive": true}),
    );
    call.cancel.cancel();
    let err = f.node.call(call).await.unwrap_err();
    assert_eq!(err.code, ErrorCode::Cancelled);
    assert!(keep.join("one.txt").exists());
}

#[tokio::test]
async fn concurrent_create_is_strictly_exclusive_and_expect_hash_and_edit_are_serialized() {
    let f = Fixture::new();

    // Sixteen concurrent `mode: "create"` calls racing on the same path: exactly one wins, the
    // other fifteen fail with `exists`, and the file holds the winner's content.
    let target = f.work.join("exclusive.txt");
    let mut create_tasks = Vec::new();
    for idx in 0..16 {
        let node = f.node.clone();
        let path = f.p(&target);
        create_tasks.push(tokio::spawn(async move {
            let content = format!("writer-{idx:02}");
            let res = node
                .call(Call::new(
                    Door::Cli,
                    "fs.write",
                    json!({"path": path, "content": content, "mode": "create"}),
                ))
                .await;
            (content, res)
        }));
    }
    let mut winners = Vec::new();
    let mut exists_errors = 0;
    for task in create_tasks {
        let (content, res) = task.await.unwrap();
        match res {
            Ok(_) => winners.push(content),
            Err(err) => {
                assert_eq!(err.code, ErrorCode::Exists, "{err}");
                exists_errors += 1;
            }
        }
    }
    assert_eq!(winners.len(), 1);
    assert_eq!(exists_errors, 15);
    assert_eq!(std::fs::read_to_string(&target).unwrap(), winners[0]);

    // Eight concurrent `expect_hash` writes sharing the initial hash: exactly one succeeds and the
    // other seven see `conflict`.
    let raced = f.work.join("raced.txt");
    let init = f
        .call("fs.write", json!({"path": f.p(&raced), "content": "v0"}))
        .await
        .unwrap();
    let initial_hash = init["hash"].as_str().unwrap().to_owned();
    let mut hash_tasks = Vec::new();
    for idx in 0..8 {
        let node = f.node.clone();
        let path = f.p(&raced);
        let expect = initial_hash.clone();
        hash_tasks.push(tokio::spawn(async move {
            node.call(Call::new(
                Door::Cli,
                "fs.write",
                json!({
                    "path": path,
                    "content": format!("v-{idx}"),
                    "expect_hash": expect,
                }),
            ))
            .await
        }));
    }
    let mut ok_count = 0;
    let mut conflict_count = 0;
    for task in hash_tasks {
        match task.await.unwrap() {
            Ok(_) => ok_count += 1,
            Err(err) => {
                assert_eq!(err.code, ErrorCode::Conflict, "{err}");
                conflict_count += 1;
            }
        }
    }
    assert_eq!(ok_count, 1);
    assert_eq!(conflict_count, 7);

    // Six concurrent disjoint `fs.edit` calls on the same file are serialized by the node-wide
    // write lock so none clobbers another's edit.
    let shared = f.work.join("shared.txt");
    std::fs::write(&shared, "a\nb\nc\nd\ne\nf\n").unwrap();
    let mut edit_tasks = Vec::new();
    for (old, new) in [
        ("a", "A"),
        ("b", "B"),
        ("c", "C"),
        ("d", "D"),
        ("e", "E"),
        ("f", "F"),
    ] {
        let node = f.node.clone();
        let path = f.p(&shared);
        edit_tasks.push(tokio::spawn(async move {
            node.call(Call::new(
                Door::Cli,
                "fs.edit",
                json!({"path": path, "old": old, "new": new}),
            ))
            .await
            .unwrap()
        }));
    }
    for task in edit_tasks {
        task.await.unwrap();
    }
    assert_eq!(
        std::fs::read_to_string(&shared).unwrap(),
        "A\nB\nC\nD\nE\nF\n"
    );
}

#[tokio::test]
async fn find_and_grep_expose_cross_filesystems_opt_in() {
    let f = Fixture::new();
    std::fs::write(f.work.join("one.txt"), "hello\n").unwrap();

    let registry = legion_tools::registry();
    for tool in ["fs.find", "fs.grep"] {
        let schema = &registry.get(tool).unwrap().spec().input_schema;
        assert_eq!(
            schema["properties"]["cross_filesystems"]["type"],
            json!("boolean"),
            "{tool} schema missing cross_filesystems"
        );
    }

    let found = f
        .call(
            "fs.find",
            json!({"path": "~/projects", "cross_filesystems": true}),
        )
        .await
        .unwrap();
    assert_eq!(found["matches"].as_array().unwrap().len(), 1);

    let grepped = f
        .call(
            "fs.grep",
            json!({"path": "~/projects", "pattern": "hello", "cross_filesystems": true}),
        )
        .await
        .unwrap();
    assert_eq!(grepped["matches"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn size_caps_and_reply_byte_budgets_protect_write_edit_read_list_find_and_grep() {
    const ONE_MIB: usize = 1024 * 1024;
    let f = Fixture::new();

    // `fs.write` rejects `content` larger than 8 MiB with `invalid_args` and hints at `fs.transfer`.
    let oversized_write = f.work.join("too_big_write.txt");
    let big_content = "a".repeat(8 * 1024 * 1024 + 1);
    let write_err = f
        .call(
            "fs.write",
            json!({"path": f.p(&oversized_write), "content": big_content}),
        )
        .await
        .unwrap_err();
    assert_eq!(write_err.code, ErrorCode::InvalidArgs);
    assert!(
        write_err
            .hint
            .as_deref()
            .is_some_and(|h| h.contains("fs.transfer")),
        "{write_err}"
    );
    assert!(!oversized_write.exists());

    // `fs.edit` rejects files larger than 16 MiB with `invalid_args` and hints at `fs.transfer`.
    let oversized_edit = f.work.join("too_big_edit.txt");
    {
        let file = std::fs::File::create(&oversized_edit).unwrap();
        file.set_len(16 * 1024 * 1024 + 1).unwrap();
    }
    let edit_err = f
        .call(
            "fs.edit",
            json!({"path": f.p(&oversized_edit), "old": "a", "new": "b"}),
        )
        .await
        .unwrap_err();
    assert_eq!(edit_err.code, ErrorCode::InvalidArgs);
    assert!(
        edit_err
            .hint
            .as_deref()
            .is_some_and(|h| h.contains("fs.transfer")),
        "{edit_err}"
    );

    // Base64 `fs.read` (both explicit and auto-detected binary) keeps the serialized JSON reply
    // within 1 MiB, reports the smaller slice via `length`/`eof: false`, and pages cleanly.
    let bin = f.work.join("binary_900k.bin");
    let raw: Vec<u8> = (0..(900 * 1024))
        .map(|i| if i == 0 { 0xFF } else { (i % 251) as u8 })
        .collect();
    std::fs::write(&bin, &raw).unwrap();

    for explicit in [true, false] {
        let mut args = json!({"path": f.p(&bin)});
        if explicit {
            args["encoding"] = json!("base64");
        }
        let page1 = f.call("fs.read", args).await.unwrap();
        let page1_json_len = serde_json::to_vec(&page1).unwrap().len();
        assert!(
            page1_json_len <= ONE_MIB,
            "explicit={explicit} reply was {page1_json_len} bytes"
        );
        assert_eq!(page1["encoding"], json!("base64"));
        assert_eq!(page1["eof"], json!(false));
        assert!(page1.get("hash").is_none());
        let len1 = page1["length"].as_u64().unwrap();
        assert!(len1 > 0 && (len1 as usize) < raw.len());

        let page2 = f
            .call(
                "fs.read",
                json!({"path": f.p(&bin), "offset": len1, "encoding": "base64"}),
            )
            .await
            .unwrap();
        assert_eq!(page2["eof"], json!(true));
        let mut reassembled = BASE64.decode(page1["content"].as_str().unwrap()).unwrap();
        reassembled.extend_from_slice(&BASE64.decode(page2["content"].as_str().unwrap()).unwrap());
        assert_eq!(reassembled, raw);
    }

    // `fs.list` and `fs.find` enforce the 1 MiB reply budget before hitting `limit: 5000`.
    let many = f.work.join("budget");
    std::fs::create_dir_all(&many).unwrap();
    // Names long enough that the listing passes the budget wherever the temporary directory is:
    // the reply's size depends on the length of the path in front of them.
    let pad = "a".repeat(220);
    let total_files = 4500usize;
    for idx in 0..total_files {
        std::fs::File::create(many.join(format!("f_{idx:04}_{pad}.txt"))).unwrap();
    }

    let listed = f
        .call("fs.list", json!({"path": f.p(&many), "limit": 5000}))
        .await
        .unwrap();
    assert_eq!(listed["truncated"], json!(true));
    let listed_count = listed["entries"].as_array().unwrap().len();
    assert!(listed_count > 0 && listed_count < total_files);
    let listed_bytes = serde_json::to_vec(&listed).unwrap().len();
    assert!(listed_bytes <= ONE_MIB, "fs.list reply was {listed_bytes}");

    let found = f
        .call("fs.find", json!({"path": f.p(&many), "limit": 5000}))
        .await
        .unwrap();
    assert_eq!(found["truncated"], json!(true));
    let found_count = found["matches"].as_array().unwrap().len();
    assert!(found_count > 0 && found_count < total_files);
    let found_bytes = serde_json::to_vec(&found).unwrap().len();
    assert!(found_bytes <= ONE_MIB, "fs.find reply was {found_bytes}");

    // `fs.grep` enforces the 1 MiB reply budget (including context lines) before `limit: 1000`.
    let big_log = f.work.join("big_grep.log");
    let mut log_text = String::with_capacity(700 * 500);
    let line_pad = "x".repeat(470);
    for idx in 0..700 {
        log_text.push_str(&format!("MATCH_{idx:04}_{line_pad}\n"));
    }
    std::fs::write(&big_log, &log_text).unwrap();

    let grepped = f
        .call(
            "fs.grep",
            json!({
                "path": f.p(&big_log),
                "pattern": "MATCH_",
                "context": 2,
                "limit": 1000,
            }),
        )
        .await
        .unwrap();
    assert_eq!(grepped["truncated"], json!(true));
    let grep_matches = grepped["matches"].as_array().unwrap();
    assert!(!grep_matches.is_empty() && grep_matches.len() < 700);
    let grep_bytes = serde_json::to_vec(&grepped).unwrap().len();
    assert!(grep_bytes <= ONE_MIB, "fs.grep reply was {grep_bytes}");
    assert_eq!(
        grep_matches
            .last()
            .unwrap()
            .get("after")
            .unwrap()
            .as_array()
            .unwrap()
            .len(),
        2
    );
}

#[cfg(unix)]
#[tokio::test]
async fn non_regular_files_are_refused_before_opening() {
    use std::time::Duration;

    let f = Fixture::new();
    let fifo = f.work.join("blocked.fifo");
    let status = std::process::Command::new("mkfifo")
        .arg(&fifo)
        .status()
        .expect("mkfifo should be available on Unix");
    assert!(status.success(), "mkfifo failed: {status}");

    let regular = f.work.join("regular.txt");
    std::fs::write(&regular, "needle\n").unwrap();

    // Opening a FIFO without a writer/reader would block forever; every file-reading/editing tool
    // must reject it with `invalid_args` immediately via `stat` before calling `open`.
    for (tool, args) in [
        ("fs.read", json!({"path": f.p(&fifo)})),
        ("fs.hash", json!({"path": f.p(&fifo)})),
        (
            "fs.edit",
            json!({"path": f.p(&fifo), "old": "a", "new": "b"}),
        ),
        ("fs.grep", json!({"path": f.p(&fifo), "pattern": "needle"})),
        (
            "fs.write",
            json!({"path": f.p(&fifo), "content": "x", "mode": "append"}),
        ),
        (
            "fs.write",
            json!({"path": f.p(&fifo), "content": "x", "expect_hash": "blake3:0000"}),
        ),
    ] {
        let res = tokio::time::timeout(Duration::from_secs(2), f.call(tool, args.clone()))
            .await
            .unwrap_or_else(|_| panic!("{tool} blocked on FIFO {fifo:?}"));
        let err = res.unwrap_err();
        assert_eq!(err.code, ErrorCode::InvalidArgs, "{tool}: {err}");
    }

    // `fs.thumb` and `fs.tags` refuse a FIFO with per-path `invalid_args` immediately without blocking.
    for tool in ["fs.thumb", "fs.tags"] {
        let fifo_res = tokio::time::timeout(
            Duration::from_secs(2),
            f.call(tool, json!({"paths": [f.p(&fifo)]})),
        )
        .await
        .unwrap_or_else(|_| panic!("{tool} blocked on FIFO"))
        .unwrap();
        assert_eq!(
            fifo_res["results"][0]["error"]["code"],
            json!("invalid_args"),
            "{tool}: {fifo_res}"
        );
    }

    // Grepping a directory containing a FIFO skips it without blocking and still matches regular files.
    let dir_grep = tokio::time::timeout(
        Duration::from_secs(2),
        f.call(
            "fs.grep",
            json!({"path": f.p(&f.work), "pattern": "needle"}),
        ),
    )
    .await
    .expect("directory fs.grep blocked on FIFO")
    .unwrap();
    assert_eq!(dir_grep["matches"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn list_find_and_stat_report_category_and_mime_for_regular_files_only() {
    let f = Fixture::new();
    let subdir = f.work.join("folder.jpg");
    std::fs::create_dir_all(&subdir).unwrap();
    let photo = f.work.join("holiday.JPG");
    let clip = f.work.join("clip.mp4");
    let unknown = f.work.join("mystery.unknownext");
    std::fs::write(&photo, "not opened").unwrap();
    std::fs::write(&clip, "not opened").unwrap();
    std::fs::write(&unknown, "not opened").unwrap();

    // `fs.stat` classifies regular files by extension and omits category/mime for directories.
    let st_photo = f
        .call("fs.stat", json!({"path": f.p(&photo)}))
        .await
        .unwrap();
    assert_eq!(st_photo["kind"], json!("file"));
    assert_eq!(st_photo["category"], json!("image"));
    assert_eq!(st_photo["mime"], json!("image/jpeg"));

    let st_unknown = f
        .call("fs.stat", json!({"path": f.p(&unknown)}))
        .await
        .unwrap();
    assert_eq!(st_unknown["kind"], json!("file"));
    assert_eq!(st_unknown["category"], json!("other"));
    assert_eq!(st_unknown["mime"], json!("application/octet-stream"));

    let st_dir = f
        .call("fs.stat", json!({"path": f.p(&subdir)}))
        .await
        .unwrap();
    assert_eq!(st_dir["kind"], json!("dir"));
    assert!(st_dir.get("category").is_none());
    assert!(st_dir.get("mime").is_none());

    // `fs.list` reports category/mime on files and omits them on directories.
    let listed = f
        .call("fs.list", json!({"path": f.p(&f.work)}))
        .await
        .unwrap();
    let entries = listed["entries"].as_array().unwrap();
    let dir_entry = entries
        .iter()
        .find(|e| e["name"] == json!("folder.jpg"))
        .unwrap();
    assert_eq!(dir_entry["kind"], json!("dir"));
    assert!(dir_entry.get("category").is_none());
    assert!(dir_entry.get("mime").is_none());
    let clip_entry = entries
        .iter()
        .find(|e| e["name"] == json!("clip.mp4"))
        .unwrap();
    assert_eq!(clip_entry["kind"], json!("file"));
    assert_eq!(clip_entry["category"], json!("video"));
    assert_eq!(clip_entry["mime"], json!("video/mp4"));

    // `fs.find` reports category/mime on files and omits them on directories.
    let found = f
        .call("fs.find", json!({"path": f.p(&f.work)}))
        .await
        .unwrap();
    let matches = found["matches"].as_array().unwrap();
    let found_dir = matches.iter().find(|m| m["kind"] == json!("dir")).unwrap();
    assert!(found_dir.get("category").is_none());
    assert!(found_dir.get("mime").is_none());
    let found_photo = matches
        .iter()
        .find(|m| m["path"].as_str().unwrap().ends_with("holiday.JPG"))
        .unwrap();
    assert_eq!(found_photo["category"], json!("image"));
    assert_eq!(found_photo["mime"], json!("image/jpeg"));

    // On Unix, symlinks keep `kind: "symlink"` and omit category/mime.
    #[cfg(unix)]
    {
        let link = f.work.join("link.png");
        std::os::unix::fs::symlink(&photo, &link).unwrap();
        let st_link = f
            .call("fs.stat", json!({"path": f.p(&link)}))
            .await
            .unwrap();
        assert_eq!(st_link["kind"], json!("symlink"));
        assert!(st_link.get("category").is_none());
        assert!(st_link.get("mime").is_none());
    }
}

#[tokio::test]
async fn list_supports_natural_sort_modified_size_categories_and_paging() {
    use std::time::{Duration, UNIX_EPOCH};

    let f = Fixture::new();
    std::fs::create_dir_all(f.work.join("dir10")).unwrap();
    std::fs::create_dir_all(f.work.join("dir2")).unwrap();
    std::fs::create_dir_all(f.work.join("AlphaDir")).unwrap();

    let write_with_mtime = |name: &str, bytes: &[u8], secs: u64| {
        let p = f.work.join(name);
        let file = std::fs::File::create(&p).unwrap();
        std::io::Write::write_all(&mut &file, bytes).unwrap();
        file.set_modified(UNIX_EPOCH + Duration::from_secs(secs))
            .unwrap();
    };
    write_with_mtime("file10.jpg", &[0u8; 50], 1_700_000_100);
    write_with_mtime("file2.png", &[0u8; 200], 1_700_000_300);
    write_with_mtime("Beta.txt", &[0u8; 10], 1_700_000_200);
    write_with_mtime("alpha.mp4", &[0u8; 500], 1_700_000_050);

    // Default natural case-insensitive name order, directories first.
    let natural = f
        .call("fs.list", json!({"path": "~/projects"}))
        .await
        .unwrap();
    let names = |v: &Value| -> Vec<String> {
        v["entries"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["name"].as_str().unwrap().to_owned())
            .collect()
    };
    assert_eq!(
        names(&natural),
        vec![
            "AlphaDir",
            "dir2",
            "dir10",
            "alpha.mp4",
            "Beta.txt",
            "file2.png",
            "file10.jpg"
        ]
    );
    assert_eq!(natural["total"], json!(7));
    assert_eq!(natural["truncated"], json!(false));

    // Descending name order still keeps directories first.
    let desc_name = f
        .call("fs.list", json!({"path": "~/projects", "order": "desc"}))
        .await
        .unwrap();
    assert_eq!(
        names(&desc_name),
        vec![
            "dir10",
            "dir2",
            "AlphaDir",
            "file10.jpg",
            "file2.png",
            "Beta.txt",
            "alpha.mp4"
        ]
    );

    // Sort by size descending: directories still first, files largest to smallest.
    let by_size = f
        .call(
            "fs.list",
            json!({"path": "~/projects", "sort": "size", "order": "desc"}),
        )
        .await
        .unwrap();
    let file_names_size: Vec<&str> = by_size["entries"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["kind"] == json!("file"))
        .map(|e| e["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        file_names_size,
        vec!["alpha.mp4", "file2.png", "file10.jpg", "Beta.txt"]
    );
    // First 3 entries are still directories.
    assert!(
        by_size["entries"].as_array().unwrap()[..3]
            .iter()
            .all(|e| e["kind"] == json!("dir"))
    );

    // Sort by modified descending: newest files first.
    let by_mtime = f
        .call(
            "fs.list",
            json!({"path": "~/projects", "sort": "modified", "order": "desc"}),
        )
        .await
        .unwrap();
    let file_names_mtime: Vec<&str> = by_mtime["entries"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["kind"] == json!("file"))
        .map(|e| e["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        file_names_mtime,
        vec!["file2.png", "Beta.txt", "file10.jpg", "alpha.mp4"]
    );

    // Category filter keeps directories and only matching files.
    let images = f
        .call(
            "fs.list",
            json!({"path": "~/projects", "categories": ["image"]}),
        )
        .await
        .unwrap();
    assert_eq!(
        names(&images),
        vec!["AlphaDir", "dir2", "dir10", "file2.png", "file10.jpg"]
    );
    assert_eq!(images["total"], json!(5));
    assert_eq!(images["truncated"], json!(false));

    // Paging with `offset` and `limit` reports accurate `total` and `truncated`.
    let page1 = f
        .call(
            "fs.list",
            json!({"path": "~/projects", "offset": 0, "limit": 3}),
        )
        .await
        .unwrap();
    assert_eq!(names(&page1), vec!["AlphaDir", "dir2", "dir10"]);
    assert_eq!(page1["total"], json!(7));
    assert_eq!(page1["truncated"], json!(true));

    let page2 = f
        .call(
            "fs.list",
            json!({"path": "~/projects", "offset": 3, "limit": 3}),
        )
        .await
        .unwrap();
    assert_eq!(names(&page2), vec!["alpha.mp4", "Beta.txt", "file2.png"]);
    assert_eq!(page2["total"], json!(7));
    assert_eq!(page2["truncated"], json!(true));

    let page3 = f
        .call(
            "fs.list",
            json!({"path": "~/projects", "offset": 6, "limit": 3}),
        )
        .await
        .unwrap();
    assert_eq!(names(&page3), vec!["file10.jpg"]);
    assert_eq!(page3["total"], json!(7));
    assert_eq!(page3["truncated"], json!(false));

    let past_end = f
        .call(
            "fs.list",
            json!({"path": "~/projects", "offset": 20, "limit": 3}),
        )
        .await
        .unwrap();
    assert!(past_end["entries"].as_array().unwrap().is_empty());
    assert_eq!(past_end["total"], json!(7));
    assert_eq!(past_end["truncated"], json!(false));
}

#[tokio::test]
async fn find_supports_words_categories_size_time_hidden_sort_and_unicode() {
    use std::time::{Duration, UNIX_EPOCH};

    let f = Fixture::new();
    std::fs::write(f.work.join(".gitignore"), "ignored_pic.jpg\n").unwrap();
    std::fs::create_dir_all(f.work.join("sub")).unwrap();

    let write_file = |rel: &str, size: usize, secs: u64| {
        let p = f.work.join(rel);
        let file = std::fs::File::create(&p).unwrap();
        std::io::Write::write_all(&mut &file, &vec![b'x'; size]).unwrap();
        file.set_modified(UNIX_EPOCH + Duration::from_secs(secs))
            .unwrap();
    };

    write_file("Holiday_Beach_2.jpg", 100, 1_700_000_100);
    write_file("holiday_beach_10.PNG", 500, 1_700_000_300);
    write_file("sub/Übung_Sommer_2026.pdf", 250, 1_700_000_200);
    write_file("ignored_pic.jpg", 400, 1_700_000_400);
    write_file(".hidden_pic.jpg", 300, 1_700_000_500);
    write_file("notes.txt", 50, 1_700_000_150);

    // Multi-word name filter (case-insensitive, any word order) + machine name in result.
    let by_words = f
        .call(
            "fs.find",
            json!({"path": "~/projects", "name": "BEACH holiday", "sort": "name"}),
        )
        .await
        .unwrap();
    assert_eq!(by_words["machine"], json!(f.node.name()));
    assert_eq!(by_words["truncated"], json!(false));
    assert!(by_words.get("reason").is_none());
    let word_names: Vec<&str> = by_words["matches"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| {
            Path::new(m["path"].as_str().unwrap())
                .file_name()
                .unwrap()
                .to_str()
                .unwrap()
        })
        .collect();
    // Natural ordering puts 2 before 10.
    assert_eq!(
        word_names,
        vec!["Holiday_Beach_2.jpg", "holiday_beach_10.PNG"]
    );

    // Unicode case-insensitive word matching.
    let unicode = f
        .call(
            "fs.find",
            json!({"path": "~/projects", "name": "übung 2026", "categories": ["document"]}),
        )
        .await
        .unwrap();
    assert_eq!(unicode["matches"].as_array().unwrap().len(), 1);
    assert!(
        unicode["matches"][0]["path"]
            .as_str()
            .unwrap()
            .ends_with("Übung_Sommer_2026.pdf")
    );
    assert_eq!(unicode["matches"][0]["category"], json!("document"));

    // Size bounds filter regular files inclusively.
    let by_size = f
        .call(
            "fs.find",
            json!({
                "path": "~/projects",
                "min_size": 100,
                "max_size": 300,
                "sort": "size",
                "order": "desc"
            }),
        )
        .await
        .unwrap();
    let sized: Vec<&str> = by_size["matches"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| {
            Path::new(m["path"].as_str().unwrap())
                .file_name()
                .unwrap()
                .to_str()
                .unwrap()
        })
        .collect();
    assert_eq!(sized, vec!["Übung_Sommer_2026.pdf", "Holiday_Beach_2.jpg"]);

    // Modification time bounds using the same RFC 3339 format returned by fs.stat.
    let t_after = jiff::Timestamp::from_second(1_700_000_150)
        .unwrap()
        .strftime("%Y-%m-%dT%H:%M:%S%.3fZ")
        .to_string();
    let t_before = jiff::Timestamp::from_second(1_700_000_250)
        .unwrap()
        .strftime("%Y-%m-%dT%H:%M:%S%.3fZ")
        .to_string();
    let by_time = f
        .call(
            "fs.find",
            json!({
                "path": "~/projects",
                "kind": "file",
                "modified_after": t_after,
                "modified_before": t_before,
                "sort": "modified",
                "order": "asc"
            }),
        )
        .await
        .unwrap();
    let timed: Vec<&str> = by_time["matches"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| {
            Path::new(m["path"].as_str().unwrap())
                .file_name()
                .unwrap()
                .to_str()
                .unwrap()
        })
        .collect();
    assert_eq!(timed, vec!["notes.txt", "Übung_Sommer_2026.pdf"]);

    // `hidden` is separate from `ignore`:
    // `ignore: false, hidden: false` includes gitignored files while skipping dotfiles.
    let no_ignore_no_hidden = f
        .call(
            "fs.find",
            json!({
                "path": "~/projects",
                "categories": ["image"],
                "ignore": false,
                "hidden": false,
                "sort": "name"
            }),
        )
        .await
        .unwrap();
    let imgs_no_hidden: Vec<&str> = no_ignore_no_hidden["matches"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| {
            Path::new(m["path"].as_str().unwrap())
                .file_name()
                .unwrap()
                .to_str()
                .unwrap()
        })
        .collect();
    assert_eq!(
        imgs_no_hidden,
        vec![
            "Holiday_Beach_2.jpg",
            "holiday_beach_10.PNG",
            "ignored_pic.jpg"
        ]
    );

    // `ignore: true, hidden: true` respects `.gitignore` while still finding dotfiles.
    let ignore_with_hidden = f
        .call(
            "fs.find",
            json!({
                "path": "~/projects",
                "categories": ["image"],
                "ignore": true,
                "hidden": true,
                "sort": "name"
            }),
        )
        .await
        .unwrap();
    let imgs_with_hidden: Vec<&str> = ignore_with_hidden["matches"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| {
            Path::new(m["path"].as_str().unwrap())
                .file_name()
                .unwrap()
                .to_str()
                .unwrap()
        })
        .collect();
    assert_eq!(
        imgs_with_hidden,
        vec![
            ".hidden_pic.jpg",
            "Holiday_Beach_2.jpg",
            "holiday_beach_10.PNG"
        ]
    );

    // Sorted top-K with `limit` returns the true top items and reports `reason: "limit"`.
    let top_two = f
        .call(
            "fs.find",
            json!({
                "path": "~/projects",
                "categories": ["image"],
                "ignore": false,
                "hidden": true,
                "sort": "size",
                "order": "desc",
                "limit": 2
            }),
        )
        .await
        .unwrap();
    assert_eq!(top_two["truncated"], json!(true));
    assert_eq!(top_two["reason"], json!("limit"));
    let top_names: Vec<&str> = top_two["matches"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| {
            Path::new(m["path"].as_str().unwrap())
                .file_name()
                .unwrap()
                .to_str()
                .unwrap()
        })
        .collect();
    assert_eq!(top_names, vec!["holiday_beach_10.PNG", "ignored_pic.jpg"]);

    // Invalid bounds and timestamps are rejected cleanly.
    for bad in [
        json!({"path": "~/projects", "budget_ms": 0}),
        json!({"path": "~/projects", "budget_ms": 60001}),
        json!({"path": "~/projects", "min_size": 500, "max_size": 100}),
        json!({"path": "~/projects", "modified_after": "not-a-timestamp"}),
        json!({
            "path": "~/projects",
            "modified_after": "2026-11-25T00:00:00Z",
            "modified_before": "2026-11-24T00:00:00Z"
        }),
    ] {
        let err = f.call("fs.find", bad.clone()).await.unwrap_err();
        assert_eq!(err.code, ErrorCode::InvalidArgs, "{bad}");
    }
}

#[cfg(unix)]
#[tokio::test]
async fn find_never_follows_symlinks_out_of_allowed_roots() {
    use std::os::unix::fs::symlink;

    let f = Fixture::new();
    f.write_config(
        "policy.toml",
        r#"
[principals.jailed]
role = "jailed"

[roles.jailed]
allow = ["effect:read"]
paths = ["~/projects/**"]
"#,
    );
    let outside = f.home.join("outside_secret_dir");
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::write(outside.join("secret_needle.txt"), "secret").unwrap();
    std::fs::write(f.work.join("inside_needle.txt"), "ok").unwrap();
    symlink(&outside, f.work.join("escape_dir")).unwrap();

    let res = f
        .call_as(
            "jailed",
            "fs.find",
            json!({"path": "~/projects", "name": "needle"}),
        )
        .await
        .unwrap();
    let paths: Vec<&str> = res["matches"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["path"].as_str().unwrap())
        .collect();
    assert_eq!(paths.len(), 1, "{paths:?}");
    assert!(paths[0].ends_with("inside_needle.txt"));
}

#[cfg(windows)]
#[tokio::test]
async fn list_and_find_respect_windows_hidden_attribute() {
    let f = Fixture::new();
    let visible = f.work.join("visible.txt");
    let hidden = f.work.join("win_hidden.txt");
    std::fs::write(&visible, "ok").unwrap();
    std::fs::write(&hidden, "secret").unwrap();
    let status = std::process::Command::new("attrib")
        .args(["+H", hidden.to_str().unwrap()])
        .status()
        .expect("attrib should run on Windows");
    assert!(status.success(), "attrib +H failed: {status}");

    let listed = f
        .call("fs.list", json!({"path": "~/projects", "hidden": false}))
        .await
        .unwrap();
    let list_names: Vec<&str> = listed["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["name"].as_str().unwrap())
        .collect();
    assert_eq!(list_names, vec!["visible.txt"]);

    let found = f
        .call(
            "fs.find",
            json!({"path": "~/projects", "ignore": false, "hidden": false}),
        )
        .await
        .unwrap();
    let found_names: Vec<&str> = found["matches"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| {
            Path::new(m["path"].as_str().unwrap())
                .file_name()
                .unwrap()
                .to_str()
                .unwrap()
        })
        .collect();
    assert_eq!(found_names, vec!["visible.txt"]);
}

#[tokio::test]
async fn find_large_directory_cuts_off_within_budget() {
    use std::time::{Duration, Instant};

    let f = Fixture::new();
    let big = f.work.join("huge");
    std::fs::create_dir_all(&big).unwrap();

    // 200,000 files on Unix; use a smaller count on Windows CI where Defender scans each newly
    // created file on NTFS so the fixture setup itself stays fast.
    #[cfg(not(windows))]
    const TOTAL_FILES: usize = 200_000;
    #[cfg(windows)]
    const TOTAL_FILES: usize = 10_000;

    let workers = std::thread::available_parallelism()
        .map(|n| n.get().clamp(2, 8))
        .unwrap_or(4);
    let per_worker = TOTAL_FILES.div_ceil(workers);
    std::thread::scope(|scope| {
        for w in 0..workers {
            let dir = big.clone();
            scope.spawn(move || {
                let start = w * per_worker;
                let end = (start + per_worker).min(TOTAL_FILES);
                for idx in start..end {
                    let _ = std::fs::File::create(dir.join(format!("f_{idx:06}.txt")));
                }
            });
        }
    });

    let started = Instant::now();
    let res = f
        .call(
            "fs.find",
            json!({
                "path": f.p(&big),
                "name": "does_not_match_any_file",
                "ignore": false,
                // The smallest budget there is: no machine walks this directory in a millisecond,
                // so the cut-off does not depend on how fast the runner happens to be.
                "budget_ms": 1,
            }),
        )
        .await
        .unwrap();
    let elapsed = started.elapsed();
    assert_eq!(
        res["truncated"],
        json!(true),
        "expected time truncation: {res}"
    );
    assert_eq!(res["reason"], json!("time"));
    assert!(
        elapsed < Duration::from_secs(2),
        "fs.find took {elapsed:?} for a 1ms budget"
    );
}

fn make_rgb_image(width: u32, height: u32, color: [u8; 3]) -> image::DynamicImage {
    image::DynamicImage::ImageRgb8(image::ImageBuffer::from_pixel(
        width,
        height,
        image::Rgb(color),
    ))
}

#[tokio::test]
async fn thumb_decodes_supported_formats_fit_modes_and_validates_args() {
    use image::codecs::gif::GifEncoder;
    use image::{Frame, ImageBuffer, ImageFormat, Rgba};

    let f = Fixture::new();

    // Create valid JPEG, opaque PNG, transparent PNG, animated GIF (2 frames), WebP, BMP.
    let jpg_path = f.work.join("wide.jpg");
    make_rgb_image(400, 200, [200, 40, 40])
        .save_with_format(&jpg_path, ImageFormat::Jpeg)
        .unwrap();

    let png_opaque_path = f.work.join("opaque.png");
    make_rgb_image(300, 150, [40, 200, 40])
        .save_with_format(&png_opaque_path, ImageFormat::Png)
        .unwrap();

    let png_alpha_path = f.work.join("alpha.png");
    let mut alpha_img =
        ImageBuffer::<Rgba<u8>, Vec<u8>>::from_pixel(160, 160, Rgba([40, 40, 200, 255]));
    alpha_img.put_pixel(10, 10, Rgba([40, 40, 200, 0]));
    image::DynamicImage::ImageRgba8(alpha_img)
        .save_with_format(&png_alpha_path, ImageFormat::Png)
        .unwrap();

    let gif_path = f.work.join("anim.gif");
    {
        let file = std::fs::File::create(&gif_path).unwrap();
        let mut enc = GifEncoder::new(file);
        let frame1 =
            ImageBuffer::<Rgba<u8>, Vec<u8>>::from_pixel(120, 80, Rgba([220, 20, 20, 255]));
        let frame2 =
            ImageBuffer::<Rgba<u8>, Vec<u8>>::from_pixel(120, 80, Rgba([20, 220, 20, 255]));
        enc.encode_frame(Frame::new(frame1)).unwrap();
        enc.encode_frame(Frame::new(frame2)).unwrap();
    }

    let webp_path = f.work.join("pic.webp");
    make_rgb_image(320, 240, [200, 200, 40])
        .save_with_format(&webp_path, ImageFormat::WebP)
        .unwrap();

    let bmp_path = f.work.join("scan.bmp");
    make_rgb_image(300, 300, [40, 200, 200])
        .save_with_format(&bmp_path, ImageFormat::Bmp)
        .unwrap();

    let res = f
        .call(
            "fs.thumb",
            json!({
                "paths": [
                    f.p(&jpg_path),
                    f.p(&png_opaque_path),
                    f.p(&png_alpha_path),
                    f.p(&gif_path),
                    f.p(&webp_path),
                    f.p(&bmp_path),
                ],
                "size": 128,
                "fit": "contain"
            }),
        )
        .await
        .unwrap();

    let items = res["results"].as_array().unwrap();
    assert_eq!(items.len(), 6);

    // JPEG 400x200 -> contain 128 -> 128x64 JPEG.
    assert_eq!(items[0]["ok"]["mime"], json!("image/jpeg"));
    assert_eq!(items[0]["ok"]["width"], json!(128));
    assert_eq!(items[0]["ok"]["height"], json!(64));
    let decoded0 = BASE64
        .decode(items[0]["ok"]["content"].as_str().unwrap())
        .unwrap();
    assert_eq!(&decoded0[..2], &[0xFF, 0xD8]);

    // Opaque PNG -> JPEG; transparent PNG -> PNG.
    assert_eq!(items[1]["ok"]["mime"], json!("image/jpeg"));
    assert_eq!(items[2]["ok"]["mime"], json!("image/png"));
    let decoded2 = BASE64
        .decode(items[2]["ok"]["content"].as_str().unwrap())
        .unwrap();
    assert_eq!(&decoded2[..4], &[137, 80, 78, 71]);

    // Animated GIF decodes first frame (red, 120x80 not upscaled).
    assert_eq!(items[3]["ok"]["width"], json!(120));
    assert_eq!(items[3]["ok"]["height"], json!(80));
    let gif_bytes = BASE64
        .decode(items[3]["ok"]["content"].as_str().unwrap())
        .unwrap();
    let gif_img = image::load_from_memory(&gif_bytes).unwrap().to_rgb8();
    let px = gif_img.get_pixel(60, 40);
    assert!(
        px.0[0] > 180 && px.0[1] < 80,
        "expected first (red) GIF frame, got {px:?}"
    );

    // WebP and BMP both produce valid JPEG thumbnails.
    assert_eq!(items[4]["ok"]["mime"], json!("image/jpeg"));
    assert_eq!(items[4]["ok"]["width"], json!(128));
    assert_eq!(items[4]["ok"]["height"], json!(96));
    assert_eq!(items[5]["ok"]["mime"], json!("image/jpeg"));
    assert_eq!(items[5]["ok"]["width"], json!(128));
    assert_eq!(items[5]["ok"]["height"], json!(128));

    // `fit: "cover"` crops to exact square dimensions.
    let cover = f
        .call(
            "fs.thumb",
            json!({"paths": [f.p(&jpg_path)], "size": 96, "fit": "cover"}),
        )
        .await
        .unwrap();
    assert_eq!(cover["results"][0]["ok"]["width"], json!(96));
    assert_eq!(cover["results"][0]["ok"]["height"], json!(96));

    // `size: 1024` is admitted (max thumbnail size raised for the player).
    let large = f
        .call(
            "fs.thumb",
            json!({"paths": [f.p(&jpg_path)], "size": 1024, "fit": "cover"}),
        )
        .await
        .unwrap();
    assert_eq!(large["results"][0]["ok"]["width"], json!(1024));
    assert_eq!(large["results"][0]["ok"]["height"], json!(1024));

    // Argument validation rejects out-of-range counts and sizes.
    let too_many: Vec<String> = (0..33).map(|i| format!("~/projects/p{i}.jpg")).collect();
    for bad in [
        json!({"paths": []}),
        json!({"paths": too_many}),
        json!({"paths": [f.p(&jpg_path)], "size": 95}),
        json!({"paths": [f.p(&jpg_path)], "size": 1025}),
    ] {
        let err = f.call("fs.thumb", bad.clone()).await.unwrap_err();
        assert_eq!(err.code, ErrorCode::InvalidArgs, "{bad}");
    }
}

#[tokio::test]
async fn thumb_handles_hostile_images_without_panicking_or_exhausting_memory() {
    let f = Fixture::new();

    // 1. Valid image in the same batch must still succeed alongside hostile inputs.
    let good = f.work.join("good.jpg");
    make_rgb_image(200, 150, [10, 120, 220])
        .save_with_format(&good, image::ImageFormat::Jpeg)
        .unwrap();

    // 2. Truncated JPEG.
    let mut full_jpg = std::fs::read(&good).unwrap();
    full_jpg.truncate(full_jpg.len() / 3);
    let truncated = f.work.join("truncated.jpg");
    std::fs::write(&truncated, &full_jpg).unwrap();

    // 3. Zero-byte PNG.
    let empty = f.work.join("empty.png");
    std::fs::write(&empty, b"").unwrap();

    // 4. Wrong extension: text content named .png, and valid JPEG bytes named .txt.
    let fake_png = f.work.join("not_an_image.png");
    std::fs::write(&fake_png, b"just plain text, definitely not a PNG").unwrap();
    let jpeg_as_txt = f.work.join("actually_jpeg.txt");
    std::fs::copy(&good, &jpeg_as_txt).unwrap();

    // 5. Decompression bomb: tiny PNG declaring 25000 x 25000 pixels (625 MP > 100 MP).
    let bomb = f.work.join("bomb.png");
    {
        let crc32 = |bytes: &[u8]| -> u32 {
            let mut crc = 0xFFFF_FFFFu32;
            for &b in bytes {
                crc ^= u32::from(b);
                for _ in 0..8 {
                    let mask = (crc & 1).wrapping_neg();
                    crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
                }
            }
            !crc
        };
        let mut data = vec![137, 80, 78, 71, 13, 10, 26, 10];
        let mut ihdr = b"IHDR".to_vec();
        ihdr.extend_from_slice(&25_000u32.to_be_bytes());
        ihdr.extend_from_slice(&25_000u32.to_be_bytes());
        ihdr.extend_from_slice(&[8, 6, 0, 0, 0]);
        data.extend_from_slice(&13u32.to_be_bytes());
        data.extend_from_slice(&ihdr);
        data.extend_from_slice(&crc32(&ihdr).to_be_bytes());
        let idat_payload = [
            0x78u8, 0x01, 0x01, 0x00, 0x00, 0xFF, 0xFF, 0x00, 0x00, 0x00, 0x01,
        ];
        let mut idat = b"IDAT".to_vec();
        idat.extend_from_slice(&idat_payload);
        data.extend_from_slice(&(idat_payload.len() as u32).to_be_bytes());
        data.extend_from_slice(&idat);
        data.extend_from_slice(&crc32(&idat).to_be_bytes());
        data.extend_from_slice(&0u32.to_be_bytes());
        data.extend_from_slice(b"IEND");
        data.extend_from_slice(&crc32(b"IEND").to_be_bytes());
        std::fs::write(&bomb, data).unwrap();
    }

    // 6. Oversized source (> 64 MiB sparse file).
    let oversized = f.work.join("huge.png");
    {
        let file = std::fs::File::create(&oversized).unwrap();
        file.set_len(65 * 1024 * 1024).unwrap();
    }

    // 7. Unsupported image format (.svg) and directory.
    let svg = f.work.join("icon.svg");
    std::fs::write(&svg, b"<svg xmlns='http://www.w3.org/2000/svg'/>").unwrap();
    let dir = f.work.join("subdir");
    std::fs::create_dir_all(&dir).unwrap();

    let res = f
        .call(
            "fs.thumb",
            json!({
                "paths": [
                    f.p(&good),
                    f.p(&truncated),
                    f.p(&empty),
                    f.p(&fake_png),
                    f.p(&jpeg_as_txt),
                    f.p(&bomb),
                    f.p(&oversized),
                    f.p(&svg),
                    f.p(&dir),
                ]
            }),
        )
        .await
        .unwrap();

    let items = res["results"].as_array().unwrap();
    assert_eq!(items.len(), 9);
    assert!(
        items[0].get("ok").is_some(),
        "good image failed: {}",
        items[0]
    );
    assert_eq!(
        items[1]["error"]["code"],
        json!("failed"),
        "truncated: {}",
        items[1]
    );
    assert_eq!(
        items[2]["error"]["code"],
        json!("failed"),
        "empty: {}",
        items[2]
    );
    assert_eq!(
        items[3]["error"]["code"],
        json!("failed"),
        "fake_png: {}",
        items[3]
    );
    assert_eq!(
        items[4]["error"]["code"],
        json!("unsupported"),
        "jpeg_as_txt: {}",
        items[4]
    );
    assert_eq!(
        items[5]["error"]["code"],
        json!("failed"),
        "bomb: {}",
        items[5]
    );
    assert_eq!(
        items[6]["error"]["code"],
        json!("failed"),
        "oversized: {}",
        items[6]
    );
    assert_eq!(
        items[7]["error"]["code"],
        json!("unsupported"),
        "svg: {}",
        items[7]
    );
    assert_eq!(
        items[8]["error"]["code"],
        json!("invalid_args"),
        "dir: {}",
        items[8]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn thumb_cache_hits_invalidates_is_protected_from_non_admin_and_safe_under_concurrency() {
    let f = Fixture::new();
    f.write_config(
        "policy.toml",
        r#"
[principals.viewer]
role = "viewer"

[principals.no_read]
role = "no_read"

[principals.jailed]
role = "jailed"

[roles.viewer]
allow = ["effect:read", "effect:write", "effect:destroy"]

[roles.no_read]
allow = ["fs.thumb", "fs.list"]
deny = ["fs.read"]

[roles.jailed]
allow = ["effect:read"]
paths = ["~/projects/**"]
"#,
    );

    let pic = f.work.join("cached.png");
    make_rgb_image(200, 200, [240, 20, 20])
        .save_with_format(&pic, image::ImageFormat::Png)
        .unwrap();
    let outside = f.home.join("private.png");
    make_rgb_image(150, 150, [20, 240, 20])
        .save_with_format(&outside, image::ImageFormat::Png)
        .unwrap();

    // First call populates the cache under state/thumb-cache.
    let first = f
        .call("fs.thumb", json!({"paths": [f.p(&pic)], "size": 128}))
        .await
        .unwrap();
    let first_content = first["results"][0]["ok"]["content"]
        .as_str()
        .unwrap()
        .to_owned();
    let cache_dir = f.dir.path().join("state").join("thumb-cache");
    let count_thumbs = || {
        std::fs::read_dir(&cache_dir)
            .unwrap()
            .filter_map(Result::ok)
            .filter(|e| e.file_name().to_string_lossy().ends_with(".thumb"))
            .count()
    };
    assert_eq!(count_thumbs(), 1);

    // Second call hits the cache and returns identical content without creating another entry.
    let second = f
        .call("fs.thumb", json!({"paths": [f.p(&pic)], "size": 128}))
        .await
        .unwrap();
    assert_eq!(
        second["results"][0]["ok"]["content"].as_str().unwrap(),
        first_content
    );
    assert_eq!(count_thumbs(), 1);

    // Modifying the file invalidates the cache and returns a fresh thumbnail with different bytes.
    make_rgb_image(300, 150, [20, 20, 240])
        .save_with_format(&pic, image::ImageFormat::Png)
        .unwrap();
    let updated = f
        .call("fs.thumb", json!({"paths": [f.p(&pic)], "size": 128}))
        .await
        .unwrap();
    assert_eq!(updated["results"][0]["ok"]["width"], json!(128));
    assert_eq!(updated["results"][0]["ok"]["height"], json!(64));
    assert_ne!(
        updated["results"][0]["ok"]["content"].as_str().unwrap(),
        first_content
    );
    assert_eq!(count_thumbs(), 2);

    // Non-admin role cannot read, list, write, remove or thumbnail anything in the cache directory.
    let thumb_file = std::fs::read_dir(&cache_dir)
        .unwrap()
        .filter_map(Result::ok)
        .find(|e| e.file_name().to_string_lossy().ends_with(".thumb"))
        .unwrap()
        .path();
    for (tool, args) in [
        ("fs.list", json!({"path": f.p(&cache_dir)})),
        ("fs.read", json!({"path": f.p(&thumb_file)})),
        (
            "fs.write",
            json!({"path": f.p(&cache_dir.join("injected.thumb")), "content": "x"}),
        ),
        ("fs.remove", json!({"path": f.p(&thumb_file)})),
    ] {
        let err = f.call_as("viewer", tool, args).await.unwrap_err();
        assert_eq!(err.code, ErrorCode::Denied, "viewer {tool}: {err}");
    }
    let thumb_cache_attempt = f
        .call_as("viewer", "fs.thumb", json!({"paths": [f.p(&thumb_file)]}))
        .await
        .unwrap();
    assert_eq!(
        thumb_cache_attempt["results"][0]["error"]["code"],
        json!("denied")
    );

    // Role denied `fs.read` is denied `fs.thumb` even when `fs.thumb` itself is allowed.
    let no_read_err = f
        .call_as("no_read", "fs.thumb", json!({"paths": [f.p(&pic)]}))
        .await
        .unwrap_err();
    assert_eq!(no_read_err.code, ErrorCode::Denied);

    // Owner caches a thumbnail for a file outside `jailed`'s `paths`; `jailed` must still get
    // `denied` (never a cache hit!), and symlink escapes are refused too.
    let owner_outside = f
        .call("fs.thumb", json!({"paths": [f.p(&outside)], "size": 128}))
        .await
        .unwrap();
    assert!(owner_outside["results"][0].get("ok").is_some());

    let jailed_outside = f
        .call_as(
            "jailed",
            "fs.thumb",
            json!({"paths": [f.p(&outside)], "size": 128}),
        )
        .await
        .unwrap();
    assert_eq!(
        jailed_outside["results"][0]["error"]["code"],
        json!("denied")
    );

    #[cfg(unix)]
    {
        let escape_link = f.work.join("escape_pic.png");
        std::os::unix::fs::symlink(&outside, &escape_link).unwrap();
        let jailed_link = f
            .call_as(
                "jailed",
                "fs.thumb",
                json!({"paths": [f.p(&escape_link)], "size": 128}),
            )
            .await
            .unwrap();
        assert_eq!(jailed_link["results"][0]["error"]["code"], json!("denied"));
    }

    // Concurrent `fs.thumb` calls across threads all succeed cleanly.
    let mut handles = Vec::new();
    for i in 0..12 {
        let node = f.node.clone();
        let path = f.p(&pic);
        let size = if i % 2 == 0 { 128 } else { 192 };
        handles.push(tokio::spawn(async move {
            node.call(Call::new(
                Door::Cli,
                "fs.thumb",
                json!({"paths": [path], "size": size}),
            ))
            .await
        }));
    }
    for h in handles {
        let out = h.await.unwrap().unwrap();
        assert!(out["results"][0].get("ok").is_some(), "{out}");
    }
}

#[tokio::test]
async fn thumb_extracts_video_posters_when_ffmpeg_is_installed() {
    // Only run the positive ffmpeg extraction check when `ffmpeg` is present on PATH.
    let has_ffmpeg = std::process::Command::new("ffmpeg")
        .arg("-version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success());
    if !has_ffmpeg {
        return;
    }

    let f = Fixture::new();
    let clip = f.work.join("sample.mp4");
    let short_clip = f.work.join("short.mp4");
    for (path, duration) in [(&clip, "2"), (&short_clip, "0.2")] {
        let status = std::process::Command::new("ffmpeg")
            .args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-f",
                "lavfi",
                "-i",
                &format!("testsrc=duration={duration}:size=320x180:rate=10"),
                "-pix_fmt",
                "yuv420p",
                path.to_str().unwrap(),
            ])
            .status()
            .unwrap();
        assert!(status.success(), "ffmpeg fixture failed for {path:?}");
    }

    let res = f
        .call(
            "fs.thumb",
            json!({"paths": [f.p(&clip), f.p(&short_clip)], "size": 160, "fit": "contain"}),
        )
        .await
        .unwrap();
    let items = res["results"].as_array().unwrap();
    assert_eq!(items.len(), 2);
    for item in items {
        assert_eq!(item["ok"]["mime"], json!("image/jpeg"), "{item}");
        assert_eq!(item["ok"]["width"], json!(160), "{item}");
        assert_eq!(item["ok"]["height"], json!(90), "{item}");
    }
}

#[tokio::test]
async fn list_bounds_recursive_counting_by_limit_while_keeping_depth_one_exact() {
    let f = Fixture::new();
    let deep = f.work.join("deep_tree");
    for d in 0..40 {
        let sub = deep.join(format!("dir_{d:02}"));
        std::fs::create_dir_all(&sub).unwrap();
        for i in 0..30 {
            std::fs::write(sub.join(format!("file_{i:02}.txt")), "x").unwrap();
        }
    }

    // `depth: 1` always counts every direct child accurately and sets `total_exact: true`.
    let shallow = f
        .call(
            "fs.list",
            json!({"path": f.p(&deep), "depth": 1, "limit": 5}),
        )
        .await
        .unwrap();
    assert_eq!(shallow["entries"].as_array().unwrap().len(), 5);
    assert_eq!(shallow["total"], json!(40));
    assert_eq!(shallow["total_exact"], json!(true));
    assert_eq!(shallow["truncated"], json!(true));

    // `depth: 3, limit: 5` on a 1,240-entry tree stops promptly instead of walking all 1,240
    // entries, reporting `total_exact: false` and a bounded lower-bound `total`.
    let bounded = f
        .call(
            "fs.list",
            json!({"path": f.p(&deep), "depth": 3, "limit": 5}),
        )
        .await
        .unwrap();
    assert_eq!(bounded["entries"].as_array().unwrap().len(), 5);
    assert_eq!(bounded["total_exact"], json!(false), "{bounded}");
    assert_eq!(bounded["truncated"], json!(true));
    let lower_bound = bounded["total"].as_u64().unwrap();
    assert!(
        (6..=50).contains(&lower_bound),
        "expected limit-bounded lower bound, got {lower_bound}"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn find_checks_budget_even_when_directories_are_unreadable() {
    use std::os::unix::fs::PermissionsExt;

    // Skip if running as root, since root ignores 0o000 directory permissions.
    let f = Fixture::new();
    let root = f.work.join("unreadable_tree");
    std::fs::create_dir_all(&root).unwrap();
    let mut locked_dirs = Vec::new();
    for i in 0..200 {
        let sub = root.join(format!("d_{i:03}"));
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(sub.join("hidden_inside.txt"), "x").unwrap();
        std::fs::set_permissions(&sub, std::fs::Permissions::from_mode(0o000)).unwrap();
        locked_dirs.push(sub);
    }

    // Confirm permissions actually deny reading (non-root check).
    let actually_denied = std::fs::read_dir(&locked_dirs[0]).is_err();
    let res = f
        .call(
            "fs.find",
            json!({
                "path": f.p(&root),
                "ignore": false,
                "budget_ms": 1,
            }),
        )
        .await;

    // Always restore permissions before asserting so TempDir cleans up cleanly.
    for sub in &locked_dirs {
        let _ = std::fs::set_permissions(sub, std::fs::Permissions::from_mode(0o755));
    }

    if !actually_denied {
        return;
    }
    let val = res.unwrap();
    // Either completed fast or cut off with `reason: "time"` — never hung or failed.
    if val["truncated"] == json!(true) {
        assert_eq!(val["reason"], json!("time"));
    }
}

#[tokio::test]
async fn thumb_blocks_ffmpeg_manifest_playlist_symlink_and_http_attacks() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    let has_ffmpeg = std::process::Command::new("ffmpeg")
        .arg("-version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success());
    if !has_ffmpeg {
        return;
    }

    let f = Fixture::new();
    f.write_config(
        "policy.toml",
        r#"
[principals.jailed]
role = "jailed"

[roles.jailed]
allow = ["fs.*"]
paths = ["~/projects/**"]
"#,
    );

    // Create a real secret video outside `~/projects` and a real legitimate video inside.
    let outside_dir = f.home.join("outside");
    std::fs::create_dir_all(&outside_dir).unwrap();
    let secret_mp4 = outside_dir.join("secret.mp4");
    let legit_mp4 = f.work.join("legit.mp4");
    for (path, color) in [(&secret_mp4, "red"), (&legit_mp4, "blue")] {
        let status = std::process::Command::new("ffmpeg")
            .args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-f",
                "lavfi",
                "-i",
                &format!("color=c={color}:s=160x90:d=1:r=10"),
                "-pix_fmt",
                "yuv420p",
                path.to_str().unwrap(),
            ])
            .status()
            .unwrap();
        assert!(status.success(), "ffmpeg fixture failed for {path:?}");
    }

    // Forward-slash representation works inside manifests/playlists on both Unix and Windows.
    let secret_fwd = secret_mp4.to_string_lossy().replace('\\', "/");
    let secret_file_url = if secret_fwd.starts_with('/') {
        format!("file://{secret_fwd}")
    } else {
        format!("file:///{secret_fwd}")
    };

    // 1. DASH `.mpd` manifest renamed to `.mp4` pointing at the outside secret video.
    let evil_dash = f.work.join("evil_dash.mp4");
    std::fs::write(
        &evil_dash,
        format!(
            r#"<?xml version="1.0" encoding="utf-8"?>
<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" type="static" mediaPresentationDuration="PT1S" minBufferTime="PT1S">
  <Period>
    <AdaptationSet mimeType="video/mp4">
      <Representation id="1" bandwidth="100000" width="160" height="90">
        <BaseURL>{secret_file_url}</BaseURL>
      </Representation>
    </AdaptationSet>
  </Period>
</MPD>
"#
        ),
    )
    .unwrap();

    // 2. `ffconcat` playlist renamed to `.mp4` pointing through a relative symlink (or relative
    // path on platforms without unprivileged symlinks) to the outside video.
    let evil_concat_sym = f.work.join("evil_concat_sym.mp4");
    let concat_rel_target = {
        #[cfg(unix)]
        {
            let link_dir = f.work.join("link_inside");
            std::os::unix::fs::symlink(&outside_dir, &link_dir).unwrap();
            "link_inside/secret.mp4".to_owned()
        }
        #[cfg(not(unix))]
        {
            "../outside/secret.mp4".to_owned()
        }
    };
    std::fs::write(
        &evil_concat_sym,
        format!("ffconcat version 1.0\nfile '{concat_rel_target}'\n"),
    )
    .unwrap();

    // 3. HLS `.m3u8` playlist renamed to `.mp4` referencing the outside video.
    let evil_hls = f.work.join("evil_hls.mp4");
    std::fs::write(
        &evil_hls,
        format!(
            "#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-TARGETDURATION:10\n#EXTINF:1.0,\n{secret_fwd}\n#EXT-X-ENDLIST\n"
        ),
    )
    .unwrap();

    // 4. `ffconcat` playlist renamed to `.mp4` naming the absolute path to the outside video.
    let evil_abs = f.work.join("evil_abs.mp4");
    std::fs::write(
        &evil_abs,
        format!("ffconcat version 1.0\nfile '{secret_fwd}'\n"),
    )
    .unwrap();

    // 5. Playlist renamed to `.mp4` referencing an `http://127.0.0.1:<port>/secret.mp4` endpoint.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let addr = listener.local_addr().unwrap();
    let http_hit = Arc::new(AtomicBool::new(false));
    let http_stop = Arc::new(AtomicBool::new(false));
    let http_hit_srv = Arc::clone(&http_hit);
    let http_stop_srv = Arc::clone(&http_stop);
    let server = std::thread::spawn(move || {
        while !http_stop_srv.load(Ordering::Relaxed) {
            if listener.accept().is_ok() {
                http_hit_srv.store(true, Ordering::SeqCst);
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    });
    let evil_http = f.work.join("evil_http.mp4");
    std::fs::write(
        &evil_http,
        format!(
            "#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-TARGETDURATION:10\n#EXTINF:1.0,\nhttp://{addr}/secret.mp4\n#EXT-X-ENDLIST\n"
        ),
    )
    .unwrap();

    // Sanity check: without our `-format_whitelist` / `-protocol_whitelist`, raw `ffmpeg` WOULD
    // follow `evil_concat_sym.mp4` on Unix (confirming the attack fixture is genuine and fails
    // loudly if the restrictions in `fs.thumb` are removed).
    #[cfg(unix)]
    {
        let probe_out = f.dir.path().join("probe_without_whitelist.jpg");
        let probe_status = std::process::Command::new("ffmpeg")
            .args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-i",
                &format!("file:{}", evil_concat_sym.to_string_lossy()),
                "-frames:v",
                "1",
                "-y",
                probe_out.to_str().unwrap(),
            ])
            .status()
            .unwrap();
        assert!(
            probe_status.success() && probe_out.exists(),
            "expected unrestricted ffmpeg to follow ffconcat symlink fixture"
        );
    }

    // Call `fs.thumb` as the jailed role on all 5 attack files plus the legitimate video.
    let res = f
        .call_as(
            "jailed",
            "fs.thumb",
            json!({
                "paths": [
                    f.p(&evil_dash),
                    f.p(&evil_concat_sym),
                    f.p(&evil_hls),
                    f.p(&evil_abs),
                    f.p(&evil_http),
                    f.p(&legit_mp4),
                ],
                "size": 128,
            }),
        )
        .await
        .unwrap();
    http_stop.store(true, Ordering::SeqCst);
    let _ = server.join();

    let items = res["results"].as_array().unwrap();
    assert_eq!(items.len(), 6);
    for (idx, label) in [
        (0, "DASH .mpd renamed to .mp4"),
        (1, "ffconcat via symlink"),
        (2, "HLS .m3u8 renamed to .mp4"),
        (3, "ffconcat with absolute path"),
        (4, "HLS playlist with http:// URL"),
    ] {
        assert!(
            items[idx].get("ok").is_none(),
            "{label} must be rejected, got: {}",
            items[idx]
        );
        assert!(
            items[idx].get("error").is_some(),
            "{label} must return a per-path error, got: {}",
            items[idx]
        );
    }
    assert!(
        !http_hit.load(Ordering::SeqCst),
        "ffmpeg must never connect to an http:// URL referenced in a playlist"
    );
    // Legitimate video inside `~/projects` still succeeds under the strict whitelists.
    assert!(
        items[5].get("ok").is_some(),
        "legitimate mp4 should still thumbnail: {}",
        items[5]
    );
}

#[cfg(unix)]
#[tokio::test]
async fn jailed_role_refuses_symlink_in_middle_path_component() {
    use std::os::unix::fs::symlink;

    let f = Fixture::new();
    f.write_config(
        "policy.toml",
        r#"
[principals.jailed]
role = "jailed"

[roles.jailed]
allow = ["fs.*"]
paths = ["~/projects/**"]
"#,
    );

    let outside = f.home.join("outside_secret");
    std::fs::create_dir_all(&outside).unwrap();
    let secret_pic = outside.join("photo.png");
    make_rgb_image(128, 128, [220, 40, 40])
        .save_with_format(&secret_pic, image::ImageFormat::Png)
        .unwrap();

    // Middle-component symlink inside `~/projects` pointing outside the jail.
    let mid_link = f.work.join("mid_link");
    symlink(&outside, &mid_link).unwrap();
    let escaped_photo = mid_link.join("photo.png");

    let thumb_res = f
        .call_as(
            "jailed",
            "fs.thumb",
            json!({"paths": [f.p(&escaped_photo)], "size": 128}),
        )
        .await
        .unwrap();
    assert_eq!(
        thumb_res["results"][0]["error"]["code"],
        json!("denied"),
        "fs.thumb must refuse middle-component symlink: {thumb_res}"
    );

    let list_err = f
        .call_as("jailed", "fs.list", json!({"path": f.p(&mid_link)}))
        .await
        .unwrap_err();
    assert_eq!(list_err.code, ErrorCode::Denied);

    let find_err = f
        .call_as("jailed", "fs.find", json!({"path": f.p(&mid_link)}))
        .await
        .unwrap_err();
    assert_eq!(find_err.code, ErrorCode::Denied);
}

#[tokio::test]
async fn thumb_cache_invalidates_on_same_size_same_mtime_rename_swap() {
    use std::time::{Duration, SystemTime};

    let f = Fixture::new();
    let target = f.work.join("swapped.bmp");
    let replacement = f.work.join("replacement.bmp");

    // Two solid-color BMPs of identical dimensions always have the exact same byte length.
    make_rgb_image(128, 128, [240, 10, 10])
        .save_with_format(&target, image::ImageFormat::Bmp)
        .unwrap();
    make_rgb_image(128, 128, [10, 10, 240])
        .save_with_format(&replacement, image::ImageFormat::Bmp)
        .unwrap();

    // Pin both files to the exact same modification timestamp.
    let fixed_mtime = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
    std::fs::File::options()
        .write(true)
        .open(&target)
        .unwrap()
        .set_modified(fixed_mtime)
        .unwrap();
    std::fs::File::options()
        .write(true)
        .open(&replacement)
        .unwrap()
        .set_modified(fixed_mtime)
        .unwrap();

    let meta_before = std::fs::metadata(&target).unwrap();

    // First call caches the red thumbnail.
    let first = f
        .call("fs.thumb", json!({"paths": [f.p(&target)], "size": 96}))
        .await
        .unwrap();
    let first_b64 = first["results"][0]["ok"]["content"]
        .as_str()
        .unwrap()
        .to_owned();
    let first_img = image::load_from_memory(&BASE64.decode(&first_b64).unwrap())
        .unwrap()
        .to_rgb8();
    let first_px = first_img.get_pixel(48, 48).0;
    assert!(
        first_px[0] > 180 && first_px[2] < 60,
        "expected red: {first_px:?}"
    );

    // Atomically rename `replacement` (blue) over `target` (red) keeping identical size and mtime.
    std::fs::rename(&replacement, &target).unwrap();
    let meta_after = std::fs::metadata(&target).unwrap();
    assert_eq!(meta_before.len(), meta_after.len());
    assert_eq!(
        meta_before.modified().unwrap(),
        meta_after.modified().unwrap()
    );

    // Second call MUST miss the cache (different FileId / ctime) and return the blue thumbnail.
    let second = f
        .call("fs.thumb", json!({"paths": [f.p(&target)], "size": 96}))
        .await
        .unwrap();
    let second_b64 = second["results"][0]["ok"]["content"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_ne!(
        first_b64, second_b64,
        "cache returned stale thumbnail after same-size, same-mtime rename swap"
    );
    let second_img = image::load_from_memory(&BASE64.decode(&second_b64).unwrap())
        .unwrap()
        .to_rgb8();
    let second_px = second_img.get_pixel(48, 48).0;
    assert!(
        second_px[2] > 180 && second_px[0] < 60,
        "expected blue: {second_px:?}"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn confined_role_does_not_hang_on_fifo_socket_or_device_in_any_fs_tool() {
    use std::os::unix::net::UnixListener;
    use std::time::Duration;

    let f = Fixture::new();
    f.write_config(
        "policy.toml",
        r#"
[principals.jailed]
role = "jailed"

[roles.jailed]
allow = ["fs.*"]
paths = ["~/projects/**"]
"#,
    );

    // 1. Named FIFO inside the jailed root (opening it without O_NONBLOCK hangs forever!).
    let fifo = f.work.join("pipe.png");
    let mkfifo_status = std::process::Command::new("mkfifo")
        .arg(&fifo)
        .status()
        .unwrap();
    assert!(mkfifo_status.success(), "mkfifo failed");

    // 2. Unix domain socket inside the jailed root.
    let sock = f.work.join("sock.png");
    let _listener = UnixListener::bind(&sock).unwrap();

    // 3. Symlink to `/dev/zero` inside the jailed root.
    let dev_link = f.work.join("zero.mp4");
    std::os::unix::fs::symlink("/dev/zero", &dev_link).unwrap();

    let copy_dst = f.work.join("copy_dst.bin");
    let move_dst = f.work.join("move_dst.bin");
    let timeout = Duration::from_secs(5);

    for (label, target) in [("fifo", &fifo), ("socket", &sock), ("dev_zero", &dev_link)] {
        let target_str = f.p(target);
        let calls = [
            ("fs.stat", json!({"path": target_str})),
            ("fs.list", json!({"path": target_str})),
            ("fs.read", json!({"path": target_str})),
            ("fs.hash", json!({"path": target_str})),
            ("fs.write", json!({"path": target_str, "content": "hi"})),
            (
                "fs.edit",
                json!({"path": target_str, "old": "a", "new": "b"}),
            ),
            ("fs.mkdir", json!({"path": target_str})),
            ("fs.copy", json!({"from": target_str, "to": f.p(&copy_dst)})),
            ("fs.find", json!({"path": target_str})),
            ("fs.grep", json!({"path": target_str, "pattern": "x"})),
            ("fs.tags", json!({"paths": [target_str]})),
            ("fs.thumb", json!({"paths": [target_str], "size": 128})),
        ];

        for (tool, args) in calls {
            let res = tokio::time::timeout(timeout, f.call_as("jailed", tool, args))
                .await
                .unwrap_or_else(|_| panic!("{tool} hung on {label} as jailed role"));
            match (tool, label, res) {
                // `fs.stat` succeeds on FIFO/socket (reports kind="other") and on symlink (reports
                // kind="symlink" or denied for symlink escaping to /dev/zero).
                ("fs.stat", "fifo" | "socket", Ok(val)) => {
                    assert_eq!(val["kind"], json!("other"), "{tool} {label}: {val}");
                }
                ("fs.stat", "dev_zero", Err(err)) => {
                    assert_eq!(err.code, ErrorCode::Denied, "{tool} {label}: {err}");
                }
                ("fs.thumb" | "fs.tags", _, Ok(val)) => {
                    assert!(
                        val["results"][0].get("error").is_some(),
                        "{tool} must report per-path error on {label}: {val}"
                    );
                }
                (_, _, Err(err)) => {
                    assert!(
                        matches!(
                            err.code,
                            ErrorCode::InvalidArgs
                                | ErrorCode::Denied
                                | ErrorCode::Exists
                                | ErrorCode::Failed
                                | ErrorCode::Unsupported
                        ),
                        "unexpected error code for {tool} on {label}: {err}"
                    );
                }
                (_, _, Ok(val)) => {
                    panic!("{tool} unexpectedly succeeded on {label}: {val}");
                }
            }
        }
    }

    // `fs.move` and `fs.remove` on FIFO (separate so FIFO still exists for the earlier calls).
    let mv = tokio::time::timeout(
        timeout,
        f.call_as(
            "jailed",
            "fs.move",
            json!({"from": f.p(&fifo), "to": f.p(&move_dst)}),
        ),
    )
    .await
    .expect("fs.move hung on fifo as jailed role");
    assert!(
        mv.is_ok(),
        "fs.move of fifo inside jail should complete: {mv:?}"
    );

    let rm = tokio::time::timeout(
        timeout,
        f.call_as("jailed", "fs.remove", json!({"path": f.p(&move_dst)})),
    )
    .await
    .expect("fs.remove hung on fifo as jailed role");
    assert!(
        rm.is_ok(),
        "fs.remove of fifo inside jail should complete: {rm:?}"
    );
}

fn decode_thumb_rgb(item: &Value) -> image::Rgb<u8> {
    let content = item["ok"]["content"].as_str().expect("base64 content");
    let raw = BASE64.decode(content).expect("valid base64");
    let img = image::load_from_memory(&raw)
        .expect("valid image")
        .to_rgb8();
    *img.get_pixel(img.width() / 2, img.height() / 2)
}

#[tokio::test]
async fn tags_and_thumb_read_every_audio_format_with_and_without_tags_and_covers() {
    use audio_fixtures as af;

    let f = Fixture::new();
    let red_jpg = af::encode_solid_jpeg(160, 160, [220, 20, 20]);
    let blue_png = af::encode_solid_png(160, 160, [20, 40, 220]);
    let alpha_png = af::encode_solid_rgba_png(160, 160, [40, 180, 120, 128]);

    let mp3_v23 = f.work.join("v23_unsync.mp3");
    let mp3_v22 = f.work.join("v22.mp3");
    let mp3_v24 = f.work.join("v24_dli.mp3");
    let mp3_v1 = f.work.join("v1_only.mp3");
    let mp3_bare = f.work.join("bare.mp3");
    let wav_tagged = f.work.join("tagged.wav");
    let wav_bare = f.work.join("bare.wav");
    let aiff_tagged = f.work.join("tagged.aiff");
    let aif_bare = f.work.join("bare.aif");
    let flac_tagged = f.work.join("tagged.flac");
    let flac_bare = f.work.join("bare.flac");
    let ogg_vorbis = f.work.join("tagged.ogg");
    let ogg_opus = f.work.join("tagged.opus");
    let m4a_tagged = f.work.join("tagged.m4a");
    let m4b_tagged = f.work.join("book.m4b");
    let mp4_audio = f.work.join("audio_only.mp4");

    std::fs::write(
        &mp3_v23,
        af::build_mp3_id3v23(&[("image/jpeg", 3, &red_jpg)], true, true),
    )
    .unwrap();
    std::fs::write(&mp3_v22, af::build_mp3_id3v22(Some(&red_jpg))).unwrap();
    std::fs::write(&mp3_v24, af::build_mp3_id3v24(Some(&red_jpg))).unwrap();
    std::fs::write(&mp3_v1, af::build_mp3_id3v1()).unwrap();
    std::fs::write(&mp3_bare, af::build_mp3_no_tags()).unwrap();
    std::fs::write(&wav_tagged, af::build_wav(true, Some(&red_jpg), 4)).unwrap();
    std::fs::write(&wav_bare, af::build_wav(false, None, 2)).unwrap();
    std::fs::write(&aiff_tagged, af::build_aiff(true, Some(&red_jpg), 6)).unwrap();
    std::fs::write(&aif_bare, af::build_aiff(false, None, 3)).unwrap();
    std::fs::write(
        &flac_tagged,
        af::build_flac(
            &[
                ("TITLE", "Flac song"),
                ("ARTIST", "Someone"),
                ("ALBUM", "Flac album"),
                ("ALBUMARTIST", "Album artist"),
                ("TRACKNUMBER", "2/9"),
                ("DISCNUMBER", "1/2"),
                ("DATE", "2025-02-01"),
            ],
            &[(3, "image/png", &alpha_png)],
            8,
        ),
    )
    .unwrap();
    std::fs::write(&flac_bare, af::build_flac(&[], &[], 5)).unwrap();
    std::fs::write(
        &ogg_vorbis,
        af::build_ogg_vorbis(
            &[
                ("TITLE", "Vorbis song"),
                ("ARTIST", "Someone"),
                ("ALBUM", "Ogg album"),
                ("ALBUMARTIST", "Album artist"),
                ("TRACKNUMBER", "4"),
                ("DISCNUMBER", "1"),
                ("DATE", "2024"),
            ],
            &[(3, "image/png", &blue_png)],
            7,
        ),
    )
    .unwrap();
    std::fs::write(
        &ogg_opus,
        af::build_ogg_opus(
            &[
                ("TITLE", "Opus song"),
                ("ARTIST", "Someone"),
                ("ALBUM", "Opus album"),
                ("TRACKNUMBER", "5"),
                ("DATE", "2025"),
            ],
            &[(3, "image/jpeg", &red_jpg)],
            9,
        ),
    )
    .unwrap();
    std::fs::write(
        &m4a_tagged,
        af::build_mp4(true, Some((13, &red_jpg)), 11, false, false),
    )
    .unwrap();
    std::fs::write(
        &m4b_tagged,
        af::build_mp4(true, Some((14, &blue_png)), 12, true, false),
    )
    .unwrap();
    std::fs::write(
        &mp4_audio,
        af::build_mp4(true, Some((13, &red_jpg)), 13, false, false),
    )
    .unwrap();

    let all_paths = vec![
        f.p(&mp3_v23),
        f.p(&mp3_v22),
        f.p(&mp3_v24),
        f.p(&mp3_v1),
        f.p(&mp3_bare),
        f.p(&wav_tagged),
        f.p(&wav_bare),
        f.p(&aiff_tagged),
        f.p(&aif_bare),
        f.p(&flac_tagged),
        f.p(&flac_bare),
        f.p(&ogg_vorbis),
        f.p(&ogg_opus),
        f.p(&m4a_tagged),
        f.p(&m4b_tagged),
        f.p(&mp4_audio),
    ];

    let res = f
        .call("fs.tags", json!({"paths": all_paths}))
        .await
        .unwrap();
    let items = res["results"].as_array().unwrap();
    assert_eq!(items.len(), all_paths.len());
    for (i, item) in items.iter().enumerate() {
        assert_eq!(item["path"], json!(all_paths[i]));
        assert!(
            item.get("ok").is_some(),
            "path {} failed: {item}",
            all_paths[i]
        );
    }

    // ID3v2.3 with global unsync, extended header, mixed encodings, Xing duration.
    let v23 = &items[0]["ok"];
    assert_eq!(v23["title"], json!("First song"));
    assert_eq!(v23["artist"], json!("Someone"));
    assert_eq!(v23["album"], json!("First album"));
    assert_eq!(v23["album_artist"], json!("Album artist"));
    assert_eq!(v23["track"], json!(3));
    assert_eq!(v23["disc"], json!(1));
    assert_eq!(v23["year"], json!(2025));
    assert!((v23["duration_s"].as_f64().unwrap() - 4.989).abs() < 0.1);
    assert_eq!(v23["cover"], json!(true));

    // ID3v2.2.
    assert_eq!(items[1]["ok"]["title"], json!("Second song"));
    assert_eq!(items[1]["ok"]["year"], json!(2022));
    assert_eq!(items[1]["ok"]["cover"], json!(true));

    // ID3v2.4 with DLI + per-frame unsync + TDRC.
    assert_eq!(items[2]["ok"]["title"], json!("Fourth song"));
    assert_eq!(items[2]["ok"]["year"], json!(2024));
    assert_eq!(items[2]["ok"]["disc"], json!(2));
    assert_eq!(items[2]["ok"]["cover"], json!(true));

    // ID3v1.1.
    assert_eq!(items[3]["ok"]["title"], json!("Old track"));
    assert_eq!(items[3]["ok"]["artist"], json!("Someone"));
    assert_eq!(items[3]["ok"]["album"], json!("Old album"));
    assert_eq!(items[3]["ok"]["track"], json!(7));
    assert_eq!(items[3]["ok"]["year"], json!(1999));
    assert_eq!(items[3]["ok"]["cover"], json!(false));
    assert!(items[3]["ok"].get("album_artist").is_none());

    // Bare MP3: absent tag fields, positive duration_s, cover: false.
    let bare = &items[4]["ok"];
    assert!(bare.get("title").is_none());
    assert!(bare.get("artist").is_none());
    assert!(bare.get("album").is_none());
    assert!(bare["duration_s"].as_f64().unwrap() > 0.1);
    assert_eq!(bare["cover"], json!(false));

    // WAV tagged & bare.
    assert_eq!(items[5]["ok"]["title"], json!("Wave song"));
    assert!((items[5]["ok"]["duration_s"].as_f64().unwrap() - 4.0).abs() < 0.05);
    assert_eq!(items[5]["ok"]["cover"], json!(true));
    assert!(items[6]["ok"].get("title").is_none());
    assert!((items[6]["ok"]["duration_s"].as_f64().unwrap() - 2.0).abs() < 0.05);
    assert_eq!(items[6]["ok"]["cover"], json!(false));

    // AIFF / AIF tagged & bare.
    assert_eq!(items[7]["ok"]["title"], json!("Aiff song"));
    assert!((items[7]["ok"]["duration_s"].as_f64().unwrap() - 6.0).abs() < 0.05);
    assert_eq!(items[7]["ok"]["cover"], json!(true));
    assert!(items[8]["ok"].get("title").is_none());
    assert!((items[8]["ok"]["duration_s"].as_f64().unwrap() - 3.0).abs() < 0.05);
    assert_eq!(items[8]["ok"]["cover"], json!(false));

    // FLAC tagged & bare.
    assert_eq!(items[9]["ok"]["title"], json!("Flac song"));
    assert_eq!(items[9]["ok"]["album_artist"], json!("Album artist"));
    assert!((items[9]["ok"]["duration_s"].as_f64().unwrap() - 8.0).abs() < 0.05);
    assert_eq!(items[9]["ok"]["cover"], json!(true));
    assert!(items[10]["ok"].get("title").is_none());
    assert!((items[10]["ok"]["duration_s"].as_f64().unwrap() - 5.0).abs() < 0.05);
    assert_eq!(items[10]["ok"]["cover"], json!(false));

    // Ogg Vorbis & Opus.
    assert_eq!(items[11]["ok"]["title"], json!("Vorbis song"));
    assert!((items[11]["ok"]["duration_s"].as_f64().unwrap() - 7.0).abs() < 0.05);
    assert_eq!(items[11]["ok"]["cover"], json!(true));
    assert_eq!(items[12]["ok"]["title"], json!("Opus song"));
    assert!((items[12]["ok"]["duration_s"].as_f64().unwrap() - 9.0).abs() < 0.05);
    assert_eq!(items[12]["ok"]["cover"], json!(true));

    // M4A, M4B (mvhd v1), and audio-only MP4.
    for (idx, expected_dur) in [(13, 11.0), (14, 12.0), (15, 13.0)] {
        assert_eq!(items[idx]["ok"]["title"], json!("First song"));
        assert_eq!(items[idx]["ok"]["artist"], json!("Someone"));
        assert_eq!(items[idx]["ok"]["album_artist"], json!("Album artist"));
        assert_eq!(items[idx]["ok"]["track"], json!(1));
        assert_eq!(items[idx]["ok"]["disc"], json!(1));
        assert_eq!(items[idx]["ok"]["year"], json!(2025));
        assert!((items[idx]["ok"]["duration_s"].as_f64().unwrap() - expected_dur).abs() < 0.05);
        assert_eq!(items[idx]["ok"]["cover"], json!(true));
    }

    // `fs.thumb` on all 16 audio files: embedded covers return thumbnails (PNG when transparent,
    // JPEG when opaque) and files with no cover return per-path `not_found`.
    let thumbs = f
        .call(
            "fs.thumb",
            json!({"paths": all_paths, "size": 1024, "fit": "cover"}),
        )
        .await
        .unwrap();
    let titems = thumbs["results"].as_array().unwrap();
    assert_eq!(titems.len(), all_paths.len());

    // Transparent PNG cover inside FLAC preserves alpha -> image/png at 1024x1024.
    assert_eq!(titems[9]["ok"]["mime"], json!("image/png"));
    assert_eq!(titems[9]["ok"]["width"], json!(1024));
    assert_eq!(titems[9]["ok"]["height"], json!(1024));

    // Embedded JPEG/PNG covers across MP3 v2.3/v2.2/v2.4, WAV, AIFF, Ogg Vorbis, Opus, M4A, M4B, MP4.
    for idx in [0, 1, 2, 5, 7, 11, 12, 13, 14, 15] {
        assert!(
            titems[idx].get("ok").is_some(),
            "expected cover thumbnail for {}: {}",
            all_paths[idx],
            titems[idx]
        );
        assert_eq!(titems[idx]["ok"]["width"], json!(1024));
        assert_eq!(titems[idx]["ok"]["height"], json!(1024));
    }
    // Blue PNG cover in Ogg Vorbis & M4B verifies cover color.
    let vorbis_px = decode_thumb_rgb(&titems[11]);
    assert!(vorbis_px.0[2] > 180 && vorbis_px.0[0] < 80, "{vorbis_px:?}");

    // Files without embedded cover and without a folder cover return per-path `not_found`.
    for idx in [3, 4, 6, 8, 10] {
        assert_eq!(
            titems[idx]["error"]["code"],
            json!("not_found"),
            "expected not_found for {}: {}",
            all_paths[idx],
            titems[idx]
        );
    }
}

#[tokio::test]
async fn audio_cover_selection_prefers_front_then_first_then_folder_cover_with_policy_and_invalidation()
 {
    use audio_fixtures as af;

    let f = Fixture::new();
    let red_jpg = af::encode_solid_jpeg(160, 160, [220, 20, 20]);
    let blue_jpg = af::encode_solid_jpeg(160, 160, [20, 30, 220]);
    let green_jpg = af::encode_solid_jpeg(160, 160, [20, 190, 40]);

    // 1. Multiple embedded pictures: back cover (type 4) first, front cover (type 3) second -> front (red) wins.
    let multi_front = f.work.join("multi_front.mp3");
    std::fs::write(
        &multi_front,
        af::build_mp3_id3v23(
            &[
                ("-->", 3, b"https://example.invalid/cover.jpg"),
                ("image/jpeg", 4, &blue_jpg),
                ("image/jpeg", 3, &red_jpg),
            ],
            false,
            false,
        ),
    )
    .unwrap();

    // 2. Multiple non-front pictures (type 4 blue, type 6 green): first valid (blue) wins.
    let multi_nonfront = f.work.join("multi_nonfront.mp3");
    std::fs::write(
        &multi_nonfront,
        af::build_mp3_id3v23(
            &[("image/jpeg", 4, &blue_jpg), ("image/jpeg", 6, &green_jpg)],
            false,
            false,
        ),
    )
    .unwrap();

    // 3. Only external URL `-->` picture -> ignored (treated as no embedded cover).
    let url_only = f.work.join("url_only.mp3");
    std::fs::write(
        &url_only,
        af::build_mp3_id3v23(
            &[("-->", 3, b"https://example.invalid/cover.jpg")],
            false,
            false,
        ),
    )
    .unwrap();

    let multi_thumbs = f
        .call(
            "fs.thumb",
            json!({
                "paths": [f.p(&multi_front), f.p(&multi_nonfront), f.p(&url_only)],
                "size": 128,
                "fit": "cover"
            }),
        )
        .await
        .unwrap();
    let px_front = decode_thumb_rgb(&multi_thumbs["results"][0]);
    assert!(
        px_front.0[0] > 180 && px_front.0[2] < 80,
        "front cover (red) must win over earlier back cover, got {px_front:?}"
    );
    let px_first = decode_thumb_rgb(&multi_thumbs["results"][1]);
    assert!(
        px_first.0[2] > 180 && px_first.0[1] < 80,
        "first picture (blue) must win when no front cover exists, got {px_first:?}"
    );
    assert_eq!(
        multi_thumbs["results"][2]["error"]["code"],
        json!("not_found"),
        "external URL APIC must be ignored"
    );

    // 4. Folder cover fallback, case-insensitivity, priority order, live add/remove invalidation,
    //    and non-regular file skipping (directory named `cover.jpg` skipped in favor of `ALBUM.PNG`).
    let album_dir = f.work.join("album");
    std::fs::create_dir_all(&album_dir).unwrap();
    let song = album_dir.join("track01.flac");
    std::fs::write(
        &song,
        af::build_flac(&[("TITLE", "Folder test"), ("ARTIST", "Someone")], &[], 4),
    )
    .unwrap();

    // Initially no folder cover -> cover: false, fs.thumb -> not_found.
    let t0 = f
        .call("fs.tags", json!({"paths": [f.p(&song)]}))
        .await
        .unwrap();
    assert_eq!(t0["results"][0]["ok"]["cover"], json!(false));
    let th0 = f
        .call("fs.thumb", json!({"paths": [f.p(&song)], "size": 128}))
        .await
        .unwrap();
    assert_eq!(th0["results"][0]["error"]["code"], json!("not_found"));

    // A directory named `cover.jpg` must be ignored; `ALBUM.PNG` (blue) is picked up immediately
    // even though `track01.flac`'s parsed tags are already cached!
    std::fs::create_dir_all(album_dir.join("cover.jpg")).unwrap();
    let album_png = album_dir.join("ALBUM.PNG");
    std::fs::write(&album_png, af::encode_solid_png(128, 128, [20, 30, 220])).unwrap();

    let t1 = f
        .call("fs.tags", json!({"paths": [f.p(&song)]}))
        .await
        .unwrap();
    assert_eq!(
        t1["results"][0]["ok"]["cover"],
        json!(true),
        "adding folder cover must reflect immediately even when tags are cached"
    );
    let th1 = f
        .call("fs.thumb", json!({"paths": [f.p(&song)], "size": 128}))
        .await
        .unwrap();
    let px_album = decode_thumb_rgb(&th1["results"][0]);
    assert!(px_album.0[2] > 180 && px_album.0[1] < 80, "{px_album:?}");

    // Adding `Folder.JPG` (green) beats `ALBUM.PNG` (priority: cover > folder > front > album).
    let folder_jpg = album_dir.join("Folder.JPG");
    std::fs::write(&folder_jpg, &green_jpg).unwrap();
    let th2 = f
        .call("fs.thumb", json!({"paths": [f.p(&song)], "size": 128}))
        .await
        .unwrap();
    let px_folder = decode_thumb_rgb(&th2["results"][0]);
    assert!(px_folder.0[1] > 150 && px_folder.0[0] < 80, "{px_folder:?}");

    // Removing both folder covers immediately reverts `cover: false` and `not_found`.
    std::fs::remove_file(&album_png).unwrap();
    std::fs::remove_file(&folder_jpg).unwrap();
    let t2 = f
        .call("fs.tags", json!({"paths": [f.p(&song)]}))
        .await
        .unwrap();
    assert_eq!(t2["results"][0]["ok"]["cover"], json!(false));
    let th3 = f
        .call("fs.thumb", json!({"paths": [f.p(&song)], "size": 128}))
        .await
        .unwrap();
    assert_eq!(th3["results"][0]["error"]["code"], json!("not_found"));

    // 5. Folder cover must satisfy `fs.read` policy: a symlink `cover.jpg` escaping allowed `paths`
    //    is denied and ignored for a confined role, and a Unix FIFO named `cover.jpg` is skipped.
    let outside = f.home.join("secret_cover.jpg");
    std::fs::write(&outside, &red_jpg).unwrap();
    std::fs::remove_dir(album_dir.join("cover.jpg")).unwrap();

    #[cfg(unix)]
    {
        let escaping_cover = album_dir.join("cover.jpg");
        std::os::unix::fs::symlink(&outside, &escaping_cover).unwrap();

        let fifo_cover = album_dir.join("folder.jpg");
        let st = std::process::Command::new("mkfifo")
            .arg(&fifo_cover)
            .status()
            .unwrap();
        assert!(st.success());

        let policy = r#"
[principals.listener]
role = "music_only"

[roles.music_only]
allow = ["fs.read", "fs.tags", "fs.thumb"]
paths = ["~/projects/album/**"]
"#;
        std::fs::create_dir_all(f.dir.path().join("config")).unwrap();
        std::fs::write(f.dir.path().join("config").join("policy.toml"), policy).unwrap();

        let confined_tags = tokio::time::timeout(
            std::time::Duration::from_secs(3),
            f.call_as("listener", "fs.tags", json!({"paths": [f.p(&song)]})),
        )
        .await
        .expect("fs.tags must not block on FIFO folder cover")
        .unwrap();
        assert_eq!(
            confined_tags["results"][0]["ok"]["cover"],
            json!(false),
            "escaping symlink and FIFO folder covers must be ignored for confined caller"
        );
        let confined_thumb = tokio::time::timeout(
            std::time::Duration::from_secs(3),
            f.call_as(
                "listener",
                "fs.thumb",
                json!({"paths": [f.p(&song)], "size": 128}),
            ),
        )
        .await
        .expect("fs.thumb must not block on FIFO folder cover")
        .unwrap();
        assert_eq!(
            confined_thumb["results"][0]["error"]["code"],
            json!("not_found")
        );

        std::fs::remove_file(&escaping_cover).unwrap();
        std::fs::remove_file(&fifo_cover).unwrap();
    }

    // 6. Modifying the audio file invalidates cached tags immediately.
    std::fs::write(
        &song,
        af::build_flac(&[("TITLE", "Updated song"), ("ARTIST", "Someone")], &[], 10),
    )
    .unwrap();
    let t_updated = f
        .call("fs.tags", json!({"paths": [f.p(&song)]}))
        .await
        .unwrap();
    assert_eq!(
        t_updated["results"][0]["ok"]["title"],
        json!("Updated song")
    );
    assert!(
        (t_updated["results"][0]["ok"]["duration_s"]
            .as_f64()
            .unwrap()
            - 10.0)
            .abs()
            < 0.05
    );
}

#[tokio::test]
async fn tags_and_audio_thumb_enforce_policy_and_handle_hostile_fixtures() {
    use audio_fixtures as af;

    let f = Fixture::new();
    let red_jpg = af::encode_solid_jpeg(128, 128, [220, 20, 20]);

    let good_mp3 = f.work.join("good.mp3");
    std::fs::write(
        &good_mp3,
        af::build_mp3_id3v23(&[("image/jpeg", 3, &red_jpg)], false, false),
    )
    .unwrap();

    // Non-audio regular file -> per-path `unsupported`.
    let txt = f.work.join("notes.txt");
    std::fs::write(&txt, "plain text").unwrap();

    // Directory -> per-path `invalid_args`.
    let subdir = f.work.join("subdir");
    std::fs::create_dir_all(&subdir).unwrap();

    // Oversized declared lengths (> 16 MiB) and truncated files for every format.
    let oversized_id3 = f.work.join("oversized.mp3");
    std::fs::write(&oversized_id3, af::build_oversized_id3v2_mp3()).unwrap();

    let trunc_flac = f.work.join("trunc.flac");
    std::fs::write(&trunc_flac, b"fLaC\x84\xFF\xFF\xFFshort").unwrap();

    let trunc_wav = f.work.join("trunc.wav");
    std::fs::write(&trunc_wav, b"RIFF\xFF\xFF\xFF\x7FWAVEID3 \xFF\xFF\xFF\x01").unwrap();

    let trunc_aiff = f.work.join("trunc.aiff");
    std::fs::write(&trunc_aiff, b"FORM\x7F\xFF\xFF\xFFAIFFID3 \x02\x00\x00\x00").unwrap();

    let trunc_ogg = f.work.join("trunc.ogg");
    std::fs::write(&trunc_ogg, b"OggS\x00\x02\x00\x00\x00").unwrap();

    let trunc_m4a = f.work.join("trunc.m4a");
    std::fs::write(
        &trunc_m4a,
        b"\x00\x00\x00\x01moov\xFF\xFF\xFF\xFF\xFF\xFF\xFF\xFF",
    )
    .unwrap();

    let empty_mp3 = f.work.join("empty.mp3");
    std::fs::write(&empty_mp3, b"").unwrap();

    // Hostile text: control chars, ANSI escapes, NULs, > 512 chars, whitespace+control-only artist.
    let hostile_text = f.work.join("hostile_text.mp3");
    std::fs::write(&hostile_text, af::build_hostile_text_mp3()).unwrap();

    // Embedded PNG decompression bomb inside MP3.
    let bomb_mp3 = f.work.join("bomb.mp3");
    std::fs::write(
        &bomb_mp3,
        af::build_mp3_with_raw_cover(
            "Bomb song",
            "image/png",
            &af::build_png_decompression_bomb(),
        ),
    )
    .unwrap();

    let batch = vec![
        f.p(&good_mp3),
        f.p(&txt),
        f.p(&subdir),
        f.p(&oversized_id3),
        f.p(&trunc_flac),
        f.p(&trunc_wav),
        f.p(&trunc_aiff),
        f.p(&trunc_ogg),
        f.p(&trunc_m4a),
        f.p(&empty_mp3),
        f.p(&hostile_text),
        f.p(&bomb_mp3),
    ];

    let res = f.call("fs.tags", json!({"paths": batch})).await.unwrap();
    let items = res["results"].as_array().unwrap();
    assert_eq!(items.len(), batch.len());

    // Valid sibling survives the hostile batch.
    assert_eq!(items[0]["ok"]["title"], json!("First song"));
    assert_eq!(items[1]["error"]["code"], json!("unsupported"));
    assert_eq!(items[2]["error"]["code"], json!("invalid_args"));
    assert_eq!(items[3]["error"]["code"], json!("failed"));
    for (idx, item) in items.iter().enumerate().take(10).skip(4) {
        assert_eq!(
            item["error"]["code"],
            json!("failed"),
            "truncated/empty file at {idx} must fail cleanly: {item}"
        );
    }

    // Hostile text sanitized: control chars removed, trimmed, <= 512 chars, empty artist absent.
    let ht = &items[10]["ok"];
    let title = ht["title"].as_str().unwrap();
    assert_eq!(title.chars().count(), 512);
    assert!(!title.chars().any(|c| c.is_control()));
    assert!(title.starts_with("[31mFirst song"));
    assert!(
        ht.get("artist").is_none(),
        "control-only artist must be absent: {ht}"
    );
    assert_eq!(ht["album"], json!("CleanAlbum"));

    // Embedded PNG bomb reports tags cleanly via `fs.tags`, and fails with `too_large` in `fs.thumb`
    // while `good_mp3` still succeeds.
    assert_eq!(items[11]["ok"]["title"], json!("Bomb song"));
    let thumb_bomb = f
        .call(
            "fs.thumb",
            json!({"paths": [f.p(&bomb_mp3), f.p(&good_mp3)], "size": 256}),
        )
        .await
        .unwrap();
    assert_eq!(thumb_bomb["results"][0]["error"]["code"], json!("failed"));
    assert_eq!(
        thumb_bomb["results"][0]["error"]["details"]["reason"],
        json!("too_large")
    );
    assert!(thumb_bomb["results"][1].get("ok").is_some());

    // Role denied `fs.read` is denied `fs.tags` even when `fs.tags` is explicitly allowed.
    let outside_mp3 = f.home.join("outside.mp3");
    std::fs::copy(&good_mp3, &outside_mp3).unwrap();
    let policy = r#"
[principals.no_read]
role = "no_read"

[principals.jailed]
role = "jailed"

[roles.no_read]
allow = ["fs.tags", "fs.list"]
deny = ["fs.read"]

[roles.jailed]
allow = ["fs.read", "fs.tags"]
paths = ["~/projects/**"]
"#;
    std::fs::create_dir_all(f.dir.path().join("config")).unwrap();
    std::fs::write(f.dir.path().join("config").join("policy.toml"), policy).unwrap();

    let denied = f
        .call_as("no_read", "fs.tags", json!({"paths": [f.p(&good_mp3)]}))
        .await
        .unwrap_err();
    assert_eq!(denied.code, ErrorCode::Denied);

    let confined = f
        .call_as(
            "jailed",
            "fs.tags",
            json!({"paths": [f.p(&good_mp3), f.p(&outside_mp3)]}),
        )
        .await
        .unwrap();
    assert!(confined["results"][0].get("ok").is_some());
    assert_eq!(confined["results"][1]["error"]["code"], json!("denied"));

    // Argument validation: 0 and 33 paths rejected with `invalid_args`.
    let too_many: Vec<String> = (0..33).map(|i| format!("~/projects/s{i}.mp3")).collect();
    for bad in [json!({"paths": []}), json!({"paths": too_many})] {
        let err = f.call("fs.tags", bad).await.unwrap_err();
        assert_eq!(err.code, ErrorCode::InvalidArgs);
    }
}
