//! Finding git working trees under a directory.
//!
//! The walk stops at each working tree it finds, since what lies inside one belongs to it, and
//! never enters hidden directories or the dependency and build directories that hold thousands of
//! other people's repositories. Symbolic links are not followed, so a link back up the tree
//! cannot make the walk go round in circles or out of the directory it was given.

use std::path::{Path, PathBuf};

/// Directories that hold dependencies or build output rather than projects of one's own.
const SKIPPED: &[&str] = &[
    "node_modules",
    "target",
    "vendor",
    "bower_components",
    "__pycache__",
    "venv",
    "dist",
    "build",
    "Pods",
];

/// The most working trees one walk reports.
pub(super) const MAX_FOUND: usize = 500;
/// The most directories one walk reads, so a walk of a huge tree still answers promptly.
const MAX_VISITED: usize = 20_000;

/// What a walk found.
#[derive(Debug, Default)]
pub(super) struct Walk {
    /// The working trees, in the order a person reading the tree would meet them.
    pub found: Vec<PathBuf>,
    /// Whether the walk stopped at a limit before it had looked everywhere.
    pub truncated: bool,
}

/// Walks `root` down to `max_depth` levels below it. `root` itself counts when it is a working
/// tree. Directories that cannot be read are passed over.
pub(super) fn walk(root: &Path, max_depth: u32) -> Walk {
    let mut walk = Walk::default();
    let mut visited = 0;
    visit(root, 0, max_depth, &mut walk, &mut visited);
    walk
}

fn visit(dir: &Path, depth: u32, max_depth: u32, walk: &mut Walk, visited: &mut usize) {
    if walk.truncated {
        return;
    }
    if is_working_tree(dir) {
        if walk.found.len() >= MAX_FOUND {
            walk.truncated = true;
        } else {
            walk.found.push(dir.to_path_buf());
        }
        return;
    }
    if depth >= max_depth {
        return;
    }
    *visited += 1;
    if *visited > MAX_VISITED {
        walk.truncated = true;
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut children: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
        .filter(|entry| {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            !name.starts_with('.') && !SKIPPED.contains(&name.as_ref())
        })
        .map(|entry| entry.path())
        .collect();
    children.sort();
    for child in children {
        visit(&child, depth + 1, max_depth, walk, visited);
    }
}

/// A working tree has `.git` in it: a directory for an ordinary clone, a file for a linked
/// worktree or a submodule.
fn is_working_tree(dir: &Path) -> bool {
    std::fs::symlink_metadata(dir.join(".git")).is_ok()
}

/// Names for the directories found, from their last part, made unique among themselves and
/// against `taken`. The shallower of two trees with the same name keeps it, since it is the one a
/// person means by the bare name: `code/site` is `site`, `code/old/site` becomes `site-2`.
pub(super) fn unique_names<'a>(
    paths: impl IntoIterator<Item = &'a Path>,
    taken: &[String],
) -> Vec<String> {
    let paths: Vec<&Path> = paths.into_iter().collect();
    let mut order: Vec<usize> = (0..paths.len()).collect();
    order.sort_by_key(|index| paths[*index].components().count());
    let mut used: Vec<String> = taken.to_vec();
    let mut names = vec![String::new(); paths.len()];
    for index in order {
        let base = super::file::name_from(
            &paths[index]
                .file_name()
                .map_or_else(|| "project".into(), |name| name.to_string_lossy()),
        );
        let mut name = base.clone();
        let mut n = 2;
        while used.contains(&name) {
            name = format!("{base}-{n}");
            n += 1;
        }
        used.push(name.clone());
        names[index] = name;
    }
    names
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tree(paths: &[&str]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for path in paths {
            let full = dir.path().join(path);
            if path.ends_with(".git-file") {
                std::fs::create_dir_all(full.parent().unwrap()).unwrap();
                std::fs::write(full.with_file_name(".git"), "gitdir: /elsewhere\n").unwrap();
            } else {
                std::fs::create_dir_all(full).unwrap();
            }
        }
        dir
    }

    fn found(walk: &Walk, root: &Path) -> Vec<String> {
        walk.found
            .iter()
            .map(|path| {
                path.strip_prefix(root)
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/")
            })
            .collect()
    }

    #[test]
    fn working_trees_are_found_without_entering_them_or_skipped_places() {
        let dir = tree(&[
            "a/.git",
            "a/inner/.git",
            "group/b/.git",
            "group/c/.git-file",
            ".hidden/d/.git",
            "node_modules/e/.git",
            "x/target/f/.git",
            "deep/1/2/3/.git",
            "plain/nothing",
        ]);
        let walk = walk(dir.path(), 3);
        assert_eq!(found(&walk, dir.path()), ["a", "group/b", "group/c"]);
        assert!(!walk.truncated);
        let deeper = super::walk(dir.path(), 4);
        assert_eq!(
            found(&deeper, dir.path()),
            ["a", "deep/1/2/3", "group/b", "group/c"]
        );
    }

    #[test]
    fn the_root_itself_can_be_the_working_tree() {
        let dir = tree(&[".git", "sub/.git"]);
        let walk = walk(dir.path(), 3);
        assert_eq!(walk.found, [dir.path().to_path_buf()]);
    }

    #[test]
    fn names_are_made_unique() {
        let paths = [
            Path::new("/home/me/a/old/site"),
            Path::new("/home/me/b/site"),
            Path::new("/home/me/My App"),
            Path::new("/home/me/c/site"),
        ];
        assert_eq!(
            unique_names(paths, &["site-2".to_owned()]),
            ["site-4", "site", "My-App", "site-3"]
        );
    }
}
