//! Pure transfer planning between a source and destination [`Manifest`].

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::manifest::{CaseCollision, Entry, EntryKind, Manifest, detect_case_collisions};
use crate::path::validate_relative_path;

/// Options controlling how [`plan`] compares source and destination entries.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanOptions {
    /// Compare regular files by BLAKE3 hash instead of size and whole-second mtime.
    pub checksum: bool,
    /// Plan removal of destination entries absent from the source manifest and allow replacing a
    /// destination directory with a source file or symlink.
    pub delete: bool,
}

/// Summary counts for a [`Plan`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanTotals {
    /// Regular files that must be transferred.
    pub files_to_send: u64,
    /// Total byte length of regular files scheduled for transfer.
    pub bytes_to_send: u64,
    /// Regular files already up to date on the destination.
    pub files_unchanged: u64,
    /// Directories that must be created on the destination.
    pub dirs_to_send: u64,
    /// Symlinks that must be created or updated on the destination.
    pub symlinks_to_send: u64,
    /// Destination entries scheduled for removal.
    pub entries_to_delete: u64,
}

/// An entry that cannot be scheduled automatically without explicit confirmation or deletion.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct PlanConflict {
    /// Relative path where the conflict occurred.
    pub path: String,
    /// Human-readable reason the entry was neither sent nor deleted.
    pub reason: String,
}

/// Actionable transfer plan produced by [`plan`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Plan {
    /// Relative paths to pack and send from the source, ordered parents-first.
    pub send: Vec<String>,
    /// Relative paths to remove from the destination before unpacking, ordered deepest-first so
    /// children precede their parent directories.
    pub delete: Vec<String>,
    /// Aggregate counts for progress reporting and dry-run summaries.
    pub totals: PlanTotals,
    /// Entries blocked by type conflicts (such as a destination directory where the source has a
    /// file or symlink while `delete` is false) or unsafe relative paths.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub conflicts: Vec<PlanConflict>,
    /// Paths across source and destination that differ only in letter case.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub case_collisions: Vec<CaseCollision>,
}

impl Plan {
    /// Returns `true` when there is nothing to send, nothing to delete, and no conflicts.
    pub fn is_empty(&self) -> bool {
        self.send.is_empty() && self.delete.is_empty() && self.conflicts.is_empty()
    }
}

/// Computes the minimal set of entries to send and delete to synchronize `dest` with `source`.
///
/// Paths are matched case-sensitively so entries differing only in case are never silently merged;
/// any such collisions within or across manifests are reported in [`Plan::case_collisions`].
///
/// When an entry's kind differs between source and destination:
/// - A destination file or symlink is scheduled for deletion before sending, regardless of
///   [`PlanOptions::delete`].
/// - A destination directory where the source has a file or symlink is deleted (children first)
///   only when [`PlanOptions::delete`] is `true`; otherwise it is neither sent nor deleted and is
///   reported in [`Plan::conflicts`].
pub fn plan(source: &Manifest, dest: &Manifest, options: &PlanOptions) -> Plan {
    let mut conflicts = Vec::new();
    let mut dest_by_path: BTreeMap<&str, &Entry> = BTreeMap::new();
    for entry in &dest.entries {
        if let Err(err) = validate_relative_path(&entry.path) {
            conflicts.push(PlanConflict {
                path: entry.path.clone(),
                reason: err.to_string(),
            });
            continue;
        }
        dest_by_path.insert(entry.path.as_str(), entry);
    }

    let mut source_paths: BTreeSet<&str> = BTreeSet::new();
    let mut send = Vec::new();
    let mut delete_set: BTreeSet<String> = BTreeSet::new();
    let mut totals = PlanTotals::default();

    for src in &source.entries {
        if let Err(err) = validate_relative_path(&src.path) {
            conflicts.push(PlanConflict {
                path: src.path.clone(),
                reason: err.to_string(),
            });
            continue;
        }
        source_paths.insert(src.path.as_str());

        if let Some(dst) = dest_by_path.get(src.path.as_str()) {
            if dst.kind != src.kind {
                if dst.kind == EntryKind::Dir {
                    if !options.delete {
                        conflicts.push(PlanConflict {
                            path: src.path.clone(),
                            reason: format!(
                                "destination is a directory while source is a {}; enable delete to replace it",
                                kind_label(src.kind)
                            ),
                        });
                        continue;
                    }
                    delete_set.insert(dst.path.clone());
                } else {
                    // Replacing a single destination file or symlink with a different kind is
                    // safe without `delete`, and must happen before unpack writes the new entry.
                    delete_set.insert(dst.path.clone());
                }
            } else if is_unchanged(src, dst, options.checksum) {
                if src.kind == EntryKind::File {
                    totals.files_unchanged += 1;
                }
                continue;
            }
        }

        send.push(src.path.clone());
        match src.kind {
            EntryKind::File => {
                totals.files_to_send += 1;
                totals.bytes_to_send += src.size;
            }
            EntryKind::Dir => {
                totals.dirs_to_send += 1;
            }
            EntryKind::Symlink => {
                totals.symlinks_to_send += 1;
            }
        }
    }

    send.sort();

    if options.delete {
        for dst in dest_by_path.values() {
            if !source_paths.contains(dst.path.as_str()) {
                delete_set.insert(dst.path.clone());
            }
        }
    }

    // Delete deepest entries first so files inside removed directories are unlinked before their
    // parent directories.
    let mut delete: Vec<String> = delete_set.into_iter().collect();
    delete.sort_by(|a, b| {
        let depth_a = a.matches('/').count();
        let depth_b = b.matches('/').count();
        depth_b.cmp(&depth_a).then_with(|| a.cmp(b))
    });
    totals.entries_to_delete = delete.len() as u64;

    conflicts.sort();
    conflicts.dedup();

    let case_collisions = detect_case_collisions(
        source
            .entries
            .iter()
            .chain(dest.entries.iter())
            .map(|e| e.path.as_str()),
    );

    Plan {
        send,
        delete,
        totals,
        conflicts,
        case_collisions,
    }
}

fn kind_label(kind: EntryKind) -> &'static str {
    match kind {
        EntryKind::File => "file",
        EntryKind::Dir => "directory",
        EntryKind::Symlink => "symlink",
    }
}

fn is_unchanged(src: &Entry, dst: &Entry, checksum: bool) -> bool {
    if src.kind != dst.kind {
        return false;
    }
    match src.kind {
        EntryKind::Dir => true,
        EntryKind::Symlink => src.link_target == dst.link_target,
        EntryKind::File => {
            if checksum {
                matches!((src.hash, dst.hash), (Some(a), Some(b)) if a == b)
            } else {
                src.size == dst.size && src.mtime.secs == dst.mtime.secs
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hash::hash_bytes;
    use crate::manifest::Timestamp;

    fn file(path: &str, size: u64, secs: i64, nanos: u32, content: Option<&[u8]>) -> Entry {
        Entry {
            path: path.to_string(),
            kind: EntryKind::File,
            size,
            mtime: Timestamp::new(secs, nanos),
            mode: Some(0o644),
            link_target: None,
            hash: content.map(hash_bytes),
        }
    }

    fn dir(path: &str, secs: i64) -> Entry {
        Entry {
            path: path.to_string(),
            kind: EntryKind::Dir,
            size: 0,
            mtime: Timestamp::new(secs, 0),
            mode: Some(0o755),
            link_target: None,
            hash: None,
        }
    }

    fn symlink(path: &str, target: &str) -> Entry {
        Entry {
            path: path.to_string(),
            kind: EntryKind::Symlink,
            size: 0,
            mtime: Timestamp::new(100, 0),
            mode: Some(0o777),
            link_target: Some(target.to_string()),
            hash: None,
        }
    }

    fn manifest(entries: Vec<Entry>) -> Manifest {
        Manifest {
            entries,
            skipped: Vec::new(),
            case_collisions: Vec::new(),
        }
    }

    #[test]
    fn empty_dest_sends_everything_with_totals() {
        let src = manifest(vec![
            dir("sub", 10),
            file("a.txt", 100, 10, 0, None),
            file("sub/b.txt", 250, 10, 0, None),
            symlink("link", "a.txt"),
        ]);
        let dst = manifest(vec![]);

        let p = plan(&src, &dst, &PlanOptions::default());
        assert_eq!(p.send, vec!["a.txt", "link", "sub", "sub/b.txt"]);
        assert!(p.delete.is_empty());
        assert!(p.conflicts.is_empty());
        assert_eq!(p.totals.files_to_send, 2);
        assert_eq!(p.totals.bytes_to_send, 350);
        assert_eq!(p.totals.files_unchanged, 0);
        assert_eq!(p.totals.dirs_to_send, 1);
        assert_eq!(p.totals.symlinks_to_send, 1);
        assert_eq!(p.totals.entries_to_delete, 0);
        assert!(!p.is_empty());
    }

    #[test]
    fn mtime_comparison_ignores_subsecond_nanoseconds() {
        let src = manifest(vec![
            dir("sub", 100),
            file("same.txt", 42, 1_700_000_000, 999_999_999, None),
            file("diff_sec.txt", 42, 1_700_000_001, 0, None),
            file("diff_size.txt", 43, 1_700_000_000, 0, None),
        ]);
        let dst = manifest(vec![
            dir("sub", 200),
            file("same.txt", 42, 1_700_000_000, 0, None),
            file("diff_sec.txt", 42, 1_700_000_000, 0, None),
            file("diff_size.txt", 42, 1_700_000_000, 0, None),
        ]);

        let p = plan(&src, &dst, &PlanOptions::default());
        assert_eq!(p.send, vec!["diff_sec.txt", "diff_size.txt"]);
        assert_eq!(p.totals.files_to_send, 2);
        assert_eq!(p.totals.bytes_to_send, 85);
        assert_eq!(p.totals.files_unchanged, 1);
        assert_eq!(p.totals.dirs_to_send, 0);
    }

    #[test]
    fn checksum_mode_uses_hashes_over_mtime_and_size() {
        let src = manifest(vec![
            // Different mtime, same content -> unchanged under checksum mode.
            file("same_hash.txt", 5, 200, 0, Some(b"hello")),
            // Same mtime and size, different content -> must send under checksum mode.
            file("diff_hash.txt", 5, 100, 0, Some(b"world")),
            // Missing hash on one side -> conservatively send.
            file("no_hash.txt", 5, 100, 0, None),
        ]);
        let dst = manifest(vec![
            file("same_hash.txt", 5, 100, 0, Some(b"hello")),
            file("diff_hash.txt", 5, 100, 0, Some(b"hello")),
            file("no_hash.txt", 5, 100, 0, Some(b"hello")),
        ]);

        let p = plan(
            &src,
            &dst,
            &PlanOptions {
                checksum: true,
                delete: false,
            },
        );
        assert_eq!(p.send, vec!["diff_hash.txt", "no_hash.txt"]);
        assert_eq!(p.totals.files_unchanged, 1);
        assert_eq!(p.totals.files_to_send, 2);
        assert_eq!(p.totals.bytes_to_send, 10);
    }

    #[test]
    fn kind_changes_cover_all_combinations_with_and_without_delete() {
        // 1. Dest file or symlink replaced by a different kind -> always deleted before send,
        //    whether `delete` is false or true.
        for delete_flag in [false, true] {
            let opts = PlanOptions {
                checksum: false,
                delete: delete_flag,
            };

            // file -> symlink
            let p = plan(
                &manifest(vec![symlink("x", "target")]),
                &manifest(vec![file("x", 10, 100, 0, None)]),
                &opts,
            );
            assert_eq!(p.delete, vec!["x"]);
            assert_eq!(p.send, vec!["x"]);
            assert!(p.conflicts.is_empty());
            assert_eq!(p.totals.entries_to_delete, 1);
            assert_eq!(p.totals.symlinks_to_send, 1);

            // symlink -> file
            let p = plan(
                &manifest(vec![file("x", 10, 100, 0, None)]),
                &manifest(vec![symlink("x", "target")]),
                &opts,
            );
            assert_eq!(p.delete, vec!["x"]);
            assert_eq!(p.send, vec!["x"]);
            assert!(p.conflicts.is_empty());
            assert_eq!(p.totals.entries_to_delete, 1);
            assert_eq!(p.totals.files_to_send, 1);

            // file -> dir (with child inside dir)
            let p = plan(
                &manifest(vec![dir("x", 100), file("x/a.txt", 3, 100, 0, None)]),
                &manifest(vec![file("x", 10, 100, 0, None)]),
                &opts,
            );
            assert_eq!(p.delete, vec!["x"]);
            assert_eq!(p.send, vec!["x", "x/a.txt"]);
            assert!(p.conflicts.is_empty());
            assert_eq!(p.totals.entries_to_delete, 1);
            assert_eq!(p.totals.dirs_to_send, 1);
            assert_eq!(p.totals.files_to_send, 1);

            // symlink -> dir
            let p = plan(
                &manifest(vec![dir("x", 100)]),
                &manifest(vec![symlink("x", "target")]),
                &opts,
            );
            assert_eq!(p.delete, vec!["x"]);
            assert_eq!(p.send, vec!["x"]);
            assert!(p.conflicts.is_empty());
            assert_eq!(p.totals.entries_to_delete, 1);
            assert_eq!(p.totals.dirs_to_send, 1);
        }

        // 2. Dest directory replaced by source file or symlink -> conflict when `delete` is false,
        //    deleted children-first then sent when `delete` is true.
        let dst_tree = manifest(vec![
            dir("x", 100),
            dir("x/sub", 100),
            file("x/sub/leaf.txt", 4, 100, 0, None),
            file("x/top.txt", 2, 100, 0, None),
        ]);

        for (src_entry, expect_files, expect_symlinks) in [
            (file("x", 10, 100, 0, None), 1u64, 0u64),
            (symlink("x", "other"), 0u64, 1u64),
        ] {
            let src = manifest(vec![src_entry]);

            let without_delete = plan(
                &src,
                &dst_tree,
                &PlanOptions {
                    checksum: false,
                    delete: false,
                },
            );
            assert!(without_delete.send.is_empty());
            assert!(without_delete.delete.is_empty());
            assert_eq!(without_delete.conflicts.len(), 1);
            assert_eq!(without_delete.conflicts[0].path, "x");
            assert_eq!(without_delete.totals.files_to_send, 0);
            assert_eq!(without_delete.totals.symlinks_to_send, 0);
            assert!(!without_delete.is_empty());

            let with_delete = plan(
                &src,
                &dst_tree,
                &PlanOptions {
                    checksum: false,
                    delete: true,
                },
            );
            assert!(with_delete.conflicts.is_empty());
            assert_eq!(with_delete.send, vec!["x"]);
            assert_eq!(
                with_delete.delete,
                vec!["x/sub/leaf.txt", "x/sub", "x/top.txt", "x"]
            );
            assert_eq!(with_delete.totals.entries_to_delete, 4);
            assert_eq!(with_delete.totals.files_to_send, expect_files);
            assert_eq!(with_delete.totals.symlinks_to_send, expect_symlinks);
        }
    }

    #[test]
    fn symlink_target_change_triggers_send_without_extra_delete() {
        let src = manifest(vec![
            symlink("link_same", "a"),
            symlink("link_changed", "b"),
        ]);
        let dst = manifest(vec![
            symlink("link_same", "a"),
            symlink("link_changed", "a"),
        ]);

        let p = plan(&src, &dst, &PlanOptions::default());
        assert_eq!(p.send, vec!["link_changed"]);
        assert!(p.delete.is_empty());
        assert!(p.conflicts.is_empty());
        assert_eq!(p.totals.symlinks_to_send, 1);
    }

    #[test]
    fn unsafe_paths_in_manifests_become_conflicts_not_deletes_or_sends() {
        let src = manifest(vec![file("../escape.txt", 4, 10, 0, None)]);
        let dst = manifest(vec![file("sub/C:evil.txt", 4, 10, 0, None)]);

        let p = plan(
            &src,
            &dst,
            &PlanOptions {
                checksum: false,
                delete: true,
            },
        );
        assert!(p.send.is_empty());
        assert!(p.delete.is_empty());
        assert_eq!(p.conflicts.len(), 2);
        assert!(!p.is_empty());
    }

    #[test]
    fn delete_planning_orders_children_before_parents() {
        let src = manifest(vec![file("keep.txt", 4, 10, 0, None)]);
        let dst = manifest(vec![
            file("keep.txt", 4, 10, 0, None),
            dir("old", 10),
            dir("old/nested", 10),
            file("old/nested/deep.txt", 1, 10, 0, None),
            file("old/top.txt", 1, 10, 0, None),
            file("stale.txt", 1, 10, 0, None),
        ]);

        let without_delete = plan(&src, &dst, &PlanOptions::default());
        assert!(without_delete.is_empty());
        assert!(without_delete.delete.is_empty());

        let with_delete = plan(
            &src,
            &dst,
            &PlanOptions {
                checksum: false,
                delete: true,
            },
        );
        assert!(with_delete.send.is_empty());
        assert_eq!(
            with_delete.delete,
            vec![
                "old/nested/deep.txt",
                "old/nested",
                "old/top.txt",
                "old",
                "stale.txt",
            ]
        );
        assert_eq!(with_delete.totals.entries_to_delete, 5);
        assert!(!with_delete.is_empty());
    }

    #[test]
    fn reports_case_collisions_without_merging_entries() {
        let src = manifest(vec![
            file("Readme.md", 10, 100, 0, None),
            file("README.md", 20, 100, 0, None),
            file("App.txt", 5, 100, 0, None),
        ]);
        let dst = manifest(vec![file("app.txt", 5, 100, 0, None)]);

        let p = plan(
            &src,
            &dst,
            &PlanOptions {
                checksum: false,
                delete: true,
            },
        );
        // Distinct casing is never merged: App.txt is sent and app.txt is marked for delete.
        assert_eq!(p.send, vec!["App.txt", "README.md", "Readme.md"]);
        assert_eq!(p.delete, vec!["app.txt"]);
        assert_eq!(
            p.case_collisions,
            vec![
                CaseCollision {
                    canonical: "app.txt".to_string(),
                    paths: vec!["App.txt".to_string(), "app.txt".to_string()],
                },
                CaseCollision {
                    canonical: "readme.md".to_string(),
                    paths: vec!["README.md".to_string(), "Readme.md".to_string()],
                },
            ]
        );
    }

    #[test]
    fn identical_trees_produce_empty_plan() {
        let entries = vec![
            dir("empty", 50),
            file("a.txt", 12, 50, 123, Some(b"hello world!")),
            symlink("s", "a.txt"),
        ];
        let src = manifest(entries.clone());
        let dst = manifest(entries);

        let p1 = plan(
            &src,
            &dst,
            &PlanOptions {
                checksum: false,
                delete: true,
            },
        );
        assert!(p1.is_empty());
        assert_eq!(p1.totals.files_unchanged, 1);

        let p2 = plan(
            &src,
            &dst,
            &PlanOptions {
                checksum: true,
                delete: true,
            },
        );
        assert!(p2.is_empty());
        assert_eq!(p2.totals.files_unchanged, 1);
    }
}
