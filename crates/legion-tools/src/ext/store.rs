//! Reloadable store over `tools.d/*.toml`, implementing [`legion_core::ToolSource`].
//!
//! Comparing the directory's `.toml` file bytes (as [`legion_core::ConfigStore`] does for single
//! documents) ensures edits, additions and deletions within the same second are picked up on the
//! very next list or call without restarting long-lived doors. Broken files are skipped and
//! recorded for `ext.list` and `meta.doctor`, never fatal.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

use legion_core::proto::ToolSpec;
use legion_core::{Paths, Registry, SourceCheck, Tool, ToolSource};

use super::def::{ExtensionDef, parse_toml};
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

#[derive(Clone, PartialEq, Eq)]
enum DirFingerprint {
    Missing,
    Error(String),
    Files(BTreeMap<OsString, Result<Vec<u8>, String>>),
}

struct Cached {
    dir: PathBuf,
    fingerprint: DirFingerprint,
    snapshot: Arc<Snapshot>,
}

/// Reloadable [`ToolSource`] backed by `tools.d/*.toml`.
#[derive(Default)]
pub struct ExtensionStore {
    cached: Mutex<Option<Cached>>,
}

impl ExtensionStore {
    /// Creates an empty store; `tools.d` is read lazily on first lookup.
    pub fn new() -> Self {
        Self::default()
    }

    fn snapshot(&self, paths: &Paths, builtins: &Registry) -> Arc<Snapshot> {
        let dir = paths.tools_dir();
        let fingerprint = fingerprint_dir(&dir);
        let mut guard = self.cached.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(cached) = guard.as_ref()
            && cached.dir == dir
            && cached.fingerprint == fingerprint
        {
            return Arc::clone(&cached.snapshot);
        }
        let is_builtin = |name: &str| builtins.get(name).is_some();
        let snapshot = Arc::new(build_snapshot(&dir, &fingerprint, &is_builtin));
        *guard = Some(Cached {
            dir,
            fingerprint,
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
pub fn load_snapshot(tools_dir: &Path, is_builtin: &dyn Fn(&str) -> bool) -> Snapshot {
    let fingerprint = fingerprint_dir(tools_dir);
    build_snapshot(tools_dir, &fingerprint, is_builtin)
}

fn fingerprint_dir(dir: &Path) -> DirFingerprint {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return DirFingerprint::Missing,
        Err(error) => return DirFingerprint::Error(error.to_string()),
    };
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
        let content = std::fs::read(entry.path()).map_err(|error| error.to_string());
        files.insert(file_name, content);
    }
    DirFingerprint::Files(files)
}

fn build_snapshot(
    dir: &Path,
    fingerprint: &DirFingerprint,
    is_builtin: &dyn Fn(&str) -> bool,
) -> Snapshot {
    match fingerprint {
        DirFingerprint::Missing => Snapshot {
            tools: BTreeMap::new(),
            broken: Vec::new(),
            dir_exists: false,
            dir_error: None,
        },
        DirFingerprint::Error(error) => Snapshot {
            tools: BTreeMap::new(),
            broken: vec![BrokenFile {
                file: dir.to_path_buf(),
                reason: format!("cannot be read: {error}"),
                line: None,
            }],
            dir_exists: true,
            dir_error: Some(error.clone()),
        },
        DirFingerprint::Files(files) => {
            let mut tools: BTreeMap<String, LoadedTool> = BTreeMap::new();
            let mut broken = Vec::new();
            for (file_name, content) in files {
                let path = dir.join(file_name);
                let bytes = match content {
                    Ok(bytes) => bytes,
                    Err(error) => {
                        broken.push(BrokenFile {
                            file: path,
                            reason: format!("cannot be read: {error}"),
                            line: None,
                        });
                        continue;
                    }
                };
                match parse_toml(bytes, is_builtin) {
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
            Snapshot {
                tools,
                broken,
                dir_exists: true,
                dir_error: None,
            }
        }
    }
}
