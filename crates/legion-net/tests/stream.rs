//! The stream endpoint, one operation at a time, through its public entry point: what each
//! operation answers, what it changes, who may do it, and the audit line it leaves.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::fs;
use std::io::{BufRead, Cursor, Read};
use std::path::{Path, PathBuf};

use std::io;

use legion_core::{Door, Paths};
use legion_net::stream::{Host, STREAM_PROTOCOL, StreamServeOptions, serve, serve_with_options};
use legion_proto::ErrorCode;
use serde_json::{Value, json};

/// A node in a temporary directory: its own configuration, state and home.
struct Node {
    dir: tempfile::TempDir,
    host: Host,
}

impl Node {
    fn new() -> Node {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        fs::create_dir_all(home.join("projects")).unwrap();
        fs::create_dir_all(home.join("other")).unwrap();
        let paths = Paths::under(dir.path().join("legion")).with_home(&home);
        fs::create_dir_all(paths.config_dir()).unwrap();
        Node {
            host: Host::new(paths, "tower"),
            dir,
        }
    }

    fn home(&self) -> PathBuf {
        self.dir.path().join("home")
    }

    fn config(&self) -> PathBuf {
        self.dir.path().join("legion").join("config")
    }

    fn policy(&self, text: &str) {
        fs::write(self.config().join("policy.toml"), text).unwrap();
    }

    /// Runs one operation: the header, then `body` on its input.
    /// Runs one operation: the header, which speaks the current protocol unless it says
    /// otherwise, then `body` on its input.
    fn run(&self, pinned: Option<&str>, header: &Value, body: &[u8]) -> Ran {
        let mut header = header.clone();
        if header.get("protocol").is_none() {
            header["protocol"] = json!(STREAM_PROTOCOL);
        }
        let mut input = header.to_string().into_bytes();
        input.push(b'\n');
        input.extend_from_slice(body);
        let mut output = Vec::new();
        let result = serve(&self.host, pinned, Cursor::new(input), &mut output);
        Ran { result, output }
    }

    fn audit(&self) -> Vec<Value> {
        fs::read_to_string(self.dir.path().join("legion/state/audit.jsonl"))
            .unwrap_or_default()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    fn last_audit(&self) -> Value {
        self.audit().pop().expect("an audit line")
    }

    fn p(&self, rel: &str) -> String {
        self.home().join(rel).to_string_lossy().into_owned()
    }
}

struct Ran {
    result: Result<(), legion_proto::ErrorBody>,
    output: Vec<u8>,
}

impl Ran {
    /// The output's first line, as JSON, and what follows it.
    /// What follows the line every stream opens with, which says the protocol it speaks.
    fn replies(&self) -> &[u8] {
        after_protocol(&self.output)
    }

    /// The first reply, as JSON, and what follows it.
    fn first(&self) -> (Value, &[u8]) {
        let replies = self.replies();
        let end = replies.iter().position(|b| *b == b'\n').expect("a line");
        (
            serde_json::from_slice(&replies[..end]).unwrap(),
            &replies[end + 1..],
        )
    }

    fn lines(&self) -> Vec<Value> {
        self.replies()
            .lines()
            .map(|line| serde_json::from_str(&line.unwrap()).unwrap())
            .collect()
    }

    fn ok(&self) -> &Self {
        assert!(
            self.result.is_ok(),
            "{:?}\n{}",
            self.result,
            String::from_utf8_lossy(&self.output)
        );
        self
    }

    fn refused(&self, code: ErrorCode) -> Value {
        let error = self.result.as_ref().unwrap_err();
        assert_eq!(error.code, code, "{error:?}");
        assert_eq!(error.machine.as_deref(), Some("tower"));
        let (line, rest) = self.first();
        assert!(rest.is_empty(), "nothing follows a refusal");
        assert_eq!(line["error"]["code"], json!(code.as_str()), "{line}");
        line["error"].clone()
    }
}

/// A stream's output after its first line, which must say the protocol it speaks.
fn after_protocol(output: &[u8]) -> &[u8] {
    let end = output.iter().position(|b| *b == b'\n').expect("a line");
    let opening: Value = serde_json::from_slice(&output[..end]).unwrap();
    assert_eq!(opening["protocol"], json!(STREAM_PROTOCOL), "{opening}");
    assert_eq!(
        opening["capabilities"],
        json!(legion_net::stream::STREAM_CAPABILITIES),
        "{opening}"
    );
    &output[end + 1..]
}

fn hash(bytes: &[u8]) -> String {
    legion_xfer::hash::hash_bytes(bytes).to_string()
}

fn tar_of(root: &Path, names: &[&str]) -> Vec<u8> {
    let mut archive = Vec::new();
    legion_xfer::pack(root, names, &mut archive).unwrap();
    archive
}

const CONFINED: &str = r#"
[principals."agent:x"]
role = "worker"

[principals.reader]
role = "observer"

[principals.writer]
role = "keeper"

[roles.worker]
allow = ["effect:read", "effect:write", "effect:destroy"]
paths = ["~/projects"]

[roles.observer]
allow = ["effect:read"]

[roles.keeper]
allow = ["effect:read", "effect:write"]
"#;

#[test]
fn stat_describes_files_directories_and_missing_paths() {
    let node = Node::new();
    fs::write(node.home().join("projects/a.txt"), b"hello").unwrap();
    fs::write(node.home().join("projects/b.txt.legion-part"), b"he").unwrap();

    let ran = node.run(
        None,
        &json!({"op": "stat", "path": "~/projects/a.txt", "checksum": true}),
        b"",
    );
    let (line, _) = ran.ok().first();
    assert_eq!(line["kind"], "file");
    assert_eq!(line["size"], 5);
    assert_eq!(line["hash"], json!(hash(b"hello")));

    let ran = node.run(
        None,
        &json!({"op": "stat", "path": "~/projects/a.txt", "checksum": true, "if_size": 4}),
        b"",
    );
    assert!(
        ran.ok().first().0.get("hash").is_none(),
        "only hashed when the size matches"
    );

    let ran = node.run(
        None,
        &json!({"op": "stat", "path": "~/projects/b.txt"}),
        b"",
    );
    let (line, _) = ran.ok().first();
    assert_eq!(line["kind"], "missing");
    assert_eq!(line["partial"], 2);

    let ran = node.run(
        None,
        &json!({"op": "stat", "path": node.p("projects")}),
        b"",
    );
    assert_eq!(ran.ok().first().0["kind"], "dir");

    let audit = node.last_audit();
    assert_eq!(audit["door"], "stream");
    assert_eq!(audit["tool"], "stream.stat");
    assert_eq!(audit["principal"], "owner");
    assert_eq!(audit["target"], "tower");
    assert_eq!(audit["outcome"], "ok");
}

#[test]
fn stat_tells_whether_a_directory_ignores_letter_case_without_writing() {
    let node = Node::new();
    let dir = node.home().join("projects");
    fs::write(dir.join("Probe.txt"), b"").unwrap();
    let ignores_case = dir.join("probe.txt").exists();
    let before: Vec<_> = fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();

    for path in ["~/projects", "~/projects/not/yet"] {
        let ran = node.run(None, &json!({"op": "stat", "path": path}), b"");
        let (line, _) = ran.ok().first();
        assert_eq!(
            line["case_insensitive"],
            json!(ignores_case),
            "{path}: {line}"
        );
    }
    let after: Vec<_> = fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    assert_eq!(before, after);

    // An empty directory says nothing about its own file system: one mounted there may differ
    // from the one its name is in. It is left unsaid, which a transfer takes as ignoring case.
    let ran = node.run(None, &json!({"op": "stat", "path": "~/other"}), b"");
    let (line, _) = ran.ok().first();
    assert_eq!(line["kind"], "dir");
    assert!(line.get("case_insensitive").is_none(), "{line}");
}

#[cfg(unix)]
#[test]
fn the_line_that_ends_a_listing_stays_short_however_much_could_not_be_read() {
    use std::os::unix::fs::PermissionsExt;
    let node = Node::new();
    let root = node.home().join("projects/locked");
    fs::create_dir_all(&root).unwrap();
    let probe = root.join("probe");
    fs::create_dir(&probe).unwrap();
    fs::set_permissions(&probe, fs::Permissions::from_mode(0o000)).unwrap();
    let reads_anything = fs::read_dir(&probe).is_ok();
    fs::set_permissions(&probe, fs::Permissions::from_mode(0o755)).unwrap();
    fs::remove_dir(&probe).unwrap();
    if reads_anything {
        return;
    }
    let names: Vec<_> = (0..3000)
        .map(|at| root.join(format!("an-unreadable-directory-with-a-long-name-{at:05}")))
        .collect();
    for dir in &names {
        fs::create_dir(dir).unwrap();
        fs::set_permissions(dir, fs::Permissions::from_mode(0o000)).unwrap();
    }
    let ran = node.run(
        None,
        &json!({"op": "manifest", "root": "~/projects/locked"}),
        b"",
    );
    for dir in &names {
        fs::set_permissions(dir, fs::Permissions::from_mode(0o755)).unwrap();
    }
    let done = ran.ok().lines().pop().unwrap();
    assert_eq!(done["skipped_total"], 3000);
    let kept = done["skipped"].as_array().unwrap().len();
    assert!(kept > 0 && kept < 3000, "{kept}");
    assert!(done.to_string().len() < 512 * 1024);
}

#[test]
fn get_sends_the_size_then_the_bytes_from_the_offset() {
    let node = Node::new();
    let data: Vec<u8> = (0..200_000u32).map(|i| (i % 251) as u8).collect();
    let file = node.home().join("projects/big.bin");
    fs::write(&file, &data).unwrap();
    let expected_mtime =
        legion_xfer::Timestamp::from_system_time(fs::metadata(&file).unwrap().modified().unwrap());

    // Legacy caller omitting `length` still receives offset..EOF and gets full metadata.
    let ran = node.run(
        None,
        &json!({"op": "get", "path": "~/projects/big.bin", "offset": 1000}),
        b"",
    );
    let (line, rest) = ran.ok().first();
    assert_eq!(line["size"], json!(200_000));
    assert_eq!(line["offset"], json!(1000));
    assert_eq!(line["length"], json!(199_000));
    assert_eq!(
        line["mtime"],
        json!({"secs": expected_mtime.secs, "nanos": expected_mtime.nanos})
    );
    #[cfg(unix)]
    assert!(line["file_id"].as_u64().is_some(), "{line}");
    assert_eq!(rest, &data[1000..]);
    assert_eq!(node.last_audit()["tool"], "stream.get");

    // Bounded slice with explicit `length`.
    let ran = node.run(
        None,
        &json!({"op": "get", "path": "~/projects/big.bin", "offset": 4096, "length": 1234}),
        b"",
    );
    let (line, rest) = ran.ok().first();
    assert_eq!(line["size"], json!(200_000));
    assert_eq!(line["offset"], json!(4096));
    assert_eq!(line["length"], json!(1234));
    assert_eq!(
        line["mtime"],
        json!({"secs": expected_mtime.secs, "nanos": expected_mtime.nanos})
    );
    assert_eq!(rest, &data[4096..4096 + 1234]);

    // `length` extending past EOF clamps to the remaining bytes.
    let ran = node.run(
        None,
        &json!({"op": "get", "path": "~/projects/big.bin", "offset": 199_900, "length": 500}),
        b"",
    );
    let (line, rest) = ran.ok().first();
    assert_eq!(line["length"], json!(100));
    assert_eq!(rest, &data[199_900..]);

    // `length: 0` returns metadata and zero bytes (useful for HEAD / range probes).
    let ran = node.run(
        None,
        &json!({"op": "get", "path": "~/projects/big.bin", "offset": 0, "length": 0}),
        b"",
    );
    let (line, rest) = ran.ok().first();
    assert_eq!(line["size"], json!(200_000));
    assert_eq!(line["offset"], json!(0));
    assert_eq!(line["length"], json!(0));
    assert!(rest.is_empty());

    // `offset == size` succeeds with `length: 0`.
    let ran = node.run(
        None,
        &json!({"op": "get", "path": "~/projects/big.bin", "offset": 200_000, "length": 10}),
        b"",
    );
    let (line, rest) = ran.ok().first();
    assert_eq!(line["length"], json!(0));
    assert!(rest.is_empty());

    // `offset > size` fails with Conflict and carries `requested` / `actual`.
    let ran = node.run(
        None,
        &json!({"op": "get", "path": "~/projects/big.bin", "offset": 300_000}),
        b"",
    );
    let err = ran.refused(ErrorCode::Conflict);
    assert_eq!(err["details"]["requested"], json!(300_000));
    assert_eq!(err["details"]["actual"], json!(200_000));

    let ran = node.run(None, &json!({"op": "get", "path": "~/projects/none"}), b"");
    ran.refused(ErrorCode::NotFound);
    assert_eq!(node.last_audit()["outcome"], "not_found");
}

#[test]
fn put_says_where_to_start_verifies_and_renames() {
    let node = Node::new();
    let data = b"0123456789abcdefghij".to_vec();
    let header = |offset: u64| json!({"op": "put", "path": "~/projects/new/dir/f.bin", "size": data.len(), "hash": hash(&data), "offset": offset, "mode": 0o640});

    // Cut after 7 bytes: the partial file stays for the next attempt.
    let ran = node.run(None, &header(0), &data[..7]);
    assert!(ran.result.is_err());
    let lines = ran.lines();
    assert_eq!(lines[0], json!({"offset": 0}));
    assert_eq!(lines[1]["error"]["code"], "failed");
    let target = node.home().join("projects/new/dir/f.bin");
    assert!(!target.exists());
    assert_eq!(
        fs::read(node.home().join("projects/new/dir/f.bin.legion-part")).unwrap(),
        &data[..7]
    );

    // The sender's offset is a hint; what is on disk decides.
    let ran = node.run(None, &header(0), &data[7..]);
    let lines = ran.ok().lines();
    assert_eq!(lines[0], json!({"offset": 7}));
    assert_eq!(lines[1]["ok"], json!(true), "{}", lines[1]);
    assert_eq!(lines[1]["size"], json!(data.len()), "{}", lines[1]);
    assert_eq!(lines[1]["hash"], json!(hash(&data)), "{}", lines[1]);
    assert!(lines[1].get("file_id").is_some(), "{}", lines[1]);
    assert_eq!(fs::read(&target).unwrap(), data);
    let audit = node.last_audit();
    assert_eq!(
        (
            audit["door"].as_str(),
            audit["tool"].as_str(),
            audit["outcome"].as_str()
        ),
        (Some("stream"), Some("stream.put"), Some("ok"))
    );
    assert_eq!(audit["args"]["path"], "~/projects/new/dir/f.bin");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(&target).unwrap().permissions().mode() & 0o777,
            0o640
        );
    }

    // A wrong hash leaves the old file alone and drops the partial file.
    let ran = node.run(
        None,
        &json!({"op": "put", "path": "~/projects/new/dir/f.bin", "size": 3, "hash": hash(b"xyz"), "offset": 0}),
        b"abc",
    );
    let lines = ran.lines();
    assert_eq!(lines[0], json!({"offset": 0}));
    assert_eq!(lines[1]["error"]["code"], "conflict");
    assert_eq!(fs::read(&target).unwrap(), data);
    assert!(
        !node
            .home()
            .join("projects/new/dir/f.bin.legion-part")
            .exists()
    );

    // A directory is never replaced by a file.
    let ran = node.run(
        None,
        &json!({"op": "put", "path": "~/projects/new", "size": 3, "hash": hash(b"abc")}),
        b"abc",
    );
    ran.refused(ErrorCode::Conflict);
}

#[test]
fn manifest_streams_one_line_per_entry_and_ends_with_a_count() {
    let node = Node::new();
    let root = node.home().join("projects/app");
    fs::create_dir_all(root.join("src/empty")).unwrap();
    fs::write(root.join("src/main.rs"), b"fn main() {}").unwrap();
    fs::write(root.join("build.log"), b"x").unwrap();

    let ran = node.run(
        None,
        &json!({"op": "manifest", "root": "~/projects/app", "exclude": ["*.log"]}),
        b"",
    );
    let lines = ran.ok().lines();
    let paths: Vec<&str> = lines[..lines.len() - 1]
        .iter()
        .map(|line| line["path"].as_str().unwrap())
        .collect();
    assert_eq!(paths, ["src", "src/empty", "src/main.rs"]);
    assert_eq!(lines[2]["size"], 12);
    assert!(lines[2]["mtime"]["secs"].is_i64());
    assert_eq!(
        lines.last().unwrap(),
        &json!({"done": true, "entries": 3, "skipped": []})
    );
    assert_eq!(node.last_audit()["tool"], "stream.manifest");

    let ran = node.run(
        None,
        &json!({"op": "manifest", "root": "~/projects/app", "exclude": ["[bad"]}),
        b"",
    );
    ran.refused(ErrorCode::InvalidArgs);
}

#[test]
fn manifest_of_a_home_leaves_out_legions_own_files_for_a_role_without_admin() {
    let node = Node::new();
    // The configuration lives under the home this time, as it does by default.
    let paths = Paths::under(node.home().join(".legion")).with_home(node.home());
    fs::create_dir_all(paths.config_dir()).unwrap();
    fs::write(
        paths.config_dir().join("policy.toml"),
        CONFINED.replace("paths = [\"~/projects\"]\n", ""),
    )
    .unwrap();
    fs::write(node.home().join("notes.txt"), b"n").unwrap();
    let host = Host::new(paths, "tower");
    let listing = |pinned: Option<&str>| {
        let mut output = Vec::new();
        serve(
            &host,
            pinned,
            Cursor::new(b"{\"op\":\"manifest\",\"root\":\"~\",\"protocol\":1}\n".to_vec()),
            &mut output,
        )
        .unwrap();
        after_protocol(&output)
            .lines()
            .map(|line| serde_json::from_str::<Value>(&line.unwrap()).unwrap())
            .collect::<Vec<_>>()
    };
    let owner = listing(None);
    assert!(
        owner
            .iter()
            .any(|line| line["path"] == ".legion/config/policy.toml")
    );
    let agent = listing(Some("agent:x"));
    assert!(
        agent.iter().all(|line| !line["path"]
            .as_str()
            .is_some_and(|p| p.starts_with(".legion/"))),
        "{agent:?}"
    );
    assert!(agent.iter().any(|line| line["path"] == "notes.txt"));
    let done = agent.last().unwrap();
    assert!(
        done["skipped"].to_string().contains("effect:admin"),
        "{done}"
    );
}

#[test]
fn pack_names_what_it_leaves_out_then_unpack_writes_the_tree() {
    let node = Node::new();
    let src = node.home().join("projects/src");
    fs::create_dir_all(src.join("nested/empty")).unwrap();
    fs::write(src.join("nested/a.txt"), b"alpha").unwrap();
    fs::write(src.join("odd name #1.txt"), b"odd").unwrap();

    let ran = node.run(
        None,
        &json!({"op": "pack", "root": "~/projects/src", "names": ["nested", "nested/a.txt", "nested/empty", "odd name #1.txt", "gone.txt"]}),
        b"",
    );
    let (line, archive) = ran.ok().first();
    assert_eq!(line["ready"], true);
    assert_eq!(line["skipped"][0]["path"], "gone.txt");
    let archive = archive.to_vec();
    assert_eq!(node.last_audit()["tool"], "stream.pack");

    let ran = node.run(
        None,
        &json!({"op": "unpack", "root": "~/projects/dst", "entries": [
            {"path": "nested", "kind": "dir"},
            {"path": "nested/a.txt", "kind": "file", "size": 5},
            {"path": "nested/empty", "kind": "dir"},
            {"path": "odd name #1.txt", "kind": "file", "size": 3},
        ]}),
        &archive,
    );
    let lines = ran.ok().lines();
    assert_eq!(lines[0], json!({"ready": true}));
    assert_eq!(lines[1]["ok"], true);
    assert_eq!(lines[1]["files"], 2);
    assert_eq!(lines[1]["dirs"], 2);
    assert_eq!(lines[1]["bytes"], 8);
    let dst = node.home().join("projects/dst");
    assert_eq!(fs::read(dst.join("nested/a.txt")).unwrap(), b"alpha");
    assert_eq!(fs::read(dst.join("odd name #1.txt")).unwrap(), b"odd");
    assert!(dst.join("nested/empty").is_dir());
    assert_eq!(node.last_audit()["tool"], "stream.unpack");

    let ran = node.run(
        None,
        &json!({"op": "pack", "root": "~/projects/src/nested/a.txt", "names": []}),
        b"",
    );
    ran.refused(ErrorCode::Conflict);
}

#[test]
fn unpack_checks_where_each_entry_lands_and_leaves_legions_own_files_alone() {
    let node = Node::new();
    // The configuration lives under the home, as it does by default, so a tree unpacked into the
    // home could carry a policy of its own.
    let paths = Paths::under(node.home().join(".legion")).with_home(node.home());
    fs::create_dir_all(paths.config_dir()).unwrap();
    fs::write(paths.config_dir().join("policy.toml"), CONFINED).unwrap();
    let host = Host::new(paths.clone(), "tower");
    let staging = tempfile::tempdir().unwrap();
    fs::create_dir_all(staging.path().join(".legion/config")).unwrap();
    fs::write(
        staging.path().join(".legion/config/policy.toml"),
        b"default = \"writer\"\n",
    )
    .unwrap();
    fs::write(staging.path().join("fine.txt"), b"fine").unwrap();
    let archive = tar_of(staging.path(), &[".legion/config/policy.toml", "fine.txt"]);

    let mut input =
        b"{\"op\":\"unpack\",\"root\":\"~\",\"entries\":[{\"path\":\".legion/config/policy.toml\",\"kind\":\"file\",\"size\":19},{\"path\":\"fine.txt\",\"kind\":\"file\",\"size\":4}],\"protocol\":1}\n"
            .to_vec();
    input.extend_from_slice(&archive);
    let mut output = Vec::new();
    serve(&host, Some("writer"), Cursor::new(input), &mut output).unwrap();
    let done: Value = serde_json::from_slice(
        after_protocol(&output)
            .split(|b| *b == b'\n')
            .nth(1)
            .unwrap(),
    )
    .unwrap();
    assert_eq!(done["files"], 1, "{done}");
    let skipped = done["skipped"].to_string();
    assert!(
        skipped.contains(".legion/config/policy.toml") && skipped.contains("configuration"),
        "{skipped}"
    );
    assert_eq!(
        fs::read_to_string(paths.config_dir().join("policy.toml")).unwrap(),
        CONFINED
    );
    assert_eq!(fs::read(node.home().join("fine.txt")).unwrap(), b"fine");
}

#[test]
fn delete_removes_children_before_parents_and_never_follows_links() {
    let node = Node::new();
    let root = node.home().join("projects/t");
    fs::create_dir_all(root.join("d/e")).unwrap();
    fs::write(root.join("d/e/f.txt"), b"f").unwrap();
    fs::write(root.join("keep.txt"), b"k").unwrap();
    fs::create_dir_all(root.join("full")).unwrap();
    fs::write(root.join("full/left.txt"), b"l").unwrap();
    fs::write(node.home().join("other/precious.txt"), b"p").unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(node.home().join("other"), root.join("link")).unwrap();

    let mut names = vec![
        "d",
        "d/e",
        "d/e/f.txt",
        "full",
        "../other/precious.txt",
        "missing",
    ];
    if cfg!(unix) {
        names.push("link");
    }
    let ran = node.run(
        None,
        &json!({"op": "delete", "root": "~/projects/t", "names": names}),
        b"",
    );
    let (line, _) = ran.ok().first();
    assert_eq!(line["removed"], if cfg!(unix) { 4 } else { 3 }, "{line}");
    let skipped = line["skipped"].as_array().unwrap();
    let skipped_paths: Vec<&str> = skipped
        .iter()
        .map(|s| s["path"].as_str().unwrap())
        .collect();
    assert_eq!(skipped_paths, ["../other/precious.txt", "full"], "{line}");
    assert!(!root.join("d").exists());
    assert!(root.join("keep.txt").exists());
    assert!(root.join("full/left.txt").exists());
    assert!(node.home().join("other/precious.txt").exists());
    assert!(node.home().join("other").is_dir());
    assert_eq!(node.last_audit()["tool"], "stream.delete");
}

#[cfg(unix)]
#[test]
fn pack_and_delete_never_go_through_a_link_inside_a_name() {
    let node = Node::new();
    let tree = node.home().join("projects/tree");
    fs::create_dir_all(&tree).unwrap();
    fs::create_dir_all(node.home().join("precious")).unwrap();
    fs::write(node.home().join("precious/secret.txt"), b"secret").unwrap();
    fs::write(tree.join("plain.txt"), b"plain").unwrap();
    std::os::unix::fs::symlink(node.home().join("precious"), tree.join("link")).unwrap();

    let ran = node.run(
        None,
        &json!({"op": "pack", "root": "~/projects/tree", "names": ["link/secret.txt", "plain.txt", "link"]}),
        b"",
    );
    let (ready, archive) = ran.ok().first();
    assert_eq!(ready["skipped"][0]["path"], "link/secret.txt", "{ready}");
    assert!(
        ready["skipped"][0]["reason"]
            .as_str()
            .unwrap()
            .contains("symbolic link"),
        "{ready}"
    );
    // The link itself is packed as a link; what it leads to is not read.
    let unpacked = tempfile::tempdir().unwrap();
    let report = legion_xfer::unpack(unpacked.path(), archive).unwrap();
    assert_eq!(report.files, ["plain.txt"]);
    assert_eq!(report.symlinks, ["link"]);
    assert!(!String::from_utf8_lossy(archive).contains("secret"));

    let ran = node.run(
        None,
        &json!({"op": "delete", "root": "~/projects/tree", "names": ["link/secret.txt"]}),
        b"",
    );
    let (done, _) = ran.ok().first();
    assert_eq!(done["removed"], 0, "{done}");
    assert_eq!(done["skipped"][0]["path"], "link/secret.txt", "{done}");
    assert!(node.home().join("precious/secret.txt").exists());

    // The last part, when it is a link, is removed as a link.
    let ran = node.run(
        None,
        &json!({"op": "delete", "root": "~/projects/tree", "names": ["link"]}),
        b"",
    );
    assert_eq!(ran.ok().first().0["removed"], 1);
    assert!(fs::symlink_metadata(tree.join("link")).is_err());
    assert!(node.home().join("precious/secret.txt").exists());
}

#[test]
fn a_partial_file_beside_a_target_is_checked_like_the_target() {
    let node = Node::new();
    node.policy(
        "[principals.boxed]\nrole = \"boxed\"\n[roles.boxed]\nallow = [\"effect:read\", \"effect:write\"]\npaths = [\"~/sandbox\"]\n",
    );
    // The role's root does not exist yet, so its partial file would sit outside it, in the home.
    let outside = node.home().join("sandbox.legion-part");
    fs::write(&outside, b"not yours").unwrap();

    let ran = node.run(
        Some("boxed"),
        &json!({"op": "stat", "path": "~/sandbox"}),
        b"",
    );
    let (line, _) = ran.ok().first();
    assert_eq!(line["kind"], "missing");
    assert!(line.get("partial").is_none(), "{line}");

    let error = node
        .run(
            Some("boxed"),
            &json!({"op": "put", "path": "~/sandbox", "size": 3, "hash": hash(b"abc")}),
            b"abc",
        )
        .refused(ErrorCode::Denied);
    assert!(
        error["message"].as_str().unwrap().contains("legion-part"),
        "{error}"
    );
    assert_eq!(fs::read(&outside).unwrap(), b"not yours");
    assert!(!node.home().join("sandbox").exists());

    // Inside the root, a partial file with a matching state-dir metadata sidecar is resumed.
    fs::create_dir_all(node.home().join("sandbox")).unwrap();
    let target_f = node.home().join("sandbox/f.bin");
    fs::write(node.home().join("sandbox/f.bin.legion-part"), b"ab").unwrap();
    legion_xfer::write_partial_meta_in_state(
        &node.dir.path().join("legion/state"),
        &target_f,
        3,
        legion_xfer::hash::hash_bytes(b"abc"),
    )
    .unwrap();
    let ran = node.run(
        Some("boxed"),
        &json!({"op": "stat", "path": "~/sandbox/f.bin"}),
        b"",
    );
    assert_eq!(ran.ok().first().0["partial"], 2);
    let ran = node.run(
        Some("boxed"),
        &json!({"op": "put", "path": "~/sandbox/f.bin", "size": 3, "hash": hash(b"abc")}),
        b"c",
    );
    assert_eq!(ran.ok().lines()[0], json!({"offset": 2}));
    assert_eq!(fs::read(node.home().join("sandbox/f.bin")).unwrap(), b"abc");
}

#[test]
fn each_operation_needs_what_the_file_tool_it_amounts_to_needs() {
    let node = Node::new();
    node.policy(CONFINED);
    fs::write(node.home().join("projects/a.txt"), b"a").unwrap();
    fs::write(node.home().join("other/b.txt"), b"b").unwrap();
    let put = |path: &str| json!({"op": "put", "path": path, "size": 1, "hash": hash(b"a")});

    // An observer reads but may not write or remove.
    node.run(
        Some("reader"),
        &json!({"op": "get", "path": "~/projects/a.txt"}),
        b"",
    )
    .ok();
    let error = node
        .run(Some("reader"), &put("~/projects/c.txt"), b"a")
        .refused(ErrorCode::Denied);
    assert!(
        error["message"].as_str().unwrap().contains("fs.write"),
        "{error}"
    );
    node.run(
        Some("reader"),
        &json!({"op": "unpack", "root": "~/projects/x", "entries": []}),
        b"",
    )
    .refused(ErrorCode::Denied);
    node.run(
        Some("reader"),
        &json!({"op": "delete", "root": "~/projects", "names": ["a.txt"]}),
        b"",
    )
    .refused(ErrorCode::Denied);
    let audit = node.last_audit();
    assert_eq!(audit["principal"], "reader");
    assert_eq!(audit["tool"], "stream.delete");
    assert_eq!(audit["outcome"], "denied");

    // A role that writes but does not destroy may not delete.
    let error = node
        .run(
            Some("writer"),
            &json!({"op": "delete", "root": "~/projects", "names": ["a.txt"]}),
            b"",
        )
        .refused(ErrorCode::Denied);
    assert!(
        error["message"].as_str().unwrap().contains("fs.remove"),
        "{error}"
    );
    assert!(node.home().join("projects/a.txt").exists());

    // A confined role stays in its paths, whichever operation it uses.
    node.run(
        Some("agent:x"),
        &json!({"op": "get", "path": "~/other/b.txt"}),
        b"",
    )
    .refused(ErrorCode::Denied);
    node.run(Some("agent:x"), &put("~/other/c.txt"), b"a")
        .refused(ErrorCode::Denied);
    node.run(
        Some("agent:x"),
        &json!({"op": "manifest", "root": "~/other"}),
        b"",
    )
    .refused(ErrorCode::Denied);
    node.run(
        Some("agent:x"),
        &json!({"op": "pack", "root": "~/other", "names": ["b.txt"]}),
        b"",
    )
    .refused(ErrorCode::Denied);
    assert!(!node.home().join("other/c.txt").exists());

    // Legion's own configuration belongs to admin, whatever a role's paths say.
    let config = node
        .config()
        .join("evil.toml")
        .to_string_lossy()
        .into_owned();
    let error = node
        .run(Some("writer"), &put(&config), b"a")
        .refused(ErrorCode::Denied);
    assert!(
        error["message"].as_str().unwrap().contains("configuration"),
        "{error}"
    );
    node.run(
        Some("writer"),
        &json!({"op": "unpack", "root": node.config(), "entries": []}),
        b"",
    )
    .refused(ErrorCode::Denied);

    // A principal the policy does not name may do nothing.
    node.run(
        Some("stranger"),
        &json!({"op": "stat", "path": "~/projects/a.txt"}),
        b"",
    )
    .refused(ErrorCode::Denied);
}

#[test]
fn a_pinned_principal_is_kept_and_only_a_delegating_role_acts_as_another() {
    let node = Node::new();
    node.policy(CONFINED);
    fs::write(node.home().join("other/b.txt"), b"b").unwrap();
    let get =
        |as_principal: &str| json!({"op": "get", "path": "~/other/b.txt", "as": as_principal});

    // Pinned to a confined agent, asking to be the owner changes nothing.
    node.run(Some("agent:x"), &get("owner"), b"")
        .refused(ErrorCode::Denied);
    assert_eq!(node.last_audit()["principal"], "agent:x");

    // The owner, who delegates, may act as the agent, and is then held to its paths.
    node.run(None, &get("agent:x"), b"")
        .refused(ErrorCode::Denied);
    assert_eq!(node.last_audit()["principal"], "agent:x");
    node.run(None, &get("owner"), b"").ok();
    let audit = node.last_audit();
    assert_eq!(audit["principal"], "owner");
    assert!(audit["args"].get("as").is_none(), "{audit}");

    // Without a pinned principal the caller is the policy's default, which may not widen itself.
    node.policy(&format!("default = \"reader\"\n{CONFINED}"));
    let error = node
        .run(None, &get("owner"), b"")
        .refused(ErrorCode::Denied);
    assert!(
        error["message"]
            .as_str()
            .unwrap()
            .contains("may not act as owner"),
        "{error}"
    );
    assert_eq!(node.last_audit()["principal"], "reader");
}

#[test]
fn a_header_that_cannot_be_read_is_refused_on_the_first_line() {
    let node = Node::new();
    for input in [
        &b""[..],
        b"not json\n",
        b"{\"op\":\"stat\",\"protocol\":1}\n",
        b"[1]\n",
    ] {
        let mut output = Vec::new();
        let error = serve(&node.host, None, Cursor::new(input.to_vec()), &mut output).unwrap_err();
        assert_eq!(error.code, ErrorCode::InvalidArgs, "{input:?}");
        // Even a header that cannot be read is answered after the line saying the protocol.
        let line: Value = serde_json::from_slice(
            after_protocol(&output)
                .split(|b| *b == b'\n')
                .next()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(line["error"]["code"], "invalid_args");
    }
    let relative = node.run(None, &json!({"op": "stat", "path": "relative/path"}), b"");
    relative.refused(ErrorCode::InvalidArgs);
}

#[test]
fn a_header_in_another_protocol_or_with_an_unknown_operation_is_unsupported() {
    let node = Node::new();
    for (header, names) in [
        (
            json!({"op": "stat", "path": "~", "protocol": null}),
            "protocol",
        ),
        (
            json!({"op": "stat", "path": "~", "protocol": 99}),
            "protocol 99",
        ),
        (json!({"op": "format", "path": "/"}), "format"),
        (json!({"path": "/"}), "operation"),
    ] {
        let error = node.run(None, &header, b"").refused(ErrorCode::Unsupported);
        assert!(
            error["message"].as_str().unwrap().contains(names),
            "{error}"
        );
        assert_eq!(
            error["details"]["protocol"],
            json!([STREAM_PROTOCOL]),
            "{error}"
        );
        assert!(
            error["details"]["ops"]
                .as_array()
                .unwrap()
                .contains(&json!("unpack")),
            "{error}"
        );
        assert!(
            error["hint"].as_str().unwrap().contains("Update legion"),
            "{error}"
        );
    }
}

/// Two archives run together, so every entry in them comes twice.
fn twice(archive: &[u8]) -> Vec<u8> {
    let mut first = archive.to_vec();
    while first.len() >= 512 && first[first.len() - 512..].iter().all(|b| *b == 0) {
        first.truncate(first.len() - 512);
    }
    first.extend_from_slice(archive);
    first
}

#[test]
fn unpack_takes_each_planned_entry_once_as_planned_and_nothing_else() {
    let staging = tempfile::tempdir().unwrap();
    let stage = staging.path();
    fs::create_dir_all(stage.join("dir")).unwrap();
    fs::write(stage.join("dir/wanted.txt"), b"wanted").unwrap();
    fs::write(stage.join(".env"), b"SECRET").unwrap();
    let wanted = || json!({"path": "dir/wanted.txt", "kind": "file", "size": 6});
    let dir = || json!({"path": "dir", "kind": "dir"});
    let tar = |names: &[&str]| tar_of(stage, names);
    // Windows cannot make the links the last two cases need.
    #[cfg_attr(not(unix), allow(unused_mut))]
    let mut cases = vec![
        // Something never planned stops the unpack at once.
        (
            "unplanned",
            json!([dir(), wanted()]),
            tar(&["dir", "dir/wanted.txt", ".env"]),
            ".env",
            "not among",
        ),
        // A file of another size than listed.
        (
            "size",
            json!([dir(), {"path": "dir/wanted.txt", "kind": "file", "size": 5}]),
            tar(&["dir", "dir/wanted.txt"]),
            "dir/wanted.txt",
            "planned with 5 bytes",
        ),
        // A directory where a file was planned.
        (
            "kind",
            json!([{"path": "dir", "kind": "file", "size": 1}]),
            tar(&["dir"]),
            "dir",
            "planned as a file",
        ),
        // The same entry twice.
        (
            "twice",
            json!([dir(), wanted()]),
            twice(&tar(&["dir", "dir/wanted.txt"])),
            "dir",
            "second time",
        ),
        // A planned entry that never comes.
        (
            "missing",
            json!([dir(), wanted(), {"path": "gone.txt", "kind": "file", "size": 1}]),
            tar(&["dir", "dir/wanted.txt"]),
            "gone.txt",
            "never came",
        ),
        // Content other than the hash the source listed.
        (
            "hash",
            json!([dir(), {"path": "dir/wanted.txt", "kind": "file", "size": 6, "hash": hash(b"WANTED")}]),
            tar(&["dir", "dir/wanted.txt"]),
            "dir/wanted.txt",
            "hash differs",
        ),
    ];
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink("/etc/passwd", stage.join("link")).unwrap();
        std::os::unix::fs::symlink("elsewhere", stage.join("other")).unwrap();
        // A link where a file was planned.
        cases.push((
            "symlink",
            json!([{"path": "link", "kind": "file", "size": 6}]),
            tar(&["link"]),
            "link",
            "planned as a file and came as a symbolic link",
        ));
        // A link to somewhere other than listed.
        cases.push((
            "target",
            json!([{"path": "other", "kind": "symlink", "target": "planned"}]),
            tar(&["other"]),
            "other",
            "link to planned",
        ));
    }
    for (case, entries, archive, path, says) in cases {
        let node = Node::new();
        let dst = node.home().join("projects/dst");
        fs::create_dir_all(dst.join("dir")).unwrap();
        fs::write(dst.join("dir/wanted.txt"), b"before").unwrap();
        let ran = node.run(
            None,
            &json!({"op": "unpack", "root": "~/projects/dst", "entries": entries}),
            &archive,
        );
        let error = ran.result.as_ref().unwrap_err();
        assert_eq!(error.code, ErrorCode::Failed, "{case}: {error:?}");
        let violation = &error.details.as_ref().unwrap()["violation"];
        assert_eq!(violation["path"], path, "{case}: {error:?}");
        assert!(
            violation["reason"].as_str().unwrap().contains(says),
            "{case}: {error:?}"
        );
        assert!(!dst.join(".env").exists(), "{case}");
        assert!(fs::symlink_metadata(dst.join("link")).is_err(), "{case}");
        // A file refused for its size or hash never replaced what was there.
        if matches!(case, "size" | "hash") {
            assert_eq!(
                fs::read(dst.join("dir/wanted.txt")).unwrap(),
                b"before",
                "{case}"
            );
        }
        let temporaries: Vec<_> = fs::read_dir(dst.join("dir"))
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .filter(|name| name.to_string_lossy().ends_with(".tmp"))
            .collect();
        assert!(temporaries.is_empty(), "{case}: {temporaries:?}");
    }
}

#[test]
fn a_refused_unpack_reads_nothing_of_what_follows() {
    use std::sync::{Arc, Mutex};
    struct SharedCursor(Arc<Mutex<Cursor<Vec<u8>>>>);
    impl Read for SharedCursor {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().read(buf)
        }
    }

    let node = Node::new();
    node.policy(CONFINED);
    let mut input =
        b"{\"op\":\"unpack\",\"root\":\"~/projects/x\",\"entries\":[],\"protocol\":1}\n".to_vec();
    input.extend(std::iter::repeat_n(0u8, 1 << 20));
    let shared = Arc::new(Mutex::new(Cursor::new(input)));
    let mut output = Vec::new();
    let error = serve(
        &node.host,
        Some("reader"),
        SharedCursor(Arc::clone(&shared)),
        &mut output,
    )
    .unwrap_err();
    assert_eq!(error.code, ErrorCode::Denied);
    // It answered and returned instead of draining its input; dropping that input is what tells
    // a sender to stop.
    let mut rest = Vec::new();
    shared.lock().unwrap().read_to_end(&mut rest).unwrap();
    assert!(rest.len() > 900_000, "{}", rest.len());
}

#[test]
fn stream_rename_is_limited_to_same_file_case_or_normalization_and_honours_fs_move_protection() {
    let node = Node::new();
    node.policy(
        r#"
[principals.unrestricted_nonadmin]
role = "power_user"

[roles.power_user]
allow = ["effect:read", "effect:write", "effect:destroy"]
"#,
    );
    fs::create_dir_all(node.home().join(".config")).unwrap();
    fs::write(node.home().join(".config/policy.toml"), b"keep").unwrap();
    fs::write(node.home().join("projects/a.txt"), b"alpha").unwrap();

    // 1. Non-admin with read+write+destroy CANNOT rename `~/.config` or `~` or Legion's config dir.
    node.run(
        Some("unrestricted_nonadmin"),
        &json!({"op": "rename", "from": "~/.config", "to": "~/.config2"}),
        b"",
    )
    .refused(ErrorCode::Denied);
    assert!(node.home().join(".config/policy.toml").exists());

    let parent_escape = node
        .home()
        .parent()
        .unwrap()
        .join("home2")
        .to_string_lossy()
        .into_owned();
    node.run(
        Some("unrestricted_nonadmin"),
        &json!({"op": "rename", "from": "~", "to": parent_escape}),
        b"",
    )
    .refused(ErrorCode::Denied);
    assert!(node.home().exists());

    let cfg_dir = node.config().to_string_lossy().into_owned();
    let cfg_dir2 = format!("{cfg_dir}2");
    node.run(
        Some("unrestricted_nonadmin"),
        &json!({"op": "rename", "from": cfg_dir, "to": cfg_dir2}),
        b"",
    )
    .refused(ErrorCode::Denied);

    // 2. Arbitrary rename of distinct entries (`a.txt -> b.txt`) is refused with `invalid_args`
    //    even for owner: stream `rename` is ONLY for same-file case/normalization renames.
    node.run(
        None,
        &json!({"op": "rename", "from": "~/projects/a.txt", "to": "~/projects/b.txt"}),
        b"",
    )
    .refused(ErrorCode::InvalidArgs);
    assert!(node.home().join("projects/a.txt").exists());
    assert!(!node.home().join("projects/b.txt").exists());

    // 3. `probe_write remove` never unlinks non-empty or unowned empty files even if named like a
    //    probe marker, and DOES unlink markers it created with a matching ownership record.
    #[cfg(feature = "move-between-places")]
    {
        let fake_probe = node.home().join("projects/.legion-move-probe-fake.tmp");
        fs::write(&fake_probe, b"non-empty-user-data").unwrap();
        node.run(
            None,
            &json!({
                "op": "probe_write",
                "action": "remove",
                "dir": "~/projects",
                "marker": ".legion-move-probe-fake.tmp"
            }),
            b"",
        )
        .ok();
        assert_eq!(
            fs::read(&fake_probe).unwrap(),
            b"non-empty-user-data",
            "probe_write remove must never delete non-empty files"
        );

        let unowned_empty = node.home().join("projects/.legion-move-probe-unowned.tmp");
        fs::write(&unowned_empty, b"").unwrap();
        node.run(
            None,
            &json!({
                "op": "probe_write",
                "action": "remove",
                "dir": "~/projects",
                "marker": ".legion-move-probe-unowned.tmp"
            }),
            b"",
        )
        .ok();
        assert!(
            unowned_empty.exists(),
            "probe_write remove must never delete an unowned 0-byte file by name"
        );

        let created = node
            .run(
                None,
                &json!({
                    "op": "probe_write",
                    "action": "create",
                    "dir": "~/projects"
                }),
                b"",
            )
            .ok()
            .first()
            .0;
        let marker_name = created["marker"].as_str().unwrap();
        assert!(node.home().join("projects").join(marker_name).exists());
        node.run(
            None,
            &json!({
                "op": "probe_write",
                "action": "remove",
                "dir": "~/projects",
                "marker": marker_name
            }),
            b"",
        )
        .ok();
        assert!(
            !node.home().join("projects").join(marker_name).exists(),
            "probe_write remove should unlink its own recorded probe marker"
        );
    }
}

#[cfg(not(feature = "move-between-places"))]
#[test]
fn move_only_stream_ops_are_refused_and_capabilities_omitted() {
    let node = Node::new();
    fs::write(node.home().join("projects/keep.txt"), b"keep").unwrap();

    // Greeting advertises `if_exists` and `rename`, and omits `lock`, `move`, and `probe`.
    let stat_ran = node.run(
        None,
        &json!({"op": "stat", "path": "~/projects/keep.txt"}),
        b"",
    );
    stat_ran.ok();
    let nl = stat_ran.output.iter().position(|&b| b == b'\n').unwrap();
    let greeting: Value = serde_json::from_slice(&stat_ran.output[..nl]).unwrap();
    let caps = greeting["capabilities"].as_array().unwrap();
    assert!(caps.contains(&json!("if_exists")), "{greeting}");
    assert!(caps.contains(&json!("rename")), "{greeting}");
    assert!(!caps.contains(&json!("lock")), "{greeting}");
    assert!(!caps.contains(&json!("move")), "{greeting}");
    assert!(!caps.contains(&json!("probe")), "{greeting}");

    // Move-only operations and move-only `delete` fields are all refused with `unsupported`.
    let keep_hash = hash(b"keep");
    for header in [
        json!({"op": "lock", "paths": ["~/projects/keep.txt"]}),
        json!({"op": "probe_write", "action": "create", "dir": "~/projects"}),
        json!({"op": "probe_write", "action": "behavior", "dir": "~/projects"}),
        json!({"op": "probe_read", "dir": "~/projects", "marker": ".legion-move-probe-x.tmp"}),
        json!({"op": "delete", "root": "~/projects/keep.txt", "names": [], "remove_root": true}),
        json!({"op": "delete", "root": "~/projects/keep.txt", "names": [], "dry_run": true}),
        json!({"op": "delete", "root": "~/projects/keep.txt", "names": [], "expect_root_hash": keep_hash}),
        json!({"op": "delete", "root": "~/projects", "names": ["keep.txt"], "expect_hashes": {"keep.txt": keep_hash}}),
        json!({"op": "delete", "root": "~/projects/keep.txt", "names": [], "forbid_file_id": [1, 2]}),
        json!({"op": "delete", "root": "~/projects", "names": ["keep.txt"], "lock_id": "0123456789abcdef0123456789abcdef"}),
    ] {
        node.run(None, &header, b"").refused(ErrorCode::Unsupported);
    }
    assert_eq!(
        fs::read(node.home().join("projects/keep.txt")).unwrap(),
        b"keep",
        "refused move-only delete must never touch files"
    );

    // Plain `delete` (syncing extra entries in a destination tree) still works.
    let done = node
        .run(
            None,
            &json!({"op": "delete", "root": "~/projects", "names": ["keep.txt"]}),
            b"",
        )
        .ok()
        .first()
        .0;
    assert_eq!(done["removed"], json!(1), "{done}");
    assert!(!node.home().join("projects/keep.txt").exists());
}

#[test]
fn stat_and_manifest_never_mutate_pending_renames() {
    let node = Node::new();
    let dir = node.home().join("projects/rec");
    fs::create_dir_all(&dir).unwrap();
    let temp_name = "legion-renaming-42-1-Draft.txt";
    fs::write(dir.join(temp_name), b"payload").unwrap();
    let renames_dir = node
        .dir
        .path()
        .join("legion")
        .join("state")
        .join("xfer")
        .join("renames");
    fs::create_dir_all(&renames_dir).unwrap();
    let intent_path = renames_dir.join("pending.json");
    fs::write(
        &intent_path,
        serde_json::to_vec(&json!({
            "parent": dir.to_string_lossy(),
            "from_name": "Draft.txt",
            "temp_name": temp_name,
            "to_name": "draft.txt",
        }))
        .unwrap(),
    )
    .unwrap();

    // `stat` and `manifest` are reads and must touch nothing.
    let st = node
        .run(
            None,
            &json!({"op": "stat", "path": "~/projects/rec/draft.txt"}),
            b"",
        )
        .ok()
        .first()
        .0;
    assert_eq!(st["kind"], json!("missing"), "{st}");
    assert!(dir.join(temp_name).exists());
    assert!(!dir.join("draft.txt").exists());
    assert!(intent_path.exists());

    node.run(
        None,
        &json!({"op": "manifest", "root": "~/projects/rec"}),
        b"",
    )
    .ok();
    assert!(dir.join(temp_name).exists());
    assert!(!dir.join("draft.txt").exists());
    assert!(intent_path.exists());

    // A write operation (`put`) in the same folder DOES recover the pending rename.
    let body = b"other";
    node.run(
        None,
        &json!({
            "op": "put",
            "path": "~/projects/rec/other.txt",
            "size": body.len(),
            "hash": hash(body),
        }),
        body,
    )
    .ok();
    assert_eq!(fs::read(dir.join("draft.txt")).unwrap(), b"payload");
    assert!(!dir.join(temp_name).exists());
    assert!(!intent_path.exists());
}

#[cfg(feature = "move-between-places")]
#[test]
fn stream_lock_is_exclusive_and_returns_conflict_while_held() {
    use std::sync::mpsc;

    let node = Node::new();
    fs::write(node.home().join("projects/x.txt"), b"hello").unwrap();

    struct HoldAfterHeader {
        header: Cursor<Vec<u8>>,
        release: mpsc::Receiver<()>,
    }
    impl Read for HoldAfterHeader {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            let n = self.header.read(buf)?;
            if n > 0 {
                return Ok(n);
            }
            let _ = self.release.recv();
            Ok(0)
        }
    }

    struct NotifyWriter {
        tx: mpsc::Sender<Vec<u8>>,
    }
    impl std::io::Write for NotifyWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            let _ = self.tx.send(buf.to_vec());
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    let (rel_tx, rel_rx) = mpsc::channel();
    let (out_tx, out_rx) = mpsc::channel();
    let host = node.host.clone();
    let mut header =
        json!({"protocol": STREAM_PROTOCOL, "op": "lock", "paths": ["~/projects/x.txt"]})
            .to_string()
            .into_bytes();
    header.push(b'\n');
    let handle = std::thread::spawn(move || {
        serve(
            &host,
            None,
            HoldAfterHeader {
                header: Cursor::new(header),
                release: rel_rx,
            },
            NotifyWriter { tx: out_tx },
        )
    });

    let mut gathered = Vec::new();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while gathered.iter().filter(|b| **b == b'\n').count() < 2 {
        assert!(std::time::Instant::now() < deadline);
        if let Ok(chunk) = out_rx.recv_timeout(std::time::Duration::from_millis(50)) {
            gathered.extend_from_slice(&chunk);
        }
    }
    let reply_bytes = after_protocol(&gathered);
    let ready: Value = serde_json::from_slice(reply_bytes).unwrap();
    assert_eq!(ready["locked"], true, "{ready}");
    assert_eq!(ready["entries"][0]["exists"], true, "{ready}");
    assert_eq!(ready["entries"][0]["size"], 5, "{ready}");

    node.run(
        None,
        &json!({"op": "lock", "paths": ["~/projects/x.txt"]}),
        b"",
    )
    .refused(ErrorCode::Conflict);

    let _ = rel_tx.send(());
    handle.join().unwrap().unwrap();

    let after = node
        .run(
            None,
            &json!({"op": "lock", "paths": ["~/projects/x.txt"]}),
            b"",
        )
        .ok()
        .first()
        .0;
    assert_eq!(after["locked"], true, "{after}");
}

#[cfg(feature = "move-between-places")]
#[test]
fn stream_lock_lease_expiry_quota_cleanup_metadata_privacy_and_holder_visibility() {
    use std::sync::mpsc;

    let node = Node::new();
    fs::write(node.home().join("projects/secret.txt"), b"top-secret").unwrap();
    node.policy(
        r#"
default = "owner"

[principals.confined]
role = "confined"

[principals.writeonly]
role = "writeonly"

[roles.confined]
allow = ["effect:read", "effect:write"]
paths = ["~/projects"]

[roles.writeonly]
allow = ["fs.write"]
paths = ["~/projects"]
"#,
    );

    struct FeedLines {
        header: Cursor<Vec<u8>>,
        lines: mpsc::Receiver<Vec<u8>>,
        buf: Cursor<Vec<u8>>,
    }
    impl Read for FeedLines {
        fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
            let n = self.header.read(out)?;
            if n > 0 {
                return Ok(n);
            }
            let n = self.buf.read(out)?;
            if n > 0 {
                return Ok(n);
            }
            match self.lines.recv() {
                Ok(next) if !next.is_empty() => {
                    self.buf = Cursor::new(next);
                    self.buf.read(out)
                }
                _ => Ok(0),
            }
        }
    }

    struct NotifyWriter {
        tx: mpsc::Sender<Vec<u8>>,
    }
    impl std::io::Write for NotifyWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            let _ = self.tx.send(buf.to_vec());
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    let wait_ready = |rx: &mpsc::Receiver<Vec<u8>>| -> Value {
        let mut gathered = Vec::new();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        while gathered.iter().filter(|b| **b == b'\n').count() < 2 {
            assert!(
                std::time::Instant::now() < deadline,
                "timed out waiting for lock ready reply; got so far: {:?}",
                String::from_utf8_lossy(&gathered)
            );
            if let Ok(chunk) = rx.recv_timeout(std::time::Duration::from_millis(25)) {
                gathered.extend_from_slice(&chunk);
            }
        }
        serde_json::from_slice(after_protocol(&gathered)).unwrap()
    };

    // 1. Write-only role (`writeonly`) acquires a lock cleanly, learns ZERO metadata (`entries: []`),
    //    and leaves ZERO `.lock` files behind on release.
    let wo_reply = node
        .run(
            Some("writeonly"),
            &json!({"op": "lock", "paths": ["~/projects/secret.txt", "~/projects/nope.txt"]}),
            b"",
        )
        .ok()
        .first()
        .0;
    assert_eq!(wo_reply["locked"], true, "{wo_reply}");
    assert_eq!(
        wo_reply["entries"].as_array().unwrap().len(),
        0,
        "write-only role must not receive file existence/size/mtime/inode via lock: {wo_reply}"
    );
    let lock_dir = node.dir.path().join("legion/state/xfer/move-lock");
    let remaining_locks: Vec<_> = fs::read_dir(&lock_dir)
        .into_iter()
        .flatten()
        .flatten()
        .collect();
    assert!(
        remaining_locks.is_empty(),
        "lock files must be removed on release: {remaining_locks:?}"
    );

    // 2. Active lock returns an unguessable `lock_id` + `lease_ms`, answers `"renew\n"` with
    //    `{"status":"held"}`, shows holder details to unconfined owner (including on Windows via
    //    unlocked sibling `.meta`), authorizes `delete` ONLY for its covered path while live, and
    //    refuses `delete` with `Conflict` once released (leaving the file intact).
    //    Uses a generous 10 s lease so loaded CI runners never flake on wall-clock timing;
    //    lease expiry without wall-clock sleeps is verified in `stream::tests` via an injected clock.
    let (line_tx, line_rx) = mpsc::channel();
    let (out_tx, out_rx) = mpsc::channel();
    let host = node.host.clone();
    let mut hdr = json!({
        "protocol": STREAM_PROTOCOL,
        "op": "lock",
        "paths": ["~/projects/secret.txt"],
        "lease_ms": 10_000,
    })
    .to_string()
    .into_bytes();
    hdr.push(b'\n');
    let holder = std::thread::spawn(move || {
        serve(
            &host,
            Some("confined"),
            FeedLines {
                header: Cursor::new(hdr),
                lines: line_rx,
                buf: Cursor::new(Vec::new()),
            },
            NotifyWriter { tx: out_tx },
        )
    });
    let mut gathered = Vec::new();
    let read_next_json =
        |buf: &mut Vec<u8>, rx: &mpsc::Receiver<Vec<u8>>, skip_lines: usize| -> Value {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            while buf.iter().filter(|&&b| b == b'\n').count() <= skip_lines {
                assert!(
                    std::time::Instant::now() < deadline,
                    "timed out waiting for stream reply line {}: {:?}",
                    skip_lines + 1,
                    String::from_utf8_lossy(buf)
                );
                if let Ok(chunk) = rx.recv_timeout(std::time::Duration::from_millis(25)) {
                    buf.extend_from_slice(&chunk);
                }
            }
            let line = String::from_utf8_lossy(buf)
                .lines()
                .nth(skip_lines)
                .unwrap()
                .to_owned();
            serde_json::from_str(&line).unwrap()
        };
    let ready = read_next_json(&mut gathered, &out_rx, 1);
    assert_eq!(ready["locked"], true, "{ready}");
    let lock_id = ready["lock_id"].as_str().unwrap().to_owned();
    assert_eq!(lock_id.len(), 32, "lock_id must be 128-bit hex: {ready}");
    assert_eq!(ready["lease_ms"], 10_000, "{ready}");

    // While held: owner (unconfined) sees the holder principal in the conflict error.
    let owner_conflict = node
        .run(
            Some("owner"),
            &json!({"op": "lock", "paths": ["~/projects/secret.txt"]}),
            b"",
        )
        .refused(ErrorCode::Conflict);
    assert!(
        owner_conflict["message"]
            .as_str()
            .is_some_and(|m| m.contains("confined")),
        "unconfined owner should see who holds the lock: {owner_conflict:?}"
    );

    // Explicit `"renew\n"` replies `{"status":"held"}` immediately.
    line_tx.send(b"renew\n".to_vec()).unwrap();
    let renew_reply = read_next_json(&mut gathered, &out_rx, 2);
    assert_eq!(renew_reply["status"], "held", "{renew_reply}");

    // `delete` carrying a forged `lock_id` or another principal's `lock_id` is refused with
    // `Conflict` and removes nothing.
    let forged_delete = node
        .run(
            Some("owner"),
            &json!({
                "op": "delete",
                "root": "~/projects/secret.txt",
                "names": [],
                "remove_root": true,
                "lock_id": "00000000000000000000000000000000",
            }),
            b"",
        )
        .refused(ErrorCode::Conflict);
    assert!(
        forged_delete["message"]
            .as_str()
            .unwrap_or("")
            .contains("source was kept"),
        "{forged_delete:?}"
    );
    assert!(node.home().join("projects/secret.txt").exists());

    // Even `owner` cannot reuse `confined`'s `lock_id` (principal mismatch -> `Conflict`).
    let wrong_principal_delete = node
        .run(
            Some("owner"),
            &json!({
                "op": "delete",
                "root": "~/projects/secret.txt",
                "names": [],
                "remove_root": true,
                "lock_id": &lock_id,
            }),
            b"",
        )
        .refused(ErrorCode::Conflict);
    assert!(
        wrong_principal_delete["message"]
            .as_str()
            .unwrap_or("")
            .contains("source was kept"),
        "{wrong_principal_delete:?}"
    );
    assert!(node.home().join("projects/secret.txt").exists());

    // Release cleanly -> lock files are gone, owner can now lock, and `delete` with the released
    // `lock_id` is refused with `Conflict` without removing `secret.txt`.
    line_tx.send(b"release\n".to_vec()).unwrap();
    holder.join().unwrap().unwrap();
    drop(line_tx);

    let after_release_delete = node
        .run(
            Some("owner"),
            &json!({
                "op": "delete",
                "root": "~/projects/secret.txt",
                "names": [],
                "remove_root": true,
                "lock_id": &lock_id,
            }),
            b"",
        )
        .refused(ErrorCode::Conflict);
    assert!(
        after_release_delete["message"]
            .as_str()
            .unwrap_or("")
            .contains("source was kept"),
        "{after_release_delete:?}"
    );
    assert!(node.home().join("projects/secret.txt").exists());

    let after_expiry = node
        .run(
            Some("owner"),
            &json!({"op": "lock", "paths": ["~/projects/secret.txt"]}),
            b"",
        )
        .ok()
        .first()
        .0;
    assert_eq!(after_expiry["locked"], true, "{after_expiry}");

    // 3. Busy lock holder visibility + uncovered-path refusal for same principal: when `owner`
    //    holds a lock on `secret.txt`, a confined role hitting it must NOT learn `owner`'s
    //    principal name or job id, AND `owner` cannot delete an uncovered `other.txt` using that
    //    `lock_id`.
    let (o_tx, o_rx) = mpsc::channel();
    let (oo_tx, oo_rx) = mpsc::channel();
    let host2 = node.host.clone();
    let mut ohdr = json!({
        "protocol": STREAM_PROTOCOL,
        "op": "lock",
        "paths": ["~/projects/secret.txt"],
        "lease_ms": 10_000,
    })
    .to_string()
    .into_bytes();
    ohdr.push(b'\n');
    let owner_holder = std::thread::spawn(move || {
        serve(
            &host2,
            Some("owner"),
            FeedLines {
                header: Cursor::new(ohdr),
                lines: o_rx,
                buf: Cursor::new(Vec::new()),
            },
            NotifyWriter { tx: oo_tx },
        )
    });
    let owner_ready = wait_ready(&oo_rx);
    assert_eq!(owner_ready["locked"], true);
    let owner_lock_id = owner_ready["lock_id"].as_str().unwrap().to_owned();

    fs::write(node.home().join("projects/other.txt"), "untouched").unwrap();
    let uncovered_delete = node
        .run(
            Some("owner"),
            &json!({
                "op": "delete",
                "root": "~/projects/other.txt",
                "names": [],
                "remove_root": true,
                "lock_id": &owner_lock_id,
            }),
            b"",
        )
        .refused(ErrorCode::Conflict);
    assert!(
        uncovered_delete["message"]
            .as_str()
            .unwrap_or("")
            .contains("source was kept"),
        "{uncovered_delete:?}"
    );
    assert!(node.home().join("projects/other.txt").exists());

    let conf_conflict = node
        .run(
            Some("confined"),
            &json!({"op": "lock", "paths": ["~/projects/secret.txt"]}),
            b"",
        )
        .refused(ErrorCode::Conflict);
    assert!(
        !conf_conflict["message"]
            .as_str()
            .unwrap_or("")
            .contains("owner"),
        "confined role must not see owner holder identity: {conf_conflict:?}"
    );
    assert!(
        conf_conflict.get("details").is_none(),
        "confined role must not receive holder details: {conf_conflict:?}"
    );
    let _ = o_tx.send(b"release\n".to_vec());
    owner_holder.join().unwrap().unwrap();

    // 4. Per-principal active lock limit (256): holding 256 locks blocks a 257th lock with `busy`,
    //    and releasing them frees the quota and removes all lock files.
    let (q_tx, q_rx) = mpsc::channel();
    let (qo_tx, qo_rx) = mpsc::channel();
    let host3 = node.host.clone();
    let paths_256: Vec<String> = (0..256)
        .map(|i| format!("~/projects/q-{i:03}.txt"))
        .collect();
    let mut qhdr = json!({
        "protocol": STREAM_PROTOCOL,
        "op": "lock",
        "paths": paths_256,
        "lease_ms": 10_000,
    })
    .to_string()
    .into_bytes();
    qhdr.push(b'\n');
    let quota_holder = std::thread::spawn(move || {
        serve(
            &host3,
            Some("confined"),
            FeedLines {
                header: Cursor::new(qhdr),
                lines: q_rx,
                buf: Cursor::new(Vec::new()),
            },
            NotifyWriter { tx: qo_tx },
        )
    });
    assert_eq!(wait_ready(&qo_rx)["locked"], true);

    let over_quota = node
        .run(
            Some("confined"),
            &json!({"op": "lock", "paths": ["~/projects/q-256.txt"]}),
            b"",
        )
        .refused(ErrorCode::Busy);
    assert!(
        over_quota["message"]
            .as_str()
            .is_some_and(|m| m.contains("256")),
        "quota error should state the lock limit: {over_quota:?}"
    );

    let _ = q_tx.send(b"release\n".to_vec());
    quota_holder.join().unwrap().unwrap();
    let remaining_after_256: Vec<_> = fs::read_dir(&lock_dir)
        .into_iter()
        .flatten()
        .flatten()
        .collect();
    assert!(
        remaining_after_256.is_empty(),
        "releasing 256 locks must remove all lock files: {}",
        remaining_after_256.len()
    );
}

#[test]
fn keep_open_get_serves_multiple_ranges_frames_done_and_coalesces_audit() {
    let node = Node::new();
    fs::create_dir_all(node.home().join("projects/src")).unwrap();
    let file = node.home().join("projects/src/clip.bin");
    fs::write(&file, b"0123456789abcdefghijklmnopqrstuvwxyz").unwrap();

    let mut input = Vec::new();
    for req in [
        json!({"protocol": 1, "op": "get", "path": "~/projects/src/clip.bin", "offset": 0, "length": 10, "keep_open": true}),
        json!({"protocol": 1, "op": "get", "path": "~/projects/src/clip.bin", "offset": 10, "length": 6, "keep_open": true}),
        json!({"protocol": 1, "op": "get", "path": "~/projects/src/clip.bin", "offset": 999, "length": 4, "keep_open": true}),
        json!({"protocol": 1, "op": "get", "path": "~/projects/src/clip.bin", "offset": 20, "length": 5, "keep_open": true}),
    ] {
        input.extend_from_slice(req.to_string().as_bytes());
        input.push(b'\n');
    }

    let mut output = Vec::new();
    serve(&node.host, None, Cursor::new(input), &mut output).unwrap();

    // Parse protocol greeting, then 4 framed replies.
    let mut rest = after_protocol(&output);

    let take_line = |slice: &mut &[u8]| -> Value {
        let pos = slice.iter().position(|&b| b == b'\n').unwrap();
        let line = &slice[..pos];
        *slice = &slice[pos + 1..];
        serde_json::from_slice(line).unwrap()
    };

    // Frame 1: 10 bytes
    let r1 = take_line(&mut rest);
    assert_eq!(r1["size"], 36);
    assert_eq!(r1["offset"], 0);
    assert_eq!(r1["length"], 10);
    assert_eq!(r1["keep_open"], true);
    #[cfg(unix)]
    assert!(r1["file_id"].as_u64().is_some());
    assert_eq!(&rest[..10], b"0123456789");
    rest = &rest[10..];
    assert_eq!(take_line(&mut rest), json!({"done": true}));

    // Frame 2: 6 bytes
    let r2 = take_line(&mut rest);
    assert_eq!(r2["offset"], 10);
    assert_eq!(r2["length"], 6);
    assert_eq!(&rest[..6], b"abcdef");
    rest = &rest[6..];
    assert_eq!(take_line(&mut rest), json!({"done": true}));

    // Frame 3: out of range error does not break the persistent pipe
    let r3 = take_line(&mut rest);
    assert_eq!(r3["error"]["code"], "conflict");

    // Frame 4: 5 bytes still succeed on the same pipe
    let r4 = take_line(&mut rest);
    assert_eq!(r4["offset"], 20);
    assert_eq!(r4["length"], 5);
    assert_eq!(&rest[..5], b"klmno");
    rest = &rest[5..];
    assert_eq!(take_line(&mut rest), json!({"done": true}));
    assert!(rest.is_empty());

    // Audit: 3 successful range reads coalesced into 1 ok `stream.get` line (21 bytes total) plus
    // 1 error line for the out-of-range request.
    let audit = node.audit();
    let ok_gets: Vec<&Value> = audit
        .iter()
        .filter(|e| e["tool"] == "stream.get" && e["outcome"] == "ok")
        .collect();
    assert_eq!(
        ok_gets.len(),
        1,
        "expected 1 coalesced ok audit line: {audit:?}"
    );
    assert_eq!(ok_gets[0]["args"]["requests"], 3);
    assert_eq!(ok_gets[0]["args"]["bytes_served"], 21);
    let err_gets: Vec<&Value> = audit
        .iter()
        .filter(|e| e["tool"] == "stream.get" && e["outcome"] != "ok")
        .collect();
    assert_eq!(err_gets.len(), 1, "expected 1 error audit line: {audit:?}");
}

/// Blocking byte reader fed by an `mpsc` channel so tests can keep a `keep_open` stream open and
/// idle across timer expirations without OS pipe portability issues.
struct ChannelReader {
    rx: std::sync::mpsc::Receiver<Vec<u8>>,
    buf: Vec<u8>,
    pos: usize,
}

impl Read for ChannelReader {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        while self.pos >= self.buf.len() {
            match self.rx.recv() {
                Ok(chunk) => {
                    self.buf = chunk;
                    self.pos = 0;
                }
                Err(_) => return Ok(0),
            }
        }
        let n = out.len().min(self.buf.len() - self.pos);
        out[..n].copy_from_slice(&self.buf[self.pos..self.pos + n]);
        self.pos += n;
        Ok(n)
    }
}

#[test]
fn keep_open_idle_audit_flushes_on_timer_and_session_exits_on_idle_timeout() {
    use std::time::{Duration, Instant};

    let node = Node::new();
    fs::create_dir_all(node.home().join("projects/src")).unwrap();
    fs::write(
        node.home().join("projects/src/clip.bin"),
        b"0123456789abcdefghijklmnopqrstuvwxyz",
    )
    .unwrap();

    let (tx, rx) = std::sync::mpsc::channel::<Vec<u8>>();
    let host = node.host.clone();
    let options = StreamServeOptions {
        audit_coalesce_window: Duration::from_millis(120),
        keep_open_idle_timeout: Duration::from_millis(450),
        ..StreamServeOptions::default()
    };
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    let server = std::thread::spawn(move || {
        let mut output = Vec::new();
        let res = serve_with_options(
            &host,
            None,
            Door::Stream,
            None,
            options,
            ChannelReader {
                rx,
                buf: Vec::new(),
                pos: 0,
            },
            &mut output,
        );
        let _ = done_tx.send(res);
    });

    // Send two ranged gets, then hold stdin open and idle.
    for (offset, length) in [(0, 10), (10, 6)] {
        let line = format!(
            "{}\n",
            json!({
                "protocol": 1,
                "op": "get",
                "path": "~/projects/src/clip.bin",
                "offset": offset,
                "length": length,
                "keep_open": true,
            })
        );
        tx.send(line.into_bytes()).unwrap();
    }

    // Before idle timeout fires, the periodic audit flush writes the coalesced entry while the
    // stream is still open and idle.
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        let ok_gets: Vec<Value> = node
            .audit()
            .into_iter()
            .filter(|e| e["tool"] == "stream.get" && e["outcome"] == "ok")
            .collect();
        if !ok_gets.is_empty() {
            assert_eq!(ok_gets.len(), 1, "{ok_gets:?}");
            assert_eq!(ok_gets[0]["args"]["requests"], 2);
            assert_eq!(ok_gets[0]["args"]["bytes_served"], 16);
            break;
        }
        assert!(
            Instant::now() < deadline,
            "coalesced stream.get audit was not flushed on timer while idle"
        );
        std::thread::sleep(Duration::from_millis(20));
    }

    // Even while `tx` stays open, the server-side idle timeout exits cleanly.
    let result = done_rx
        .recv_timeout(Duration::from_secs(3))
        .expect("keep_open stream did not exit after keep_open_idle_timeout");
    assert!(result.is_ok(), "{result:?}");
    drop(tx);
    server.join().unwrap();
}

#[test]
fn keep_open_flushes_pending_audit_on_shutdown() {
    use std::time::Duration;

    let node = Node::new();
    fs::create_dir_all(node.home().join("projects/src")).unwrap();
    fs::write(node.home().join("projects/src/a.bin"), b"abcdefghij").unwrap();

    let (tx, rx) = std::sync::mpsc::channel::<Vec<u8>>();
    let shutdown = legion_core::CancellationToken::new();
    let host = node.host.clone();
    let options = StreamServeOptions {
        audit_coalesce_window: Duration::from_secs(60),
        keep_open_idle_timeout: Duration::from_secs(120),
        shutdown: Some(shutdown.clone()),
        ..StreamServeOptions::default()
    };
    struct SharedWriter(std::sync::Arc<(std::sync::Mutex<Vec<u8>>, std::sync::Condvar)>);
    impl std::io::Write for SharedWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            let (lock, cvar) = &*self.0;
            lock.lock().unwrap().extend_from_slice(buf);
            cvar.notify_all();
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    let output_shared = std::sync::Arc::new((
        std::sync::Mutex::new(Vec::<u8>::new()),
        std::sync::Condvar::new(),
    ));
    let output_for_server = std::sync::Arc::clone(&output_shared);
    let server = std::thread::spawn(move || {
        let mut output = SharedWriter(output_for_server);
        serve_with_options(
            &host,
            None,
            Door::Stream,
            None,
            options,
            ChannelReader {
                rx,
                buf: Vec::new(),
                pos: 0,
            },
            &mut output,
        )
    });

    let line = format!(
        "{}\n",
        json!({
            "protocol": 1,
            "op": "get",
            "path": "~/projects/src/a.bin",
            "offset": 0,
            "length": 10,
            "keep_open": true,
        })
    );
    tx.send(line.into_bytes()).unwrap();
    // Wait deterministically for the keep_open get's trailing `{"done":true}` frame to be written
    // before triggering shutdown, instead of sleeping a fixed duration.
    {
        let (lock, cvar) = &*output_shared;
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        let mut guard = lock.lock().unwrap();
        while !guard
            .windows(b"\"done\":true".len())
            .any(|w| w == b"\"done\":true")
        {
            let Some(remaining) = deadline.checked_duration_since(std::time::Instant::now()) else {
                panic!(
                    "timed out waiting for keep_open get done frame before shutdown; output={:?}",
                    String::from_utf8_lossy(&guard)
                );
            };
            let (next, _) = cvar.wait_timeout(guard, remaining).unwrap();
            guard = next;
        }
    }
    // Trigger shutdown while stdin is still open and audit window has not elapsed.
    shutdown.cancel();
    let res = server.join().unwrap();
    assert!(res.is_ok(), "{res:?}");
    drop(tx);

    let ok_gets: Vec<Value> = node
        .audit()
        .into_iter()
        .filter(|e| e["tool"] == "stream.get" && e["outcome"] == "ok")
        .collect();
    assert_eq!(ok_gets.len(), 1, "{ok_gets:?}");
    assert_eq!(ok_gets[0]["args"]["requests"], 1);
    assert_eq!(ok_gets[0]["args"]["bytes_served"], 10);
}

#[test]
fn mid_stream_policy_revocation_stops_long_get() {
    use std::time::Duration;

    let node = Node::new();
    let projects = node.home().join("projects/src");
    fs::create_dir_all(&projects).unwrap();
    let payload = vec![0x5Au8; 512 * 1024];
    fs::write(projects.join("big.bin"), &payload).unwrap();
    node.policy(
        r#"
default = "owner"

[principals.reader]
role = "reader"

[roles.reader]
allow = ["fs.read"]
paths = ["~/projects/**"]
"#,
    );

    // Writer that revokes `reader`'s policy once 128 KiB have been streamed out.
    struct RevokingWriter {
        policy_path: PathBuf,
        written: usize,
        revoked: bool,
        sink: Vec<u8>,
    }

    impl io::Write for RevokingWriter {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.sink.extend_from_slice(buf);
            self.written += buf.len();
            if !self.revoked && self.written >= 128 * 1024 {
                self.revoked = true;
                fs::write(
                    &self.policy_path,
                    "default = \"owner\"\n\n[principals.reader]\nrole = \"none\"\n\n[roles.none]\nallow = []\n",
                )?;
            }
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    let req = format!(
        "{}\n",
        json!({
            "protocol": 1,
            "op": "get",
            "path": "~/projects/src/big.bin",
            "offset": 0,
            "keep_open": true,
        })
    );
    let mut writer = RevokingWriter {
        policy_path: node.config().join("policy.toml"),
        written: 0,
        revoked: false,
        sink: Vec::new(),
    };
    let options = StreamServeOptions {
        recheck_bytes: 128 * 1024,
        recheck_interval: Duration::from_secs(60),
        ..StreamServeOptions::default()
    };
    let err = serve_with_options(
        &node.host,
        Some("reader"),
        Door::Stream,
        None,
        options,
        Cursor::new(req.into_bytes()),
        &mut writer,
    )
    .unwrap_err();
    assert_eq!(err.code, ErrorCode::Denied, "{err}");
    assert!(
        writer.written < payload.len(),
        "expected get to stop before full payload ({} bytes), wrote {}",
        payload.len(),
        writer.written
    );
}
