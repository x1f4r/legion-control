//! `proj`: named projects, where each lives, what state it is in, and the tasks it offers.
//!
//! `projects.toml` records every project with its path per node name, so one file can be shared
//! between machines. Each tool works on the node it runs on and reads the path recorded for that
//! node; looking at a project on several machines is the caller's `on`, which the router fans
//! out, so nothing here knows about other machines beyond the names in the file.

mod discover;
mod file;
mod git;

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use legion_core::proto::{Effect, ErrorBody, ErrorCode, EventKind};
use legion_core::{Access, CancellationToken, Ctx, Grant, Registry, ToolError, parse_args};
use legion_os::run::OutputChunk;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::{Semaphore, mpsc};
use tokio::task::JoinSet;

pub use file::{Project, ProjectsFile, Task};
pub use git::{Commit, Dirty, Porcelain, RepoState, parse_porcelain};

use crate::exec::{self, Ran};
use crate::job::{self, JobState};
use file::Edit;
use git::{Git, Mode};

/// The longest `proj.status` spends on one project.
const PROJECT_TIMEOUT: Duration = Duration::from_secs(30);
/// How many projects `proj.status` looks at at once.
const STATUS_PARALLEL: usize = 8;
/// How long a clone may take.
const CLONE_TIMEOUT: Duration = Duration::from_secs(1800);
/// How long a pull may take.
const PULL_TIMEOUT: Duration = Duration::from_secs(600);
/// How long a task that is not a job may run when it does not say.
const TASK_TIMEOUT_S: f64 = 600.0;
/// The default and the deepest `max_depth` of `proj.discover`.
const DISCOVER_DEPTH: (u32, u32) = (3, 6);

/// Adds the `proj` tools and makes `projects.toml` known.
pub fn register(registry: &mut Registry) {
    registry.add_document::<ProjectsFile>();
    registry.add_fn(
        "proj.list",
        Effect::Read,
        "List the projects recorded in projects.toml: each one's name, description, repo, the path it has on this machine and whether that path exists, the other machines it lives on, and the names of its tasks. Every other proj tool takes a project by the name shown here. Use proj.status to see the git state of the ones that live here, and `on` to ask another machine.",
        list,
    );
    registry.add_fn(
        "proj.status",
        Effect::Read,
        "Show the git state of a project on this machine, or of every project that lives here when `project` is left out: branch, commit, upstream and how far ahead or behind it is (as of the last fetch), how many files are staged, changed, untracked or in conflict, the last commit and the stash. A project whose directory is not a git repository has no `git`; one that cannot be read has an `error`, and never stops the others from being reported. Nothing is fetched and nothing is changed.",
        status,
    );
    registry.add_fn(
        "proj.add",
        Effect::Write,
        "Record a project in projects.toml, or change one. With `path`, records where it lives on this machine; the directory must exist unless the project has a `repo` that proj.clone can fetch into it. Paths the project has on other machines are left as they are. `repo` and `description` are set when given. Comments and everything else in the file are kept.",
        add,
    );
    registry.add_fn(
        "proj.remove",
        Effect::Write,
        "Forget where a project lives on this machine, or with `everywhere: true` forget the whole project on every machine. Only projects.toml changes: no file of the project is ever deleted.",
        remove,
    );
    registry.add_fn(
        "proj.clone",
        Effect::Exec,
        "Clone a project's `repo` onto this machine and record where it went. The destination is `path` when given, else the path already recorded for this machine, else ~/projects/<name>; it must not exist or must be an empty directory. Reports git's progress as progress events and returns the commit checked out. Git never prompts: a repository that needs credentials git cannot find on its own fails instead of waiting. Git may run hooks and helpers (credential helpers, ssh) that the repository or the user configured.",
        clone,
    );
    registry.add_fn(
        "proj.pull",
        Effect::Exec,
        "Update a project on this machine from its upstream with `git pull --ff-only`, which only ever moves the branch forward. When the branches have diverged or local changes are in the way, nothing changes and the call fails with `conflict`, carrying git's own explanation in the hint. Returns the commit before and after and a summary of what changed. Git may run hooks and helpers (credential helpers, ssh) that the repository or the user configured.",
        pull,
    );
    registry.add_fn(
        "proj.run",
        Effect::Exec,
        "Run one of a project's tasks (see proj.list) in its directory on this machine. `args` are added to the task's command as separate arguments, never spliced into a command line. A plain task runs to the end and returns what exec.run returns (exit code, output, whether it timed out), with output streamed as events; it is stopped after its `timeout_s`, 600 seconds unless the task says. A task marked `job = true` starts as a background job and returns its id at once: follow it with job.logs and job.wait.",
        run_task,
    );
    registry.add_fn(
        "proj.discover",
        Effect::Read,
        "Find git working trees under `root`, down to `max_depth` levels (3 unless given, at most 6), without descending into a working tree, hidden directories or dependency and build directories such as node_modules and target. Each one found has a suggested project name, its origin remote and whether projects.toml already lists it here. With `add: true` the ones not yet listed are recorded with their paths on this machine, which needs permission to use proj.add.",
        discover,
    );
}

/// Arguments of `proj.list`.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct ListArgs {}

/// What `proj.list` returns.
#[derive(Debug, Serialize)]
pub struct Listing {
    /// The machine that answered, whose paths `path` and `present` are about.
    pub node: String,
    /// Every project, by name.
    pub projects: Vec<Listed>,
}

/// One project in `proj.list`.
#[derive(Debug, Serialize)]
pub struct Listed {
    /// Its name, which the other proj tools take.
    pub name: String,
    /// What it is.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Where proj.clone fetches it from.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,
    /// Where it lives on this machine, as projects.toml writes it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// Whether that path exists; false too when the caller may not look there.
    pub present: bool,
    /// The other machines it has a path on.
    pub elsewhere: Vec<String>,
    /// The names of its tasks, for proj.run.
    pub tasks: Vec<String>,
}

async fn list(ctx: &Ctx, _args: ListArgs) -> Result<Listing, ToolError> {
    let file = ctx.config().get::<ProjectsFile>()?;
    let node = ctx.node_name();
    let projects = file
        .projects
        .iter()
        .map(|(name, project)| {
            let path = project.on.get(node).cloned();
            let present = path.as_deref().is_some_and(|path| {
                ctx.grant()
                    .check_path(path, Access::Read)
                    .is_ok_and(|path| path.exists())
            });
            Listed {
                name: name.clone(),
                description: project.description.clone(),
                repo: project.repo.clone(),
                path,
                present,
                elsewhere: project
                    .on
                    .keys()
                    .filter(|other| *other != node)
                    .cloned()
                    .collect(),
                tasks: project.tasks.keys().cloned().collect(),
            }
        })
        .collect();
    Ok(Listing {
        node: node.to_owned(),
        projects,
    })
}

/// Arguments of `proj.status`.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct StatusArgs {
    /// The project to look at; every project that lives on this machine when absent.
    #[serde(default)]
    project: Option<String>,
}

/// What `proj.status` returns.
#[derive(Debug, Serialize)]
pub struct Statuses {
    /// One entry per project, in name order.
    pub projects: Vec<ProjectStatus>,
}

/// One project's state in `proj.status`.
#[derive(Debug, Serialize)]
pub struct ProjectStatus {
    /// The project's name.
    pub name: String,
    /// Its directory on this machine.
    pub path: String,
    /// Whether the directory exists.
    pub present: bool,
    /// Its git state, when it is a git repository.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub git: Option<RepoState>,
    /// Why its state could not be read, as an error object with the code to branch on.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<ErrorBody>,
}

async fn status(ctx: &Ctx, args: StatusArgs) -> Result<Statuses, ToolError> {
    let file = ctx.config().get::<ProjectsFile>()?;
    let mut targets: Vec<(String, Target)> = Vec::new();
    match &args.project {
        Some(name) => {
            let (_, written) = here(ctx, &file, name)?;
            let path = ctx.grant().check_path(written, Access::Read)?;
            targets.push((name.clone(), Ok(path)));
        }
        None => {
            for (name, project) in &file.projects {
                if let Some(written) = project.on.get(ctx.node_name()) {
                    let path = ctx
                        .grant()
                        .check_path(written, Access::Read)
                        .map_err(|error| (written.clone(), error));
                    targets.push((name.clone(), path));
                }
            }
        }
    }

    let limit = Arc::new(Semaphore::new(STATUS_PARALLEL));
    let mut running = JoinSet::new();
    let mut results: Vec<Option<ProjectStatus>> = Vec::with_capacity(targets.len());
    for (index, (name, target)) in targets.into_iter().enumerate() {
        results.push(None);
        let path = match target {
            Ok(path) => path,
            Err((written, error)) => {
                results[index] = Some(ProjectStatus {
                    name,
                    path: written,
                    present: false,
                    git: None,
                    error: Some(error),
                });
                continue;
            }
        };
        let limit = Arc::clone(&limit);
        let cancel = ctx.cancel_token().child_token();
        let grant = ctx.grant().clone();
        running.spawn(async move {
            let _turn = limit.acquire_owned().await;
            (index, project_status(name, path, grant, cancel).await)
        });
    }
    while let Some(done) = running.join_next().await {
        let (index, state) = done.map_err(|error| {
            ErrorBody::new(
                ErrorCode::Internal,
                format!("Looking at a project stopped unexpectedly: {error}."),
            )
        })?;
        if let Some(slot) = results.get_mut(index) {
            *slot = Some(state);
        }
    }
    Ok(Statuses {
        projects: results.into_iter().flatten().collect(),
    })
}

/// A project's path for `proj.status`, or the path as written and why it cannot be looked at.
type Target = Result<PathBuf, (String, ErrorBody)>;

async fn project_status(
    name: String,
    path: PathBuf,
    grant: Grant,
    cancel: CancellationToken,
) -> ProjectStatus {
    let shown = path.display().to_string();
    if !path.is_dir() {
        let error = path.exists().then(|| {
            ErrorBody::new(
                ErrorCode::InvalidArgs,
                format!("{shown} is a file, and a project lives in a directory."),
            )
        });
        return ProjectStatus {
            name,
            path: shown,
            present: path.exists(),
            git: None,
            error,
        };
    }
    let state =
        tokio::time::timeout(PROJECT_TIMEOUT, git::repo_state(&path, &grant, &cancel)).await;
    let (git, error) = match state {
        Ok(Ok(git)) => (git, None),
        Ok(Err(error)) => (None, Some(error)),
        Err(_) => (
            None,
            Some(ErrorBody::new(
                ErrorCode::Timeout,
                format!(
                    "git took longer than {} seconds to describe it.",
                    PROJECT_TIMEOUT.as_secs()
                ),
            )),
        ),
    };
    ProjectStatus {
        name,
        path: shown,
        present: true,
        git,
        error,
    }
}

/// Arguments of `proj.add`.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct AddArgs {
    /// The project's name: letters, digits, `-`, `_` and `.`, starting with a letter or digit.
    name: String,
    /// Where it lives on this machine, absolute or starting at `~`.
    #[serde(default)]
    path: Option<String>,
    /// Where proj.clone fetches it from: any URL or path `git clone` accepts.
    #[serde(default)]
    repo: Option<String>,
    /// One line saying what it is.
    #[serde(default)]
    description: Option<String>,
}

/// What `proj.add` returns.
#[derive(Debug, Serialize)]
pub struct Added {
    /// The project's name.
    pub name: String,
    /// `added` for a new project, `updated` for one that was already recorded.
    pub outcome: &'static str,
    /// Its path on this machine, as recorded, when it has one here.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
}

async fn add(ctx: &Ctx, args: AddArgs) -> Result<Added, ToolError> {
    file::check_name("project", &args.name)
        .map_err(|reason| invalid("proj.add", &reason, "name"))?;
    for (field, value) in [("repo", &args.repo), ("description", &args.description)] {
        if value
            .as_deref()
            .is_some_and(|value| value.trim().is_empty())
        {
            return Err(invalid("proj.add", &format!("`{field}` is empty"), field));
        }
    }
    if let Some(repo) = &args.repo {
        repo_source(repo)?;
    }
    let exists = match &args.path {
        Some(path) => {
            let resolved = ctx.grant().check_path(path, Access::Read)?;
            if resolved.exists() && !resolved.is_dir() {
                return Err(ErrorBody::new(
                    ErrorCode::InvalidArgs,
                    format!(
                        "{} is a file, and a project lives in a directory.",
                        resolved.display()
                    ),
                )
                .with_hint("Give the project's top directory."));
            }
            resolved.exists()
        }
        None => true,
    };
    let node = ctx.node_name().to_owned();
    edit(ctx, move |current| {
        let known = current.projects.get(&args.name);
        let has_repo = args.repo.is_some() || known.is_some_and(|project| project.repo.is_some());
        if !exists && !has_repo {
            let path = args.path.as_deref().unwrap_or_default();
            return Err(ErrorBody::new(
                ErrorCode::NotFound,
                format!("{path} does not exist, and {} has no repo to clone into it.", args.name),
            )
            .with_hint("Create the directory first, or give `repo` too and then call proj.clone to fetch it there."));
        }
        let path = args
            .path
            .clone()
            .or_else(|| known.and_then(|project| project.on.get(&node).cloned()));
        let outcome = if known.is_some() { "updated" } else { "added" };
        let edits = vec![Edit::Set {
            name: args.name.clone(),
            node: node.clone(),
            path: args.path,
            repo: args.repo,
            description: args.description,
        }];
        Ok((
            edits,
            Added {
                name: args.name,
                outcome,
                path,
            },
        ))
    })
    .await
}

/// Arguments of `proj.remove`.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct RemoveArgs {
    /// The project's name.
    name: String,
    /// Forget the whole project on every machine, not just where it lives on this one.
    #[serde(default)]
    everywhere: bool,
}

/// What `proj.remove` returns.
#[derive(Debug, Serialize)]
pub struct Removed {
    /// The project's name.
    pub name: String,
    /// `here` when only this machine's path was forgotten, `everywhere` when the whole project was.
    pub removed: &'static str,
}

async fn remove(ctx: &Ctx, args: RemoveArgs) -> Result<Removed, ToolError> {
    let node = ctx.node_name().to_owned();
    edit(ctx, move |current| {
        let project = find(current, &args.name)?;
        if args.everywhere {
            return Ok((
                vec![Edit::Forget {
                    name: args.name.clone(),
                }],
                Removed {
                    name: args.name,
                    removed: "everywhere",
                },
            ));
        }
        if !project.on.contains_key(&node) {
            return Err(not_here(&args.name, project, &node).with_hint(format!(
                "Nothing to forget here. To forget {} on every machine, pass `everywhere: true`.",
                args.name
            )));
        }
        Ok((
            vec![Edit::ForgetHere {
                name: args.name.clone(),
                node,
            }],
            Removed {
                name: args.name,
                removed: "here",
            },
        ))
    })
    .await
}

/// Arguments of `proj.clone`.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct CloneArgs {
    /// The project to clone; it must have a `repo`.
    project: String,
    /// Where to clone it, absolute or starting at `~`; the path recorded for this machine, or
    /// ~/projects/<name>, when absent.
    #[serde(default)]
    path: Option<String>,
}

/// What `proj.clone` returns.
#[derive(Debug, Serialize)]
pub struct Cloned {
    /// The project's name.
    pub name: String,
    /// Where it was cloned.
    pub path: String,
    /// The commit checked out; absent when the repository has no commits yet.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub head: Option<String>,
}

async fn clone(ctx: &Ctx, args: CloneArgs) -> Result<Cloned, ToolError> {
    let file = ctx.config().get::<ProjectsFile>()?;
    let project = find(&file, &args.project)?;
    let Some(repo) = project.repo.clone() else {
        return Err(ErrorBody::new(
            ErrorCode::NotFound,
            format!(
                "{} has no repo recorded, so there is nothing to clone it from.",
                args.project
            ),
        )
        .with_hint(format!(
            "Record one with proj.add, such as name={} repo=https://git.example/me/{}.git.",
            args.project, args.project
        )));
    };
    let source = match repo_source(&repo)? {
        RepoSource::Remote(url) => OsString::from(url),
        RepoSource::Local(path) => ctx
            .grant()
            .check_path(&path, Access::Read)?
            .into_os_string(),
    };
    let written = args
        .path
        .clone()
        .or_else(|| project.on.get(ctx.node_name()).cloned())
        .unwrap_or_else(|| format!("~/projects/{}", args.project));
    let destination = ctx.grant().check_path(&written, Access::Write)?;
    if destination.exists() {
        let empty = destination.is_dir()
            && std::fs::read_dir(&destination).is_ok_and(|mut entries| entries.next().is_none());
        if !empty {
            return Err(ErrorBody::new(
                ErrorCode::Exists,
                format!(
                    "{} already exists and is not an empty directory, so {} cannot be cloned there.",
                    destination.display(),
                    args.project
                ),
            )
            .with_hint("Give another `path`, or, if it is already the project, record it with proj.add and update it with proj.pull."));
        }
    }

    ctx.emit(
        EventKind::Progress,
        Value::String(format!("Cloning {repo} into {}", destination.display())),
    );
    let (sender, mut receiver) = mpsc::unbounded_channel::<OutputChunk>();
    let forward = Arc::new(move |chunk: OutputChunk| {
        let _ = sender.send(chunk);
    });
    let cancel = ctx.cancel_token().clone();
    let run = Git {
        args: vec![
            OsString::from("clone"),
            OsString::from("--progress"),
            // Everything after this is a repository and a directory, even when it starts with `-`.
            OsString::from("--"),
            source,
            destination.clone().into_os_string(),
        ],
        cwd: None,
        mode: Mode::Write,
        timeout: CLONE_TIMEOUT,
        cancel: &cancel,
        on_output: Some(forward),
        // Belt and braces: repo_source already refuses every `transport::address`.
        config: vec!["protocol.ext.allow=never".to_owned()],
    }
    .run();
    tokio::pin!(run);
    let mut lines = Lines::default();
    let output = loop {
        tokio::select! {
            Some(chunk) = receiver.recv() => lines.push(ctx, &chunk.bytes),
            output = &mut run => break output?,
        }
    };
    while let Ok(chunk) = receiver.try_recv() {
        lines.push(ctx, &chunk.bytes);
    }
    if output.exit_code != Some(0) {
        return Err(remote_error(
            &format!("{} could not be cloned from {repo}", args.project),
            &output,
        ));
    }

    let head = git::head(&destination, &cancel).await?;
    let node = ctx.node_name().to_owned();
    let name = args.project.clone();
    let recorded = project.on.get(&node).cloned();
    if recorded.as_deref() != Some(written.as_str()) {
        edit(ctx, move |_| {
            Ok((
                vec![Edit::Set {
                    name,
                    node,
                    path: Some(written),
                    repo: None,
                    description: None,
                }],
                (),
            ))
        })
        .await?;
    }
    Ok(Cloned {
        name: args.project,
        path: destination.display().to_string(),
        head,
    })
}

/// Where a clone comes from, once its form has been checked.
#[derive(Debug, PartialEq, Eq)]
enum RepoSource {
    /// A URL git fetches over the network: `https://`, `ssh://`, `git://` or `user@host:path`.
    Remote(String),
    /// A repository on this machine, which the caller must be allowed to read.
    Local(String),
}

/// Sorts a `repo` into the forms a clone accepts and refuses every other.
///
/// git understands many more: `transport::address` runs a helper program (`ext::` runs any
/// command), an argument starting with `-` is taken for an option by some transports, and a local
/// path is a way to copy a repository the caller may not read. So only the network forms people
/// use, and local paths that go through the grant like every file a tool reads, are let through.
fn repo_source(repo: &str) -> Result<RepoSource, ErrorBody> {
    let refuse = |why: &str| {
        ErrorBody::new(
            ErrorCode::InvalidArgs,
            format!("{repo:?} is not a repository proj.clone fetches from: {why}."),
        )
        .with_hint("Record an https://, ssh:// or git:// URL, user@host:path, or an absolute path on this machine, with proj.add.")
    };
    let lower = repo.to_ascii_lowercase();
    if repo.starts_with('-') {
        return Err(refuse("it would be taken for an option"));
    }
    if ["https://", "ssh://", "git://"]
        .iter()
        .any(|scheme| lower.starts_with(scheme))
    {
        return Ok(RepoSource::Remote(repo.to_owned()));
    }
    if let Some(rest) = lower
        .strip_prefix("file://")
        .map(|_| &repo["file://".len()..])
    {
        // file:///home/me/x, or file:///C:/Users/me/x on Windows; no host.
        let Some(path) = rest.strip_prefix('/') else {
            return Err(refuse(
                "a file:// URL names no host and starts its path with /",
            ));
        };
        let drive = path.as_bytes().get(1) == Some(&b':');
        return Ok(RepoSource::Local(if drive {
            path.to_owned()
        } else {
            format!("/{path}")
        }));
    }
    if repo.contains("://") {
        return Err(refuse(
            "only https://, ssh://, git:// and file:// URLs are taken",
        ));
    }
    if repo.contains("::") {
        return Err(refuse("a transport helper can run any program"));
    }
    if let Some((before, _)) = repo.split_once(':')
        && let Some((user, host)) = before.split_once('@')
        && !before.contains(['/', '\\'])
        && !user.is_empty()
        && !host.is_empty()
        && !host.starts_with('-')
    {
        return Ok(RepoSource::Remote(repo.to_owned()));
    }
    if repo.starts_with('~') || Path::new(repo).is_absolute() {
        return Ok(RepoSource::Local(repo.to_owned()));
    }
    Err(refuse(
        "it is not a URL, user@host:path or an absolute path",
    ))
}

/// Splits git's progress output, which redraws one line with carriage returns, into the lines a
/// person would have seen, each sent as a progress event.
#[derive(Default)]
struct Lines {
    pending: Vec<u8>,
}

impl Lines {
    fn push(&mut self, ctx: &Ctx, bytes: &[u8]) {
        self.pending.extend_from_slice(bytes);
        while let Some(end) = self
            .pending
            .iter()
            .position(|byte| matches!(byte, b'\r' | b'\n'))
        {
            let line: Vec<u8> = self.pending.drain(..=end).collect();
            let text = String::from_utf8_lossy(&line);
            let text = text.trim();
            if !text.is_empty() {
                ctx.emit(EventKind::Progress, Value::String(text.to_owned()));
            }
        }
    }
}

/// Arguments of `proj.pull`.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct PullArgs {
    /// The project to update.
    project: String,
}

/// What `proj.pull` returns.
#[derive(Debug, Serialize)]
pub struct Pulled {
    /// The project's name.
    pub name: String,
    /// Its directory.
    pub path: String,
    /// The commit checked out before.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub before: Option<String>,
    /// The commit checked out after.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub after: Option<String>,
    /// Whether anything changed.
    pub updated: bool,
    /// What changed, in git's words.
    pub summary: String,
}

async fn pull(ctx: &Ctx, args: PullArgs) -> Result<Pulled, ToolError> {
    let file = ctx.config().get::<ProjectsFile>()?;
    let (_, written) = here(ctx, &file, &args.project)?;
    let path = existing_dir(ctx, &args.project, written, Access::Write)?;
    let cancel = ctx.cancel_token().clone();
    match git::layout(&path, &cancel).await? {
        Some(layout) => layout.check(ctx.grant(), Access::Write)?,
        None => {
            return Err(ErrorBody::new(
                ErrorCode::Failed,
                format!(
                    "{} is not a git repository, so there is nothing to pull.",
                    path.display()
                ),
            ));
        }
    }
    let before = git::head(&path, &cancel).await?;
    let output = Git {
        args: ["pull", "--ff-only", "--no-rebase"]
            .into_iter()
            .map(OsString::from)
            .collect(),
        cwd: Some(&path),
        mode: Mode::Write,
        timeout: PULL_TIMEOUT,
        cancel: &cancel,
        on_output: None,
        config: Vec::new(),
    }
    .run()
    .await?;
    if git::not_a_repository(&output) {
        return Err(ErrorBody::new(
            ErrorCode::Failed,
            format!(
                "{} is not a git repository, so there is nothing to pull.",
                path.display()
            ),
        ));
    }
    if output.exit_code != Some(0) {
        let said = git::explanation(&output);
        let brief = git::brief(&output);
        let refused = [
            "Not possible to fast-forward",
            "diverg",
            "would be overwritten",
            "commit your changes or stash them",
            "You have unstaged changes",
            "unmerged files",
            "not concluded your merge",
        ];
        if refused.iter().any(|sign| said.contains(sign)) {
            return Err(ErrorBody::new(
                ErrorCode::Conflict,
                format!(
                    "{} was not updated: git will not fast-forward it as it stands.",
                    args.project
                ),
            )
            .with_hint(format!(
                "Nothing was changed. git said: {brief}\nMerge or rebase by hand, or commit or stash the local changes, then pull again."
            ))
            .with_details(json!({"git": said})));
        }
        return Err(remote_error(
            &format!("{} could not be pulled", args.project),
            &output,
        ));
    }
    let after = git::head(&path, &cancel).await?;
    let updated = before != after;
    let summary = match (&before, &after) {
        (Some(before), Some(after)) if updated => {
            let range = format!("{before}..{after}");
            let stat = Git::read(
                &path,
                &[
                    "diff",
                    "--no-ext-diff",
                    "--no-textconv",
                    "--shortstat",
                    &range,
                ],
                &cancel,
            )
            .run()
            .await
            .ok()
            .and_then(|output| git::first_line(&output))
            .unwrap_or_default();
            format!(
                "Fast-forwarded {} to {}. {stat}",
                short(before),
                short(after)
            )
            .trim_end()
            .to_owned()
        }
        _ if updated => "Updated.".to_owned(),
        _ => "Already up to date.".to_owned(),
    };
    Ok(Pulled {
        name: args.project,
        path: path.display().to_string(),
        before,
        after,
        updated,
        summary,
    })
}

fn short(hash: &str) -> &str {
    hash.get(..12).unwrap_or(hash)
}

/// A failure of a command that talked to a remote, in the closed codes.
fn remote_error(what: &str, output: &legion_os::run::RunOutput) -> ErrorBody {
    let said = git::explanation(output);
    let brief = git::brief(output);
    let lower = said.to_lowercase();
    let unreachable = [
        "could not resolve host",
        "could not read from remote repository",
        "connection refused",
        "connection timed out",
        "network is unreachable",
        "unable to access",
    ];
    let missing = [
        "does not appear to be a git repository",
        "repository not found",
        "does not exist",
    ];
    let credentials = [
        "terminal prompts disabled",
        "authentication failed",
        "permission denied",
    ];
    let (code, advice) = if credentials.iter().any(|sign| lower.contains(sign)) {
        (
            ErrorCode::Failed,
            "The remote wants credentials git could not find without asking. Set up an ssh key or a credential helper for the account legion runs as.",
        )
    } else if missing.iter().any(|sign| lower.contains(sign)) {
        (
            ErrorCode::NotFound,
            "Check the repo address recorded for the project (proj.list shows it).",
        )
    } else if unreachable.iter().any(|sign| lower.contains(sign)) {
        (
            ErrorCode::Unreachable,
            "Check that this machine can reach the remote, then try again.",
        )
    } else {
        (
            ErrorCode::Failed,
            "Look at what git said, put it right, and try again.",
        )
    };
    ErrorBody::new(code, format!("{what}."))
        .with_hint(format!("{advice} git said: {brief}"))
        .with_details(json!({"git": said}))
}

/// Arguments of `proj.run`.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct RunArgs {
    /// The project whose task to run.
    project: String,
    /// The task's name, as proj.list shows it.
    task: String,
    /// Arguments added after the task's command, each passed as one argument.
    #[serde(default)]
    args: Vec<String>,
}

/// What `proj.run` returns for a task that runs to the end.
#[derive(Debug, Serialize)]
pub struct TaskRan {
    /// The project's name.
    pub project: String,
    /// The task's name.
    pub task: String,
    /// What the command did, as exec.run reports it.
    #[serde(flatten)]
    pub ran: Ran,
}

/// What `proj.run` returns for a task that starts a job.
#[derive(Debug, Serialize)]
pub struct TaskStarted {
    /// The project's name.
    pub project: String,
    /// The task's name.
    pub task: String,
    /// The job's id, for job.logs, job.wait, job.status and job.cancel.
    pub job: String,
    /// The job's state when the call returned.
    pub state: JobState,
    /// What to do next.
    pub hint: String,
}

/// What `proj.run` returns.
#[derive(Debug, Serialize)]
#[serde(untagged)]
pub enum TaskOutcome {
    /// The task ran to the end.
    Ran(TaskRan),
    /// The task started a job.
    Started(TaskStarted),
}

async fn run_task(ctx: &Ctx, args: RunArgs) -> Result<TaskOutcome, ToolError> {
    let file = ctx.config().get::<ProjectsFile>()?;
    let (project, written) = here(ctx, &file, &args.project)?;
    let Some(task) = project.tasks.get(&args.task) else {
        let known: Vec<&str> = project.tasks.keys().map(String::as_str).collect();
        let hint = if known.is_empty() {
            format!(
                "{} has no tasks; add them under [projects.{}.tasks] in projects.toml.",
                args.project, args.project
            )
        } else {
            format!("Its tasks are {}.", known.join(", "))
        };
        return Err(ErrorBody::new(
            ErrorCode::NotFound,
            format!("{} has no task called {}.", args.project, args.task),
        )
        .with_hint(hint));
    };
    let cwd = existing_dir(ctx, &args.project, written, Access::Read)?;
    let mut command = task_command(task, &args.task, &args.args, ctx.os())?;
    command.insert("cwd".to_owned(), Value::String(cwd.display().to_string()));

    if task.job {
        let starting = ctx
            .tools()
            .into_iter()
            .find(|spec| spec.name == "job.start")
            .is_some_and(|spec| ctx.grant().allows(&spec));
        if !starting {
            return Err(may_not(
                ctx.grant(),
                "job.start",
                &format!("task {} of {} runs as a job", args.task, args.project),
            ));
        }
        command.insert(
            "name".to_owned(),
            Value::String(format!("{}.{}", args.project, args.task)),
        );
        let started = job::start(ctx, parse_args("proj.run", Value::Object(command))?).await?;
        let hint = format!(
            "The task runs in the background. Read its output with job.logs id={} (pass back next_offset to follow it), wait for it with job.wait, or stop it with job.cancel.",
            started.id
        );
        return Ok(TaskOutcome::Started(TaskStarted {
            project: args.project,
            task: args.task,
            job: started.id,
            state: started.state,
            hint,
        }));
    }
    command.insert(
        "timeout_s".to_owned(),
        json!(task.timeout_s.unwrap_or(TASK_TIMEOUT_S)),
    );
    let ran = exec::run_command(ctx, parse_args("proj.run", Value::Object(command))?).await?;
    Ok(TaskOutcome::Ran(TaskRan {
        project: args.project,
        task: args.task,
        ran,
    }))
}

/// The `cmd` or `argv` a task runs with `extra` arguments, as exec.run and job.start take them.
///
/// For a shell task the arguments become the shell's positional parameters (`$1`, `$2`, …),
/// with the task's name as `$0`, so they reach the command without being parsed by the shell.
/// PowerShell, the shell on Windows, has no such parameters for a command line it is handed,
/// so there a shell task takes no arguments.
fn task_command(
    task: &Task,
    name: &str,
    extra: &[String],
    os: &str,
) -> Result<serde_json::Map<String, Value>, ErrorBody> {
    let mut command = serde_json::Map::new();
    match (&task.argv, &task.cmd) {
        (Some(argv), _) => {
            let argv: Vec<&String> = argv.iter().chain(extra).collect();
            command.insert("argv".to_owned(), json!(argv));
        }
        (None, Some(cmd)) if extra.is_empty() => {
            command.insert("cmd".to_owned(), json!(cmd));
        }
        (None, Some(_)) if os == "windows" => {
            return Err(ErrorBody::new(
                ErrorCode::InvalidArgs,
                format!("Task {name} is a shell command line, and on Windows such a task takes no `args`."),
            )
            .with_hint("PowerShell cannot be handed arguments apart from the command line. Write the task as `argv` in projects.toml to pass arguments, or run it without `args`."));
        }
        (None, Some(cmd)) => {
            let mut argv = vec![
                "sh".to_owned(),
                "-c".to_owned(),
                cmd.clone(),
                name.to_owned(),
            ];
            argv.extend(extra.iter().cloned());
            command.insert("argv".to_owned(), json!(argv));
        }
        (None, None) => {
            return Err(ErrorBody::new(
                ErrorCode::Internal,
                format!(
                    "Task {name} has no command, which projects.toml should not have accepted."
                ),
            ));
        }
    }
    Ok(command)
}

/// Arguments of `proj.discover`.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct DiscoverArgs {
    /// The directory to search, absolute or starting at `~`.
    root: String,
    /// How many levels below `root` to look, 0 to 6; 3 when absent.
    #[serde(default)]
    #[schemars(range(max = 6))]
    max_depth: Option<u32>,
    /// Record the working trees not yet listed as projects living on this machine.
    #[serde(default)]
    add: bool,
}

/// What `proj.discover` returns.
#[derive(Debug, Serialize)]
pub struct Discovered {
    /// The working trees found.
    pub found: Vec<Found>,
    /// Whether the search stopped at a limit before it had looked everywhere.
    pub truncated: bool,
    /// The projects recorded, with `add`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub added: Option<Vec<String>>,
}

/// One working tree `proj.discover` found.
#[derive(Debug, Serialize)]
pub struct Found {
    /// Its directory.
    pub path: String,
    /// Its project name: the one it is listed under, or one made from the directory's name.
    pub name: String,
    /// Where its `origin` remote points.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,
    /// Whether projects.toml already records it as living here.
    pub already_listed: bool,
    /// Why its repository could not be read, such as git data outside the caller's paths.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<ErrorBody>,
}

async fn discover(ctx: &Ctx, args: DiscoverArgs) -> Result<Discovered, ToolError> {
    let depth = args.max_depth.unwrap_or(DISCOVER_DEPTH.0);
    if depth > DISCOVER_DEPTH.1 {
        return Err(invalid(
            "proj.discover",
            &format!("`max_depth` is at most {}, not {depth}", DISCOVER_DEPTH.1),
            "max_depth",
        ));
    }
    if args.add {
        let adding = ctx
            .tools()
            .into_iter()
            .find(|spec| spec.name == "proj.add")
            .is_some_and(|spec| ctx.grant().allows(&spec));
        if !adding {
            return Err(may_not(
                ctx.grant(),
                "proj.add",
                "proj.discover with `add: true` records projects",
            ));
        }
    }
    let root = ctx.grant().check_path(&args.root, Access::Read)?;
    if !root.is_dir() {
        return Err(ErrorBody::new(
            ErrorCode::NotFound,
            format!("{} is not a directory that exists.", root.display()),
        )
        .with_hint("Give the directory to search, such as ~/projects."));
    }
    let walked = blocking(move || Ok(discover::walk(&root, depth))).await?;

    let cancel = ctx.cancel_token().clone();
    let mut repos = Vec::with_capacity(walked.found.len());
    for path in &walked.found {
        repos.push(origin(path, ctx.grant(), &cancel).await);
    }

    let grant = ctx.grant().clone();
    let node = ctx.node_name().to_owned();
    let home = ctx.paths().home().map(Path::to_path_buf);
    let found_paths = walked.found;
    let classify = move |file: &ProjectsFile| classify(file, &node, &grant, &found_paths, repos);
    let (found, added) = if args.add {
        let node = ctx.node_name().to_owned();
        let file = ctx.config().path_of::<ProjectsFile>();
        let lock = lock_path(ctx);
        blocking(move || {
            file::update(&file, &lock, |current| {
                let found = classify(current);
                let mut edits = Vec::new();
                let mut added = Vec::new();
                for entry in found.iter().filter(|entry| !entry.already_listed) {
                    edits.push(Edit::Set {
                        name: entry.name.clone(),
                        node: node.clone(),
                        path: Some(written_form(Path::new(&entry.path), home.as_deref())),
                        repo: entry.repo.clone(),
                        description: None,
                    });
                    added.push(entry.name.clone());
                }
                Ok((edits, (found, Some(added))))
            })
        })
        .await?
    } else {
        let current = ctx.config().get::<ProjectsFile>()?;
        (classify(&current), None)
    };
    Ok(Discovered {
        found,
        truncated: walked.truncated,
        added,
    })
}

/// Names the working trees found and says which are listed already: a tree recorded for this
/// node keeps its project's name, and a new one gets a name no project has.
fn classify(
    file: &ProjectsFile,
    node: &str,
    grant: &Grant,
    paths: &[PathBuf],
    repos: Vec<Origin>,
) -> Vec<Found> {
    let listed: BTreeMap<PathBuf, &str> = file
        .projects
        .iter()
        .filter_map(|(name, project)| {
            let path = grant.check_path(project.on.get(node)?, Access::Read).ok()?;
            Some((path, name.as_str()))
        })
        .collect();
    let taken: Vec<String> = file.projects.keys().cloned().collect();
    let new_paths = paths.iter().filter(|path| !listed.contains_key(*path));
    let mut new_names = discover::unique_names(new_paths.map(PathBuf::as_path), &taken).into_iter();
    paths
        .iter()
        .zip(repos)
        .map(|(path, (repo, error))| {
            let (name, already_listed) = match listed.get(path) {
                Some(name) => ((*name).to_owned(), true),
                None => (new_names.next().unwrap_or_default(), false),
            };
            Found {
                path: path.display().to_string(),
                name,
                repo,
                already_listed,
                error,
            }
        })
        .collect()
}

/// Where a working tree's `origin` points, or why it was not read.
type Origin = (Option<String>, Option<ErrorBody>);

/// Reads the `origin` remote of the working tree at `path`, from its own configuration file only:
/// an include could pull in a file from anywhere, and the caller may not be allowed to read there.
async fn origin(path: &Path, grant: &Grant, cancel: &CancellationToken) -> Origin {
    let layout = match git::layout(path, cancel).await {
        Ok(Some(layout)) => layout,
        Ok(None) => return (None, None),
        Err(error) => return (None, Some(error)),
    };
    if let Err(error) = layout.check(grant, Access::Read) {
        return (None, Some(error));
    }
    let repo = Git::read(
        path,
        &[
            "config",
            "--local",
            "--no-includes",
            "--get",
            "remote.origin.url",
        ],
        cancel,
    )
    .run()
    .await
    .ok()
    .and_then(|output| git::first_line(&output));
    (repo, None)
}

/// A path as projects.toml should hold it: from `~` when it is under the home directory, which
/// keeps the file readable and the same on machines whose home directories differ.
fn written_form(path: &Path, home: Option<&Path>) -> String {
    let homes = home.into_iter().flat_map(|home| {
        [
            home.to_path_buf(),
            dunce::canonicalize(home).unwrap_or_else(|_| home.to_path_buf()),
        ]
    });
    for home in homes {
        if let Ok(rest) = path.strip_prefix(&home)
            && !rest.as_os_str().is_empty()
        {
            let parts: Vec<String> = rest
                .components()
                .map(|part| part.as_os_str().to_string_lossy().into_owned())
                .collect();
            return format!("~/{}", parts.join("/"));
        }
    }
    path.display().to_string()
}

/// The project called `name`, or `not_found` naming the projects there are.
fn find<'a>(file: &'a ProjectsFile, name: &str) -> Result<&'a Project, ErrorBody> {
    file.projects.get(name).ok_or_else(|| {
        let known: Vec<&str> = file.projects.keys().map(String::as_str).collect();
        let hint = if known.is_empty() {
            "No projects are recorded yet: proj.add records one, and proj.discover finds the git repositories under a directory.".to_owned()
        } else {
            format!("The projects are {}.", known.join(", "))
        };
        ErrorBody::new(
            ErrorCode::NotFound,
            format!("There is no project called {name} in projects.toml."),
        )
        .with_hint(hint)
    })
}

/// The project and its path on this node as written, or `not_found` saying where it lives.
fn here<'a>(
    ctx: &Ctx,
    file: &'a ProjectsFile,
    name: &str,
) -> Result<(&'a Project, &'a str), ErrorBody> {
    let project = find(file, name)?;
    match project.on.get(ctx.node_name()) {
        Some(path) => Ok((project, path)),
        None => Err(not_here(name, project, ctx.node_name())),
    }
}

fn not_here(name: &str, project: &Project, node: &str) -> ErrorBody {
    let elsewhere: Vec<&str> = project.on.keys().map(String::as_str).collect();
    let hint = if elsewhere.is_empty() {
        format!(
            "It has no path on any machine yet. Record one with proj.add name={name} path=…, or fetch it with proj.clone."
        )
    } else {
        format!(
            "It lives on {}: ask there with `on`. To have it here too, use proj.clone, or proj.add name={name} path=… for a copy that is already here.",
            elsewhere.join(", ")
        )
    };
    ErrorBody::new(
        ErrorCode::NotFound,
        format!("{name} does not live on {node}: projects.toml records no path for it here."),
    )
    .with_hint(hint)
    .with_details(json!({"elsewhere": elsewhere}))
}

/// The project's directory, checked for `access` and required to exist.
fn existing_dir(
    ctx: &Ctx,
    name: &str,
    written: &str,
    access: Access,
) -> Result<PathBuf, ErrorBody> {
    let path = ctx.grant().check_path(written, access)?;
    if !path.is_dir() {
        return Err(ErrorBody::new(
            ErrorCode::NotFound,
            format!(
                "{name} is recorded at {}, which is not a directory that exists.",
                path.display()
            ),
        )
        .with_hint(format!(
            "Fetch it with proj.clone project={name}, or record where it really is with proj.add."
        )));
    }
    Ok(path)
}

fn invalid(tool: &str, reason: &str, field: &str) -> ErrorBody {
    ErrorBody::new(ErrorCode::InvalidArgs, format!("{tool}: {reason}."))
        .with_hint(format!(
            "`legion help {tool}` shows the arguments it takes."
        ))
        .with_details(json!({"field": field}))
}

fn may_not(grant: &Grant, tool: &str, because: &str) -> ErrorBody {
    ErrorBody::new(
        ErrorCode::Denied,
        format!(
            "{} may not use {tool}, and {because}: its role {} does not allow it.",
            grant.principal(),
            grant.role()
        ),
    )
    .with_hint(format!(
        "Ask the owner to allow {tool} for the role in policy.toml."
    ))
}

fn lock_path(ctx: &Ctx) -> PathBuf {
    ctx.paths().state_dir().join("projects.lock")
}

/// Changes `projects.toml` as `plan` says, off the runtime's threads since it waits on a lock and
/// the disk.
async fn edit<T: Send + 'static>(
    ctx: &Ctx,
    plan: impl FnOnce(&ProjectsFile) -> Result<(Vec<Edit>, T), ErrorBody> + Send + 'static,
) -> Result<T, ErrorBody> {
    let file = ctx.config().path_of::<ProjectsFile>();
    let lock = lock_path(ctx);
    blocking(move || file::update(&file, &lock, plan)).await
}

async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> Result<T, ErrorBody> + Send + 'static,
) -> Result<T, ErrorBody> {
    tokio::task::spawn_blocking(work).await.map_err(|error| {
        ErrorBody::new(
            ErrorCode::Internal,
            format!("A proj tool stopped unexpectedly: {error}."),
        )
    })?
}

#[cfg(test)]
mod tests {
    use super::*;

    fn task(cmd: Option<&str>, argv: Option<&[&str]>) -> Task {
        Task {
            cmd: cmd.map(str::to_owned),
            argv: argv.map(|argv| argv.iter().map(|arg| (*arg).to_owned()).collect()),
            job: false,
            timeout_s: None,
        }
    }

    #[test]
    fn arguments_are_appended_and_never_spliced() {
        let extra = ["a b".to_owned(), "$(x)".to_owned()];
        let argv =
            task_command(&task(None, Some(&["cargo", "test"])), "t", &extra, "linux").unwrap();
        assert_eq!(argv["argv"], json!(["cargo", "test", "a b", "$(x)"]));
        let plain = task_command(&task(Some("npm test"), None), "t", &[], "windows").unwrap();
        assert_eq!(plain["cmd"], json!("npm test"));
        let shell = task_command(
            &task(Some("npm test \"$@\""), None),
            "test",
            &extra,
            "macos",
        )
        .unwrap();
        assert_eq!(
            shell["argv"],
            json!(["sh", "-c", "npm test \"$@\"", "test", "a b", "$(x)"])
        );
        let refused =
            task_command(&task(Some("npm test"), None), "t", &extra, "windows").unwrap_err();
        assert_eq!(refused.code, ErrorCode::InvalidArgs);
    }

    #[test]
    fn tools_that_let_git_run_programs_are_exec() {
        let mut registry = Registry::new();
        register(&mut registry);
        let effect = |name: &str| registry.get(name).map(|tool| tool.spec().effect);
        for name in ["proj.clone", "proj.pull", "proj.run"] {
            assert_eq!(effect(name), Some(Effect::Exec), "{name}");
            let spec = registry
                .get(name)
                .map(|tool| tool.spec().description.clone());
            if name != "proj.run" {
                assert!(spec.unwrap_or_default().contains("hooks"), "{name}");
            }
        }
        for name in ["proj.list", "proj.status", "proj.discover"] {
            assert_eq!(effect(name), Some(Effect::Read), "{name}");
        }
    }

    #[test]
    fn only_the_usual_repository_forms_are_cloned() {
        let remote = |repo: &str| Some(RepoSource::Remote(repo.to_owned()));
        let local = |path: &str| Some(RepoSource::Local(path.to_owned()));
        for (repo, expected) in [
            (
                "https://git.example/me/site.git",
                remote("https://git.example/me/site.git"),
            ),
            (
                "ssh://me@git.example/site.git",
                remote("ssh://me@git.example/site.git"),
            ),
            (
                "git://git.example/site.git",
                remote("git://git.example/site.git"),
            ),
            (
                "git@git.example:me/site.git",
                remote("git@git.example:me/site.git"),
            ),
            ("file:///home/me/site.git", local("/home/me/site.git")),
            (
                "file:///C:/Users/me/site.git",
                local("C:/Users/me/site.git"),
            ),
            ("~/site.git", local("~/site.git")),
            ("ext::sh -c touch% /x", None),
            ("fd::3", None),
            ("-uevil", None),
            ("http://git.example/site.git", None),
            ("ftp://git.example/site.git", None),
            ("file://host/site.git", None),
            ("git.example:site.git", None),
            ("me@-oProxyCommand=x:site", None),
            ("site.git", None),
        ] {
            assert_eq!(repo_source(repo).ok(), expected, "{repo}");
        }
        assert_eq!(
            repo_source("/home/me/site.git").ok(),
            Path::new("/home/me/site.git")
                .is_absolute()
                .then(|| RepoSource::Local("/home/me/site.git".to_owned()))
        );
    }

    #[test]
    fn paths_under_home_are_written_from_tilde() {
        let home = Path::new("/home/me");
        assert_eq!(
            written_form(Path::new("/home/me/code/site"), Some(home)),
            "~/code/site"
        );
        assert_eq!(
            written_form(Path::new("/srv/site"), Some(home)),
            "/srv/site"
        );
        assert_eq!(written_form(Path::new("/home/me"), Some(home)), "/home/me");
        assert_eq!(written_form(Path::new("/home/me/x"), None), "/home/me/x");
    }
}
