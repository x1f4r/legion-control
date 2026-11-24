//! Deterministic tree and single-file manifests with exclusions and case-collision checks.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use globset::{GlobBuilder, GlobSet, GlobSetBuilder};
use serde::{Deserialize, Serialize};

use crate::error::{Error, Result, classify_io};
use crate::hash::{Hash, hash_file};
use crate::platform;

/// Filesystem object kind recorded in a [`Manifest`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EntryKind {
    /// Regular file.
    File,
    /// Directory.
    Dir,
    /// Symbolic link (never followed during listing).
    Symlink,
}

/// Modification time represented as signed Unix seconds plus sub-second nanoseconds.
///
/// Using an explicit integer pair avoids floating-point drift and serializes cleanly across
/// languages and operating systems, including timestamps prior to 1970.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct Timestamp {
    /// Whole seconds since the Unix epoch (negative before 1970-01-01T00:00:00Z).
    pub secs: i64,
    /// Sub-second nanoseconds in `0..1_000_000_000`.
    pub nanos: u32,
}

impl Timestamp {
    /// Creates a timestamp from raw seconds and nanoseconds.
    pub fn new(secs: i64, nanos: u32) -> Self {
        Self { secs, nanos }
    }

    /// Converts a [`SystemTime`] into signed Unix seconds plus sub-second nanoseconds.
    pub fn from_system_time(time: SystemTime) -> Self {
        match time.duration_since(UNIX_EPOCH) {
            Ok(d) => Self {
                secs: d.as_secs().min(i64::MAX as u64) as i64,
                nanos: d.subsec_nanos(),
            },
            Err(err) => {
                let d = err.duration();
                let whole = d.as_secs().min(i64::MAX as u64) as i64;
                let nanos = d.subsec_nanos();
                if nanos == 0 {
                    Self {
                        secs: -whole,
                        nanos: 0,
                    }
                } else {
                    Self {
                        secs: -whole - 1,
                        nanos: 1_000_000_000 - nanos,
                    }
                }
            }
        }
    }

    /// Extracts the last-modification timestamp from filesystem metadata.
    pub(crate) fn from_metadata(meta: &fs::Metadata) -> Self {
        match meta.modified() {
            Ok(time) => Self::from_system_time(time),
            Err(_) => Self { secs: 0, nanos: 0 },
        }
    }
}

/// One entry in a [`Manifest`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    /// Path relative to the listed root, always using `/` separators.
    pub path: String,
    /// Kind of filesystem object.
    pub kind: EntryKind,
    /// Byte length for regular files; `0` for directories and symlinks so output is deterministic.
    pub size: u64,
    /// Last modification timestamp.
    pub mtime: Timestamp,
    /// Unix permission bits (`0o7777` mask); omitted on Windows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<u32>,
    /// Target path recorded verbatim for symlinks.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub link_target: Option<String>,
    /// BLAKE3 digest for regular files when requested via [`ManifestOptions::checksum`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hash: Option<Hash>,
}

/// An entry that could not be inspected or read during listing or unpacking.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct SkippedEntry {
    /// Relative path (lossy UTF-8 if the OS filename was not valid UTF-8).
    pub path: String,
    /// Human-readable reason the entry was skipped.
    pub reason: String,
}

/// Two or more distinct paths that fold to the same string on case-insensitive filesystems.
///
/// Reporting these explicitly prevents silent overwrites when transferring between case-sensitive
/// and case-insensitive machines.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct CaseCollision {
    /// Lowercased canonical path shared by all colliding entries.
    pub canonical: String,
    /// Distinct exact paths that collide under case folding, in sorted order.
    pub paths: Vec<String>,
}

/// Complete, deterministically ordered snapshot of a directory tree or single file.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    /// Readable entries in deterministic lexicographical order by [`Entry::path`].
    pub entries: Vec<Entry>,
    /// Entries skipped because they could not be read or represented safely.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skipped: Vec<SkippedEntry>,
    /// Entries whose paths differ only in letter case.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub case_collisions: Vec<CaseCollision>,
}

/// Options controlling how [`build`] walks and inspects a path.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ManifestOptions {
    /// When `true`, compute a streaming BLAKE3 digest for every regular file.
    pub checksum: bool,
    /// Optional glob patterns excluding matching paths (trailing `/` restricts to directories).
    pub exclude: Vec<String>,
}

/// Lists `root` (a directory tree, single file, or single symlink) into a deterministic [`Manifest`].
///
/// Symlinks are recorded with their targets and never followed. Individual entries inside a tree
/// that fail to read are reported in [`Manifest::skipped`] instead of aborting the listing.
pub fn build(root: &Path, options: &ManifestOptions) -> Result<Manifest> {
    build_pruned(root, options, |_| None)
}

/// Like [`build`], asking `prune` about each directory below `root` before looking inside it: a
/// directory for which it gives a reason is left out with everything in it, and named in
/// [`Manifest::skipped`] with that reason. Nothing inside it is read, listed or hashed.
pub fn build_pruned<F>(root: &Path, options: &ManifestOptions, mut prune: F) -> Result<Manifest>
where
    F: FnMut(&Path) -> Option<String>,
{
    let matcher = ExcludeMatcher::compile(&options.exclude)?;
    let root_meta = fs::symlink_metadata(root).map_err(|e| classify_io(root, e))?;

    let mut manifest = Manifest::default();
    let ft = root_meta.file_type();

    if ft.is_dir() {
        // Verify the root directory itself is readable; unreadable child directories go to skipped.
        let root_entries = read_dir_sorted(root).map_err(|e| classify_io(root, e))?;
        walk_entries(
            root_entries,
            "",
            &matcher,
            options.checksum,
            &mut prune,
            &mut manifest.entries,
            &mut manifest.skipped,
        );
    } else {
        let Some(name_os) = root.file_name() else {
            return Err(Error::Conflict {
                path: root.to_path_buf(),
                reason: "source path has no file name".to_string(),
            });
        };
        let Some(name) = name_os.to_str() else {
            manifest.skipped.push(SkippedEntry {
                path: name_os.to_string_lossy().into_owned(),
                reason: "non-UTF-8 file name".to_string(),
            });
            return Ok(manifest);
        };
        if !platform::is_internal_temp_name(name) && !matcher.is_excluded(name, name, false) {
            record_non_dir(
                root,
                name,
                &root_meta,
                options.checksum,
                &mut manifest.entries,
                &mut manifest.skipped,
            );
        }
    }

    manifest.entries.sort_by(|a, b| a.path.cmp(&b.path));
    manifest.skipped.sort();
    manifest.case_collisions =
        detect_case_collisions(manifest.entries.iter().map(|e| e.path.as_str()));

    Ok(manifest)
}

/// Finds all case-only path collisions among `paths`, including colliding ancestor directories.
pub fn detect_case_collisions<'a, I>(paths: I) -> Vec<CaseCollision>
where
    I: IntoIterator<Item = &'a str>,
{
    let mut grouped: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();

    for path in paths {
        if path.is_empty() {
            continue;
        }
        insert_with_ancestors(&mut grouped, path);
    }

    grouped
        .into_iter()
        .filter_map(|(canonical, set)| {
            if set.len() > 1 {
                Some(CaseCollision {
                    canonical,
                    paths: set.into_iter().collect(),
                })
            } else {
                None
            }
        })
        .collect()
}

fn insert_with_ancestors(grouped: &mut BTreeMap<String, BTreeSet<String>>, path: &str) {
    let mut current = path;
    loop {
        grouped
            .entry(current.to_lowercase())
            .or_default()
            .insert(current.to_string());
        match current.rsplit_once('/') {
            Some((parent, _)) if !parent.is_empty() => current = parent,
            _ => break,
        }
    }
}

struct DirChild {
    path: std::path::PathBuf,
    file_name: std::ffi::OsString,
}

fn read_dir_sorted(dir: &Path) -> std::io::Result<Vec<DirChild>> {
    let mut children = Vec::new();
    for item in fs::read_dir(dir)? {
        let entry = item?;
        children.push(DirChild {
            path: entry.path(),
            file_name: entry.file_name(),
        });
    }
    children.sort_by(|a, b| a.file_name.cmp(&b.file_name));
    Ok(children)
}

fn walk_entries(
    children: Vec<DirChild>,
    prefix: &str,
    matcher: &ExcludeMatcher,
    checksum: bool,
    prune: &mut dyn FnMut(&Path) -> Option<String>,
    entries: &mut Vec<Entry>,
    skipped: &mut Vec<SkippedEntry>,
) {
    for child in children {
        let Some(name) = child.file_name.to_str() else {
            let lossy = child.file_name.to_string_lossy();
            let rel = join_rel(prefix, &lossy);
            skipped.push(SkippedEntry {
                path: rel,
                reason: "non-UTF-8 file name".to_string(),
            });
            continue;
        };

        if platform::is_internal_temp_name(name) {
            continue;
        }

        let rel = join_rel(prefix, name);
        let meta = match fs::symlink_metadata(&child.path) {
            Ok(m) => m,
            Err(err) => {
                skipped.push(SkippedEntry {
                    path: rel,
                    reason: err.to_string(),
                });
                continue;
            }
        };

        let ft = meta.file_type();
        let is_dir = ft.is_dir();

        if matcher.is_excluded(&rel, name, is_dir) {
            continue;
        }

        if is_dir {
            if let Some(reason) = prune(&child.path) {
                skipped.push(SkippedEntry { path: rel, reason });
                continue;
            }
            let sub_children = match read_dir_sorted(&child.path) {
                Ok(c) => c,
                Err(err) => {
                    skipped.push(SkippedEntry {
                        path: rel,
                        reason: err.to_string(),
                    });
                    continue;
                }
            };
            entries.push(Entry {
                path: rel.clone(),
                kind: EntryKind::Dir,
                size: 0,
                mtime: Timestamp::from_metadata(&meta),
                mode: platform::entry_mode(&meta),
                link_target: None,
                hash: None,
            });
            walk_entries(
                sub_children,
                &rel,
                matcher,
                checksum,
                prune,
                entries,
                skipped,
            );
        } else {
            record_non_dir(&child.path, &rel, &meta, checksum, entries, skipped);
        }
    }
}

fn record_non_dir(
    full_path: &Path,
    rel: &str,
    meta: &fs::Metadata,
    checksum: bool,
    entries: &mut Vec<Entry>,
    skipped: &mut Vec<SkippedEntry>,
) {
    let ft = meta.file_type();
    if ft.is_symlink() {
        let target_path = match fs::read_link(full_path) {
            Ok(t) => t,
            Err(err) => {
                skipped.push(SkippedEntry {
                    path: rel.to_string(),
                    reason: err.to_string(),
                });
                return;
            }
        };
        let Some(target) = target_path.to_str() else {
            skipped.push(SkippedEntry {
                path: rel.to_string(),
                reason: "non-UTF-8 symlink target".to_string(),
            });
            return;
        };
        entries.push(Entry {
            path: rel.to_string(),
            kind: EntryKind::Symlink,
            size: 0,
            mtime: Timestamp::from_metadata(meta),
            mode: platform::entry_mode(meta),
            link_target: Some(target.to_string()),
            hash: None,
        });
    } else if ft.is_file() {
        let hash = if checksum {
            match hash_file(full_path) {
                Ok(h) => Some(h),
                Err(err) => {
                    skipped.push(SkippedEntry {
                        path: rel.to_string(),
                        reason: err.to_string(),
                    });
                    return;
                }
            }
        } else {
            None
        };
        entries.push(Entry {
            path: rel.to_string(),
            kind: EntryKind::File,
            size: meta.len(),
            mtime: Timestamp::from_metadata(meta),
            mode: platform::entry_mode(meta),
            link_target: None,
            hash,
        });
    } else {
        skipped.push(SkippedEntry {
            path: rel.to_string(),
            reason: "unsupported file type".to_string(),
        });
    }
}

fn join_rel(prefix: &str, name: &str) -> String {
    if prefix.is_empty() {
        name.to_string()
    } else {
        format!("{prefix}/{name}")
    }
}

/// Compiled exclude matcher separating path globs, basename globs, and directory-only globs.
struct ExcludeMatcher {
    any_path: GlobSet,
    any_name: GlobSet,
    dir_path: GlobSet,
    dir_name: GlobSet,
}

impl ExcludeMatcher {
    fn compile(patterns: &[String]) -> Result<Self> {
        let mut any_path = GlobSetBuilder::new();
        let mut any_name = GlobSetBuilder::new();
        let mut dir_path = GlobSetBuilder::new();
        let mut dir_name = GlobSetBuilder::new();

        for raw in patterns {
            let trimmed = raw.trim();
            if trimmed.is_empty() {
                continue;
            }
            let (dir_only, body) = match trimmed.strip_suffix('/') {
                Some(stripped) => (true, stripped),
                None => (false, trimmed),
            };
            if body.is_empty() {
                continue;
            }
            let glob = GlobBuilder::new(body)
                .literal_separator(true)
                .build()
                .map_err(|source| Error::InvalidPattern {
                    pattern: raw.clone(),
                    source,
                })?;

            let has_slash = body.contains('/');
            match (dir_only, has_slash) {
                (false, true) => {
                    any_path.add(glob);
                }
                (false, false) => {
                    any_path.add(glob.clone());
                    any_name.add(glob);
                }
                (true, true) => {
                    dir_path.add(glob);
                }
                (true, false) => {
                    dir_path.add(glob.clone());
                    dir_name.add(glob);
                }
            }
        }

        let build_set = |builder: GlobSetBuilder| -> Result<GlobSet> {
            builder.build().map_err(|source| Error::InvalidPattern {
                pattern: patterns.join(","),
                source,
            })
        };

        Ok(Self {
            any_path: build_set(any_path)?,
            any_name: build_set(any_name)?,
            dir_path: build_set(dir_path)?,
            dir_name: build_set(dir_name)?,
        })
    }

    fn is_excluded(&self, rel: &str, name: &str, is_dir: bool) -> bool {
        if self.any_path.is_match(rel) || self.any_name.is_match(name) {
            return true;
        }
        if is_dir && (self.dir_path.is_match(rel) || self.dir_name.is_match(name)) {
            return true;
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hash::hash_bytes;

    #[test]
    fn lists_tree_deterministically_with_json_shape() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        fs::create_dir_all(root.join("nested/empty")).unwrap();
        fs::write(root.join("b.txt"), b"hello").unwrap();
        fs::write(root.join("nested/a.txt"), b"").unwrap();
        fs::write(root.join("-dash file.txt"), b"x").unwrap();

        #[cfg(unix)]
        std::os::unix::fs::symlink("b.txt", root.join("link")).unwrap();

        let opts = ManifestOptions {
            checksum: true,
            exclude: Vec::new(),
        };
        let manifest = build(root, &opts).unwrap();
        assert!(manifest.skipped.is_empty());
        assert!(manifest.case_collisions.is_empty());

        let paths: Vec<&str> = manifest.entries.iter().map(|e| e.path.as_str()).collect();
        #[cfg(unix)]
        assert_eq!(
            paths,
            vec![
                "-dash file.txt",
                "b.txt",
                "link",
                "nested",
                "nested/a.txt",
                "nested/empty",
            ]
        );
        #[cfg(not(unix))]
        assert_eq!(
            paths,
            vec![
                "-dash file.txt",
                "b.txt",
                "nested",
                "nested/a.txt",
                "nested/empty",
            ]
        );

        let b_entry = manifest.entries.iter().find(|e| e.path == "b.txt").unwrap();
        assert_eq!(b_entry.kind, EntryKind::File);
        assert_eq!(b_entry.size, 5);
        assert_eq!(b_entry.hash, Some(hash_bytes(b"hello")));
        assert!(b_entry.link_target.is_none());
        #[cfg(unix)]
        assert!(b_entry.mode.is_some());
        #[cfg(not(unix))]
        assert!(b_entry.mode.is_none());

        // Verify JSON shape matches specification.
        let json = serde_json::to_value(b_entry).unwrap();
        assert_eq!(json["path"], "b.txt");
        assert_eq!(json["kind"], "file");
        assert_eq!(json["size"], 5);
        assert!(json["mtime"]["secs"].is_i64());
        assert!(json["mtime"]["nanos"].is_u64());
        assert_eq!(json["hash"], hash_bytes(b"hello").to_string());
        assert!(json.get("link_target").is_none());

        let round_tripped: Manifest =
            serde_json::from_str(&serde_json::to_string(&manifest).unwrap()).unwrap();
        assert_eq!(round_tripped, manifest);
    }

    #[test]
    fn omits_hash_when_not_requested() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("only.txt"), b"abc").unwrap();

        let manifest = build(dir.path(), &ManifestOptions::default()).unwrap();
        assert_eq!(manifest.entries.len(), 1);
        assert!(manifest.entries[0].hash.is_none());
        let json = serde_json::to_value(&manifest.entries[0]).unwrap();
        assert!(json.get("hash").is_none());
    }

    #[test]
    fn lists_single_file() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("single.bin");
        fs::write(&file, b"payload").unwrap();

        let manifest = build(
            &file,
            &ManifestOptions {
                checksum: true,
                exclude: Vec::new(),
            },
        )
        .unwrap();
        assert_eq!(manifest.entries.len(), 1);
        assert_eq!(manifest.entries[0].path, "single.bin");
        assert_eq!(manifest.entries[0].size, 7);
        assert_eq!(manifest.entries[0].hash, Some(hash_bytes(b"payload")));
    }

    #[cfg(unix)]
    #[test]
    fn does_not_follow_directory_symlinks() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("secret.txt"), b"nope").unwrap();

        std::os::unix::fs::symlink(outside.path(), dir.path().join("linked_dir")).unwrap();

        let manifest = build(dir.path(), &ManifestOptions::default()).unwrap();
        assert_eq!(manifest.entries.len(), 1);
        assert_eq!(manifest.entries[0].path, "linked_dir");
        assert_eq!(manifest.entries[0].kind, EntryKind::Symlink);
        assert_eq!(
            manifest.entries[0].link_target.as_deref(),
            outside.path().to_str()
        );
    }

    #[test]
    fn excludes_matching_globs() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("target/debug")).unwrap();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("target/debug/app"), b"bin").unwrap();
        fs::write(root.join("src/main.rs"), b"fn main() {}").unwrap();
        fs::write(root.join("src/temp.log"), b"log").unwrap();
        fs::write(root.join("root.log"), b"log").unwrap();
        fs::write(root.join("keep.txt"), b"ok").unwrap();

        let manifest = build(
            root,
            &ManifestOptions {
                checksum: false,
                exclude: vec!["target/".to_string(), "*.log".to_string()],
            },
        )
        .unwrap();

        let paths: Vec<&str> = manifest.entries.iter().map(|e| e.path.as_str()).collect();
        assert_eq!(paths, vec!["keep.txt", "src", "src/main.rs"]);
    }

    #[test]
    fn pruned_directories_are_named_and_never_entered() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("keep/inner")).unwrap();
        fs::create_dir_all(root.join("private/deeper")).unwrap();
        fs::write(root.join("keep/inner/a.txt"), b"a").unwrap();
        fs::write(root.join("private/deeper/secret.txt"), b"s").unwrap();

        let mut asked = Vec::new();
        let manifest = build_pruned(
            root,
            &ManifestOptions {
                checksum: true,
                exclude: Vec::new(),
            },
            |path| {
                asked.push(path.strip_prefix(root).unwrap().to_path_buf());
                (path.file_name() == Some(std::ffi::OsStr::new("private")))
                    .then(|| "not for you".to_string())
            },
        )
        .unwrap();

        let paths: Vec<&str> = manifest.entries.iter().map(|e| e.path.as_str()).collect();
        assert_eq!(paths, vec!["keep", "keep/inner", "keep/inner/a.txt"]);
        assert_eq!(
            manifest.skipped,
            vec![SkippedEntry {
                path: "private".to_string(),
                reason: "not for you".to_string(),
            }]
        );
        // Asked about each directory once, and never about what is inside a pruned one.
        asked.sort();
        assert_eq!(
            asked,
            vec![
                Path::new("keep").to_path_buf(),
                Path::new("keep/inner").to_path_buf(),
                Path::new("private").to_path_buf(),
            ]
        );
    }

    #[test]
    fn rejects_invalid_exclude_glob() {
        let dir = tempfile::tempdir().unwrap();
        let err = build(
            dir.path(),
            &ManifestOptions {
                checksum: false,
                exclude: vec!["[unclosed".to_string()],
            },
        )
        .unwrap_err();
        assert!(matches!(err, Error::InvalidPattern { .. }));
    }

    #[test]
    fn detects_case_collisions_including_directories() {
        let collisions = detect_case_collisions([
            "README.md",
            "readme.md",
            "Docs/Intro.md",
            "docs/Guide.md",
            "unique.txt",
        ]);
        assert_eq!(
            collisions,
            vec![
                CaseCollision {
                    canonical: "docs".to_string(),
                    paths: vec!["Docs".to_string(), "docs".to_string()],
                },
                CaseCollision {
                    canonical: "readme.md".to_string(),
                    paths: vec!["README.md".to_string(), "readme.md".to_string()],
                },
            ]
        );
    }

    #[cfg(unix)]
    #[test]
    fn reports_unreadable_entries_in_skipped() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let unreadable_dir = root.join("secret_dir");
        fs::create_dir(&unreadable_dir).unwrap();
        fs::write(unreadable_dir.join("inside.txt"), b"hidden").unwrap();
        fs::write(root.join("visible.txt"), b"ok").unwrap();

        fs::set_permissions(&unreadable_dir, fs::Permissions::from_mode(0o000)).unwrap();

        // Only assert skip behavior when the current user is not root (root bypasses chmod 000).
        let can_still_read = fs::read_dir(&unreadable_dir).is_ok();
        let manifest = build(root, &ManifestOptions::default()).unwrap();

        // Restore permissions so tempdir cleanup succeeds.
        fs::set_permissions(&unreadable_dir, fs::Permissions::from_mode(0o755)).unwrap();

        assert!(manifest.entries.iter().any(|e| e.path == "visible.txt"));
        if !can_still_read {
            assert_eq!(manifest.skipped.len(), 1);
            assert_eq!(manifest.skipped[0].path, "secret_dir");
        }
    }

    #[test]
    fn ignores_internal_temp_and_part_files() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("sub")).unwrap();
        fs::write(root.join("real.txt"), b"keep").unwrap();
        fs::write(root.join("real.txt.legion-part"), b"partial").unwrap();
        fs::write(root.join(".legion-unpack-42-0.tmp"), b"tmp").unwrap();
        fs::write(root.join(".legion-link-42-1.tmp"), b"tmp").unwrap();
        fs::write(root.join(".legion-CaseProbe-42-2.tmp"), b"tmp").unwrap();
        fs::write(root.join("sub/nested.bin.legion-part"), b"partial").unwrap();
        fs::write(root.join("sub/keep.bin"), b"ok").unwrap();

        let manifest = build(root, &ManifestOptions::default()).unwrap();
        let paths: Vec<&str> = manifest.entries.iter().map(|e| e.path.as_str()).collect();
        assert_eq!(paths, vec!["real.txt", "sub", "sub/keep.bin"]);
        assert!(manifest.skipped.is_empty());
    }
}
