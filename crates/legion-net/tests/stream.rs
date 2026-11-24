//! The stream endpoint, one operation at a time, through its public entry point: what each
//! operation answers, what it changes, who may do it, and the audit line it leaves.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::fs;
use std::io::{BufRead, Cursor, Read};
use std::path::{Path, PathBuf};

use legion_core::Paths;
use legion_net::stream::{Host, serve};
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
    fn run(&self, pinned: Option<&str>, header: &Value, body: &[u8]) -> Ran {
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
    fn first(&self) -> (Value, &[u8]) {
        let end = self
            .output
            .iter()
            .position(|b| *b == b'\n')
            .expect("a line");
        (
            serde_json::from_slice(&self.output[..end]).unwrap(),
            &self.output[end + 1..],
        )
    }

    fn lines(&self) -> Vec<Value> {
        self.output
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
fn get_sends_the_size_then_the_bytes_from_the_offset() {
    let node = Node::new();
    let data: Vec<u8> = (0..200_000u32).map(|i| (i % 251) as u8).collect();
    fs::write(node.home().join("projects/big.bin"), &data).unwrap();

    let ran = node.run(
        None,
        &json!({"op": "get", "path": "~/projects/big.bin", "offset": 1000}),
        b"",
    );
    let (line, rest) = ran.ok().first();
    assert_eq!(line, json!({"size": 200_000}));
    assert_eq!(rest, &data[1000..]);
    assert_eq!(node.last_audit()["tool"], "stream.get");

    let ran = node.run(
        None,
        &json!({"op": "get", "path": "~/projects/big.bin", "offset": 300_000}),
        b"",
    );
    ran.refused(ErrorCode::Conflict);
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
    assert_eq!(
        lines[1],
        json!({"ok": true, "size": data.len(), "hash": hash(&data)})
    );
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
            Cursor::new(b"{\"op\":\"manifest\",\"root\":\"~\"}\n".to_vec()),
            &mut output,
        )
        .unwrap();
        output
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
        &json!({"op": "unpack", "root": "~/projects/dst", "names": ["nested", "nested/a.txt", "nested/empty", "odd name #1.txt"]}),
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
        b"{\"op\":\"unpack\",\"root\":\"~\",\"names\":[\".legion/config/policy.toml\",\"fine.txt\"]}\n"
            .to_vec();
    input.extend_from_slice(&archive);
    let mut output = Vec::new();
    serve(&host, Some("writer"), Cursor::new(input), &mut output).unwrap();
    let done: Value =
        serde_json::from_slice(output.split(|b| *b == b'\n').nth(1).unwrap()).unwrap();
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
        &json!({"op": "unpack", "root": "~/projects/x", "names": []}),
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
        &json!({"op": "unpack", "root": node.config(), "names": []}),
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
        b"{\"op\":\"format\",\"path\":\"/\"}\n",
        b"{\"path\":\"/\"}\n",
        b"[1]\n",
    ] {
        let mut output = Vec::new();
        let error = serve(&node.host, None, Cursor::new(input.to_vec()), &mut output).unwrap_err();
        assert_eq!(error.code, ErrorCode::InvalidArgs, "{input:?}");
        let line: Value =
            serde_json::from_slice(output.split(|b| *b == b'\n').next().unwrap()).unwrap();
        assert_eq!(line["error"]["code"], "invalid_args");
    }
    let relative = node.run(None, &json!({"op": "stat", "path": "relative/path"}), b"");
    relative.refused(ErrorCode::InvalidArgs);
}

#[test]
fn unpack_takes_only_the_planned_entries_and_the_directories_on_their_way() {
    let node = Node::new();
    let staging = tempfile::tempdir().unwrap();
    fs::create_dir_all(staging.path().join("a/b")).unwrap();
    fs::create_dir_all(staging.path().join("stray")).unwrap();
    fs::write(staging.path().join("a/b/wanted.txt"), b"w").unwrap();
    fs::write(staging.path().join(".env"), b"SECRET").unwrap();
    let archive = tar_of(
        staging.path(),
        &["a", "a/b", "a/b/wanted.txt", ".env", "stray"],
    );
    let ran = node.run(
        None,
        &json!({"op": "unpack", "root": "~/projects/dst", "names": ["a/b/wanted.txt"]}),
        &archive,
    );
    let done = &ran.ok().lines()[1];
    assert_eq!(done["files"], 1, "{done}");
    assert_eq!(done["unplanned"], json!([".env", "stray"]), "{done}");
    let dst = node.home().join("projects/dst");
    assert!(dst.join("a/b/wanted.txt").exists());
    assert!(!dst.join(".env").exists());
    assert!(!dst.join("stray").exists());
}

#[test]
fn a_refused_unpack_reads_nothing_of_what_follows() {
    let node = Node::new();
    node.policy(CONFINED);
    let mut input = b"{\"op\":\"unpack\",\"root\":\"~/projects/x\",\"names\":[]}\n".to_vec();
    input.extend(std::iter::repeat_n(0u8, 1 << 20));
    let mut cursor = Cursor::new(input);
    let mut output = Vec::new();
    let error = serve(&node.host, Some("reader"), &mut cursor, &mut output).unwrap_err();
    assert_eq!(error.code, ErrorCode::Denied);
    // It answered and returned instead of draining its input; dropping that input is what tells
    // a sender to stop.
    let mut rest = Vec::new();
    cursor.read_to_end(&mut rest).unwrap();
    assert!(rest.len() > 900_000, "{}", rest.len());
}
