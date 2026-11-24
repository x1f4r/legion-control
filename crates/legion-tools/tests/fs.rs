//! Integration tests for the `fs` tool family, exercised end to end through a real [`Node`].

#![allow(clippy::unwrap_used, clippy::expect_used)]

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
fn all_twelve_fs_tools_are_registered_with_their_effects() {
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

    // Safe atomic append with matching `expect_hash` succeeds and updates hash.
    let appended = f
        .call(
            "fs.write",
            json!({
                "path": f.p(&nested),
                "content": " world",
                "mode": "append",
                "expect_hash": hash1,
            }),
        )
        .await
        .unwrap();
    assert_eq!(appended["created"], json!(false));
    assert_eq!(appended["size"], json!(11));
    assert_eq!(std::fs::read_to_string(&nested).unwrap(), "hello world");

    // Append with stale hash fails without touching the file.
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
        ErrorCode::Conflict
    );
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
allow = ["fs.*"]
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
