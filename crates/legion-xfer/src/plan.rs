//! Pure transfer planning between a source and destination [`Manifest`].

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::manifest::{CaseCollision, Entry, EntryKind, Manifest, detect_case_collisions};

/// Options controlling how [`plan`] compares source and destination entries.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanOptions {
    /// Compare regular files by BLAKE3 hash instead of size and whole-second mtime.
    pub checksum: bool,
    /// Plan removal of destination entries absent from the source manifest.
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

/// Actionable transfer plan produced by [`plan`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Plan {
    /// Relative paths to pack and send from the source, ordered parents-first.
    pub send: Vec<String>,
    /// Relative paths to remove from the destination, ordered deepest-first so children precede
    /// their parent directories.
    pub delete: Vec<String>,
    /// Aggregate counts for progress reporting and dry-run summaries.
    pub totals: PlanTotals,
    /// Paths across source and destination that differ only in letter case.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub case_collisions: Vec<CaseCollision>,
}

impl Plan {
    /// Returns `true` when neither uploads nor deletions are needed.
    pub fn is_empty(&self) -> bool {
        self.send.is_empty() && self.delete.is_empty()
    }
}

/// Computes the minimal set of entries to send and delete to synchronize `dest` with `source`.
///
/// Paths are matched case-sensitively so entries differing only in case are never silently merged;
/// any such collisions within or across manifests are reported in [`Plan::case_collisions`].
pub fn plan(source: &Manifest, dest: &Manifest, options: &PlanOptions) -> Plan {
    let dest_by_path: BTreeMap<&str, &Entry> = dest
        .entries
        .iter()
        .map(|entry| (entry.path.as_str(), entry))
        .collect();

    let mut source_paths: BTreeSet<&str> = BTreeSet::new();
    let mut send = Vec::new();
    let mut totals = PlanTotals::default();

    for src in &source.entries {
        source_paths.insert(src.path.as_str());
        let unchanged = dest_by_path
            .get(src.path.as_str())
            .is_some_and(|dst| is_unchanged(src, dst, options.checksum));

        if unchanged {
            if src.kind == EntryKind::File {
                totals.files_unchanged += 1;
            }
            continue;
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

    let mut delete = Vec::new();
    if options.delete {
        for dst in &dest.entries {
            if !source_paths.contains(dst.path.as_str()) {
                delete.push(dst.path.clone());
            }
        }
        // Delete deepest entries first so files inside removed directories are unlinked before
        // their parent directories.
        delete.sort_by(|a, b| {
            let depth_a = a.matches('/').count();
            let depth_b = b.matches('/').count();
            depth_b.cmp(&depth_a).then_with(|| a.cmp(b))
        });
        totals.entries_to_delete = delete.len() as u64;
    }

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
        case_collisions,
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
    fn kind_change_or_symlink_target_change_triggers_send() {
        let src = manifest(vec![
            file("was_dir", 10, 100, 0, None),
            dir("was_file", 100),
            symlink("link_same", "a"),
            symlink("link_changed", "b"),
        ]);
        let dst = manifest(vec![
            dir("was_dir", 100),
            file("was_file", 10, 100, 0, None),
            symlink("link_same", "a"),
            symlink("link_changed", "a"),
        ]);

        let p = plan(&src, &dst, &PlanOptions::default());
        assert_eq!(p.send, vec!["link_changed", "was_dir", "was_file"]);
        assert_eq!(p.totals.files_to_send, 1);
        assert_eq!(p.totals.dirs_to_send, 1);
        assert_eq!(p.totals.symlinks_to_send, 1);
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
