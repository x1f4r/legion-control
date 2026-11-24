//! Running the system `git` and reading what it says.
//!
//! Git runs with an argument vector, never a shell line, in the `C` locale so its messages can be
//! recognised, and with `GIT_TERMINAL_PROMPT=0` so a repository that wants credentials fails at
//! once instead of waiting for someone to type them.
//!
//! A repository is content someone else may have written, and its configuration can name programs
//! git runs: a filter for every file it reads, a file system monitor, hooks, a pager, a diff
//! program. Commands that only look are run so that none of those can start: every setting that
//! names a program is overridden on the command line, which wins over every file, and the filters
//! the repository defines are listed first and blanked one by one. They also set
//! `GIT_OPTIONAL_LOCKS=0`, so looking at a repository never takes the index lock from a person
//! working in it at the same moment.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::Duration;

use legion_core::proto::{ErrorBody, ErrorCode};
use legion_core::{Access, CancellationToken, Grant};
use legion_os::OsError;
use legion_os::run::{ChunkCallback, RunOptions, RunOutput, run};
use legion_os::shell::CommandSpec;
use serde::Serialize;

/// How long a command that only reads may take.
pub(super) const READ_TIMEOUT: Duration = Duration::from_secs(20);
/// The most of git's own explanation a message or hint carries.
const EXPLANATION_LIMIT: usize = 2000;

/// Whether a git command changes the repository.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Mode {
    /// Only looks.
    Read,
    /// Changes the repository or the working tree.
    Write,
}

/// One git command to run.
pub(super) struct Git<'a> {
    /// The arguments after `git`.
    pub args: Vec<OsString>,
    /// The directory to run in.
    pub cwd: Option<&'a Path>,
    /// Whether it changes anything.
    pub mode: Mode,
    /// When it is stopped.
    pub timeout: Duration,
    /// Cancelled with the call.
    pub cancel: &'a CancellationToken,
    /// Receives its output as it is read, for progress.
    pub on_output: Option<ChunkCallback>,
    /// Settings passed as `-c`, after the ones a read-only command always gets.
    pub config: Vec<String>,
    /// Whether ssh is to run in batch mode, which [`batch_ssh`] decides for a remote command.
    pub batch_ssh: bool,
}

impl<'a> Git<'a> {
    /// A read-only command in `cwd`.
    pub fn read(cwd: &'a Path, args: &[&str], cancel: &'a CancellationToken) -> Git<'a> {
        Git {
            args: args.iter().map(OsString::from).collect(),
            cwd: Some(cwd),
            mode: Mode::Read,
            timeout: READ_TIMEOUT,
            cancel,
            on_output: None,
            config: Vec::new(),
            batch_ssh: false,
        }
    }

    /// The same command with these `-c` settings as well.
    pub fn with_config(mut self, config: Vec<String>) -> Git<'a> {
        self.config.extend(config);
        self
    }

    /// Runs it. A command that ran is `Ok` whatever its exit code; `Err` means git could not be
    /// started, ran out of time or was cancelled.
    pub async fn run(self) -> Result<RunOutput, ErrorBody> {
        let program = OsString::from("git");
        let mut settings = if self.mode == Mode::Read {
            read_only_settings()
        } else {
            Vec::new()
        };
        if self.mode == Mode::Write {
            settings.push("credential.interactive=never".to_owned());
        }
        settings.extend(self.config);
        let mut args: Vec<OsString> = settings
            .into_iter()
            .flat_map(|setting| [OsString::from("-c"), OsString::from(setting)])
            .collect();
        args.extend(self.args);
        let mut options = RunOptions::new(CommandSpec::Argv { program, args })
            .c_locale()
            .env("GIT_TERMINAL_PROMPT", "0")
            .timeout(self.timeout);
        if self.mode == Mode::Read {
            options = options.env("GIT_OPTIONAL_LOCKS", "0");
        } else {
            // With no terminal prompt and no program to ask, a credential git cannot find on its
            // own makes it fail at once rather than wait for an answer nobody will give.
            options = options.envs(NO_PROMPTS);
            if self.batch_ssh {
                options = options.env("GIT_SSH_COMMAND", "ssh -o BatchMode=yes");
            }
        }
        if let Some(cwd) = self.cwd {
            options = options.cwd(cwd);
        }
        if let Some(on_output) = self.on_output {
            options = options.on_output(on_output);
        }
        // The run is spawned rather than awaited in place, so that a call dropped by the node on
        // cancel still stops git and everything it started (ssh, a credential helper) the patient
        // way: the guard cancels the run's own token when this future goes away.
        let token = self.cancel.child_token();
        let _stop_on_drop = token.clone().drop_guard();
        let running = tokio::spawn(run(options.cancel(token)));
        let output = running
            .await
            .map_err(|error| {
                ErrorBody::new(
                    ErrorCode::Internal,
                    format!("Running git stopped unexpectedly: {error}."),
                )
            })?
            .map_err(|error| start_error(error, self.cwd))?;
        if output.cancelled {
            return Err(ErrorBody::new(
                ErrorCode::Cancelled,
                "The call was cancelled, and git was stopped.",
            ));
        }
        if output.timed_out {
            return Err(ErrorBody::new(
                ErrorCode::Timeout,
                format!(
                    "git did not finish within {} seconds and was stopped.",
                    self.timeout.as_secs()
                ),
            )
            .with_hint("A repository on a slow disk or behind a slow network can take this long; try again, or look at it by hand."));
        }
        Ok(output)
    }
}

/// The settings that keep a command that only looks from running anything the repository names.
fn read_only_settings() -> Vec<String> {
    // Nothing can exist below a file, so a hooks directory under the running program is one that
    // is certainly missing, on every platform.
    let no_hooks = std::env::current_exe()
        .map(|exe| exe.join("no-hooks"))
        .unwrap_or_else(|_| std::path::PathBuf::from("/dev/null/no-hooks"));
    vec![
        "core.fsmonitor=false".to_owned(),
        format!("core.hooksPath={}", no_hooks.display()),
        "protocol.ext.allow=never".to_owned(),
        "credential.helper=".to_owned(),
        "core.pager=cat".to_owned(),
        "diff.external=".to_owned(),
        "core.sshCommand=".to_owned(),
    ]
}

/// Settings that blank every filter the repository's own configuration defines, files it includes
/// among it, so that reading a file through `.gitattributes` runs nothing.
///
/// Includes are followed here because the command the settings are for follows them too; only the
/// names of the keys are read, never their values. Filters from the user's own and the system's
/// configuration, such as Git LFS, are the user's programs and stay. When the filters cannot be
/// listed the repository is not read at all, since a filter left in place would run.
pub(super) async fn filter_overrides(
    path: &Path,
    cancel: &CancellationToken,
) -> Result<Vec<String>, ErrorBody> {
    let output = Git::read(
        path,
        &[
            "config",
            "--includes",
            "--show-scope",
            "--name-only",
            "--get-regexp",
            r"^filter\..*\.(clean|smudge|process|required)$",
        ],
        cancel,
    )
    .run()
    .await?;
    // 1 is git's way of saying that nothing matched.
    match output.exit_code {
        Some(0) => Ok(blank_filters(&output.stdout.text)),
        Some(1) if output.stderr.text.trim().is_empty() => Ok(Vec::new()),
        _ => Err(ErrorBody::new(
            ErrorCode::Failed,
            format!(
                "The filters {} defines could not be listed, so it was not read: {}",
                path.display(),
                explanation(&output)
            ),
        )
        .with_hint("Check the repository's configuration with `git config --list --show-origin`.")),
    }
}

/// Parses `git config --show-scope --name-only` lines of filter keys into the settings that blank
/// every driver the repository itself defines.
fn blank_filters(listing: &str) -> Vec<String> {
    let mut drivers: Vec<&str> = listing
        .lines()
        .filter_map(|line| line.split_once('\t'))
        .filter(|(scope, _)| matches!(*scope, "local" | "worktree"))
        .filter_map(|(_, key)| {
            let rest = key.strip_prefix("filter.")?;
            Some(&rest[..rest.rfind('.')?])
        })
        .collect();
    drivers.sort_unstable();
    drivers.dedup();
    drivers
        .into_iter()
        .flat_map(|driver| {
            [
                format!("filter.{driver}.clean="),
                format!("filter.{driver}.smudge="),
                format!("filter.{driver}.process="),
                format!("filter.{driver}.required=false"),
            ]
        })
        .collect()
}

/// Variables that keep git, Git Credential Manager and ssh from asking anyone anything.
const NO_PROMPTS: [(&str, &str); 3] = [
    ("GIT_ASKPASS", ""),
    ("SSH_ASKPASS", ""),
    ("GCM_INTERACTIVE", "never"),
];

/// Whether a remote command may run ssh as `ssh -o BatchMode=yes`, which makes ssh fail instead of
/// asking for a passphrase or a host key on a terminal it finds.
///
/// Only when nobody chose the ssh command: setting `GIT_SSH_COMMAND` would override one the user
/// configured, with the options it carries. When they did choose one, a prompt it shows is
/// stopped by the command's timeout and by a cancel instead.
pub(super) async fn batch_ssh(cwd: Option<&Path>, cancel: &CancellationToken) -> bool {
    if ["GIT_SSH_COMMAND", "GIT_SSH"]
        .iter()
        .any(|name| std::env::var_os(name).is_some())
    {
        return false;
    }
    let mut query = Git::read(
        cwd.unwrap_or_else(|| Path::new(".")),
        &[
            "config",
            "--includes",
            "--show-origin",
            "--get-all",
            "core.sshCommand",
        ],
        cancel,
    );
    query.cwd = cwd;
    match query.run().await {
        Ok(output) => !ssh_configured(&output.stdout.text),
        Err(_) => false,
    }
}

/// Whether a `git config --show-origin` listing of `core.sshCommand` holds a command from a file,
/// as opposed to the blank one read-only commands pass on the command line.
fn ssh_configured(listing: &str) -> bool {
    listing.lines().any(|line| {
        line.split_once('\t')
            .is_some_and(|(origin, value)| origin != "command line:" && !value.trim().is_empty())
    })
}

fn start_error(error: OsError, cwd: Option<&Path>) -> ErrorBody {
    match error {
        OsError::ProgramNotFound { .. } => ErrorBody::new(
            ErrorCode::Unsupported,
            "git is not installed on this machine, or not on the PATH legion runs with.",
        )
        .with_hint("Install git (git-scm.com, or the system's package manager) and make sure the account legion runs as finds it on its PATH."),
        OsError::NotFound { .. } => ErrorBody::new(
            ErrorCode::NotFound,
            format!(
                "The directory {} does not exist.",
                cwd.map_or_else(String::new, |cwd| cwd.display().to_string())
            ),
        ),
        other => ErrorBody::new(ErrorCode::Failed, format!("git could not be started: {other}.")),
    }
}

/// What git wrote to explain itself, cut to a length a message can carry.
pub(super) fn explanation(output: &RunOutput) -> String {
    let text = if output.stderr.text.trim().is_empty() {
        output.stdout.text.trim()
    } else {
        output.stderr.text.trim()
    };
    bounded(text)
}

/// [`explanation`] without git's `hint:` lines, which suggest commands for a person at a terminal
/// and repeat what a hint of legion's own says better; the details keep them.
pub(super) fn brief(output: &RunOutput) -> String {
    let full = explanation(output);
    let kept: Vec<&str> = full
        .lines()
        .filter(|line| !line.starts_with("hint:"))
        .collect();
    kept.join("\n")
}

fn bounded(text: &str) -> String {
    if text.len() <= EXPLANATION_LIMIT {
        return text.to_owned();
    }
    // The end is where git says what went wrong.
    let mut start = text.len() - EXPLANATION_LIMIT;
    while !text.is_char_boundary(start) {
        start += 1;
    }
    format!("…{}", &text[start..])
}

/// Whether git refused because the directory is not in a repository.
pub(super) fn not_a_repository(output: &RunOutput) -> bool {
    output.exit_code != Some(0) && output.stderr.text.contains("not a git repository")
}

/// The first line of git's standard output, when it succeeded and printed one.
pub(super) fn first_line(output: &RunOutput) -> Option<String> {
    if output.exit_code != Some(0) {
        return None;
    }
    let line = output.stdout.text.lines().next()?.trim();
    (!line.is_empty()).then(|| line.to_owned())
}

/// What `git status --porcelain=v2 --branch --show-stash -z` says.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Porcelain {
    /// The commit checked out; `None` on a branch with no commits yet.
    pub oid: Option<String>,
    /// The branch checked out; `None` when the head is detached.
    pub branch: Option<String>,
    /// The branch it tracks, when it tracks one.
    pub upstream: Option<String>,
    /// Commits here that the upstream lacks, when the upstream exists.
    pub ahead: Option<u64>,
    /// Commits the upstream has that are not here.
    pub behind: Option<u64>,
    /// Entries in the stash.
    pub stash: u64,
    /// Files whose changes are staged.
    pub staged: u64,
    /// Files changed but not staged.
    pub unstaged: u64,
    /// Files git does not track and does not ignore.
    pub untracked: u64,
    /// Files with an unresolved merge conflict.
    pub conflicted: u64,
}

/// Parses `git status --porcelain=v2 --branch --show-stash -z`.
///
/// `-z` is asked for because it leaves paths unquoted and gives the original path of a rename its
/// own record, which is the only form in which a path holding a tab, a newline or a quote cannot be
/// mistaken for the next field. Headers git may add later are skipped; an entry of a kind this does
/// not know is an error, since counting it wrongly could call a dirty tree clean.
pub fn parse_porcelain(output: &str) -> Result<Porcelain, String> {
    let mut status = Porcelain::default();
    let mut records = output.split('\0').filter(|record| !record.is_empty());
    while let Some(record) = records.next() {
        if let Some(header) = record.strip_prefix("# ") {
            let (key, value) = header.split_once(' ').unwrap_or((header, ""));
            match key {
                "branch.oid" => status.oid = (value != "(initial)").then(|| value.to_owned()),
                "branch.head" => status.branch = (value != "(detached)").then(|| value.to_owned()),
                "branch.upstream" => status.upstream = Some(value.to_owned()),
                "branch.ab" => {
                    let mut counts = value.split(' ');
                    let ahead = counts.next().and_then(|n| n.strip_prefix('+'));
                    let behind = counts.next().and_then(|n| n.strip_prefix('-'));
                    let parse = |n: Option<&str>| n.and_then(|n| n.parse::<u64>().ok());
                    match (parse(ahead), parse(behind)) {
                        (Some(ahead), Some(behind)) => {
                            status.ahead = Some(ahead);
                            status.behind = Some(behind);
                        }
                        _ => return Err(format!("unexpected branch.ab header {value:?}")),
                    }
                }
                "stash" => {
                    status.stash = value
                        .parse()
                        .map_err(|_| format!("unexpected stash header {value:?}"))?;
                }
                _ => {}
            }
            continue;
        }
        // The kind is one ASCII byte; anything else, whatever character it starts with, is an
        // entry this does not know, and the rest is only sliced after an ASCII byte.
        let kind = record.as_bytes().first().copied().unwrap_or_default();
        let rest = if kind.is_ascii() { &record[1..] } else { "" };
        match kind {
            b'1' | b'2' => {
                let xy = rest
                    .trim_start()
                    .get(..2)
                    .ok_or_else(|| format!("short entry {record:?}"))?;
                let mut flags = xy.chars();
                if flags.next().is_some_and(|x| x != '.') {
                    status.staged += 1;
                }
                if flags.next().is_some_and(|y| y != '.') {
                    status.unstaged += 1;
                }
                if kind == b'2' {
                    // A rename or copy: the path it came from is the next record.
                    records
                        .next()
                        .ok_or("a rename without the path it came from")?;
                }
            }
            b'u' => status.conflicted += 1,
            b'?' => status.untracked += 1,
            b'!' => {}
            _ => return Err(format!("unexpected entry {record:?}")),
        }
    }
    Ok(status)
}

/// The state of one repository, as `proj.status` reports it.
#[derive(Debug, Serialize)]
pub struct RepoState {
    /// The branch checked out, unless the head is detached.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    /// Whether the head is detached, pointing at a commit rather than a branch.
    pub detached: bool,
    /// The commit checked out; absent on a branch with no commits yet.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub head: Option<String>,
    /// The branch it tracks.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub upstream: Option<String>,
    /// Commits not yet pushed to the upstream.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ahead: Option<u64>,
    /// Commits on the upstream not yet pulled, as of the last fetch.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub behind: Option<u64>,
    /// Files changed in each way.
    pub dirty: Dirty,
    /// Whether nothing is changed, staged, untracked or conflicted.
    pub clean: bool,
    /// The commit checked out; absent on a branch with no commits yet.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_commit: Option<Commit>,
    /// Entries in the stash.
    pub stash: u64,
}

/// How many files are changed in each way.
#[derive(Debug, Serialize)]
pub struct Dirty {
    /// Staged for the next commit.
    pub staged: u64,
    /// Changed but not staged.
    pub unstaged: u64,
    /// Not tracked and not ignored.
    pub untracked: u64,
    /// With an unresolved conflict.
    pub conflicted: u64,
}

/// A commit, as a person would look it up.
#[derive(Debug, Serialize)]
pub struct Commit {
    /// Its full hash.
    pub hash: String,
    /// The first line of its message.
    pub subject: String,
    /// Who wrote it.
    pub author: String,
    /// When it was committed, RFC 3339 UTC.
    pub time: String,
}

impl RepoState {
    fn new(status: Porcelain, last_commit: Option<Commit>) -> RepoState {
        let dirty = Dirty {
            staged: status.staged,
            unstaged: status.unstaged,
            untracked: status.untracked,
            conflicted: status.conflicted,
        };
        let clean = dirty.staged + dirty.unstaged + dirty.untracked + dirty.conflicted == 0;
        RepoState {
            detached: status.branch.is_none(),
            branch: status.branch,
            head: status.oid,
            upstream: status.upstream,
            ahead: status.ahead,
            behind: status.behind,
            dirty,
            clean,
            last_commit,
            stash: status.stash,
        }
    }
}

/// Where a working tree keeps its git data. A `.git` file or `core.worktree` can put either part
/// anywhere, so both are checked before anything is read through them.
#[derive(Debug)]
pub(super) struct Layout {
    /// This working tree's own git directory.
    pub git_dir: PathBuf,
    /// The directory shared by all its worktrees, with the objects and most of the refs.
    pub common_dir: PathBuf,
}

/// The git directories of the repository `path` is in: `Ok(None)` when it is in none.
pub(super) async fn layout(
    path: &Path,
    cancel: &CancellationToken,
) -> Result<Option<Layout>, ErrorBody> {
    let output = Git::read(
        path,
        &["rev-parse", "--absolute-git-dir", "--git-common-dir"],
        cancel,
    )
    .run()
    .await?;
    if not_a_repository(&output) {
        return Ok(None);
    }
    let mut lines = output.stdout.text.lines();
    match (output.exit_code, lines.next(), lines.next()) {
        (Some(0), Some(git_dir), Some(common_dir)) => Ok(Some(Layout {
            git_dir: PathBuf::from(git_dir),
            // A relative common directory is relative to where git ran.
            common_dir: path.join(common_dir),
        })),
        _ => Err(ErrorBody::new(
            ErrorCode::Failed,
            format!(
                "git could not say where the repository at {} keeps its data: {}",
                path.display(),
                explanation(&output)
            ),
        )),
    }
}

impl Layout {
    /// Checks that the caller may use both git directories as `access` says; a confined role must
    /// not reach a repository outside its paths through a working tree inside them.
    pub(super) fn check(&self, grant: &Grant, access: Access) -> Result<(), ErrorBody> {
        for dir in [&self.git_dir, &self.common_dir] {
            grant.check_path(dir, access).map_err(|error| {
                error.with_hint(format!(
                    "The working tree keeps its git data in {}, through a .git file or the repository's configuration, and the caller may not use that place.",
                    dir.display()
                ))
            })?;
        }
        Ok(())
    }
}

/// The repository at `path`: `Ok(None)` when it is not in one.
pub(super) async fn repo_state(
    path: &Path,
    grant: &Grant,
    cancel: &CancellationToken,
) -> Result<Option<RepoState>, ErrorBody> {
    let failed = |message: String| ErrorBody::new(ErrorCode::Failed, message);
    let Some(layout) = layout(path, cancel).await? else {
        return Ok(None);
    };
    layout.check(grant, Access::Read)?;
    let filters = filter_overrides(path, cancel).await?;
    let mut output = Git::read(path, &status_args(true), cancel)
        .with_config(filters.clone())
        .run()
        .await?;
    if not_a_repository(&output) {
        return Ok(None);
    }
    // `--show-stash` is from git 2.35; an older git refuses the option, and is asked without it.
    let old_git = output.exit_code != Some(0) && output.stderr.text.contains("show-stash");
    if old_git {
        output = Git::read(path, &status_args(false), cancel)
            .with_config(filters)
            .run()
            .await?;
    }
    if output.exit_code != Some(0) {
        return Err(failed(format!(
            "git status failed: {}",
            explanation(&output)
        )));
    }
    if output.stdout.truncated {
        return Err(failed("git status printed more than legion reads; the working tree has too many changes to count.".to_owned()));
    }
    let mut status = parse_porcelain(&output.stdout.text).map_err(|error| {
        failed(format!(
            "git status said something legion does not understand: {error}."
        ))
    })?;
    if old_git {
        status.stash = stash_count(path, cancel).await;
    }
    let last_commit = match &status.oid {
        Some(_) => last_commit(path, cancel).await.map_err(failed)?,
        None => None,
    };
    Ok(Some(RepoState::new(status, last_commit)))
}

/// The arguments of the `git status` that [`parse_porcelain`] reads; `--show-stash` only for a git
/// that knows it (2.35 and later).
fn status_args(show_stash: bool) -> Vec<&'static str> {
    let mut args = vec![
        "status",
        "--porcelain=v2",
        "--branch",
        "--ignore-submodules=all",
        // Said outright, so a `status.showUntrackedFiles=no` cannot make a tree look clean.
        "--untracked-files=normal",
    ];
    if show_stash {
        args.push("--show-stash");
    }
    args.push("-z");
    args
}

/// The stash's size from its reflog, for a git too old to report it with `status`. No stash is
/// a failure of the command, and so is zero.
async fn stash_count(path: &Path, cancel: &CancellationToken) -> u64 {
    let output = Git::read(
        path,
        &["rev-list", "--walk-reflogs", "--count", "refs/stash"],
        cancel,
    )
    .run()
    .await;
    output
        .ok()
        .and_then(|output| first_line(&output))
        .and_then(|count| count.parse().ok())
        .unwrap_or(0)
}

async fn last_commit(path: &Path, cancel: &CancellationToken) -> Result<Option<Commit>, String> {
    let output = Git::read(
        path,
        &[
            "log",
            "-1",
            "--no-show-signature",
            "--format=%H%x00%s%x00%an%x00%ct",
            "HEAD",
        ],
        cancel,
    )
    .run()
    .await
    .map_err(|error| error.message)?;
    if output.exit_code != Some(0) {
        return Err(format!("git log failed: {}", explanation(&output)));
    }
    Ok(parse_commit(output.stdout.text.trim_end_matches('\n')))
}

/// Parses `%H%x00%s%x00%an%x00%ct`.
fn parse_commit(text: &str) -> Option<Commit> {
    let mut fields = text.split('\0');
    let hash = fields.next()?.to_owned();
    let subject = fields.next()?.to_owned();
    let author = fields.next()?.to_owned();
    let seconds: i64 = fields.next()?.trim().parse().ok()?;
    let time = jiff::Timestamp::from_second(seconds).ok()?.to_string();
    Some(Commit {
        hash,
        subject,
        author,
        time,
    })
}

/// The commit checked out in `path`, when there is one.
pub(super) async fn head(
    path: &Path,
    cancel: &CancellationToken,
) -> Result<Option<String>, ErrorBody> {
    let output = Git::read(path, &["rev-parse", "--verify", "--quiet", "HEAD"], cancel)
        .run()
        .await?;
    Ok(first_line(&output))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn z(records: &[&str]) -> String {
        records.iter().map(|record| format!("{record}\0")).collect()
    }

    const OID: &str = "0123456789abcdef0123456789abcdef01234567";

    #[test]
    fn a_clean_tree_on_a_tracking_branch() {
        let status = parse_porcelain(&z(&[
            &format!("# branch.oid {OID}"),
            "# branch.head main",
            "# branch.upstream origin/main",
            "# branch.ab +0 -0",
        ]))
        .unwrap();
        assert_eq!(
            status,
            Porcelain {
                oid: Some(OID.to_owned()),
                branch: Some("main".to_owned()),
                upstream: Some("origin/main".to_owned()),
                ahead: Some(0),
                behind: Some(0),
                ..Porcelain::default()
            }
        );
        let state = RepoState::new(status, None);
        assert!(state.clean);
        assert!(!state.detached);
    }

    #[test]
    fn every_kind_of_change_is_counted() {
        let status = parse_porcelain(&z(&[
            &format!("# branch.oid {OID}"),
            "# branch.head main",
            "# stash 2",
            &format!("1 M. N... 100644 100644 100644 {OID} {OID} staged.txt"),
            &format!("1 .M N... 100644 100644 100644 {OID} {OID} unstaged.txt"),
            &format!("1 MM N... 100644 100644 100644 {OID} {OID} both.txt"),
            &format!("1 A. N... 000000 100644 100644 {OID} {OID} added.txt"),
            &format!("1 .D N... 100644 100644 000000 {OID} {OID} deleted.txt"),
            &format!("u UU N... 100644 100644 100644 100644 {OID} {OID} {OID} conflict.txt"),
            "? new file.txt",
            "? dir/",
            "! ignored.log",
        ]))
        .unwrap();
        assert_eq!(status.staged, 3);
        assert_eq!(status.unstaged, 3);
        assert_eq!(status.conflicted, 1);
        assert_eq!(status.untracked, 2);
        assert_eq!(status.stash, 2);
        assert_eq!(status.upstream, None);
        assert_eq!(status.ahead, None);
        assert!(!RepoState::new(status, None).clean);
    }

    #[test]
    fn a_detached_head_has_no_branch() {
        let status = parse_porcelain(&z(&[
            &format!("# branch.oid {OID}"),
            "# branch.head (detached)",
        ]))
        .unwrap();
        assert_eq!(status.branch, None);
        assert_eq!(status.oid.as_deref(), Some(OID));
        let state = RepoState::new(status, None);
        assert!(state.detached);
    }

    #[test]
    fn ahead_and_behind_are_read() {
        let status = parse_porcelain(&z(&[
            &format!("# branch.oid {OID}"),
            "# branch.head feature/x",
            "# branch.upstream origin/feature/x",
            "# branch.ab +3 -12",
        ]))
        .unwrap();
        assert_eq!((status.ahead, status.behind), (Some(3), Some(12)));
        assert_eq!(status.branch.as_deref(), Some("feature/x"));
        assert!(parse_porcelain(&z(&["# branch.ab 3 12"])).is_err());
    }

    #[test]
    fn an_upstream_that_is_gone_has_no_counts() {
        // git leaves out branch.ab when the upstream branch no longer exists.
        let status = parse_porcelain(&z(&[
            &format!("# branch.oid {OID}"),
            "# branch.head main",
            "# branch.upstream origin/gone",
        ]))
        .unwrap();
        assert_eq!(status.upstream.as_deref(), Some("origin/gone"));
        assert_eq!(status.ahead, None);
    }

    #[test]
    fn an_unborn_branch_has_no_commit() {
        let status = parse_porcelain(&z(&[
            "# branch.oid (initial)",
            "# branch.head main",
            "? README.md",
        ]))
        .unwrap();
        assert_eq!(status.oid, None);
        assert_eq!(status.branch.as_deref(), Some("main"));
        assert_eq!(status.untracked, 1);
        let state = RepoState::new(status, None);
        assert_eq!(state.head, None);
        assert!(!state.detached);
    }

    #[test]
    fn a_rename_takes_two_records_and_paths_may_hold_anything() {
        let status = parse_porcelain(&z(&[
            &format!("# branch.oid {OID}"),
            "# branch.head main",
            &format!("2 R. N... 100644 100644 100644 {OID} {OID} R100 new name.txt"),
            "old name.txt",
            &format!("2 RM N... 100644 100644 100644 {OID} {OID} R087 dir/b\tc.txt"),
            // An original path that looks like an entry must not be counted as one.
            "? looks untracked",
            &format!("1 .M N... 100644 100644 100644 {OID} {OID} with \"quotes\"\nand a newline"),
        ]))
        .unwrap();
        assert_eq!(status.staged, 2);
        assert_eq!(status.unstaged, 2);
        assert_eq!(status.untracked, 0);
        assert!(
            parse_porcelain(&z(&[&format!(
                "2 R. N... 100644 100644 100644 {OID} {OID} R100 x"
            )]))
            .is_err()
        );
    }

    #[test]
    fn unknown_headers_are_skipped_and_unknown_entries_refused() {
        let status = parse_porcelain(&z(&["# branch.oid (initial)", "# something.new 1"])).unwrap();
        assert_eq!(status, Porcelain::default());
        assert!(parse_porcelain(&z(&["x what"])).is_err());
        assert!(parse_porcelain(&z(&["1"])).is_err());
        assert_eq!(parse_porcelain("").unwrap(), Porcelain::default());
    }

    #[test]
    fn only_the_repository_s_own_filters_are_blanked() {
        let listing = "local\tfilter.evil.clean\nlocal\tfilter.evil.process\nworktree\tfilter.a.b.smudge\nglobal\tfilter.lfs.clean\nlocal\tfilter.evil.required\n";
        assert_eq!(
            blank_filters(listing),
            [
                "filter.a.b.clean=",
                "filter.a.b.smudge=",
                "filter.a.b.process=",
                "filter.a.b.required=false",
                "filter.evil.clean=",
                "filter.evil.smudge=",
                "filter.evil.process=",
                "filter.evil.required=false",
            ]
        );
        assert!(blank_filters("").is_empty());
    }

    #[test]
    fn only_an_ssh_command_from_a_file_counts_as_chosen() {
        assert!(!ssh_configured(""));
        assert!(!ssh_configured("command line:\t\n"));
        assert!(ssh_configured(
            "file:/home/me/.gitconfig\tssh -i ~/.ssh/work\ncommand line:\t\n"
        ));
        assert!(!ssh_configured("file:.git/config\t\n"));
    }

    #[test]
    fn an_older_git_is_asked_the_same_without_the_stash() {
        let current = status_args(true);
        let older = status_args(false);
        assert!(current.contains(&"--show-stash"));
        assert!(!older.contains(&"--show-stash"), "{older:?}");
        assert_eq!(
            older,
            current
                .iter()
                .copied()
                .filter(|arg| *arg != "--show-stash")
                .collect::<Vec<_>>()
        );
        assert!(
            older.contains(&"-z") && older.contains(&"--porcelain=v2"),
            "{older:?}"
        );
    }

    #[test]
    fn a_record_starting_with_any_character_is_refused_without_panicking() {
        for record in ["é what", "😀", "\u{301}1 .M"] {
            assert!(parse_porcelain(&z(&[record])).is_err(), "{record:?}");
        }
    }

    #[test]
    fn a_commit_line_becomes_a_commit() {
        let commit = parse_commit(&format!("{OID}\0Fix the thing\0Me\x001700000000")).unwrap();
        assert_eq!(commit.hash, OID);
        assert_eq!(commit.subject, "Fix the thing");
        assert_eq!(commit.author, "Me");
        assert_eq!(commit.time, "2023-11-14T22:13:20Z");
        assert!(parse_commit("").is_none());
        assert!(parse_commit(&format!("{OID}\0s\0a\0soon")).is_none());
    }
}
