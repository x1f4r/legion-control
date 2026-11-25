//! `projects.toml`: the named projects, where each lives per machine, and the tasks it offers.
//!
//! Paths are kept per node name because the same file is meant to be shared between machines,
//! where one project lives in a different directory on each. Edits go through `toml_edit` so the
//! comments and layout a person gave the file survive a tool changing one entry.

use std::collections::BTreeMap;
use std::fmt;
use std::path::Path;
use std::time::Duration;

use legion_core::Document;
use legion_core::config::NodeName;
use legion_core::proto::{ErrorBody, ErrorCode};
use legion_os::OsError;
use legion_os::lock::{FileLock, LockMode};
use serde::de::{self, MapAccess, Visitor};
use serde::{Deserialize, Deserializer};
use toml_edit::{DocumentMut, InlineTable, Item, Table, Value};

/// The longest project or task name.
pub(super) const MAX_NAME: usize = 64;
/// The longest a task may run before it is stopped, as for `exec.run`.
pub(super) const MAX_TASK_TIMEOUT_S: f64 = 3600.0;

/// `projects.toml`.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectsFile {
    /// Every project, by name.
    #[serde(default)]
    pub projects: BTreeMap<String, Project>,
}

/// One project.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Project {
    /// What it is, for people and agents choosing between projects.
    #[serde(default)]
    pub description: Option<String>,
    /// Where `proj.clone` fetches it from: anything `git clone` accepts.
    #[serde(default)]
    pub repo: Option<String>,
    /// Where it lives, by node name.
    #[serde(default)]
    pub on: BTreeMap<String, String>,
    /// What `proj.run` can run in it, by name.
    #[serde(default)]
    pub tasks: BTreeMap<String, Task>,
}

/// A task: a command line for the shell, or a table saying more.
#[derive(Clone, Debug, PartialEq)]
pub struct Task {
    /// The command line handed to the platform shell, when the task is one.
    pub cmd: Option<String>,
    /// The program and its arguments, run with no shell, when the task is one.
    pub argv: Option<Vec<String>>,
    /// Whether it runs as a background job that outlives the call.
    pub job: bool,
    /// Seconds before a task that is not a job is stopped.
    pub timeout_s: Option<f64>,
}

/// The table form of a task, as written.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TaskTable {
    #[serde(default)]
    cmd: Option<String>,
    #[serde(default)]
    argv: Option<Vec<String>>,
    #[serde(default)]
    job: bool,
    #[serde(default)]
    timeout_s: Option<f64>,
}

// Written by hand rather than as an untagged enum, whose only error is "did not match any
// variant": a misspelt key in a task table should be named.
impl<'de> Deserialize<'de> for Task {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Task, D::Error> {
        struct TaskVisitor;

        impl<'de> Visitor<'de> for TaskVisitor {
            type Value = Task;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a command line, or a table with `cmd` or `argv`")
            }

            fn visit_str<E: de::Error>(self, cmd: &str) -> Result<Task, E> {
                Ok(Task {
                    cmd: Some(cmd.to_owned()),
                    argv: None,
                    job: false,
                    timeout_s: None,
                })
            }

            fn visit_map<M: MapAccess<'de>>(self, map: M) -> Result<Task, M::Error> {
                let table = TaskTable::deserialize(de::value::MapAccessDeserializer::new(map))?;
                Ok(Task {
                    cmd: table.cmd,
                    argv: table.argv,
                    job: table.job,
                    timeout_s: table.timeout_s,
                })
            }
        }

        deserializer.deserialize_any(TaskVisitor)
    }
}

impl Document for ProjectsFile {
    const FILE: &'static str = "projects.toml";

    fn validate(&self) -> Result<(), String> {
        for (name, project) in &self.projects {
            check_name("project", name)?;
            if project
                .repo
                .as_deref()
                .is_some_and(|repo| repo.trim().is_empty())
            {
                return Err(format!("project {name}: `repo` is empty"));
            }
            for (node, path) in &project.on {
                NodeName::try_from(node.clone())
                    .map_err(|reason| format!("project {name}: `on.{node}`: {reason}"))?;
                if path.trim().is_empty() {
                    return Err(format!("project {name}: the path for {node} is empty"));
                }
            }
            for (task_name, task) in &project.tasks {
                check_name("task", task_name)
                    .map_err(|reason| format!("project {name}: {reason}"))?;
                check_task(task)
                    .map_err(|reason| format!("project {name}, task {task_name}: {reason}"))?;
            }
        }
        Ok(())
    }
}

impl ProjectsFile {
    /// The project's path on `node`, as written.
    pub fn path_on(&self, project: &str, node: &str) -> Option<&str> {
        self.projects.get(project)?.on.get(node).map(String::as_str)
    }
}

fn check_task(task: &Task) -> Result<(), String> {
    match (&task.cmd, &task.argv) {
        (Some(_), Some(_)) => return Err("give `cmd` or `argv`, not both".to_owned()),
        (None, None) => return Err("give the command to run, as `cmd` or `argv`".to_owned()),
        (Some(cmd), None) if cmd.trim().is_empty() => return Err("`cmd` is empty".to_owned()),
        (None, Some(argv)) if argv.first().is_none_or(String::is_empty) => {
            return Err("`argv` must start with the program to run".to_owned());
        }
        _ => {}
    }
    if let Some(seconds) = task.timeout_s {
        if task.job {
            return Err(
                "a job runs until it ends or job.cancel stops it, so it takes no `timeout_s`"
                    .to_owned(),
            );
        }
        if !(seconds > 0.0 && seconds <= MAX_TASK_TIMEOUT_S) {
            return Err(format!(
                "`timeout_s` must be more than 0 and at most {MAX_TASK_TIMEOUT_S}, not {seconds}"
            ));
        }
    }
    Ok(())
}

/// Checks a project or task name: letters, digits, `-`, `_` and `.`, starting with a letter or a
/// digit, at most [`MAX_NAME`] characters. `kind` names what it is in the message.
pub(super) fn check_name(kind: &str, name: &str) -> Result<(), String> {
    let starts_well = name
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_alphanumeric());
    let rest_fits = name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
    if starts_well && rest_fits && name.len() <= MAX_NAME {
        Ok(())
    } else {
        Err(format!(
            "{name:?} is not a {kind} name: use letters, digits, -, _ and ., starting with a letter or digit, at most {MAX_NAME} characters"
        ))
    }
}

/// Turns any text into a project name, for directories `proj.discover` finds: characters a name
/// cannot hold become `-`.
pub(super) fn name_from(text: &str) -> String {
    let cleaned: String = text
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                c
            } else {
                '-'
            }
        })
        .collect();
    let trimmed = cleaned.trim_start_matches(['-', '_', '.']);
    let mut name: String = trimmed.chars().take(MAX_NAME - 4).collect();
    if name.is_empty() {
        name.push_str("project");
    }
    name
}

/// One change to `projects.toml`.
#[derive(Clone, Debug)]
pub(super) enum Edit {
    /// Adds the project if it is new and sets what is given; other machines' paths stay.
    Set {
        name: String,
        node: String,
        path: Option<String>,
        repo: Option<String>,
        description: Option<String>,
    },
    /// Forgets the project's path on one node.
    ForgetHere { name: String, node: String },
    /// Forgets the whole project.
    Forget { name: String },
}

/// The longest a tool waits for another call that is changing `projects.toml`.
const LOCK_TIMEOUT: Duration = Duration::from_secs(10);

/// Changes `projects.toml` under a kernel lock, so two calls changing it at once cannot lose
/// each other's change.
///
/// `plan` sees the file as it is once the lock is held and says what to change and what to
/// return; the file is rewritten only when there is something to change, only with text that
/// parses, and atomically, so a reader never sees half a file. A file that does not parse is
/// never touched: the person who broke it is the one to mend it.
pub(super) fn update<T>(
    file: &Path,
    lock: &Path,
    plan: impl FnOnce(&ProjectsFile) -> Result<(Vec<Edit>, T), ErrorBody>,
) -> Result<T, ErrorBody> {
    let failed = |what: &str, error: &dyn fmt::Display| {
        ErrorBody::new(ErrorCode::Failed, format!("{what}: {error}."))
    };
    if let Some(dir) = lock.parent() {
        std::fs::create_dir_all(dir)
            .map_err(|error| failed(&format!("{} cannot be created", dir.display()), &error))?;
    }
    let _held =
        FileLock::acquire_timeout(lock, LockMode::Exclusive, LOCK_TIMEOUT).map_err(|error| {
            match error {
                OsError::TimedOut { .. } => ErrorBody::new(
                    ErrorCode::Busy,
                    "Another call has been changing projects.toml for too long.",
                )
                .with_hint("Try again in a moment."),
                other => failed(&format!("{} cannot be locked", lock.display()), &other),
            }
        })?;
    // A missing file is the empty default; one that exists must parse, empty or not, since an
    // editor caught half way through rewriting it can leave it empty for a moment.
    let text = legion_core::config::read_regular_file(file)?;
    let current = match &text {
        Some(bytes) => legion_core::config::parse::<ProjectsFile>(file, bytes)?,
        None => ProjectsFile::default(),
    };
    let (edits, out) = plan(&current)?;
    if edits.is_empty() {
        return Ok(out);
    }
    // parse() has just accepted the bytes as UTF-8.
    let text = String::from_utf8_lossy(text.as_deref().unwrap_or_default());
    let edited = apply(file, &text, &edits)?;
    if let Some(dir) = file.parent() {
        std::fs::create_dir_all(dir)
            .map_err(|error| failed(&format!("{} cannot be created", dir.display()), &error))?;
    }
    legion_core::config::write_atomically(file, edited.as_bytes())
        .map_err(|error| failed(&format!("{} cannot be written", file.display()), &error))?;
    Ok(out)
}

/// Applies edits to the text of `projects.toml` and returns the new text, checked as the document
/// it must be. The text is the empty string for a file that does not exist yet.
pub(super) fn apply(file: &Path, text: &str, edits: &[Edit]) -> Result<String, ErrorBody> {
    let broken = |message: String| {
        ErrorBody::new(ErrorCode::Failed, format!("{}: {message}.", file.display()))
            .with_hint("Fix the file by hand; tools change it only while it is valid.")
    };
    let mut doc: DocumentMut = text.parse().map_err(|error: toml_edit::TomlError| {
        broken(error.message().trim_end_matches('.').to_owned())
    })?;
    for edit in edits {
        apply_one(&mut doc, edit).map_err(broken)?;
    }
    let edited = doc.to_string();
    // The file is replaced only by text that a call reading it would accept.
    legion_core::config::parse::<ProjectsFile>(file, edited.as_bytes())?;
    Ok(edited)
}

fn apply_one(doc: &mut DocumentMut, edit: &Edit) -> Result<(), String> {
    let root = doc.as_table_mut();
    if !root.contains_key("projects") {
        let mut projects = Table::new();
        // Without this the file would gain an empty `[projects]` header above the first project.
        projects.set_implicit(true);
        root.insert("projects", Item::Table(projects));
    }
    let projects = root
        .get_mut("projects")
        .and_then(Item::as_table_like_mut)
        .ok_or("`projects` is not a table")?;
    match edit {
        Edit::Set {
            name,
            node,
            path,
            repo,
            description,
        } => {
            let name = name.as_str();
            if !projects.contains_key(name) {
                projects.insert(name, Item::Table(Table::new()));
            }
            let project = projects
                .get_mut(name)
                .ok_or_else(|| format!("project {name} vanished while it was being edited"))?;
            if let Some(description) = description {
                set_value(project, "description", description, name)?;
            }
            if let Some(repo) = repo {
                set_value(project, "repo", repo, name)?;
            }
            if let Some(path) = path {
                let on = child_table(project, "on", name)?;
                set_value(on, node, path, name)?;
            }
            Ok(())
        }
        Edit::ForgetHere { name, node } => {
            let on = projects
                .get_mut(name.as_str())
                .and_then(|project| project.as_table_like_mut())
                .and_then(|project| project.get_mut("on"))
                .and_then(Item::as_table_like_mut);
            if let Some(on) = on {
                on.remove(node.as_str());
            }
            Ok(())
        }
        Edit::Forget { name } => {
            let removed = projects.remove(name.as_str());
            // A file whose last project went would say nothing, which the loader refuses as a
            // file caught half written; an empty `[projects]` table says there are none.
            if projects.is_empty()
                && let Some(Item::Table(projects)) = root.get_mut("projects")
            {
                projects.set_implicit(false);
            }
            if let Some(Item::Table(removed)) = removed {
                keep_leading_comments(doc, &removed);
            }
            Ok(())
        }
    }
}

/// Keeps the comments above a removed table that a blank line sets apart from it.
///
/// toml_edit hangs every comment above a table header on that table, so the comment that opens
/// the file belongs to the first project. Text the blank line separates from the header is about
/// the file or what comes before, not the project, so it moves to the next table, or to the end of
/// the file when none follows.
fn keep_leading_comments(doc: &mut DocumentMut, removed: &Table) {
    // A project written only as `[projects.site.on]` has no header of its own; the comments above
    // it hang on the first of its tables the file spells out.
    let Some(first) = first_written(removed) else {
        return;
    };
    let Some(prefix) = first.decor().prefix().and_then(|prefix| prefix.as_str()) else {
        return;
    };
    let lines: Vec<&str> = prefix.split_inclusive('\n').collect();
    let Some(last_blank) = lines.iter().rposition(|line| line.trim().is_empty()) else {
        return;
    };
    let kept: String = lines[..=last_blank].concat();
    if !kept.contains('#') {
        return;
    }
    let next = first
        .position()
        .and_then(|after| next_position(doc.as_table(), after));
    match next.and_then(|position| table_at(doc.as_table_mut(), position)) {
        Some(table) => {
            let rest = table
                .decor()
                .prefix()
                .and_then(|prefix| prefix.as_str())
                .unwrap_or_default()
                .to_owned();
            table.decor_mut().set_prefix(format!("{kept}{rest}"));
        }
        None => {
            let trailing = doc.trailing().as_str().unwrap_or_default().to_owned();
            doc.set_trailing(format!("{trailing}{kept}"));
        }
    }
}

/// The table among `table` and those under it whose header comes first in the file, leaving out
/// the ones only implied by a longer header.
fn first_written(table: &Table) -> Option<&Table> {
    let own = (!table.is_implicit() && table.position().is_some()).then_some(table);
    let children = table
        .iter()
        .filter_map(|(_, item)| item.as_table())
        .filter_map(first_written);
    own.into_iter()
        .chain(children)
        .min_by_key(|table| table.position())
}

/// The position of the first table written after `after`.
fn next_position(table: &Table, after: isize) -> Option<isize> {
    table
        .iter()
        .filter_map(|(_, item)| item.as_table())
        .flat_map(|child| {
            let own = child.position().filter(|position| *position > after);
            own.into_iter().chain(next_position(child, after))
        })
        .min()
}

/// The table written at `position`.
fn table_at(table: &mut Table, position: isize) -> Option<&mut Table> {
    for (_, item) in table.iter_mut() {
        if let Item::Table(child) = item {
            if child.position() == Some(position) {
                return Some(child);
            }
            if holds(child, position) {
                return table_at(child, position);
            }
        }
    }
    None
}

fn holds(table: &Table, position: isize) -> bool {
    table
        .iter()
        .filter_map(|(_, item)| item.as_table())
        .any(|child| child.position() == Some(position) || holds(child, position))
}

/// Sets a string value in a table, however the table is written.
fn set_value(item: &mut Item, key: &str, value: &str, project: &str) -> Result<(), String> {
    let table = item
        .as_table_like_mut()
        .ok_or_else(|| format!("project {project} is not a table"))?;
    let added = match table.get_mut(key) {
        // Assigning in place keeps the comment written beside the old value.
        Some(Item::Value(old)) => {
            let decor = old.decor().clone();
            *old = Value::from(value);
            *old.decor_mut() = decor;
            false
        }
        _ => {
            table.insert(key, toml_edit::value(value));
            true
        }
    };
    // A key added to `{ a = "x" }` would otherwise keep the space that closed the table.
    if added && let Some(inline) = item.as_inline_table_mut() {
        inline.fmt();
    }
    Ok(())
}

/// The sub-table `key` of a table, created in the same style as its parent when it is missing.
fn child_table<'a>(parent: &'a mut Item, key: &str, project: &str) -> Result<&'a mut Item, String> {
    let not_table = || format!("project {project}: `{key}` is not a table");
    match parent {
        Item::Table(table) => {
            if !table.contains_key(key) {
                table.insert(key, Item::Table(Table::new()));
            }
        }
        Item::Value(Value::InlineTable(table)) => {
            if !table.contains_key(key) {
                table.insert(key, Value::InlineTable(InlineTable::new()));
            }
        }
        _ => return Err(format!("project {project} is not a table")),
    }
    let child = parent
        .as_table_like_mut()
        .and_then(|table| table.get_mut(key))
        .ok_or_else(not_table)?;
    if child.is_table_like() {
        Ok(child)
    } else {
        Err(not_table())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> Result<ProjectsFile, ErrorBody> {
        legion_core::config::parse::<ProjectsFile>(Path::new("projects.toml"), text.as_bytes())
    }

    const SAMPLE: &str = r#"# The machines' projects.

[projects.site]
description = "The public website"   # shown by proj.list
repo = "git@git.example:me/site.git"

[projects.site.on]                 # where it lives
tower = "~/projects/site"
laptop = "~/code/site"

[projects.site.tasks]
test = "npm test"
dev = { cmd = "npm run dev", job = true }
build = { argv = ["cargo", "build", "--release"], timeout_s = 1800 }

# Notes, kept apart.
[projects."notes.d"]
on = { pi = "/home/me/notes" }
"#;

    #[test]
    fn the_example_file_parses_with_every_kind_of_task() {
        let file = parse(SAMPLE).unwrap();
        let site = &file.projects["site"];
        assert_eq!(site.description.as_deref(), Some("The public website"));
        assert_eq!(site.on["tower"], "~/projects/site");
        assert_eq!(site.tasks["test"].cmd.as_deref(), Some("npm test"));
        assert!(!site.tasks["test"].job);
        assert!(site.tasks["dev"].job);
        assert_eq!(
            site.tasks["build"].argv.as_deref(),
            Some(
                &[
                    "cargo".to_owned(),
                    "build".to_owned(),
                    "--release".to_owned()
                ][..]
            )
        );
        assert_eq!(site.tasks["build"].timeout_s, Some(1800.0));
        assert_eq!(file.path_on("notes.d", "pi"), Some("/home/me/notes"));
        assert_eq!(file.path_on("notes.d", "tower"), None);
        assert!(parse("[projects]\n").unwrap().projects.is_empty());
    }

    #[test]
    fn mistakes_are_named_with_their_line() {
        for (text, expected) in [
            ("[projects.site]\nrepository = \"x\"\n", "repository"),
            (
                "[projects.site.tasks]\nt = { cmd = \"x\", jobs = true }\n",
                "jobs",
            ),
            ("[projects.site.tasks]\nt = 3\n", "a command line"),
            ("[projects.\"bad name\"]\n", "not a project name"),
            ("[projects.\".hidden\"]\n", "not a project name"),
            (
                "[projects.site.tasks]\n\"a/b\" = \"x\"\n",
                "not a task name",
            ),
            ("[projects.site.on]\nTower = \"/x\"\n", "on.Tower"),
            (
                "[projects.site.on]\ntower = \"\"\n",
                "path for tower is empty",
            ),
            ("[projects.site]\nrepo = \" \"\n", "`repo` is empty"),
            (
                "[projects.site.tasks]\nt = { cmd = \"x\", argv = [\"x\"] }\n",
                "not both",
            ),
            (
                "[projects.site.tasks]\nt = { job = true }\n",
                "as `cmd` or `argv`",
            ),
            (
                "[projects.site.tasks]\nt = { argv = [] }\n",
                "must start with the program",
            ),
            ("[projects.site.tasks]\nt = \"  \"\n", "`cmd` is empty"),
            (
                "[projects.site.tasks]\nt = { cmd = \"x\", timeout_s = 0 }\n",
                "more than 0",
            ),
            (
                "[projects.site.tasks]\nt = { cmd = \"x\", timeout_s = 4000 }\n",
                "at most 3600",
            ),
            (
                "[projects.site.tasks]\nt = { cmd = \"x\", job = true, timeout_s = 5 }\n",
                "job.cancel",
            ),
        ] {
            let error = parse(text).unwrap_err();
            assert!(
                error.message.contains(expected),
                "{text:?}: {}",
                error.message
            );
            assert!(error.message.contains("projects.toml"), "{}", error.message);
        }
        let error = parse("\n\n[projects.site]\nrepository = \"x\"\n").unwrap_err();
        assert!(error.message.contains("line 4"), "{}", error.message);
    }

    #[test]
    fn names_are_short_and_plain() {
        for good in ["site", "my-site", "a_b.c", "1x", &"a".repeat(MAX_NAME)] {
            assert!(check_name("project", good).is_ok(), "{good}");
        }
        for bad in [
            "",
            ".",
            "..",
            "-x",
            "a b",
            "a/b",
            "é",
            &"a".repeat(MAX_NAME + 1),
        ] {
            assert!(check_name("project", bad).is_err(), "{bad}");
        }
        assert_eq!(name_from("My Site!"), "My-Site-");
        assert_eq!(name_from(".dotfiles"), "dotfiles");
        assert_eq!(name_from("..."), "project");
        assert!(check_name("project", &name_from(&"é".repeat(200))).is_ok());
    }

    fn set(name: &str, node: &str, path: Option<&str>) -> Edit {
        Edit::Set {
            name: name.to_owned(),
            node: node.to_owned(),
            path: path.map(str::to_owned),
            repo: None,
            description: None,
        }
    }

    #[test]
    fn adding_a_path_keeps_comments_and_other_machines() {
        let file = Path::new("projects.toml");
        let edited = apply(file, SAMPLE, &[set("site", "pi", Some("/srv/site"))]).unwrap();
        assert!(edited.contains("# The machines' projects."), "{edited}");
        assert!(edited.contains("# shown by proj.list"), "{edited}");
        assert!(edited.contains("# where it lives"), "{edited}");
        assert!(edited.contains("# Notes, kept apart."), "{edited}");
        let parsed = parse(&edited).unwrap();
        assert_eq!(parsed.path_on("site", "pi"), Some("/srv/site"));
        assert_eq!(parsed.path_on("site", "laptop"), Some("~/code/site"));
        assert_eq!(parsed.projects["site"].tasks.len(), 3);

        // Changing a value keeps the comment beside it, and an inline table stays inline.
        let edited = apply(
            file,
            &edited,
            &[
                Edit::Set {
                    name: "site".into(),
                    node: "tower".into(),
                    path: Some("~/elsewhere".into()),
                    repo: None,
                    description: Some("The website".into()),
                },
                set("notes.d", "tower", Some("~/notes")),
            ],
        )
        .unwrap();
        assert!(
            edited.contains("description = \"The website\"   # shown by proj.list"),
            "{edited}"
        );
        assert!(
            edited.contains("on = { pi = \"/home/me/notes\", tower = \"~/notes\" }"),
            "{edited}"
        );
        let parsed = parse(&edited).unwrap();
        assert_eq!(parsed.path_on("site", "tower"), Some("~/elsewhere"));
    }

    #[test]
    fn a_new_project_in_a_new_file_reads_naturally() {
        let file = Path::new("projects.toml");
        let edited = apply(
            file,
            "",
            &[Edit::Set {
                name: "site".into(),
                node: "tower".into(),
                path: Some("~/projects/site".into()),
                repo: Some("https://git.example/me/site.git".into()),
                description: None,
            }],
        )
        .unwrap();
        assert_eq!(
            edited,
            "[projects.site]\nrepo = \"https://git.example/me/site.git\"\n\n[projects.site.on]\ntower = \"~/projects/site\"\n"
        );
        let added = apply(file, &edited, &[set("my.app", "tower", Some("/x"))]).unwrap();
        assert!(added.contains("[projects.\"my.app\".on]"), "{added}");
        assert_eq!(
            parse(&added).unwrap().path_on("my.app", "tower"),
            Some("/x")
        );
    }

    #[test]
    fn forgetting_removes_one_path_or_the_whole_project() {
        let file = Path::new("projects.toml");
        let here = apply(
            file,
            SAMPLE,
            &[Edit::ForgetHere {
                name: "site".into(),
                node: "tower".into(),
            }],
        )
        .unwrap();
        let parsed = parse(&here).unwrap();
        assert_eq!(parsed.path_on("site", "tower"), None);
        assert_eq!(parsed.path_on("site", "laptop"), Some("~/code/site"));
        assert!(here.contains("# where it lives"), "{here}");
        let gone = apply(
            file,
            &here,
            &[Edit::Forget {
                name: "site".into(),
            }],
        )
        .unwrap();
        let parsed = parse(&gone).unwrap();
        assert!(!parsed.projects.contains_key("site"));
        assert!(parsed.projects.contains_key("notes.d"));
        assert!(gone.contains("# The machines' projects."), "{gone}");
    }

    #[test]
    fn forgetting_the_last_project_leaves_a_file_the_loader_reads() {
        let file = Path::new("projects.toml");
        let text = "# Mine.\n\n[projects.site]\nrepo = \"https://git.example/me/site.git\"\n";
        let gone = apply(
            file,
            text,
            &[Edit::Forget {
                name: "site".into(),
            }],
        )
        .unwrap();
        assert!(parse(&gone).unwrap().projects.is_empty(), "{gone}");
        assert!(gone.contains("# Mine."), "{gone}");
        let dotted = apply(
            file,
            "[projects.site.on]\ntower = \"/x\"\n",
            &[Edit::Forget {
                name: "site".into(),
            }],
        )
        .unwrap();
        assert!(parse(&dotted).unwrap().projects.is_empty(), "{dotted}");
    }

    #[test]
    fn forgetting_a_project_written_only_through_its_subtables_keeps_the_header() {
        let file = Path::new("projects.toml");
        let text = "# Header.\n\n# About site.\n[projects.site.on]\ntower = \"/x\"\n\n[projects.site.tasks]\nt = \"x\"\n\n# Other.\n[projects.other.on]\npi = \"/y\"\n";
        let gone = apply(
            file,
            text,
            &[Edit::Forget {
                name: "site".into(),
            }],
        )
        .unwrap();
        assert!(gone.starts_with("# Header.\n"), "{gone}");
        assert!(!gone.contains("About site"), "{gone}");
        assert!(gone.contains("# Other."), "{gone}");
        assert!(!parse(&gone).unwrap().projects.contains_key("site"));
        let alone = apply(
            file,
            "# Header.\n\n[projects.site.on]\ntower = \"/x\"\n",
            &[Edit::Forget {
                name: "site".into(),
            }],
        )
        .unwrap();
        assert!(alone.contains("# Header."), "{alone}");
        assert!(parse(&alone).unwrap().projects.is_empty(), "{alone}");
    }

    #[test]
    fn a_missing_file_is_created_and_an_empty_one_refused() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("config").join("projects.toml");
        let lock = dir.path().join("state").join("projects.lock");
        let edits = || vec![set("site", "tower", Some("/x"))];
        update(&file, &lock, |_| Ok((edits(), ()))).unwrap();
        assert_eq!(
            parse(&std::fs::read_to_string(&file).unwrap())
                .unwrap()
                .path_on("site", "tower"),
            Some("/x")
        );
        std::fs::write(&file, "").unwrap();
        let error = update(&file, &lock, |_| Ok((edits(), ()))).unwrap_err();
        assert!(error.message.contains("says nothing"), "{}", error.message);
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "");
    }

    #[test]
    fn an_edit_that_would_leave_an_invalid_file_is_refused() {
        let file = Path::new("projects.toml");
        let error = apply(
            file,
            "[projects]\nsite = 3\n",
            &[set("site", "tower", Some("/x"))],
        )
        .unwrap_err();
        assert_eq!(error.code, ErrorCode::Failed);
        let error = apply(file, "projects = 1\n", &[set("site", "tower", Some("/x"))]).unwrap_err();
        assert!(error.message.contains("not a table"), "{}", error.message);
        let error = apply(
            file,
            "[projects.site]\nrepository = 1\n",
            &[set("site", "tower", Some("/x"))],
        )
        .unwrap_err();
        assert!(error.message.contains("repository"), "{}", error.message);
        assert!(apply(file, "[x", &[]).is_err());
    }
}
