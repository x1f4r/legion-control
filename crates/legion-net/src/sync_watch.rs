//! Native OS file watcher for sync folders using [`notify`].
//!
//! # Backend selection
//!
//! Uses [`notify::RecommendedWatcher`] with [`notify::RecursiveMode::Recursive`] and default
//! platform configuration (`notify::Config::default()`):
//! - **Linux**: `inotify` (`INotifyWatcher`) — kernel watches per directory with zero idle polling.
//! - **macOS**: `FSEvents` (`FsEventWatcher`, via `notify`'s default `macos_fsevent` feature) —
//!   one kernel event stream per watched tree without consuming a file descriptor per subdirectory.
//! - **Windows**: `ReadDirectoryChangesW` (`ReadDirectoryChangesWatcher`) — single recursive
//!   directory handle per watched tree.
//!
//! # Overflow and burst handling
//!
//! - Events under `.legion-sync` at any depth and fixed-ignored OS litter / in-flight files are
//!   dropped immediately so internal staging/bin/marker updates never trigger rescan loops.
//! - Kernel queue overflows (`event.need_rescan()`) or watcher errors immediately emit
//!   [`WatchSignal::FullRescan`].
//! - Rapid bursts affecting more than [`MAX_DIRTY_PATHS`] distinct paths collapse into a single
//!   [`WatchSignal::FullRescan`] so bulk writes (e.g. copying 10,000 files) run one streaming scan
//!   and batched commit instead of thousands of individual subtree scans.
//! - The watcher is purely an optimization: disabling it (`--no-watch`) falls back cleanly to
//!   periodic full scans, which converge identically.

use std::collections::BTreeSet;
use std::path::{Component, Path, PathBuf};
use std::sync::mpsc::Sender;

use legion_sync::{is_fixed_ignored, is_internal_sync_path};
use notify::{Config, Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};

/// Maximum distinct dirty paths tracked in one debounce batch before promoting to a full rescan.
pub const MAX_DIRTY_PATHS: usize = 256;

/// Signal emitted by [`FolderWatcher`] into a folder worker thread.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WatchSignal {
    /// One or more normalized relative paths changed under the folder root.
    DirtyPaths(Vec<String>),
    /// Queue overflow, watcher error, or bulk change requires a full folder scan.
    FullRescan,
}

/// Active native OS watcher guarding one sync folder root.
pub struct FolderWatcher {
    _watcher: RecommendedWatcher,
}

impl FolderWatcher {
    /// Starts watching `root` recursively, forwarding filtered [`WatchSignal`]s to `tx`.
    pub fn start(root: PathBuf, tx: Sender<WatchSignal>) -> Result<Self, String> {
        let canonical_root = std::fs::canonicalize(&root).unwrap_or_else(|_| root.clone());
        let raw_root = root.clone();

        let mut watcher = RecommendedWatcher::new(
            move |res: notify::Result<Event>| match res {
                Ok(event) => {
                    if let Some(sig) = classify_event(&raw_root, &canonical_root, event) {
                        let _ = tx.send(sig);
                    }
                }
                Err(_) => {
                    let _ = tx.send(WatchSignal::FullRescan);
                }
            },
            Config::default(),
        )
        .map_err(|err| format!("could not start file watcher for {}: {err}", root.display()))?;

        watcher
            .watch(&root, RecursiveMode::Recursive)
            .map_err(|err| format!("could not watch {}: {err}", root.display()))?;

        Ok(Self { _watcher: watcher })
    }
}

fn classify_event(raw_root: &Path, canonical_root: &Path, event: Event) -> Option<WatchSignal> {
    if event.need_rescan() {
        return Some(WatchSignal::FullRescan);
    }
    // Ignore pure access-only events (file open/read/close-nowrite).
    if matches!(event.kind, EventKind::Access(_)) {
        return None;
    }

    let mut dirty = Vec::new();
    for path in event.paths {
        let rel = strip_root(&path, raw_root).or_else(|| strip_root(&path, canonical_root));
        let Some(rel_path) = rel else {
            // An event outside or directly on the root itself -> rescan root.
            if path == raw_root || path == canonical_root {
                return Some(WatchSignal::FullRescan);
            }
            continue;
        };
        let Some(norm) = normalize_relative(&rel_path) else {
            return Some(WatchSignal::FullRescan);
        };
        if norm.is_empty() {
            return Some(WatchSignal::FullRescan);
        }
        let file_name = norm.rsplit('/').next().unwrap_or(&norm);
        if is_internal_sync_path(&norm) || is_fixed_ignored(&norm, file_name) {
            continue;
        }
        dirty.push(norm);
    }

    if dirty.is_empty() {
        None
    } else if dirty.len() > MAX_DIRTY_PATHS {
        Some(WatchSignal::FullRescan)
    } else {
        Some(WatchSignal::DirtyPaths(dirty))
    }
}

fn strip_root(path: &Path, root: &Path) -> Option<PathBuf> {
    path.strip_prefix(root).ok().map(Path::to_path_buf)
}

fn normalize_relative(rel: &Path) -> Option<String> {
    let mut parts = Vec::new();
    for comp in rel.components() {
        match comp {
            Component::Normal(seg) => {
                let s = seg.to_str()?;
                if !s.is_empty() {
                    parts.push(s);
                }
            }
            Component::CurDir => {}
            _ => return None,
        }
    }
    Some(parts.join("/"))
}

/// Result of coalescing a burst of watcher events for one folder into a single engine scan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CoalescedScan {
    /// Scan the entire folder in one pass (queue overflow, root event, or paths spanning multiple
    /// top-level entries).
    Full,
    /// Scan a single path or common directory subtree in one pass.
    Subtree(String),
}

/// Coalesces dirty paths and overflow signals during a debounce window so any burst of events for
/// one folder becomes a single engine scan.
#[derive(Debug, Default)]
pub struct DirtyAccumulator {
    full_rescan: bool,
    paths: BTreeSet<String>,
}

impl DirtyAccumulator {
    /// Records one [`WatchSignal`] into the pending batch.
    pub fn push(&mut self, sig: WatchSignal) {
        if self.full_rescan {
            return;
        }
        match sig {
            WatchSignal::FullRescan => {
                self.full_rescan = true;
                self.paths.clear();
            }
            WatchSignal::DirtyPaths(list) => {
                for p in list {
                    self.paths.insert(p);
                    if self.paths.len() > MAX_DIRTY_PATHS {
                        self.full_rescan = true;
                        self.paths.clear();
                        return;
                    }
                }
            }
        }
    }

    /// Marks specific relative paths dirty (e.g. unstable files waiting for a second look).
    pub fn push_paths(&mut self, iter: impl IntoIterator<Item = String>) {
        if self.full_rescan {
            return;
        }
        for p in iter {
            self.paths.insert(p);
            if self.paths.len() > MAX_DIRTY_PATHS {
                self.full_rescan = true;
                self.paths.clear();
                return;
            }
        }
    }

    /// Marks a full rescan needed.
    pub fn mark_full(&mut self) {
        self.full_rescan = true;
        self.paths.clear();
    }

    /// Returns `true` when there is pending work to scan.
    pub fn is_dirty(&self) -> bool {
        self.full_rescan || !self.paths.is_empty()
    }

    /// Drains the accumulated batch into **at most one** scan target:
    /// - `None` when nothing is dirty;
    /// - `Some(CoalescedScan::Subtree(root))` when all dirty paths are one path or share a
    ///   non-empty common directory ancestor;
    /// - `Some(CoalescedScan::Full)` when overflow occurred or paths span distinct top-level roots.
    pub fn take_coalesced(&mut self) -> Option<CoalescedScan> {
        if !self.is_dirty() {
            return None;
        }
        if self.full_rescan {
            self.full_rescan = false;
            self.paths.clear();
            return Some(CoalescedScan::Full);
        }
        let all = std::mem::take(&mut self.paths);
        let first = all.first()?;
        if all.len() == 1 {
            return Some(CoalescedScan::Subtree(first.clone()));
        }
        let last = all.last()?;
        let prefix = common_component_prefix(first, last);
        if prefix.is_empty() {
            Some(CoalescedScan::Full)
        } else {
            Some(CoalescedScan::Subtree(prefix))
        }
    }

    /// Drains the accumulated batch as minimal non-overlapping subtree roots (`None` means full
    /// rescan).
    pub fn take_batch(&mut self) -> Option<Vec<String>> {
        if self.full_rescan {
            self.full_rescan = false;
            self.paths.clear();
            return None;
        }
        let all = std::mem::take(&mut self.paths);
        // Remove descendants already covered by a shallower dirty ancestor in sorted order.
        let mut minimal: Vec<String> = Vec::with_capacity(all.len());
        for p in all {
            let covered = minimal.last().is_some_and(|prev| {
                p == *prev
                    || (p.len() > prev.len()
                        && p.starts_with(prev.as_str())
                        && p.as_bytes()[prev.len()] == b'/')
            });
            if !covered {
                minimal.push(p);
            }
        }
        Some(minimal)
    }
}

fn common_component_prefix(a: &str, b: &str) -> String {
    let mut shared = Vec::new();
    for (ca, cb) in a.split('/').zip(b.split('/')) {
        if ca == cb {
            shared.push(ca);
        } else {
            break;
        }
    }
    shared.join("/")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accumulator_coalesces_bursts_into_one_scan() {
        let mut acc = DirtyAccumulator::default();
        assert_eq!(acc.take_coalesced(), None);

        // Single file -> single subtree scan of that file.
        acc.push(WatchSignal::DirtyPaths(vec!["notes/a.txt".to_owned()]));
        assert_eq!(
            acc.take_coalesced(),
            Some(CoalescedScan::Subtree("notes/a.txt".to_owned()))
        );
        assert!(!acc.is_dirty());

        // Burst of 50 events under one directory -> ONE subtree scan of common ancestor "a".
        for i in 0..50 {
            acc.push(WatchSignal::DirtyPaths(vec![
                format!("a/b/file-{i}.txt"),
                format!("a/c/deep/other-{i}.txt"),
            ]));
        }
        assert_eq!(
            acc.take_coalesced(),
            Some(CoalescedScan::Subtree("a".to_owned()))
        );
        assert!(!acc.is_dirty());

        // Sibling prefixes that share string prefix but NOT path component ("sub" vs "sub2") -> Full.
        acc.push(WatchSignal::DirtyPaths(vec![
            "sub/a.txt".to_owned(),
            "sub2/b.txt".to_owned(),
        ]));
        assert_eq!(acc.take_coalesced(), Some(CoalescedScan::Full));

        // Burst > MAX_DIRTY_PATHS promotes to one Full scan.
        let burst: Vec<String> = (0..=MAX_DIRTY_PATHS)
            .map(|i| format!("dir/file-{i}.txt"))
            .collect();
        acc.push(WatchSignal::DirtyPaths(burst));
        assert!(acc.is_dirty());
        assert_eq!(acc.take_coalesced(), Some(CoalescedScan::Full));
        assert!(!acc.is_dirty());
    }

    #[test]
    fn accumulator_deduplicates_descendants_and_promotes_bursts() {
        let mut acc = DirtyAccumulator::default();
        acc.push(WatchSignal::DirtyPaths(vec![
            "a/b".to_owned(),
            "a/b/c.txt".to_owned(),
            "a/b/d/e.txt".to_owned(),
            "a/bc.txt".to_owned(),
        ]));
        assert_eq!(
            acc.take_batch(),
            Some(vec!["a/b".to_owned(), "a/bc.txt".to_owned()])
        );
        assert!(!acc.is_dirty());

        // Burst > MAX_DIRTY_PATHS promotes to full rescan.
        let burst: Vec<String> = (0..=MAX_DIRTY_PATHS)
            .map(|i| format!("file-{i}.txt"))
            .collect();
        acc.push(WatchSignal::DirtyPaths(burst));
        assert!(acc.is_dirty());
        assert_eq!(acc.take_batch(), None);
        assert!(!acc.is_dirty());
    }

    #[test]
    fn classify_event_ignores_internal_sync_paths_and_handles_rescan_flag() {
        let root = PathBuf::from("/tmp/sync-root");
        let internal = Event {
            kind: EventKind::Modify(notify::event::ModifyKind::Any),
            paths: vec![
                root.join(".legion-sync/tmp/abc"),
                root.join("sub/.legion-sync/marker.json"),
                root.join(".DS_Store"),
            ],
            attrs: Default::default(),
        };
        assert_eq!(classify_event(&root, &root, internal), None);

        let mut overflow = Event::new(EventKind::Other);
        overflow = overflow.set_flag(notify::event::Flag::Rescan);
        assert_eq!(
            classify_event(&root, &root, overflow),
            Some(WatchSignal::FullRescan)
        );
    }
}
