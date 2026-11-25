//! Reloadable store over `tools.d/*.toml`, implementing [`legion_core::ToolSource`].
//!
//! Re-reading file contents is gated on directory and file metadata (entry names, directory
//! modification time, file lengths, modification times, and on Unix inode and status-change
//! timestamps), so repeated lookups and listings do not read `.toml` files while nothing on disk
//! changed. Additions, removals, atomic replacements (`ext.define`), and edits whose size or
//! timestamp changes are picked up on the very next call without restarting long-lived doors.
//!
//! The one change metadata cannot catch is an in-place overwrite (not an atomic rename) of the
//! exact same byte length within the filesystem's timestamp granularity. Broken files are skipped
//! and recorded for `ext.list` and `meta.doctor`, never fatal.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::io;
#[cfg(unix)]
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
#[cfg(test)]
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::SystemTime;

use legion_core::proto::ToolSpec;
use legion_core::{Paths, Registry, SourceCheck, Tool, ToolSource};

use super::def::{ExtensionDef, parse_toml_with_reserved};
use super::run::ExtensionTool;

/// One valid extension tool loaded from disk.
pub struct LoadedTool {
    /// The parsed definition as stored.
    pub def: ExtensionDef,
    /// The file that defined it.
    pub file: PathBuf,
    /// The executable [`Tool`] wrapper.
    pub tool: Arc<ExtensionTool>,
}

/// One file in `tools.d` that could not be loaded.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BrokenFile {
    /// Full path to the broken file (or `tools.d` itself when unreadable).
    pub file: PathBuf,
    /// Why it was skipped, including line number when known.
    pub reason: String,
    /// 1-based line number when known.
    pub line: Option<usize>,
}

/// Complete view of `tools.d` at one point in time.
pub struct Snapshot {
    /// Valid extension tools keyed by tool name.
    pub tools: BTreeMap<String, LoadedTool>,
    /// Files skipped because they failed to read, parse or validate.
    pub broken: Vec<BrokenFile>,
    /// Whether `tools.d` existed on disk.
    pub dir_exists: bool,
    /// Directory-level error when `tools.d` exists but cannot be read as a directory.
    pub dir_error: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct FileMeta {
    len: u64,
    modified: Option<SystemTime>,
    #[cfg(unix)]
    ino: u64,
    #[cfg(unix)]
    ctime: i64,
    #[cfg(unix)]
    ctime_nsec: i64,
}

impl FileMeta {
    fn from_metadata(meta: &std::fs::Metadata) -> Self {
        Self {
            len: meta.len(),
            modified: meta.modified().ok(),
            #[cfg(unix)]
            ino: meta.ino(),
            #[cfg(unix)]
            ctime: meta.ctime(),
            #[cfg(unix)]
            ctime_nsec: meta.ctime_nsec(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum DirMeta {
    Missing,
    Error(String),
    Present {
        dir_modified: Option<SystemTime>,
        files: BTreeMap<OsString, Result<FileMeta, String>>,
    },
}

struct Cached {
    dir: PathBuf,
    meta: DirMeta,
    snapshot: Arc<Snapshot>,
}

/// Reloadable [`ToolSource`] backed by `tools.d/*.toml`.
#[derive(Default)]
pub struct ExtensionStore {
    cached: Mutex<Option<Cached>>,
    #[cfg(test)]
    file_reads: AtomicUsize,
}

impl ExtensionStore {
    /// Creates an empty store; `tools.d` is read lazily on first lookup.
    pub fn new() -> Self {
        Self::default()
    }

    #[cfg(test)]
    fn file_reads(&self) -> usize {
        self.file_reads.load(Ordering::Relaxed)
    }

    fn snapshot(&self, paths: &Paths, builtins: &Registry) -> Arc<Snapshot> {
        let dir = paths.tools_dir();
        let meta = scan_dir_meta(&dir);
        let mut guard = self.cached.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(cached) = guard.as_ref()
            && cached.dir == dir
            && cached.meta == meta
        {
            return Arc::clone(&cached.snapshot);
        }
        let is_builtin = |name: &str| builtins.get(name).is_some();
        let is_reserved_family = |family: &str| builtins.has_family(family);
        let (snapshot, _reads) = build_snapshot(&dir, &meta, &is_builtin, &is_reserved_family);
        #[cfg(test)]
        self.file_reads.fetch_add(_reads, Ordering::Relaxed);
        let snapshot = Arc::new(snapshot);
        *guard = Some(Cached {
            dir,
            meta,
            snapshot: Arc::clone(&snapshot),
        });
        snapshot
    }
}

impl ToolSource for ExtensionStore {
    fn get(&self, paths: &Paths, builtins: &Registry, name: &str) -> Option<Arc<dyn Tool>> {
        let snapshot = self.snapshot(paths, builtins);
        snapshot
            .tools
            .get(name)
            .map(|loaded| Arc::clone(&loaded.tool) as Arc<dyn Tool>)
    }

    fn specs(&self, paths: &Paths, builtins: &Registry) -> Vec<ToolSpec> {
        let snapshot = self.snapshot(paths, builtins);
        snapshot
            .tools
            .values()
            .map(|loaded| loaded.tool.spec().clone())
            .collect()
    }

    fn checks(&self, paths: &Paths, builtins: &Registry) -> Vec<SourceCheck> {
        let dir = paths.tools_dir();
        let snapshot = self.snapshot(paths, builtins);
        if let Some(error) = &snapshot.dir_error {
            return vec![SourceCheck {
                name: "tools.d".to_owned(),
                ok: false,
                detail: format!("{} cannot be read: {error}.", dir.display()),
                hint: Some(
                    "Ensure tools.d is a directory readable by the account legion runs as."
                        .to_owned(),
                ),
            }];
        }
        if !snapshot.dir_exists {
            return vec![SourceCheck {
                name: "tools.d".to_owned(),
                ok: true,
                detail: format!(
                    "{} does not exist, so no extension tools are loaded.",
                    dir.display()
                ),
                hint: None,
            }];
        }
        if snapshot.broken.is_empty() {
            let count = snapshot.tools.len();
            let detail = match count {
                0 => format!("{} exists and has no extension tools.", dir.display()),
                1 => format!("{}: 1 extension tool parses.", dir.display()),
                n => format!("{}: {n} extension tools parse.", dir.display()),
            };
            return vec![SourceCheck {
                name: "tools.d".to_owned(),
                ok: true,
                detail,
                hint: None,
            }];
        }
        snapshot
            .broken
            .iter()
            .map(|broken| {
                let name = broken
                    .file
                    .file_name()
                    .map(|file| format!("tools.d/{}", file.to_string_lossy()))
                    .unwrap_or_else(|| "tools.d".to_owned());
                SourceCheck {
                    name,
                    ok: false,
                    detail: format!("{}: {}.", broken.file.display(), broken.reason),
                    hint: Some(
                        "Fix or remove the file; broken extension definitions are skipped."
                            .to_owned(),
                    ),
                }
            })
            .collect()
    }
}

/// Reads a fresh [`Snapshot`] directly from `tools_dir` without caching (used by `ext.*` tools).
pub fn load_snapshot(
    tools_dir: &Path,
    is_builtin: &dyn Fn(&str) -> bool,
    is_reserved_family: &dyn Fn(&str) -> bool,
) -> Snapshot {
    let meta = scan_dir_meta(tools_dir);
    let (snapshot, _) = build_snapshot(tools_dir, &meta, is_builtin, is_reserved_family);
    snapshot
}

fn scan_dir_meta(dir: &Path) -> DirMeta {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return DirMeta::Missing,
        Err(error) => return DirMeta::Error(error.to_string()),
    };
    let dir_modified = std::fs::metadata(dir).ok().and_then(|m| m.modified().ok());
    let mut files = BTreeMap::new();
    for entry in entries {
        let Ok(entry) = entry else {
            continue;
        };
        let file_name = entry.file_name();
        let lossy = file_name.to_string_lossy();
        if lossy.starts_with('.') || !lossy.ends_with(".toml") {
            continue;
        }
        let meta = std::fs::metadata(entry.path())
            .map(|m| FileMeta::from_metadata(&m))
            .map_err(|error| error.to_string());
        files.insert(file_name, meta);
    }
    DirMeta::Present {
        dir_modified,
        files,
    }
}

fn build_snapshot(
    dir: &Path,
    meta: &DirMeta,
    is_builtin: &dyn Fn(&str) -> bool,
    is_reserved_family: &dyn Fn(&str) -> bool,
) -> (Snapshot, usize) {
    match meta {
        DirMeta::Missing => (
            Snapshot {
                tools: BTreeMap::new(),
                broken: Vec::new(),
                dir_exists: false,
                dir_error: None,
            },
            0,
        ),
        DirMeta::Error(error) => (
            Snapshot {
                tools: BTreeMap::new(),
                broken: vec![BrokenFile {
                    file: dir.to_path_buf(),
                    reason: format!("cannot be read: {error}"),
                    line: None,
                }],
                dir_exists: true,
                dir_error: Some(error.clone()),
            },
            0,
        ),
        DirMeta::Present { files, .. } => {
            let mut tools: BTreeMap<String, LoadedTool> = BTreeMap::new();
            let mut broken = Vec::new();
            let mut reads = 0usize;
            for (file_name, file_meta) in files {
                let path = dir.join(file_name);
                if let Err(error) = file_meta {
                    broken.push(BrokenFile {
                        file: path,
                        reason: format!("cannot be read: {error}"),
                        line: None,
                    });
                    continue;
                }
                reads += 1;
                let bytes = match legion_core::config::read_regular_file(&path) {
                    Ok(Some(bytes)) => bytes,
                    Ok(None) => continue,
                    Err(error) => {
                        broken.push(BrokenFile {
                            file: path,
                            reason: error.message,
                            line: None,
                        });
                        continue;
                    }
                };
                match parse_toml_with_reserved(&bytes, is_builtin, is_reserved_family) {
                    Ok(def) => {
                        if let Some(existing) = tools.get(&def.name) {
                            broken.push(BrokenFile {
                                file: path,
                                reason: format!(
                                    "duplicate tool name {:?}, already defined in {}",
                                    def.name,
                                    existing.file.display()
                                ),
                                line: Some(1),
                            });
                            continue;
                        }
                        let name = def.name.clone();
                        let tool = Arc::new(ExtensionTool::new(def.clone()));
                        tools.insert(
                            name,
                            LoadedTool {
                                def,
                                file: path,
                                tool,
                            },
                        );
                    }
                    Err(failure) => {
                        broken.push(BrokenFile {
                            file: path,
                            reason: failure.reason,
                            line: failure.line,
                        });
                    }
                }
            }
            (
                Snapshot {
                    tools,
                    broken,
                    dir_exists: true,
                    dir_error: None,
                },
                reads,
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_ext(path: &Path, description: &str) {
        std::fs::write(
            path,
            format!(
                "name = \"meta_gate.ping\"\ndescription = \"{description}\"\neffect = \"read\"\nrun = [\"echo\", \"ok\"]\n"
            ),
        )
        .unwrap();
    }

    #[test]
    fn unchanged_directory_does_not_reread_files_and_same_second_size_change_reloads() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::under(dir.path());
        let builtins = Registry::new();
        let store = ExtensionStore::new();

        // Missing directory: zero file reads, repeated lookups stay at zero.
        assert!(store.specs(&paths, &builtins).is_empty());
        assert!(store.get(&paths, &builtins, "meta_gate.ping").is_none());
        assert_eq!(store.file_reads(), 0);

        let tools_d = paths.tools_dir();
        std::fs::create_dir_all(&tools_d).unwrap();
        let file = tools_d.join("meta_gate.ping.toml");
        write_ext(&file, "V1.");

        // First lookup reads the one file.
        let specs = store.specs(&paths, &builtins);
        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].description, "V1.");
        assert_eq!(store.file_reads(), 1);

        // Repeated lookups, listings, and doctor checks perform zero additional file reads.
        for _ in 0..5 {
            assert!(store.get(&paths, &builtins, "meta_gate.ping").is_some());
            assert_eq!(store.specs(&paths, &builtins).len(), 1);
            assert!(store.checks(&paths, &builtins)[0].ok);
        }
        assert_eq!(store.file_reads(), 1);

        // Two writes within the same second of different sizes are seen immediately.
        write_ext(&file, "V2 with a longer description.");
        let specs = store.specs(&paths, &builtins);
        assert_eq!(specs[0].description, "V2 with a longer description.");
        assert_eq!(store.file_reads(), 2);

        write_ext(&file, "V3.");
        let specs = store.specs(&paths, &builtins);
        assert_eq!(specs[0].description, "V3.");
        assert_eq!(store.file_reads(), 3);

        // Again, no extra reads while unchanged.
        assert_eq!(store.specs(&paths, &builtins)[0].description, "V3.");
        assert_eq!(store.file_reads(), 3);

        // Removal is picked up immediately without reading any file.
        std::fs::remove_file(&file).unwrap();
        assert!(store.specs(&paths, &builtins).is_empty());
        assert_eq!(store.file_reads(), 3);
    }
}
