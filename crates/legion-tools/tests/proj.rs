//! The `proj` tool family, called through a node the way every door calls it, against real git
//! repositories in temporary directories.
//!
//! Git must not read the configuration of the machine running the tests, so the test binary
//! first re-executes itself with `GIT_CONFIG_GLOBAL` pointing at an empty file and
//! `GIT_CONFIG_NOSYSTEM` set; the tools' own git commands inherit that. The test binary is also
//! the program a task runs (`--proj-helper`) and the job supervisor `job.start` starts, so no
//! test needs `sh` or any other program but git.

#![allow(clippy::unwrap_used, clippy::panic, missing_docs)]

mod support;

use std::env;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};

use legion_core::CancellationToken;
use legion_core::proto::{ErrorCode, EventKind};
use serde_json::{Value, json};
use support::{Events, Fixture, HELPER, Test, this_binary};

/// Set in the re-executed test binary, whose git reads no configuration of the machine's.
const HERMETIC: &str = "LEGION_PROJ_TEST_HERMETIC";
/// The first argument that makes the test binary a task's program.
const TASK_HELPER: &str = "--proj-helper";
/// The first argument that makes the test binary leave a marker file at the path after it.
const MARKER: &str = "--proj-mark";

fn main() -> ExitCode {
    let args: Vec<String> = env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some(TASK_HELPER) {
        return task_helper(&args[1..]);
    }
    if args.first().map(String::as_str) == Some(MARKER) {
        // Run by git as a filter, a monitor or a hook a hostile repository names.
        if let Some(path) = args.get(1) {
            let _ = std::fs::write(path, "ran");
        }
        return ExitCode::SUCCESS;
    }
    if let Some(code) = legion_tools::job::try_supervise_cli(env::args_os().skip(1)) {
        return code;
    }
    if env::var_os(HELPER).is_some() {
        return support::main(&[]);
    }
    if env::var_os(HERMETIC).is_none() {
        return hermetic(&args);
    }
    if !git_installed() {
        println!("git is not installed: the proj tests are skipped");
        return ExitCode::SUCCESS;
    }
    let tests: &[Test] = &[
        ("list_and_add_record_projects_per_machine", || {
            Box::pin(list_and_add_record_projects_per_machine())
        }),
        ("remove_forgets_here_or_everywhere", || {
            Box::pin(remove_forgets_here_or_everywhere())
        }),
        ("status_reports_clean_dirty_and_ahead_repositories", || {
            Box::pin(status_reports_clean_dirty_and_ahead_repositories())
        }),
        ("clone_fetches_from_a_local_bare_repository", || {
            Box::pin(clone_fetches_from_a_local_bare_repository())
        }),
        ("pull_fast_forwards_and_refuses_to_merge", || {
            Box::pin(pull_fast_forwards_and_refuses_to_merge())
        }),
        ("run_starts_a_task_by_argv_and_as_a_job", || {
            Box::pin(run_starts_a_task_by_argv_and_as_a_job())
        }),
        ("discover_finds_and_adds_working_trees", || {
            Box::pin(discover_finds_and_adds_working_trees())
        }),
        ("a_confined_role_sees_only_its_paths", || {
            Box::pin(a_confined_role_sees_only_its_paths())
        }),
        ("a_role_without_the_effect_is_denied", || {
            Box::pin(a_role_without_the_effect_is_denied())
        }),
        ("clone_takes_only_repositories_it_may_read", || {
            Box::pin(clone_takes_only_repositories_it_may_read())
        }),
        (
            "git_indirection_does_not_lead_a_confined_role_outside",
            || Box::pin(git_indirection_does_not_lead_a_confined_role_outside()),
        ),
        ("reading_a_hostile_repository_runs_nothing", || {
            Box::pin(reading_a_hostile_repository_runs_nothing())
        }),
    ];
    support::main(tests)
}

/// Runs the tests again in a child whose git reads an empty global configuration and no system
/// one.
fn hermetic(args: &[String]) -> ExitCode {
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("gitconfig");
    std::fs::write(&config, "").unwrap();
    let status = Command::new(this_binary())
        .args(args)
        .env(HERMETIC, "1")
        .env("GIT_CONFIG_GLOBAL", &config)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .status()
        .unwrap();
    if status.success() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

/// A task's program: prints the directory it runs in, then each argument on its own line.
fn task_helper(args: &[String]) -> ExitCode {
    let mut stdout = std::io::stdout();
    let _ = writeln!(stdout, "cwd={}", env::current_dir().unwrap().display());
    for arg in args {
        let _ = writeln!(stdout, "{arg}");
    }
    let _ = stdout.flush();
    if args.iter().any(|arg| arg == "fail") {
        ExitCode::from(3)
    } else {
        ExitCode::SUCCESS
    }
}

fn git_installed() -> bool {
    Command::new("git")
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

/// Runs git in `dir` and returns its standard output, failing the test when git fails.
fn git(dir: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?} in {} failed: {}",
        dir.display(),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

/// A new repository at `dir` with one commit, configured only inside itself.
fn repo(dir: &Path) -> String {
    std::fs::create_dir_all(dir).unwrap();
    git(dir, &["init", "--quiet", "--initial-branch=main"]);
    identity(dir);
    std::fs::write(dir.join("README.md"), "hello\n").unwrap();
    git(dir, &["add", "README.md"]);
    git(dir, &["commit", "--quiet", "-m", "First commit"]);
    git(dir, &["rev-parse", "HEAD"])
}

fn identity(dir: &Path) {
    git(dir, &["config", "user.name", "Me"]);
    git(dir, &["config", "user.email", "me@example.com"]);
    git(dir, &["config", "commit.gpgsign", "false"]);
}

/// A bare repository holding one commit on `main`, as a remote to clone from.
fn bare(root: &Path, name: &str) -> PathBuf {
    let seed = root.join(format!("{name}-seed"));
    repo(&seed);
    let bare = root.join(format!("{name}.git"));
    git(
        root,
        &[
            "clone",
            "--quiet",
            "--bare",
            seed.to_str().unwrap(),
            bare.to_str().unwrap(),
        ],
    );
    bare
}

/// Clones `bare` to `dir` with an identity of its own.
fn checkout(bare: &Path, dir: &Path) {
    git(
        dir.parent().unwrap(),
        &[
            "clone",
            "--quiet",
            bare.to_str().unwrap(),
            dir.to_str().unwrap(),
        ],
    );
    identity(dir);
}

fn commit(dir: &Path, file: &str, text: &str) -> String {
    std::fs::write(dir.join(file), text).unwrap();
    git(dir, &["add", file]);
    git(dir, &["commit", "--quiet", "-m", &format!("Change {file}")]);
    git(dir, &["rev-parse", "HEAD"])
}

/// A node called `tower`.
fn tower() -> Fixture {
    let fixture = Fixture::new();
    fixture.write_config("node.toml", "name = \"tower\"\n");
    fixture
}

/// A TOML string holding `text`; JSON's escapes are TOML's.
fn toml_string(text: &str) -> String {
    serde_json::to_string(text).unwrap()
}

fn projects_toml(fixture: &Fixture) -> String {
    std::fs::read_to_string(fixture.dir.path().join("config").join("projects.toml")).unwrap()
}

fn names(listing: &Value) -> Vec<&str> {
    listing["projects"]
        .as_array()
        .unwrap()
        .iter()
        .map(|project| project["name"].as_str().unwrap())
        .collect()
}

fn entry<'a>(listing: &'a Value, name: &str) -> &'a Value {
    listing["projects"]
        .as_array()
        .unwrap()
        .iter()
        .find(|project| project["name"] == json!(name))
        .unwrap_or_else(|| panic!("{name} is not in {listing:#}"))
}

/// The same path written the way the tools resolve it.
fn canonical(path: &Path) -> String {
    dunce::canonicalize(path).unwrap().display().to_string()
}

async fn list_and_add_record_projects_per_machine() {
    let fixture = tower();
    fixture.write_config(
        "projects.toml",
        "# Shared between the machines.\n\n[projects.site]\ndescription = \"The public website\"\nrepo = \"git@git.example:me/site.git\"\n\n[projects.site.on]   # per machine\nlaptop = \"~/code/site\"\n\n[projects.site.tasks]\ntest = \"npm test\"\ndev = { cmd = \"npm run dev\", job = true }\n",
    );
    let listing = fixture.call("proj.list", json!({})).await.unwrap();
    assert_eq!(listing["node"], json!("tower"));
    let site = entry(&listing, "site");
    assert_eq!(site["path"], Value::Null);
    assert_eq!(site["present"], json!(false));
    assert_eq!(site["elsewhere"], json!(["laptop"]));
    assert_eq!(site["tasks"], json!(["dev", "test"]));
    assert_eq!(site["repo"], json!("git@git.example:me/site.git"));

    // Recording where it lives here leaves the laptop's path and the comments alone.
    std::fs::create_dir_all(fixture.home().join("projects").join("site")).unwrap();
    let added = fixture
        .call(
            "proj.add",
            json!({"name": "site", "path": "~/projects/site"}),
        )
        .await
        .unwrap();
    assert_eq!(
        added,
        json!({"name": "site", "outcome": "updated", "path": "~/projects/site"})
    );
    let text = projects_toml(&fixture);
    assert!(text.contains("# Shared between the machines."), "{text}");
    assert!(text.contains("# per machine"), "{text}");
    assert!(text.contains("laptop = \"~/code/site\""), "{text}");
    let listing = fixture.call("proj.list", json!({})).await.unwrap();
    let site = entry(&listing, "site");
    assert_eq!(site["path"], json!("~/projects/site"));
    assert_eq!(site["present"], json!(true));

    // A new project; its path must exist unless it can be cloned there.
    let missing = fixture
        .call("proj.add", json!({"name": "notes", "path": "~/notes"}))
        .await
        .unwrap_err();
    assert_eq!(missing.code, ErrorCode::NotFound, "{missing:?}");
    assert!(missing.hint.as_deref().unwrap().contains("proj.clone"));
    let added = fixture
        .call(
            "proj.add",
            json!({"name": "notes", "path": "~/notes", "repo": "https://git.example/me/notes.git", "description": "Notes"}),
        )
        .await
        .unwrap();
    assert_eq!(added["outcome"], json!("added"));
    let listing = fixture.call("proj.list", json!({})).await.unwrap();
    assert_eq!(names(&listing), ["notes", "site"]);
    let notes = entry(&listing, "notes");
    assert_eq!(notes["present"], json!(false));
    assert_eq!(notes["description"], json!("Notes"));

    for (args, code) in [
        (json!({"name": "bad name"}), ErrorCode::InvalidArgs),
        (
            json!({"name": "x", "path": "relative/dir"}),
            ErrorCode::InvalidArgs,
        ),
        (json!({"name": "x", "repo": " "}), ErrorCode::InvalidArgs),
        (
            json!({"name": "x", "repo": "ext::sh -c x"}),
            ErrorCode::InvalidArgs,
        ),
        (
            json!({"name": "x", "colour": "red"}),
            ErrorCode::InvalidArgs,
        ),
    ] {
        let error = fixture.call("proj.add", args.clone()).await.unwrap_err();
        assert_eq!(error.code, code, "{args}: {error:?}");
    }

    // A project that is not here says where it is; an unknown one names the known ones.
    let elsewhere = fixture
        .call(
            "proj.add",
            json!({"name": "away", "repo": "https://git.example/me/away.git"}),
        )
        .await
        .unwrap();
    assert_eq!(elsewhere["outcome"], json!("added"));
    let not_here = fixture
        .call("proj.status", json!({"project": "away"}))
        .await
        .unwrap_err();
    assert_eq!(not_here.code, ErrorCode::NotFound);
    assert!(
        not_here.message.contains("does not live on tower"),
        "{}",
        not_here.message
    );
    let unknown = fixture
        .call("proj.status", json!({"project": "nothing"}))
        .await
        .unwrap_err();
    assert_eq!(unknown.code, ErrorCode::NotFound);
    assert!(
        unknown
            .hint
            .as_deref()
            .unwrap()
            .contains("away, notes, site"),
        "{unknown:?}"
    );

    // A file that does not parse is reported and never overwritten.
    fixture.write_config("projects.toml", "[projects.site]\nrepository = 1\n");
    let broken = fixture
        .call(
            "proj.add",
            json!({"name": "x", "repo": "https://git.example/x.git"}),
        )
        .await
        .unwrap_err();
    assert_eq!(broken.code, ErrorCode::Failed);
    assert!(broken.message.contains("line 2"), "{}", broken.message);
    assert_eq!(projects_toml(&fixture), "[projects.site]\nrepository = 1\n");
    let doctor = fixture.call("meta.doctor", json!({})).await.unwrap();
    assert!(doctor.to_string().contains("projects.toml"), "{doctor}");
}

async fn remove_forgets_here_or_everywhere() {
    let fixture = tower();
    fixture.write_config(
        "projects.toml",
        "[projects.site.on]\ntower = \"~/site\"\nlaptop = \"~/code/site\"\n\n[projects.other.on]\nlaptop = \"~/other\"\n",
    );
    let site = fixture.home().join("site");
    std::fs::create_dir_all(&site).unwrap();
    let removed = fixture
        .call("proj.remove", json!({"name": "site"}))
        .await
        .unwrap();
    assert_eq!(removed, json!({"name": "site", "removed": "here"}));
    let listing = fixture.call("proj.list", json!({})).await.unwrap();
    assert_eq!(entry(&listing, "site")["elsewhere"], json!(["laptop"]));
    assert!(site.is_dir(), "removing a project never deletes its files");

    let again = fixture
        .call("proj.remove", json!({"name": "site"}))
        .await
        .unwrap_err();
    assert_eq!(again.code, ErrorCode::NotFound);
    assert!(
        again.hint.as_deref().unwrap().contains("everywhere"),
        "{again:?}"
    );

    let removed = fixture
        .call("proj.remove", json!({"name": "site", "everywhere": true}))
        .await
        .unwrap();
    assert_eq!(removed["removed"], json!("everywhere"));
    let listing = fixture.call("proj.list", json!({})).await.unwrap();
    assert_eq!(names(&listing), ["other"]);
    let unknown = fixture
        .call("proj.remove", json!({"name": "site"}))
        .await
        .unwrap_err();
    assert_eq!(unknown.code, ErrorCode::NotFound);
}

async fn status_reports_clean_dirty_and_ahead_repositories() {
    let fixture = tower();
    let root = fixture.dir.path().join("remotes");
    std::fs::create_dir_all(&root).unwrap();
    let remote = bare(&root, "app");
    let projects = fixture.home().join("projects");
    std::fs::create_dir_all(&projects).unwrap();
    let clean = projects.join("clean one");
    checkout(&remote, &clean);
    let dirty = projects.join("dirty");
    checkout(&remote, &dirty);
    let ahead = projects.join("ahead");
    checkout(&remote, &ahead);
    let plain = projects.join("plain");
    std::fs::create_dir_all(&plain).unwrap();
    let fresh = projects.join("fresh");
    std::fs::create_dir_all(&fresh).unwrap();
    git(&fresh, &["init", "--quiet", "--initial-branch=main"]);

    std::fs::write(dirty.join("README.md"), "changed\n").unwrap();
    std::fs::write(dirty.join("staged.txt"), "new\n").unwrap();
    git(&dirty, &["add", "staged.txt"]);
    std::fs::write(dirty.join("loose file.txt"), "untracked\n").unwrap();
    let ahead_head = commit(&ahead, "more.txt", "more\n");
    std::fs::write(ahead.join("README.md"), "set aside\n").unwrap();
    git(&ahead, &["stash", "--quiet"]);

    let mut text = String::new();
    for (name, dir) in [
        ("clean", "clean one"),
        ("dirty", "dirty"),
        ("ahead", "ahead"),
        ("plain", "plain"),
        ("fresh", "fresh"),
        ("gone", "gone"),
    ] {
        text.push_str(&format!(
            "[projects.{name}.on]\ntower = \"~/projects/{dir}\"\n"
        ));
    }
    text.push_str("[projects.away.on]\nlaptop = \"~/away\"\n");
    fixture.write_config("projects.toml", &text);

    let status = fixture.call("proj.status", json!({})).await.unwrap();
    assert_eq!(
        names(&status),
        ["ahead", "clean", "dirty", "fresh", "gone", "plain"],
        "{status:#}"
    );

    let clean_state = &entry(&status, "clean")["git"];
    assert_eq!(clean_state["branch"], json!("main"), "{status:#}");
    assert_eq!(clean_state["detached"], json!(false));
    assert_eq!(clean_state["upstream"], json!("origin/main"));
    assert_eq!(
        (clean_state["ahead"].clone(), clean_state["behind"].clone()),
        (json!(0), json!(0))
    );
    assert_eq!(clean_state["clean"], json!(true));
    assert_eq!(clean_state["stash"], json!(0));
    assert_eq!(clean_state["last_commit"]["subject"], json!("First commit"));
    assert_eq!(clean_state["last_commit"]["author"], json!("Me"));
    assert!(
        clean_state["last_commit"]["time"]
            .as_str()
            .unwrap()
            .ends_with('Z')
    );
    assert_eq!(entry(&status, "clean")["path"], json!(canonical(&clean)));

    let dirty_state = &entry(&status, "dirty")["git"];
    assert_eq!(dirty_state["clean"], json!(false));
    assert_eq!(
        dirty_state["dirty"],
        json!({"staged": 1, "unstaged": 1, "untracked": 1, "conflicted": 0})
    );

    let ahead_state = &entry(&status, "ahead")["git"];
    assert_eq!(ahead_state["ahead"], json!(1));
    assert_eq!(ahead_state["behind"], json!(0));
    assert_eq!(ahead_state["head"], json!(ahead_head));
    assert_eq!(ahead_state["last_commit"]["hash"], json!(ahead_head));
    assert_eq!(ahead_state["stash"], json!(1));
    assert_eq!(ahead_state["clean"], json!(true));

    let plain_entry = entry(&status, "plain");
    assert_eq!(plain_entry["present"], json!(true));
    assert_eq!(plain_entry.get("git"), None, "{plain_entry:#}");
    assert_eq!(plain_entry.get("error"), None, "{plain_entry:#}");

    let fresh_state = &entry(&status, "fresh")["git"];
    assert_eq!(fresh_state["branch"], json!("main"));
    assert_eq!(fresh_state.get("head"), None);
    assert_eq!(fresh_state.get("last_commit"), None);

    assert_eq!(entry(&status, "gone")["present"], json!(false));

    // A detached head, asked for by name.
    git(&clean, &["checkout", "--quiet", "--detach"]);
    let one = fixture
        .call("proj.status", json!({"project": "clean"}))
        .await
        .unwrap();
    assert_eq!(names(&one), ["clean"]);
    assert_eq!(one["projects"][0]["git"]["detached"], json!(true));
    assert_eq!(one["projects"][0]["git"].get("branch"), None);
}

async fn clone_fetches_from_a_local_bare_repository() {
    let fixture = tower();
    let root = fixture.dir.path().join("remotes");
    std::fs::create_dir_all(&root).unwrap();
    let remote = bare(&root, "lib");
    let remote_head = git(&remote, &["rev-parse", "HEAD"]);
    fixture
        .call(
            "proj.add",
            json!({"name": "lib", "repo": remote.to_str().unwrap()}),
        )
        .await
        .unwrap();

    let events = Events::default();
    let cloned = fixture
        .call_with(
            "proj.clone",
            json!({"project": "lib"}),
            &events,
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let destination = fixture.home().join("projects").join("lib");
    assert_eq!(cloned["name"], json!("lib"));
    assert_eq!(cloned["path"], json!(canonical(&destination)));
    assert_eq!(cloned["head"], json!(remote_head));
    assert!(destination.join("README.md").is_file());
    assert!(
        events.text(EventKind::Progress).contains("Cloning"),
        "no progress was reported"
    );
    let listing = fixture.call("proj.list", json!({})).await.unwrap();
    assert_eq!(entry(&listing, "lib")["path"], json!("~/projects/lib"));
    assert_eq!(entry(&listing, "lib")["present"], json!(true));

    let again = fixture
        .call("proj.clone", json!({"project": "lib"}))
        .await
        .unwrap_err();
    assert_eq!(again.code, ErrorCode::Exists, "{again:?}");

    // Into an empty directory given as `path`, which is recorded.
    let empty = fixture.home().join("elsewhere");
    std::fs::create_dir_all(&empty).unwrap();
    let cloned = fixture
        .call(
            "proj.clone",
            json!({"project": "lib", "path": "~/elsewhere"}),
        )
        .await
        .unwrap();
    assert_eq!(cloned["head"], json!(remote_head));
    let listing = fixture.call("proj.list", json!({})).await.unwrap();
    assert_eq!(entry(&listing, "lib")["path"], json!("~/elsewhere"));

    // A repository that does not exist fails and leaves nothing behind.
    fixture
        .call(
            "proj.add",
            json!({"name": "ghost", "repo": root.join("ghost.git").to_str().unwrap()}),
        )
        .await
        .unwrap();
    let missing = fixture
        .call("proj.clone", json!({"project": "ghost"}))
        .await
        .unwrap_err();
    assert_eq!(missing.code, ErrorCode::NotFound, "{missing:?}");
    assert!(!fixture.home().join("projects").join("ghost").exists());

    fixture
        .call("proj.add", json!({"name": "norepo", "path": "~/elsewhere"}))
        .await
        .unwrap();
    let no_repo = fixture
        .call("proj.clone", json!({"project": "norepo"}))
        .await
        .unwrap_err();
    assert_eq!(no_repo.code, ErrorCode::NotFound);
    assert!(no_repo.message.contains("no repo"), "{}", no_repo.message);
}

async fn pull_fast_forwards_and_refuses_to_merge() {
    let fixture = tower();
    let root = fixture.dir.path().join("remotes");
    std::fs::create_dir_all(&root).unwrap();
    let remote = bare(&root, "app");
    let upstream = root.join("upstream");
    checkout(&remote, &upstream);
    let here = fixture.home().join("app");
    checkout(&remote, &here);
    fixture.write_config("projects.toml", "[projects.app.on]\ntower = \"~/app\"\n");

    let unchanged = fixture
        .call("proj.pull", json!({"project": "app"}))
        .await
        .unwrap();
    assert_eq!(unchanged["updated"], json!(false), "{unchanged:#}");
    assert_eq!(unchanged["before"], unchanged["after"]);
    assert_eq!(unchanged["summary"], json!("Already up to date."));

    let before = git(&here, &["rev-parse", "HEAD"]);
    let pushed = commit(&upstream, "new.txt", "one\ntwo\n");
    git(&upstream, &["push", "--quiet", "origin", "main"]);
    let pulled = fixture
        .call("proj.pull", json!({"project": "app"}))
        .await
        .unwrap();
    assert_eq!(pulled["updated"], json!(true), "{pulled:#}");
    assert_eq!(pulled["before"], json!(before));
    assert_eq!(pulled["after"], json!(pushed));
    assert!(
        pulled["summary"]
            .as_str()
            .unwrap()
            .contains("1 file changed"),
        "{pulled:#}"
    );
    assert!(here.join("new.txt").is_file());

    // Diverged: a local commit and a different one upstream.
    let local = commit(&here, "local.txt", "mine\n");
    commit(&upstream, "theirs.txt", "theirs\n");
    git(&upstream, &["push", "--quiet", "origin", "main"]);
    let refused = fixture
        .call("proj.pull", json!({"project": "app"}))
        .await
        .unwrap_err();
    assert_eq!(refused.code, ErrorCode::Conflict, "{refused:?}");
    let hint = refused.hint.as_deref().unwrap();
    assert!(hint.contains("git said:"), "{hint}");
    assert!(hint.to_lowercase().contains("fast-forward"), "{hint}");
    assert_eq!(
        git(&here, &["rev-parse", "HEAD"]),
        local,
        "a refused pull changes nothing"
    );

    let missing = fixture
        .call("proj.pull", json!({"project": "nothing"}))
        .await
        .unwrap_err();
    assert_eq!(missing.code, ErrorCode::NotFound);
}

async fn run_starts_a_task_by_argv_and_as_a_job() {
    let fixture = tower();
    let dir = fixture.home().join("projects").join("tool");
    std::fs::create_dir_all(&dir).unwrap();
    let program = toml_string(&this_binary());
    fixture.write_config(
        "projects.toml",
        &format!(
            "[projects.tool.on]\ntower = \"~/projects/tool\"\n\n[projects.tool.tasks]\necho = {{ argv = [{program}, \"{TASK_HELPER}\", \"fixed\"], timeout_s = 60 }}\nbackground = {{ argv = [{program}, \"{TASK_HELPER}\", \"in the background\"], job = true }}\n"
        ),
    );

    let events = Events::default();
    let ran = fixture
        .call_with(
            "proj.run",
            json!({"project": "tool", "task": "echo", "args": ["a b", "$HOME", "'quoted'"]}),
            &events,
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(ran["project"], json!("tool"));
    assert_eq!(ran["task"], json!("echo"));
    assert_eq!(ran["exit_code"], json!(0), "{ran:#}");
    let lines: Vec<&str> = ran["stdout"].as_str().unwrap().lines().collect();
    assert_eq!(
        lines,
        [
            format!("cwd={}", canonical(&dir)).as_str(),
            "fixed",
            "a b",
            "$HOME",
            "'quoted'"
        ]
    );
    assert!(events.text(EventKind::Stdout).contains("fixed"));
    assert_eq!(ran["timed_out"], json!(false));

    let failed = fixture
        .call(
            "proj.run",
            json!({"project": "tool", "task": "echo", "args": ["fail"]}),
        )
        .await
        .unwrap();
    assert_eq!(
        failed["exit_code"],
        json!(3),
        "a failing task is a result, not an error"
    );

    let started = fixture
        .call("proj.run", json!({"project": "tool", "task": "background"}))
        .await
        .unwrap();
    assert_eq!(started["task"], json!("background"));
    assert!(started["hint"].as_str().unwrap().contains("job.logs"));
    let id = started["job"].as_str().unwrap().to_owned();
    let waited = fixture
        .call("job.wait", json!({"id": id, "timeout_s": 30}))
        .await
        .unwrap();
    assert_eq!(waited["state"], json!("succeeded"), "{waited:#}");
    assert_eq!(waited["name"], json!("tool.background"));
    let logs = fixture.call("job.logs", json!({"id": id})).await.unwrap();
    assert!(
        logs["content"]
            .as_str()
            .unwrap()
            .contains("in the background"),
        "{logs:#}"
    );

    let unknown = fixture
        .call("proj.run", json!({"project": "tool", "task": "deploy"}))
        .await
        .unwrap_err();
    assert_eq!(unknown.code, ErrorCode::NotFound);
    assert!(
        unknown
            .hint
            .as_deref()
            .unwrap()
            .contains("background, echo"),
        "{unknown:?}"
    );
}

async fn discover_finds_and_adds_working_trees() {
    let fixture = tower();
    let code = fixture.home().join("code");
    let site = code.join("site");
    repo(&site);
    git(
        &site,
        &["remote", "add", "origin", "https://git.example/me/site.git"],
    );
    repo(&code.join("site").join("vendor-inside"));
    repo(&code.join("group").join("api"));
    repo(&code.join("other").join("site"));
    repo(&code.join("node_modules").join("dep"));
    repo(&code.join(".cache").join("hidden"));
    repo(&code.join("a").join("b").join("c").join("deep"));
    std::fs::create_dir_all(code.join("empty")).unwrap();
    fixture.write_config(
        "projects.toml",
        "[projects.api.on]\ntower = \"~/code/group/api\"\n",
    );

    let found = fixture
        .call("proj.discover", json!({"root": "~/code"}))
        .await
        .unwrap();
    let entries = found["found"].as_array().unwrap();
    let summary: Vec<(String, String, bool)> = entries
        .iter()
        .map(|entry| {
            (
                entry["name"].as_str().unwrap().to_owned(),
                Path::new(entry["path"].as_str().unwrap())
                    .strip_prefix(canonical(&code))
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/"),
                entry["already_listed"].as_bool().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        summary,
        [
            ("api".to_owned(), "group/api".to_owned(), true),
            ("site-2".to_owned(), "other/site".to_owned(), false),
            ("site".to_owned(), "site".to_owned(), false),
        ],
        "{found:#}"
    );
    assert_eq!(found["truncated"], json!(false));
    assert_eq!(entries[2]["repo"], json!("https://git.example/me/site.git"));
    assert_eq!(entries[1].get("repo"), None);
    assert_eq!(found.get("added"), None);
    assert!(
        !projects_toml(&fixture).contains("site"),
        "discovering alone records nothing"
    );

    let deeper = fixture
        .call("proj.discover", json!({"root": "~/code", "max_depth": 4}))
        .await
        .unwrap();
    assert_eq!(deeper["found"].as_array().unwrap().len(), 4);
    let too_deep = fixture
        .call("proj.discover", json!({"root": "~/code", "max_depth": 7}))
        .await
        .unwrap_err();
    assert_eq!(too_deep.code, ErrorCode::InvalidArgs);

    let added = fixture
        .call("proj.discover", json!({"root": "~/code", "add": true}))
        .await
        .unwrap();
    assert_eq!(added["added"], json!(["site-2", "site"]));
    let listing = fixture.call("proj.list", json!({})).await.unwrap();
    assert_eq!(names(&listing), ["api", "site", "site-2"]);
    assert_eq!(entry(&listing, "site")["path"], json!("~/code/site"));
    assert_eq!(
        entry(&listing, "site")["repo"],
        json!("https://git.example/me/site.git")
    );
    let again = fixture
        .call("proj.discover", json!({"root": "~/code", "add": true}))
        .await
        .unwrap();
    assert_eq!(again["added"], json!([]));
    assert!(
        again["found"]
            .as_array()
            .unwrap()
            .iter()
            .all(|entry| entry["already_listed"] == json!(true)),
        "{again:#}"
    );
}

async fn a_confined_role_sees_only_its_paths() {
    let fixture = tower();
    fixture.write_config(
        "policy.toml",
        "[principals.\"agent:claude\"]\nrole = \"builder\"\n[roles.builder]\nallow = [\"effect:read\", \"effect:write\", \"effect:exec\"]\npaths = [\"~/projects/**\"]\n",
    );
    let inside = fixture.home().join("projects").join("in");
    repo(&inside);
    let outside = fixture.home().join("private").join("out");
    repo(&outside);
    fixture.write_config(
        "projects.toml",
        "[projects.in.on]\ntower = \"~/projects/in\"\n[projects.out.on]\ntower = \"~/private/out\"\n",
    );
    let agent = "agent:claude";

    let status = fixture
        .call_as(agent, "proj.status", json!({}))
        .await
        .unwrap();
    assert_eq!(
        entry(&status, "in")["git"]["clean"],
        json!(true),
        "{status:#}"
    );
    let out = entry(&status, "out");
    assert_eq!(out.get("git"), None);
    assert_eq!(out["error"]["code"], json!("denied"), "{out:#}");
    assert!(
        out["error"]["message"]
            .as_str()
            .unwrap()
            .contains("confines"),
        "{out:#}"
    );
    let listing = fixture
        .call_as(agent, "proj.list", json!({}))
        .await
        .unwrap();
    assert_eq!(entry(&listing, "out")["present"], json!(false));

    for (tool, args) in [
        ("proj.status", json!({"project": "out"})),
        ("proj.pull", json!({"project": "out"})),
        ("proj.add", json!({"name": "x", "path": "~/private/out"})),
        ("proj.discover", json!({"root": "~/private"})),
    ] {
        let error = fixture.call_as(agent, tool, args).await.unwrap_err();
        assert_eq!(error.code, ErrorCode::Denied, "{tool}: {error:?}");
    }
    let found = fixture
        .call_as(agent, "proj.discover", json!({"root": "~/projects"}))
        .await
        .unwrap();
    assert_eq!(found["found"][0]["name"], json!("in"));
}

async fn a_role_without_the_effect_is_denied() {
    let fixture = tower();
    fixture.write_config(
        "policy.toml",
        concat!(
            "[principals.observer]\nrole = \"observer\"\n",
            "[principals.worker]\nrole = \"worker\"\n",
            "[roles.observer]\nallow = [\"effect:read\"]\n",
            "[roles.worker]\nallow = [\"effect:read\", \"effect:exec\"]\ndeny = [\"job.*\"]\n",
        ),
    );
    let dir = fixture.home().join("app");
    std::fs::create_dir_all(&dir).unwrap();
    let program = toml_string(&this_binary());
    fixture.write_config(
        "projects.toml",
        &format!(
            "[projects.app.on]\ntower = \"~/app\"\n[projects.app.tasks]\nserve = {{ argv = [{program}, \"{TASK_HELPER}\"], job = true }}\n"
        ),
    );

    let listing = fixture
        .call_as("observer", "proj.list", json!({}))
        .await
        .unwrap();
    assert_eq!(names(&listing), ["app"]);
    fixture
        .call_as("observer", "proj.discover", json!({"root": "~"}))
        .await
        .unwrap();
    for (tool, args) in [
        ("proj.add", json!({"name": "x", "path": "~/app"})),
        ("proj.remove", json!({"name": "app"})),
        ("proj.clone", json!({"project": "app"})),
        ("proj.pull", json!({"project": "app"})),
        ("proj.run", json!({"project": "app", "task": "serve"})),
        ("proj.discover", json!({"root": "~", "add": true})),
    ] {
        let error = fixture.call_as("observer", tool, args).await.unwrap_err();
        assert_eq!(error.code, ErrorCode::Denied, "{tool}: {error:?}");
    }
    assert!(!projects_toml(&fixture).contains("[projects.x"));

    // proj.run is allowed, but a job task also needs job.start.
    let error = fixture
        .call_as(
            "worker",
            "proj.run",
            json!({"project": "app", "task": "serve"}),
        )
        .await
        .unwrap_err();
    assert_eq!(error.code, ErrorCode::Denied, "{error:?}");
    assert!(error.message.contains("job.start"), "{}", error.message);
}

/// A command line, for git to run through its shell, that leaves a marker file named `name`.
fn marker_command(dir: &Path, name: &str) -> String {
    let quote = |text: &str| format!("'{}'", text.replace('\'', "'\\''"));
    format!(
        "{} {MARKER} {}",
        quote(&this_binary()),
        quote(&dir.join(name).to_string_lossy())
    )
}

async fn reading_a_hostile_repository_runs_nothing() {
    let fixture = tower();
    let markers = fixture.dir.path().join("markers");
    std::fs::create_dir_all(&markers).unwrap();
    let hostile = fixture.home().join("projects").join("hostile");
    repo(&hostile);
    std::fs::write(hostile.join(".gitattributes"), "* filter=evil\n").unwrap();
    std::fs::write(hostile.join("data.txt"), "data\n").unwrap();
    git(&hostile, &["add", ".gitattributes", "data.txt"]);
    git(&hostile, &["commit", "--quiet", "-m", "Add data"]);
    let hooks = fixture.dir.path().join("hooks");
    std::fs::create_dir_all(&hooks).unwrap();
    for hook in [
        "post-index-change",
        "reference-transaction",
        "post-checkout",
    ] {
        std::fs::write(
            hooks.join(hook),
            format!("#!/bin/sh\n{}\n", marker_command(&markers, hook)),
        )
        .unwrap();
    }
    let included = fixture.dir.path().join("included.cfg");
    std::fs::write(
        &included,
        format!(
            "[filter \"evil\"]\n\tsmudge = {0}\n[core]\n\tfsmonitor = {1}\n",
            marker_command(&markers, "included")
                .replace('\\', "\\\\")
                .replace('"', "\\\""),
            marker_command(&markers, "included-monitor")
                .replace('\\', "\\\\")
                .replace('"', "\\\""),
        ),
    )
    .unwrap();
    for (key, value) in [
        ("filter.evil.clean", marker_command(&markers, "clean")),
        ("filter.evil.process", marker_command(&markers, "process")),
        ("filter.evil.required", "true".to_owned()),
        ("core.fsmonitor", marker_command(&markers, "monitor")),
        ("core.hooksPath", hooks.to_string_lossy().into_owned()),
        ("core.pager", marker_command(&markers, "pager")),
        ("include.path", included.to_string_lossy().into_owned()),
    ] {
        git(&hostile, &["config", key, &value]);
    }
    // A file whose times differ from the index makes status read it again, through its filter.
    let old = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000_000);
    for file in ["data.txt", "README.md"] {
        std::fs::File::options()
            .write(true)
            .open(hostile.join(file))
            .unwrap()
            .set_modified(old)
            .unwrap();
    }
    fixture.write_config(
        "projects.toml",
        "[projects.hostile.on]\ntower = \"~/projects/hostile\"\n",
    );

    let status = fixture.call("proj.status", json!({})).await.unwrap();
    let state = &entry(&status, "hostile")["git"];
    assert_eq!(state["branch"], json!("main"), "{status:#}");
    let found = fixture
        .call("proj.discover", json!({"root": "~/projects"}))
        .await
        .unwrap();
    assert_eq!(found["found"][0]["name"], json!("hostile"), "{found:#}");
    let ran: Vec<String> = std::fs::read_dir(&markers)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert!(ran.is_empty(), "reading the repository ran {ran:?}");
}

async fn git_indirection_does_not_lead_a_confined_role_outside() {
    let fixture = tower();
    fixture.write_config(
        "policy.toml",
        "[principals.\"agent:claude\"]\nrole = \"builder\"\n[roles.builder]\nallow = [\"effect:read\", \"effect:write\", \"effect:exec\"]\npaths = [\"~/projects/**\"]\n",
    );
    let agent = "agent:claude";
    let private = fixture.home().join("private");
    let real = private.join("real");
    repo(&real);
    commit(&real, "secret.txt", "secret\n");
    git(
        &real,
        &["commit", "--quiet", "--amend", "-m", "Secret subject"],
    );
    let projects = fixture.home().join("projects");
    let linked = projects.join("linked");
    std::fs::create_dir_all(&linked).unwrap();
    std::fs::write(
        linked.join(".git"),
        format!("gitdir: {}\n", real.join(".git").display()),
    )
    .unwrap();
    let included = projects.join("included");
    repo(&included);
    let secret_config = private.join("secret.cfg");
    std::fs::write(
        &secret_config,
        "[remote \"origin\"]\n\turl = https://git.example/me/secret.git\n",
    )
    .unwrap();
    git(
        &included,
        &["config", "include.path", secret_config.to_str().unwrap()],
    );
    fixture.write_config(
        "projects.toml",
        "[projects.linked.on]\ntower = \"~/projects/linked\"\n[projects.included.on]\ntower = \"~/projects/included\"\n",
    );

    let status = fixture
        .call_as(agent, "proj.status", json!({}))
        .await
        .unwrap();
    let text = status.to_string();
    assert!(!text.contains("Secret subject"), "{status:#}");
    let linked_entry = entry(&status, "linked");
    assert_eq!(linked_entry.get("git"), None, "{status:#}");
    assert_eq!(linked_entry["error"]["code"], json!("denied"), "{status:#}");
    assert_eq!(entry(&status, "included")["git"]["clean"], json!(true));

    let found = fixture
        .call_as(agent, "proj.discover", json!({"root": "~/projects"}))
        .await
        .unwrap();
    assert!(!found.to_string().contains("secret.git"), "{found:#}");
    let linked_found = found["found"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["name"] == json!("linked"))
        .unwrap();
    assert_eq!(linked_found["error"]["code"], json!("denied"), "{found:#}");

    let pulled = fixture
        .call_as(agent, "proj.pull", json!({"project": "linked"}))
        .await
        .unwrap_err();
    assert_eq!(pulled.code, ErrorCode::Denied, "{pulled:?}");

    // The owner, who may go anywhere, still sees through the link.
    let owner = fixture
        .call("proj.status", json!({"project": "linked"}))
        .await
        .unwrap();
    assert_eq!(
        owner["projects"][0]["git"]["last_commit"]["subject"],
        json!("Secret subject"),
        "{owner:#}"
    );
}

async fn clone_takes_only_repositories_it_may_read() {
    let fixture = tower();
    fixture.write_config(
        "policy.toml",
        concat!(
            "[principals.\"agent:claude\"]\nrole = \"builder\"\n",
            "[principals.worker]\nrole = \"worker\"\n",
            "[roles.builder]\nallow = [\"effect:read\", \"effect:write\", \"effect:exec\"]\npaths = [\"~/projects/**\"]\n",
            "[roles.worker]\nallow = [\"effect:read\", \"effect:write\", \"effect:exec\"]\n",
        ),
    );
    let remotes = fixture.dir.path().join("remotes");
    std::fs::create_dir_all(&remotes).unwrap();
    let outside = bare(&remotes, "lib");
    let outside_url = format!(
        "file:///{}",
        outside
            .to_string_lossy()
            .replace('\\', "/")
            .trim_start_matches('/')
    );
    let mut text = String::new();
    for (name, repo) in [
        ("ext", "ext::sh -c touch% /x".to_owned()),
        ("option", "--upload-pack=touch /x".to_owned()),
        ("plain", "http://git.example/me/x.git".to_owned()),
        ("relative", "remotes/lib.git".to_owned()),
        ("path", outside.to_string_lossy().into_owned()),
        ("url", outside_url.clone()),
    ] {
        text.push_str(&format!(
            "[projects.{name}]\nrepo = {}\n",
            toml_string(&repo)
        ));
    }
    fixture.write_config("projects.toml", &text);

    for name in ["ext", "option", "plain", "relative"] {
        let error = fixture
            .call("proj.clone", json!({"project": name}))
            .await
            .unwrap_err();
        assert_eq!(error.code, ErrorCode::InvalidArgs, "{name}: {error:?}");
        assert!(
            !fixture.home().join("projects").join(name).exists(),
            "{name}"
        );
    }
    // A local repository outside the confined role's paths cannot be read through a clone.
    for name in ["path", "url"] {
        let error = fixture
            .call_as("agent:claude", "proj.clone", json!({"project": name}))
            .await
            .unwrap_err();
        assert_eq!(error.code, ErrorCode::Denied, "{name}: {error:?}");
    }
    let cloned = fixture
        .call("proj.clone", json!({"project": "url"}))
        .await
        .unwrap();
    assert_eq!(cloned["name"], json!("url"), "{cloned:#}");

    // Legion's own configuration is not a place to clone into without admin.
    let config = fixture.dir.path().join("config").join("lib");
    let error = fixture
        .call_as(
            "worker",
            "proj.clone",
            json!({"project": "path", "path": config.to_str().unwrap()}),
        )
        .await
        .unwrap_err();
    assert_eq!(error.code, ErrorCode::Denied, "{error:?}");
    assert!(!config.exists());
}
