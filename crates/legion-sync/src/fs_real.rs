//! Real-disk [`SyncFs`] implementation over `std::fs`, `legion-xfer`, and `legion-os`.

use std::fs::{self, File, FileTimes, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use legion_os::atomic::{
    WriteOptions, available_space, exchange_files, file_change_id,
    rename_no_replace_or_link as rename_no_replace, replace_with_backup, same_device, write_atomic,
};
use legion_xfer::hash::hash_reader;
use legion_xfer::{EntryKind, Hash, Timestamp};

use crate::error::{FsErrorKind, Result, SyncError};
use crate::model::FolderMarker;
use crate::path::{
    IgnoreMatcher, SYNC_BIN_DIR, SYNC_MARKER_FILE, SYNC_META_DIR, SYNC_TMP_DIR, ancestors,
    check_fs_compatibility, is_dot_legion_sync_segment, validate_flat_internal_name,
    validate_staged_tmp_token, validate_sync_path,
};
use crate::traits::{FsCapabilities, FsMeta, FsWalkEntry, FsWalkResult, SyncFs, SyncWrite};

static REAL_TMP_SEQ: AtomicU64 = AtomicU64::new(0);

/// Checks that `new_root` is neither equal to, inside, nor containing any path in
/// `existing_roots`.
pub fn check_folders_not_nested(new_root: &Path, existing_roots: &[&Path]) -> Result<()> {
    let new_norm = canonical_or_lexical(new_root);
    for &existing in existing_roots {
        let ex_norm = canonical_or_lexical(existing);
        if new_norm == ex_norm || new_norm.starts_with(&ex_norm) || ex_norm.starts_with(&new_norm) {
            return Err(SyncError::InvalidConfig {
                message: format!(
                    "sync folder {} overlaps with existing sync folder {}",
                    new_root.display(),
                    existing.display()
                ),
                hint: Some(
                    "A sync folder cannot be created inside another sync folder or contain one."
                        .to_string(),
                ),
            });
        }
    }
    Ok(())
}

fn canonical_or_lexical(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

/// Filesystem mutation or directory sync recorded when [`RealFs::enable_durability_trace`] is
/// turned on (disabled by default with zero runtime overhead).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RealFsDurabilityEvent {
    /// Created directory `path` inside `parent`.
    CreateDir {
        /// Enclosing parent directory that received the new directory entry.
        parent: PathBuf,
        /// Newly created directory path.
        path: PathBuf,
    },
    /// Atomically wrote or appended+synced a file `path` inside `parent`.
    WriteFileSync {
        /// Enclosing parent directory of the written file.
        parent: PathBuf,
        /// File path that was written and synced.
        path: PathBuf,
    },
    /// Renamed or cross-device moved an entry between `src_parent` and `dst_parent`.
    Rename {
        /// Source parent directory whose entry was removed.
        src_parent: PathBuf,
        /// Destination parent directory whose entry was inserted.
        dst_parent: PathBuf,
    },
    /// Atomically swapped two entries in `src_parent` and `dst_parent`.
    Exchange {
        /// First parent directory participating in the exchange.
        src_parent: PathBuf,
        /// Second parent directory participating in the exchange.
        dst_parent: PathBuf,
    },
    /// Removed a file or directory `path` from `parent`.
    Remove {
        /// Enclosing parent directory from which `path` was unlinked.
        parent: PathBuf,
        /// Path that was removed.
        path: PathBuf,
    },
    /// Flushed directory `dir` to durable storage (`fsync` on Unix).
    SyncDir {
        /// Directory handle that was flushed.
        dir: PathBuf,
    },
}

/// Summary of auditing [`RealFs`] directory-sync discipline across every mutating operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RealFsDurabilityAudit {
    /// `true` iff `ensure_parent_dirs` synced the parent directory of every newly created nested
    /// ancestor directory before returning.
    pub sync_created_ancestors: bool,
    /// `true` iff `remove_tmp` synced the staging directory after unlinking a tmp file.
    pub sync_remove_tmp: bool,
    /// Descriptions of any mutating operation whose touched parent directory lacked a subsequent
    /// [`RealFsDurabilityEvent::SyncDir`]. Empty when [`RealFs`] obeys every durability invariant.
    pub unsynced_ops: Vec<String>,
}

/// Real filesystem backend rooted at one local sync folder directory.
///
/// Opening a [`RealFs`] does *not* silently recreate `.legion-sync/` or its marker file, so an
/// unmounted external drive or empty mount point is caught by [`crate::SyncEngine`] rather than
/// looking like a valid empty folder.
#[derive(Debug, Clone)]
pub struct RealFs {
    root: PathBuf,
    caps: FsCapabilities,
    trace: Option<Arc<Mutex<Vec<RealFsDurabilityEvent>>>>,
}

impl RealFs {
    /// Opens `root` as a sync folder handle without creating `.legion-sync/` or `marker.json`.
    ///
    /// Probes the filesystem's case sensitivity (without creating `.legion-sync/`) when `root`
    /// already exists, or uses OS defaults when `root` does not exist yet.
    pub fn open(root: impl AsRef<Path>) -> Result<Self> {
        let root = root.as_ref().to_path_buf();
        let caps = detect_capabilities(&root);
        Ok(Self {
            root,
            caps,
            trace: None,
        })
    }

    /// Opens `root` with explicitly supplied [`FsCapabilities`] (useful for testing Windows or
    /// case-insensitive rules against a temporary Linux directory).
    pub fn open_with_capabilities(root: impl AsRef<Path>, caps: FsCapabilities) -> Result<Self> {
        Ok(Self {
            root: root.as_ref().to_path_buf(),
            caps,
            trace: None,
        })
    }

    /// Enables opt-in durability tracing of mutations and directory syncs for testing and auditing.
    pub fn enable_durability_trace(&mut self) {
        self.trace = Some(Arc::new(Mutex::new(Vec::new())));
    }

    /// Drains and returns all recorded [`RealFsDurabilityEvent`]s since tracing was enabled or last
    /// drained.
    pub fn take_durability_trace(&self) -> Vec<RealFsDurabilityEvent> {
        self.trace
            .as_ref()
            .and_then(|t| t.lock().ok().map(|mut v| std::mem::take(&mut *v)))
            .unwrap_or_default()
    }

    fn record_trace(&self, event: RealFsDurabilityEvent) {
        if let Some(ref t) = self.trace
            && let Ok(mut guard) = t.lock()
        {
            guard.push(event);
        }
    }

    fn sync_dir(&self, dir: &Path) -> Result<()> {
        sync_dir_os(dir)?;
        self.record_trace(RealFsDurabilityEvent::SyncDir {
            dir: dir.to_path_buf(),
        });
        Ok(())
    }

    /// Returns the root directory path of this sync folder.
    pub fn root(&self) -> &Path {
        &self.root
    }

    fn meta_dir(&self) -> PathBuf {
        self.root.join(SYNC_META_DIR)
    }

    fn marker_path(&self) -> PathBuf {
        self.root.join(SYNC_MARKER_FILE)
    }

    fn tmp_dir(&self) -> PathBuf {
        self.root.join(SYNC_TMP_DIR)
    }

    fn bin_dir(&self) -> PathBuf {
        self.root.join(SYNC_BIN_DIR)
    }

    fn guard_platform_path(&self, clean: &str, kind: EntryKind) -> Result<()> {
        if self.caps.windows_naming
            && let Err((_, msg)) = check_fs_compatibility(clean, kind, &self.caps)
        {
            return Err(SyncError::InvalidPath {
                path: clean.to_string(),
                reason: msg,
            });
        }
        Ok(())
    }

    fn resolve_live(&self, rel_path: &str, kind: EntryKind) -> Result<PathBuf> {
        let clean = validate_sync_path(rel_path)?;
        self.guard_platform_path(&clean, kind)?;
        let mut out = self.root.clone();
        for seg in clean.split('/') {
            out.push(seg);
        }
        Ok(out)
    }

    /// Verifies that no strict ancestor of `rel_path` is a symbolic link.
    ///
    /// Returns `Ok(true)` if all existing ancestors are directories, or `Ok(false)` if an
    /// existing ancestor is missing or a regular file.
    fn check_ancestors_not_symlink(&self, clean: &str) -> Result<bool> {
        let mut current = self.root.clone();
        for anc in ancestors(clean) {
            let seg = anc.rsplit_once('/').map_or(anc, |(_, s)| s);
            current.push(seg);
            match fs::symlink_metadata(&current) {
                Ok(meta) => {
                    if meta.file_type().is_symlink() {
                        let reason =
                            format!("ancestor '{anc}' is a symbolic link and cannot be traversed");
                        return Err(SyncError::InvalidPath {
                            path: clean.to_string(),
                            reason,
                        });
                    }
                    if !meta.file_type().is_dir() {
                        return Ok(false);
                    }
                }
                Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(false),
                Err(err) => return Err(SyncError::from_io(Some(anc.to_string()), err)),
            }
        }
        Ok(true)
    }

    /// Walks strict ancestors of `clean`, rejecting any symbolic-link ancestor with `InvalidPath`,
    /// and returns the deepest existing directory ancestor (stopping at a missing or non-directory
    /// ancestor such as a regular file that `ensure_ancestors_are_dirs` will convert during apply).
    fn deepest_existing_dir_ancestor(&self, clean: &str) -> Result<PathBuf> {
        let mut current = self.root.clone();
        let mut deepest = self.root.clone();
        for anc in ancestors(clean) {
            let seg = anc.rsplit_once('/').map_or(anc, |(_, s)| s);
            current.push(seg);
            match fs::symlink_metadata(&current) {
                Ok(meta) => {
                    if meta.file_type().is_symlink() {
                        let reason =
                            format!("ancestor '{anc}' is a symbolic link and cannot be traversed");
                        return Err(SyncError::InvalidPath {
                            path: clean.to_string(),
                            reason,
                        });
                    }
                    if meta.file_type().is_dir() {
                        deepest = current.clone();
                    } else {
                        break;
                    }
                }
                Err(err) if err.kind() == io::ErrorKind::NotFound => break,
                Err(err) => return Err(SyncError::from_io(Some(anc.to_string()), err)),
            }
        }
        Ok(deepest)
    }

    /// Verifies that no strict ancestor directory of `rel_path` is a symlink or non-directory.
    fn ensure_no_symlink_ancestors(&self, clean: &str) -> Result<()> {
        let mut current = self.root.clone();
        for anc in ancestors(clean) {
            let seg = anc.rsplit_once('/').map_or(anc, |(_, s)| s);
            current.push(seg);
            match fs::symlink_metadata(&current) {
                Ok(meta) => {
                    if meta.file_type().is_symlink() {
                        let reason =
                            format!("ancestor '{anc}' is a symbolic link and cannot be traversed");
                        return Err(SyncError::InvalidPath {
                            path: clean.to_string(),
                            reason,
                        });
                    }
                    if !meta.file_type().is_dir() {
                        let reason = format!("ancestor '{anc}' is not a directory");
                        return Err(SyncError::Conflict {
                            path: clean.to_string(),
                            reason,
                        });
                    }
                }
                Err(err) if err.kind() == io::ErrorKind::NotFound => break,
                Err(err) => return Err(SyncError::from_io(Some(anc.to_string()), err)),
            }
        }
        Ok(())
    }

    fn ensure_parent_dirs(&self, clean: &str) -> Result<PathBuf> {
        self.ensure_no_symlink_ancestors(clean)?;
        let mut current = self.root.clone();
        for anc in ancestors(clean) {
            let seg = anc.rsplit_once('/').map_or(anc, |(_, s)| s);
            current.push(seg);
            match fs::symlink_metadata(&current) {
                Ok(meta) => {
                    if !meta.file_type().is_dir() {
                        let reason = format!("ancestor '{anc}' is not a directory");
                        return Err(SyncError::Conflict {
                            path: clean.to_string(),
                            reason,
                        });
                    }
                }
                Err(err) if err.kind() == io::ErrorKind::NotFound => {
                    fs::create_dir(&current)
                        .map_err(|e| SyncError::from_io(Some(anc.to_string()), e))?;
                    if let Some(parent) = current.parent() {
                        self.record_trace(RealFsDurabilityEvent::CreateDir {
                            parent: parent.to_path_buf(),
                            path: current.clone(),
                        });
                        self.sync_dir(parent)?;
                    }
                }
                Err(err) => return Err(SyncError::from_io(Some(anc.to_string()), err)),
            }
        }
        Ok(current)
    }

    fn resolve_tmp(&self, tmp_name: &str, create_parent: bool) -> Result<PathBuf> {
        match validate_staged_tmp_token(tmp_name)? {
            None => {
                let dir = self.tmp_dir();
                if create_parent {
                    let existed = dir.exists();
                    fs::create_dir_all(&dir)
                        .map_err(|err| SyncError::from_io(Some(SYNC_TMP_DIR.to_string()), err))?;
                    if !existed {
                        self.record_trace(RealFsDurabilityEvent::CreateDir {
                            parent: self.meta_dir(),
                            path: dir.clone(),
                        });
                        self.sync_dir(&self.meta_dir())?;
                    }
                }
                Ok(dir.join(tmp_name))
            }
            Some(clean_rel) => {
                if create_parent {
                    let _ = self.ensure_parent_dirs(&clean_rel)?;
                } else {
                    self.ensure_no_symlink_ancestors(&clean_rel)?;
                }
                self.resolve_live(&clean_rel, EntryKind::File)
            }
        }
    }

    fn tmp_label(tmp_name: &str) -> String {
        if tmp_name.contains('/') || tmp_name.starts_with(".legion-") {
            tmp_name.to_string()
        } else {
            format!("{SYNC_TMP_DIR}/{tmp_name}")
        }
    }

    fn move_file_no_replace_cross_device_safe(
        &self,
        src: &Path,
        dst: &Path,
        label: &str,
    ) -> Result<()> {
        match rename_no_replace(src, dst) {
            Ok(()) => {
                if let (Some(src_parent), Some(dst_parent)) = (src.parent(), dst.parent()) {
                    self.record_trace(RealFsDurabilityEvent::Rename {
                        src_parent: src_parent.to_path_buf(),
                        dst_parent: dst_parent.to_path_buf(),
                    });
                }
                if let Some(parent) = dst.parent() {
                    self.sync_dir(parent)?;
                }
                if let Some(parent) = src.parent()
                    && dst.parent() != Some(parent)
                {
                    self.sync_dir(parent)?;
                }
                Ok(())
            }
            Err(os_err) => {
                let sync_err = SyncError::from_os(Some(label.to_string()), os_err);
                if !matches!(
                    sync_err,
                    SyncError::Fs {
                        kind: FsErrorKind::CrossDevice,
                        ..
                    }
                ) {
                    return Err(sync_err);
                }
                let dst_parent = dst.parent().ok_or_else(|| SyncError::InvalidPath {
                    path: label.to_string(),
                    reason: "destination path has no parent directory".to_string(),
                })?;
                let seq = REAL_TMP_SEQ.fetch_add(1, Ordering::Relaxed);
                let stage =
                    dst_parent.join(format!(".legion-xdev-{}-{seq}.tmp", std::process::id()));
                let copy_res = (|| -> Result<()> {
                    let mut reader = File::open(src)
                        .map_err(|e| SyncError::from_io(Some(label.to_string()), e))?;
                    let mut writer = create_private_file(&stage)
                        .map_err(|e| SyncError::from_io(Some(label.to_string()), e))?;
                    io::copy(&mut reader, &mut writer)
                        .map_err(|e| SyncError::from_io(Some(label.to_string()), e))?;
                    writer
                        .flush()
                        .map_err(|e| SyncError::from_io(Some(label.to_string()), e))?;
                    writer
                        .sync_all()
                        .map_err(|e| SyncError::from_io(Some(label.to_string()), e))?;
                    Ok(())
                })();
                if let Err(err) = copy_res {
                    let _ = fs::remove_file(&stage);
                    return Err(err);
                }
                if let Err(os_err) = rename_no_replace(&stage, dst) {
                    let _ = fs::remove_file(&stage);
                    return Err(SyncError::from_os(Some(label.to_string()), os_err));
                }
                self.record_trace(RealFsDurabilityEvent::Rename {
                    src_parent: dst_parent.to_path_buf(),
                    dst_parent: dst_parent.to_path_buf(),
                });
                self.sync_dir(dst_parent)?;
                match fs::remove_file(src) {
                    Ok(()) => {
                        if let Some(parent) = src.parent() {
                            self.record_trace(RealFsDurabilityEvent::Remove {
                                parent: parent.to_path_buf(),
                                path: src.to_path_buf(),
                            });
                            self.sync_dir(parent)?;
                        }
                    }
                    Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                    Err(e) => return Err(SyncError::from_io(Some(label.to_string()), e)),
                }
                Ok(())
            }
        }
    }

    fn resolve_bin(&self, bin_name: &str) -> Result<PathBuf> {
        validate_flat_internal_name(bin_name)?;
        let dir = self.bin_dir();
        let existed = dir.exists();
        fs::create_dir_all(&dir)
            .map_err(|err| SyncError::from_io(Some(SYNC_BIN_DIR.to_string()), err))?;
        if !existed {
            self.record_trace(RealFsDurabilityEvent::CreateDir {
                parent: self.meta_dir(),
                path: dir.clone(),
            });
            self.sync_dir(&self.meta_dir())?;
        }
        Ok(dir.join(bin_name))
    }
}

struct RealSyncWriter {
    path: PathBuf,
    file: File,
}

impl Write for RealSyncWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.file.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}

impl SyncWrite for RealSyncWriter {
    fn finish_sync(mut self: Box<Self>) -> Result<()> {
        self.file
            .flush()
            .map_err(|e| SyncError::from_io(Some(self.path.display().to_string()), e))?;
        self.file
            .sync_all()
            .map_err(|e| SyncError::from_io(Some(self.path.display().to_string()), e))?;
        Ok(())
    }

    fn finish_sync_with_metadata(
        mut self: Box<Self>,
        mtime: Timestamp,
        mode: Option<u32>,
    ) -> Result<bool> {
        let label = self.path.display().to_string();
        self.file
            .flush()
            .map_err(|e| SyncError::from_io(Some(label.clone()), e))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let effective_mode = mode.unwrap_or(0o644) & 0o777;
            self.file
                .set_permissions(fs::Permissions::from_mode(effective_mode))
                .map_err(|e| SyncError::from_io(Some(label.clone()), e))?;
        }
        #[cfg(not(unix))]
        let _ = mode;
        if let Some(st) = timestamp_to_system_time(mtime) {
            let times = FileTimes::new().set_modified(st);
            self.file
                .set_times(times)
                .map_err(|e| SyncError::from_io(Some(label.clone()), e))?;
        }
        self.file
            .sync_all()
            .map_err(|e| SyncError::from_io(Some(label), e))?;
        Ok(true)
    }
}

impl SyncFs for RealFs {
    fn capabilities(&self) -> FsCapabilities {
        self.caps
    }

    fn read_marker(&self) -> Result<Option<FolderMarker>> {
        let path = self.marker_path();
        match fs::read(&path) {
            Ok(bytes) => {
                let marker: FolderMarker =
                    serde_json::from_slice(&bytes).map_err(|err| SyncError::CorruptData {
                        context: SYNC_MARKER_FILE.to_string(),
                        detail: err.to_string(),
                    })?;
                Ok(Some(marker))
            }
            Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(err) => Err(SyncError::from_io(Some(SYNC_MARKER_FILE.to_string()), err)),
        }
    }

    fn write_marker(&self, marker: &FolderMarker) -> Result<()> {
        self.ensure_internal_dirs()?;
        let bytes = serde_json::to_vec_pretty(marker).map_err(|err| SyncError::Storage {
            context: "serialize folder marker".to_string(),
            detail: err.to_string(),
        })?;
        let marker_path = self.marker_path();
        write_atomic(&marker_path, &bytes, WriteOptions::with_mode(0o600))
            .map_err(|err| SyncError::from_os(Some(SYNC_MARKER_FILE.to_string()), err))?;
        self.record_trace(RealFsDurabilityEvent::WriteFileSync {
            parent: self.meta_dir(),
            path: marker_path,
        });
        self.sync_dir(&self.meta_dir())?;
        Ok(())
    }

    fn ensure_internal_dirs(&self) -> Result<()> {
        fs::create_dir_all(&self.root)
            .map_err(|err| SyncError::from_io(Some(self.root.display().to_string()), err))?;
        let meta_dir = self.meta_dir();
        let tmp_dir = self.tmp_dir();
        let bin_dir = self.bin_dir();
        let created_meta = !meta_dir.exists();
        let created_tmp = !tmp_dir.exists();
        let created_bin = !bin_dir.exists();
        fs::create_dir_all(&tmp_dir)
            .map_err(|err| SyncError::from_io(Some(SYNC_TMP_DIR.to_string()), err))?;
        fs::create_dir_all(&bin_dir)
            .map_err(|err| SyncError::from_io(Some(SYNC_BIN_DIR.to_string()), err))?;
        if created_meta {
            self.record_trace(RealFsDurabilityEvent::CreateDir {
                parent: self.root.clone(),
                path: meta_dir.clone(),
            });
            self.sync_dir(&self.root)?;
        }
        if created_tmp {
            self.record_trace(RealFsDurabilityEvent::CreateDir {
                parent: meta_dir.clone(),
                path: tmp_dir,
            });
        }
        if created_bin {
            self.record_trace(RealFsDurabilityEvent::CreateDir {
                parent: meta_dir.clone(),
                path: bin_dir,
            });
        }
        if created_meta || created_tmp || created_bin {
            self.sync_dir(&meta_dir)?;
        }
        Ok(())
    }

    fn check_not_nested(&self) -> Result<()> {
        // 1. Refuse creating/opening inside an ancestor that already has a `.legion-sync` marker.
        let abs = canonical_or_lexical(&self.root);
        let mut cursor = abs.parent();
        while let Some(anc) = cursor {
            if anc.join(SYNC_META_DIR).is_dir() {
                return Err(SyncError::InvalidConfig {
                    message: format!(
                        "sync folder {} cannot be inside existing sync folder {}",
                        self.root.display(),
                        anc.display()
                    ),
                    hint: Some("Choose a directory outside any existing sync folder.".to_string()),
                });
            }
            cursor = anc.parent();
        }

        // 2. Refuse opening a directory that already contains a nested `.legion-sync` folder.
        if self.root.is_dir()
            && let Some(nested) = find_nested_sync_dir(&self.root, true)
        {
            return Err(SyncError::InvalidConfig {
                message: format!(
                    "sync folder {} cannot contain nested sync folder {}",
                    self.root.display(),
                    nested.display()
                ),
                hint: Some(
                    "Remove the nested sync folder or choose a separate directory.".to_string(),
                ),
            });
        }
        Ok(())
    }

    fn available_space_for(&self, live_rel: &str) -> Result<Option<u64>> {
        let clean = validate_sync_path(live_rel)?;
        let probe = self.deepest_existing_dir_ancestor(&clean)?;
        Ok(available_space(&probe).ok())
    }

    fn needs_local_staging(&self, live_rel: &str) -> Result<bool> {
        let clean = validate_sync_path(live_rel)?;
        let probe = self.deepest_existing_dir_ancestor(&clean)?;
        let tmp_dir = self.tmp_dir();
        match same_device(&tmp_dir, &probe) {
            Ok(same) => Ok(!same),
            Err(_) => Ok(false),
        }
    }

    fn walk(&self, ignore: &IgnoreMatcher) -> Result<FsWalkResult> {
        let mut entries = Vec::new();
        let mut skipped = Vec::new();
        walk_dir(&self.root, "", ignore, &mut entries, &mut skipped)?;
        entries.sort_by(|a, b| a.path.cmp(&b.path));
        skipped.sort();
        Ok(FsWalkResult { entries, skipped })
    }

    fn walk_subtree(&self, rel_root: &str, ignore: &IgnoreMatcher) -> Result<FsWalkResult> {
        let clean = validate_sync_path(rel_root)?;
        let mut entries = Vec::new();
        let mut skipped = Vec::new();
        if !self.check_ancestors_not_symlink(&clean)? {
            return Ok(FsWalkResult { entries, skipped });
        }
        let full = self.resolve_live(&clean, EntryKind::Dir)?;
        let meta = match fs::symlink_metadata(&full) {
            Ok(m) => m,
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                return Ok(FsWalkResult { entries, skipped });
            }
            Err(err) => {
                skipped.push((clean, err.to_string()));
                return Ok(FsWalkResult { entries, skipped });
            }
        };

        let ft = meta.file_type();
        let is_dir = ft.is_dir();
        if ignore.is_path_or_ancestor_ignored(&clean, is_dir) {
            return Ok(FsWalkResult { entries, skipped });
        }

        if is_dir {
            entries.push(FsWalkEntry {
                path: clean.clone(),
                meta: FsMeta {
                    kind: EntryKind::Dir,
                    size: 0,
                    mtime: metadata_mtime(&meta),
                    mode: entry_mode(&meta),
                    link_target: None,
                    change_id: file_change_id(&full, &meta),
                    file_identity: file_identity(&meta),
                },
            });
            walk_dir(&full, &clean, ignore, &mut entries, &mut skipped)?;
        } else if let Some(fs_meta) = stat_path(&full, &clean)? {
            entries.push(FsWalkEntry {
                path: clean,
                meta: fs_meta,
            });
        }

        entries.sort_by(|a, b| a.path.cmp(&b.path));
        skipped.sort();
        Ok(FsWalkResult { entries, skipped })
    }

    fn stat_live(&self, rel_path: &str) -> Result<Option<FsMeta>> {
        let clean = validate_sync_path(rel_path)?;
        self.guard_platform_path(&clean, EntryKind::File)?;
        if !self.check_ancestors_not_symlink(&clean)? {
            return Ok(None);
        }
        let full = self.resolve_live(&clean, EntryKind::File)?;
        stat_path(&full, &clean)
    }

    fn hash_live(&self, rel_path: &str) -> Result<(Hash, u64)> {
        let clean = validate_sync_path(rel_path)?;
        self.guard_platform_path(&clean, EntryKind::File)?;
        self.ensure_no_symlink_ancestors(&clean)?;
        let full = self.resolve_live(&clean, EntryKind::File)?;
        let meta =
            fs::symlink_metadata(&full).map_err(|e| SyncError::from_io(Some(clean.clone()), e))?;
        if !meta.file_type().is_file() {
            return Err(SyncError::Conflict {
                path: clean,
                reason: "expected a regular file when hashing".to_string(),
            });
        }
        let mut file = File::open(&full).map_err(|e| SyncError::from_io(Some(clean.clone()), e))?;
        hash_reader(&mut file).map_err(SyncError::from)
    }

    fn open_live_read_at(&self, rel_path: &str, offset: u64) -> Result<Box<dyn Read + Send>> {
        let clean = validate_sync_path(rel_path)?;
        self.guard_platform_path(&clean, EntryKind::File)?;
        self.ensure_no_symlink_ancestors(&clean)?;
        let full = self.resolve_live(&clean, EntryKind::File)?;
        let meta =
            fs::symlink_metadata(&full).map_err(|e| SyncError::from_io(Some(clean.clone()), e))?;
        if !meta.file_type().is_file() {
            return Err(SyncError::Conflict {
                path: clean,
                reason: "expected a regular file to stream to peer".to_string(),
            });
        }
        let mut file = File::open(&full).map_err(|e| SyncError::from_io(Some(clean.clone()), e))?;
        if offset > 0 {
            file.seek(SeekFrom::Start(offset))
                .map_err(|e| SyncError::from_io(Some(clean), e))?;
        }
        Ok(Box::new(file))
    }

    fn create_dir_all_live(&self, rel_path: &str) -> Result<()> {
        let clean = validate_sync_path(rel_path)?;
        self.guard_platform_path(&clean, EntryKind::Dir)?;
        let parent = self.ensure_parent_dirs(&clean)?;
        let full = self.resolve_live(&clean, EntryKind::Dir)?;
        match fs::symlink_metadata(&full) {
            Ok(meta) if meta.file_type().is_dir() => Ok(()),
            Ok(_) => Err(SyncError::Conflict {
                path: clean,
                reason: "a non-directory entry already exists at this path".to_string(),
            }),
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                fs::create_dir(&full).map_err(|e| SyncError::from_io(Some(clean), e))?;
                self.record_trace(RealFsDurabilityEvent::CreateDir {
                    parent: parent.clone(),
                    path: full,
                });
                self.sync_dir(&parent)?;
                Ok(())
            }
            Err(err) => Err(SyncError::from_io(Some(clean), err)),
        }
    }

    fn list_dir_live(&self, rel_path: &str) -> Result<Vec<String>> {
        let clean = validate_sync_path(rel_path)?;
        self.guard_platform_path(&clean, EntryKind::Dir)?;
        self.ensure_no_symlink_ancestors(&clean)?;
        let full = self.resolve_live(&clean, EntryKind::Dir)?;
        let mut names = Vec::new();
        for item in fs::read_dir(&full).map_err(|e| SyncError::from_io(Some(clean.clone()), e))? {
            let entry = item.map_err(|e| SyncError::from_io(Some(clean.clone()), e))?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if is_dot_legion_sync_segment(&name) {
                continue;
            }
            names.push(name);
        }
        names.sort();
        Ok(names)
    }

    fn remove_empty_dir_live(&self, rel_path: &str) -> Result<()> {
        let clean = validate_sync_path(rel_path)?;
        self.guard_platform_path(&clean, EntryKind::Dir)?;
        self.ensure_no_symlink_ancestors(&clean)?;
        let full = self.resolve_live(&clean, EntryKind::Dir)?;
        fs::remove_dir(&full).map_err(|e| SyncError::from_io(Some(clean), e))?;
        if let Some(parent) = full.parent() {
            self.record_trace(RealFsDurabilityEvent::Remove {
                parent: parent.to_path_buf(),
                path: full.clone(),
            });
            self.sync_dir(parent)?;
        }
        Ok(())
    }

    fn create_symlink_live(&self, rel_path: &str, target: &str) -> Result<()> {
        let clean = validate_sync_path(rel_path)?;
        self.guard_platform_path(&clean, EntryKind::Symlink)?;
        let parent = self.ensure_parent_dirs(&clean)?;
        let full = self.resolve_live(&clean, EntryKind::Symlink)?;
        create_symlink_no_replace(Path::new(target), &full, &clean)?;
        self.record_trace(RealFsDurabilityEvent::WriteFileSync {
            parent: parent.clone(),
            path: full,
        });
        self.sync_dir(&parent)?;
        Ok(())
    }

    fn remove_symlink_live(&self, rel_path: &str) -> Result<()> {
        let clean = validate_sync_path(rel_path)?;
        self.guard_platform_path(&clean, EntryKind::Symlink)?;
        self.ensure_no_symlink_ancestors(&clean)?;
        let full = self.resolve_live(&clean, EntryKind::Symlink)?;
        let meta =
            fs::symlink_metadata(&full).map_err(|e| SyncError::from_io(Some(clean.clone()), e))?;
        if !meta.file_type().is_symlink() {
            return Err(SyncError::Conflict {
                path: clean,
                reason: "expected a symbolic link to remove".to_string(),
            });
        }
        fs::remove_file(&full).map_err(|e| SyncError::from_io(Some(clean), e))?;
        if let Some(parent) = full.parent() {
            self.record_trace(RealFsDurabilityEvent::Remove {
                parent: parent.to_path_buf(),
                path: full.clone(),
            });
            self.sync_dir(parent)?;
        }
        Ok(())
    }

    fn set_live_metadata(&self, rel_path: &str, mtime: Timestamp, mode: Option<u32>) -> Result<()> {
        let clean = validate_sync_path(rel_path)?;
        self.guard_platform_path(&clean, EntryKind::File)?;
        self.ensure_no_symlink_ancestors(&clean)?;
        let full = self.resolve_live(&clean, EntryKind::File)?;
        let meta =
            fs::symlink_metadata(&full).map_err(|e| SyncError::from_io(Some(clean.clone()), e))?;
        if meta.file_type().is_symlink() {
            return Ok(());
        }
        if meta.file_type().is_file() {
            if let Some(mode_bits) = mode {
                set_unix_mode(&full, mode_bits & 0o777, &clean)?;
            }
            if let Some(st) = timestamp_to_system_time(mtime) {
                let file = OpenOptions::new()
                    .write(true)
                    .open(&full)
                    .map_err(|e| SyncError::from_io(Some(clean.clone()), e))?;
                let times = FileTimes::new().set_modified(st);
                file.set_times(times)
                    .map_err(|e| SyncError::from_io(Some(clean), e))?;
            }
        }
        Ok(())
    }

    fn rename_live_to_live_no_replace(&self, from_rel: &str, to_rel: &str) -> Result<()> {
        let clean_from = validate_sync_path(from_rel)?;
        let clean_to = validate_sync_path(to_rel)?;
        self.guard_platform_path(&clean_from, EntryKind::File)?;
        self.guard_platform_path(&clean_to, EntryKind::File)?;
        self.ensure_no_symlink_ancestors(&clean_from)?;
        let parent_to = self.ensure_parent_dirs(&clean_to)?;
        let full_from = self.resolve_live(&clean_from, EntryKind::File)?;
        let full_to = self.resolve_live(&clean_to, EntryKind::File)?;
        rename_no_replace(&full_from, &full_to)
            .map_err(|err| SyncError::from_os(Some(clean_to), err))?;
        if let Some(parent_from) = full_from.parent() {
            self.record_trace(RealFsDurabilityEvent::Rename {
                src_parent: parent_from.to_path_buf(),
                dst_parent: parent_to.clone(),
            });
        }
        self.sync_dir(&parent_to)?;
        if let Some(parent_from) = full_from.parent()
            && parent_from != parent_to
        {
            self.sync_dir(parent_from)?;
        }
        Ok(())
    }

    fn sync_parent_dir_live(&self, live_rel: &str) -> Result<()> {
        let clean = validate_sync_path(live_rel)?;
        let full = self.resolve_live(&clean, EntryKind::File)?;
        if let Some(parent) = full.parent() {
            self.sync_dir(parent)?;
        }
        Ok(())
    }

    fn sync_tmp_dir(&self) -> Result<()> {
        self.sync_dir(&self.tmp_dir())
    }

    fn create_tmp(&self, tmp_name: &str) -> Result<Box<dyn SyncWrite>> {
        let full = self.resolve_tmp(tmp_name, true)?;
        let file = create_private_file(&full)
            .map_err(|e| SyncError::from_io(Some(Self::tmp_label(tmp_name)), e))?;
        Ok(Box::new(RealSyncWriter { path: full, file }))
    }

    fn open_tmp_append(&self, tmp_name: &str) -> Result<Box<dyn SyncWrite>> {
        let full = self.resolve_tmp(tmp_name, true)?;
        let file = OpenOptions::new()
            .append(true)
            .create(true)
            .open(&full)
            .map_err(|e| SyncError::from_io(Some(Self::tmp_label(tmp_name)), e))?;
        Ok(Box::new(RealSyncWriter { path: full, file }))
    }

    fn stat_tmp(&self, tmp_name: &str) -> Result<Option<FsMeta>> {
        let full = self.resolve_tmp(tmp_name, false)?;
        stat_path(&full, &Self::tmp_label(tmp_name))
    }

    fn set_tmp_metadata(&self, tmp_name: &str, mtime: Timestamp, mode: Option<u32>) -> Result<()> {
        let label = Self::tmp_label(tmp_name);
        let full = self.resolve_tmp(tmp_name, false)?;
        let meta =
            fs::symlink_metadata(&full).map_err(|e| SyncError::from_io(Some(label.clone()), e))?;
        if meta.file_type().is_file() {
            let effective_mode = mode.unwrap_or(0o644) & 0o777;
            set_unix_mode(&full, effective_mode, &label)?;
            if let Some(st) = timestamp_to_system_time(mtime) {
                let file = OpenOptions::new()
                    .write(true)
                    .open(&full)
                    .map_err(|e| SyncError::from_io(Some(label.clone()), e))?;
                let times = FileTimes::new().set_modified(st);
                file.set_times(times)
                    .map_err(|e| SyncError::from_io(Some(label.clone()), e))?;
                file.sync_all()
                    .map_err(|e| SyncError::from_io(Some(label), e))?;
            }
        }
        Ok(())
    }

    fn hash_tmp(&self, tmp_name: &str) -> Result<(Hash, u64)> {
        let full = self.resolve_tmp(tmp_name, false)?;
        let mut file = File::open(&full)
            .map_err(|e| SyncError::from_io(Some(Self::tmp_label(tmp_name)), e))?;
        hash_reader(&mut file).map_err(SyncError::from)
    }

    fn remove_tmp(&self, tmp_name: &str) -> Result<()> {
        let full = self.resolve_tmp(tmp_name, false)?;
        match fs::remove_file(&full) {
            Ok(()) => {
                if let Some(parent) = full.parent() {
                    self.record_trace(RealFsDurabilityEvent::Remove {
                        parent: parent.to_path_buf(),
                        path: full.clone(),
                    });
                    self.sync_dir(parent)?;
                }
                Ok(())
            }
            Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(err) => Err(SyncError::from_io(Some(Self::tmp_label(tmp_name)), err)),
        }
    }

    fn list_tmp(&self) -> Result<Vec<String>> {
        list_flat_dir(&self.tmp_dir(), SYNC_TMP_DIR)
    }

    fn commit_tmp_to_live_no_replace(&self, tmp_name: &str, live_rel: &str) -> Result<()> {
        let clean = validate_sync_path(live_rel)?;
        self.guard_platform_path(&clean, EntryKind::File)?;
        let parent = self.ensure_parent_dirs(&clean)?;
        let src = self.resolve_tmp(tmp_name, false)?;
        let dst = self.resolve_live(&clean, EntryKind::File)?;
        rename_no_replace(&src, &dst).map_err(|err| SyncError::from_os(Some(clean), err))?;
        if let Some(src_parent) = src.parent() {
            self.record_trace(RealFsDurabilityEvent::Rename {
                src_parent: src_parent.to_path_buf(),
                dst_parent: parent.clone(),
            });
        }
        self.sync_dir(&parent)?;
        if let Some(src_parent) = src.parent()
            && src_parent != parent
        {
            self.sync_dir(src_parent)?;
        }
        Ok(())
    }

    fn commit_tmp_to_live_no_replace_deferred_dir_sync(
        &self,
        tmp_name: &str,
        live_rel: &str,
    ) -> Result<()> {
        let clean = validate_sync_path(live_rel)?;
        self.guard_platform_path(&clean, EntryKind::File)?;
        let parent = self.ensure_parent_dirs(&clean)?;
        let src = self.resolve_tmp(tmp_name, false)?;
        let dst = self.resolve_live(&clean, EntryKind::File)?;
        rename_no_replace(&src, &dst).map_err(|err| SyncError::from_os(Some(clean), err))?;
        if let Some(src_parent) = src.parent() {
            self.record_trace(RealFsDurabilityEvent::Rename {
                src_parent: src_parent.to_path_buf(),
                dst_parent: parent,
            });
        }
        Ok(())
    }

    fn exchange_tmp_with_live(&self, tmp_name: &str, live_rel: &str) -> Result<bool> {
        let clean = validate_sync_path(live_rel)?;
        self.guard_platform_path(&clean, EntryKind::File)?;
        self.ensure_no_symlink_ancestors(&clean)?;
        let src = self.resolve_tmp(tmp_name, false)?;
        let dst = self.resolve_live(&clean, EntryKind::File)?;
        match exchange_files(&src, &dst) {
            Ok(()) => {
                if let (Some(src_parent), Some(dst_parent)) = (src.parent(), dst.parent()) {
                    self.record_trace(RealFsDurabilityEvent::Exchange {
                        src_parent: src_parent.to_path_buf(),
                        dst_parent: dst_parent.to_path_buf(),
                    });
                }
                if let Some(parent) = dst.parent() {
                    self.sync_dir(parent)?;
                }
                if let Some(src_parent) = src.parent()
                    && dst.parent() != Some(src_parent)
                {
                    self.sync_dir(src_parent)?;
                }
                Ok(true)
            }
            Err(legion_os::OsError::Unsupported { .. }) => Ok(false),
            Err(err) => Err(SyncError::from_os(Some(clean), err)),
        }
    }

    fn replace_live_with_tmp_backup(
        &self,
        tmp_name: &str,
        live_rel: &str,
        backup_tmp: &str,
    ) -> Result<bool> {
        let clean = validate_sync_path(live_rel)?;
        self.guard_platform_path(&clean, EntryKind::File)?;
        self.ensure_no_symlink_ancestors(&clean)?;
        let staged = self.resolve_tmp(tmp_name, false)?;
        let target = self.resolve_live(&clean, EntryKind::File)?;
        let backup = self.resolve_tmp(backup_tmp, true)?;
        match replace_with_backup(&staged, &target, &backup) {
            Ok(()) => {
                if let (Some(src_parent), Some(dst_parent)) = (staged.parent(), target.parent()) {
                    self.record_trace(RealFsDurabilityEvent::Exchange {
                        src_parent: src_parent.to_path_buf(),
                        dst_parent: dst_parent.to_path_buf(),
                    });
                }
                if let Some(parent) = target.parent() {
                    self.sync_dir(parent)?;
                }
                if let Some(src_parent) = staged.parent()
                    && target.parent() != Some(src_parent)
                {
                    self.sync_dir(src_parent)?;
                }
                Ok(true)
            }
            Err(legion_os::OsError::Unsupported { .. }) => Ok(false),
            Err(err) => Err(SyncError::from_os(Some(clean), err)),
        }
    }

    fn move_live_to_tmp_no_replace(&self, live_rel: &str, tmp_name: &str) -> Result<()> {
        let clean = validate_sync_path(live_rel)?;
        self.guard_platform_path(&clean, EntryKind::File)?;
        self.ensure_no_symlink_ancestors(&clean)?;
        let src = self.resolve_live(&clean, EntryKind::File)?;
        let dst = self.resolve_tmp(tmp_name, true)?;
        rename_no_replace(&src, &dst).map_err(|err| SyncError::from_os(Some(clean), err))?;
        if let (Some(src_parent), Some(dst_parent)) = (src.parent(), dst.parent()) {
            self.record_trace(RealFsDurabilityEvent::Rename {
                src_parent: src_parent.to_path_buf(),
                dst_parent: dst_parent.to_path_buf(),
            });
        }
        if let Some(dst_parent) = dst.parent() {
            self.sync_dir(dst_parent)?;
        }
        if let Some(parent) = src.parent()
            && dst.parent() != Some(parent)
        {
            self.sync_dir(parent)?;
        }
        Ok(())
    }

    fn move_tmp_to_bin_no_replace(&self, tmp_name: &str, bin_data_name: &str) -> Result<()> {
        let src = self.resolve_tmp(tmp_name, false)?;
        let dst = self.resolve_bin(bin_data_name)?;
        self.move_file_no_replace_cross_device_safe(
            &src,
            &dst,
            &format!("{SYNC_BIN_DIR}/{bin_data_name}"),
        )
    }

    fn move_live_to_bin_no_replace(&self, live_rel: &str, bin_data_name: &str) -> Result<()> {
        let clean = validate_sync_path(live_rel)?;
        self.guard_platform_path(&clean, EntryKind::File)?;
        self.ensure_no_symlink_ancestors(&clean)?;
        let src = self.resolve_live(&clean, EntryKind::File)?;
        let dst = self.resolve_bin(bin_data_name)?;
        self.move_file_no_replace_cross_device_safe(&src, &dst, &clean)
    }

    fn write_bin_meta(&self, meta_name: &str, bytes: &[u8]) -> Result<()> {
        let dst = self.resolve_bin(meta_name)?;
        write_atomic(&dst, bytes, WriteOptions::with_mode(0o600))
            .map_err(|err| SyncError::from_os(Some(format!("{SYNC_BIN_DIR}/{meta_name}")), err))?;
        self.record_trace(RealFsDurabilityEvent::WriteFileSync {
            parent: self.bin_dir(),
            path: dst,
        });
        self.sync_dir(&self.bin_dir())?;
        Ok(())
    }

    fn read_bin_meta(&self, meta_name: &str) -> Result<Option<Vec<u8>>> {
        let full = self.resolve_bin(meta_name)?;
        match fs::read(&full) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(err) => Err(SyncError::from_io(
                Some(format!("{SYNC_BIN_DIR}/{meta_name}")),
                err,
            )),
        }
    }

    fn stat_bin(&self, bin_name: &str) -> Result<Option<FsMeta>> {
        let full = self.resolve_bin(bin_name)?;
        stat_path(&full, &format!("{SYNC_BIN_DIR}/{bin_name}"))
    }

    fn hash_bin(&self, bin_name: &str) -> Result<(Hash, u64)> {
        let full = self.resolve_bin(bin_name)?;
        let mut file = File::open(&full)
            .map_err(|e| SyncError::from_io(Some(format!("{SYNC_BIN_DIR}/{bin_name}")), e))?;
        hash_reader(&mut file).map_err(SyncError::from)
    }

    fn copy_bin_to_tmp(&self, bin_data_name: &str, tmp_name: &str) -> Result<()> {
        let src_path = self.resolve_bin(bin_data_name)?;
        let dst_path = self.resolve_tmp(tmp_name, true)?;
        let label = Self::tmp_label(tmp_name);
        let mut src = File::open(&src_path)
            .map_err(|e| SyncError::from_io(Some(format!("{SYNC_BIN_DIR}/{bin_data_name}")), e))?;
        let mut dst = create_private_file(&dst_path)
            .map_err(|e| SyncError::from_io(Some(label.clone()), e))?;
        io::copy(&mut src, &mut dst).map_err(|e| SyncError::from_io(Some(label.clone()), e))?;
        dst.flush()
            .map_err(|e| SyncError::from_io(Some(label.clone()), e))?;
        dst.sync_all()
            .map_err(|e| SyncError::from_io(Some(label), e))?;
        Ok(())
    }

    fn move_bin_to_live_no_replace(&self, bin_data_name: &str, live_rel: &str) -> Result<()> {
        let clean = validate_sync_path(live_rel)?;
        self.guard_platform_path(&clean, EntryKind::File)?;
        let _parent = self.ensure_parent_dirs(&clean)?;
        let src = self.resolve_bin(bin_data_name)?;
        let dst = self.resolve_live(&clean, EntryKind::File)?;
        self.move_file_no_replace_cross_device_safe(&src, &dst, &clean)
    }

    fn remove_bin_file(&self, bin_name: &str) -> Result<()> {
        let full = self.resolve_bin(bin_name)?;
        match fs::remove_file(&full) {
            Ok(()) => {
                self.record_trace(RealFsDurabilityEvent::Remove {
                    parent: self.bin_dir(),
                    path: full,
                });
                self.sync_dir(&self.bin_dir())?;
                Ok(())
            }
            Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(err) => Err(SyncError::from_io(
                Some(format!("{SYNC_BIN_DIR}/{bin_name}")),
                err,
            )),
        }
    }

    fn list_bin(&self) -> Result<Vec<String>> {
        list_flat_dir(&self.bin_dir(), SYNC_BIN_DIR)
    }
}

fn find_nested_sync_dir(dir: &Path, is_root: bool) -> Option<PathBuf> {
    let rd = fs::read_dir(dir).ok()?;
    for item in rd.flatten() {
        let name = item.file_name();
        let name_str = name.to_string_lossy();
        let Ok(ft) = item.file_type() else {
            continue;
        };
        if ft.is_symlink() || !ft.is_dir() {
            continue;
        }
        if is_dot_legion_sync_segment(&name_str) {
            if is_root {
                // Root's own `.legion-sync` is expected; do not descend into it.
                continue;
            }
            return Some(dir.to_path_buf());
        }
        if let Some(found) = find_nested_sync_dir(&item.path(), false) {
            return Some(found);
        }
    }
    None
}

fn walk_dir(
    dir: &Path,
    prefix: &str,
    ignore: &IgnoreMatcher,
    entries: &mut Vec<FsWalkEntry>,
    skipped: &mut Vec<(String, String)>,
) -> Result<()> {
    let rd = match fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(err) if prefix.is_empty() => {
            return Err(SyncError::from_io(Some(dir.display().to_string()), err));
        }
        Err(err) => {
            skipped.push((prefix.to_string(), err.to_string()));
            return Ok(());
        }
    };

    let mut children = Vec::new();
    for item in rd {
        match item {
            Ok(entry) => children.push((entry.file_name(), entry.path())),
            Err(err) => skipped.push((prefix.to_string(), err.to_string())),
        }
    }
    children.sort_by(|a, b| a.0.cmp(&b.0));

    for (name_os, full_path) in children {
        let Some(name) = name_os.to_str() else {
            let rel = join_rel(prefix, &name_os.to_string_lossy());
            skipped.push((rel, "non-UTF-8 file name".to_string()));
            continue;
        };
        if is_dot_legion_sync_segment(name) {
            continue;
        }
        let rel = join_rel(prefix, name);
        let meta = match fs::symlink_metadata(&full_path) {
            Ok(m) => m,
            Err(err) => {
                skipped.push((rel, err.to_string()));
                continue;
            }
        };

        let ft = meta.file_type();
        let is_dir = ft.is_dir();
        if ignore.is_ignored(&rel, name, is_dir) {
            continue;
        }

        if is_dir {
            entries.push(FsWalkEntry {
                path: rel.clone(),
                meta: FsMeta {
                    kind: EntryKind::Dir,
                    size: 0,
                    mtime: metadata_mtime(&meta),
                    mode: entry_mode(&meta),
                    link_target: None,
                    change_id: file_change_id(&full_path, &meta),
                    file_identity: file_identity(&meta),
                },
            });
            walk_dir(&full_path, &rel, ignore, entries, skipped)?;
        } else if ft.is_symlink() {
            match fs::read_link(&full_path) {
                Ok(target) => match target.to_str() {
                    Some(t) => entries.push(FsWalkEntry {
                        path: rel,
                        meta: FsMeta {
                            kind: EntryKind::Symlink,
                            size: 0,
                            mtime: metadata_mtime(&meta),
                            mode: entry_mode(&meta),
                            link_target: Some(t.to_string()),
                            change_id: file_change_id(&full_path, &meta),
                            file_identity: file_identity(&meta),
                        },
                    }),
                    None => skipped.push((rel, "non-UTF-8 symbolic link target".to_string())),
                },
                Err(err) => skipped.push((rel, err.to_string())),
            }
        } else if ft.is_file() {
            entries.push(FsWalkEntry {
                path: rel,
                meta: FsMeta {
                    kind: EntryKind::File,
                    size: meta.len(),
                    mtime: metadata_mtime(&meta),
                    mode: entry_mode(&meta),
                    link_target: None,
                    change_id: file_change_id(&full_path, &meta),
                    file_identity: file_identity(&meta),
                },
            });
        } else {
            skipped.push((rel, "unsupported special file type".to_string()));
        }
    }

    Ok(())
}

fn stat_path(full: &Path, label: &str) -> Result<Option<FsMeta>> {
    let meta = match fs::symlink_metadata(full) {
        Ok(m) => m,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(SyncError::from_io(Some(label.to_string()), err)),
    };
    let ft = meta.file_type();
    if ft.is_dir() {
        Ok(Some(FsMeta {
            kind: EntryKind::Dir,
            size: 0,
            mtime: metadata_mtime(&meta),
            mode: entry_mode(&meta),
            link_target: None,
            change_id: file_change_id(full, &meta),
            file_identity: file_identity(&meta),
        }))
    } else if ft.is_symlink() {
        let target = fs::read_link(full).map_err(|e| SyncError::from_io(Some(label.into()), e))?;
        let Some(target_str) = target.to_str() else {
            return Err(SyncError::InvalidPath {
                path: label.to_string(),
                reason: "non-UTF-8 symbolic link target".to_string(),
            });
        };
        Ok(Some(FsMeta {
            kind: EntryKind::Symlink,
            size: 0,
            mtime: metadata_mtime(&meta),
            mode: entry_mode(&meta),
            link_target: Some(target_str.to_string()),
            change_id: file_change_id(full, &meta),
            file_identity: file_identity(&meta),
        }))
    } else if ft.is_file() {
        Ok(Some(FsMeta {
            kind: EntryKind::File,
            size: meta.len(),
            mtime: metadata_mtime(&meta),
            mode: entry_mode(&meta),
            link_target: None,
            change_id: file_change_id(full, &meta),
            file_identity: file_identity(&meta),
        }))
    } else {
        Err(SyncError::Unsupported {
            reason: format!("'{label}' is a special file type that cannot be synced"),
            hint: Some("Add it to the folder's ignore list.".to_string()),
        })
    }
}

fn list_flat_dir(dir: &Path, label: &str) -> Result<Vec<String>> {
    let rd = match fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(SyncError::from_io(Some(label.to_string()), err)),
    };
    let mut out = Vec::new();
    for item in rd {
        let entry = item.map_err(|e| SyncError::from_io(Some(label.to_string()), e))?;
        if let Some(name) = entry.file_name().to_str() {
            out.push(name.to_string());
        }
    }
    out.sort();
    Ok(out)
}

fn join_rel(prefix: &str, name: &str) -> String {
    if prefix.is_empty() {
        name.to_string()
    } else {
        format!("{prefix}/{name}")
    }
}

fn metadata_mtime(meta: &fs::Metadata) -> Timestamp {
    match meta.modified() {
        Ok(time) => Timestamp::from_system_time(time),
        Err(_) => Timestamp::new(0, 0),
    }
}

fn timestamp_to_system_time(ts: Timestamp) -> Option<SystemTime> {
    if ts.secs >= 0 {
        UNIX_EPOCH.checked_add(Duration::new(ts.secs as u64, ts.nanos))
    } else {
        let abs_secs = ts.secs.checked_neg()? as u64;
        if ts.nanos == 0 {
            UNIX_EPOCH.checked_sub(Duration::new(abs_secs, 0))
        } else {
            let sub_secs = abs_secs.saturating_sub(1);
            let sub_nanos = 1_000_000_000u32.saturating_sub(ts.nanos);
            UNIX_EPOCH.checked_sub(Duration::new(sub_secs, sub_nanos))
        }
    }
}

fn detect_capabilities(root: &Path) -> FsCapabilities {
    // Probe in `root` (or nearest existing ancestor) without creating `.legion-sync/`.
    let probe_dir = if root.is_dir() {
        Some(root)
    } else {
        root.parent().filter(|p| p.is_dir())
    };
    let case_insensitive = match probe_dir {
        Some(dir) => probe_case_insensitive(dir),
        None => cfg!(any(windows, target_os = "macos")),
    };
    #[cfg(windows)]
    {
        let root_len = root.to_string_lossy().len();
        FsCapabilities {
            case_insensitive,
            unicode_normalizing: false,
            windows_naming: true,
            supports_symlinks: false,
            max_segment_len: 255,
            max_path_len: 260usize.saturating_sub(root_len.saturating_add(2)).max(64),
        }
    }
    #[cfg(not(windows))]
    {
        let max_path_len = if cfg!(target_os = "macos") {
            1024
        } else {
            4096
        };
        FsCapabilities {
            case_insensitive,
            unicode_normalizing: cfg!(target_os = "macos"),
            windows_naming: false,
            supports_symlinks: true,
            max_segment_len: 255,
            max_path_len,
        }
    }
}

fn probe_case_insensitive(dir: &Path) -> bool {
    let pid = std::process::id();
    let seq = REAL_TMP_SEQ.fetch_add(1, Ordering::Relaxed);
    let upper = dir.join(format!(".legion-CaseProbe-{pid}-{seq}.tmp"));
    let lower = dir.join(format!(".legion-caseprobe-{pid}-{seq}.tmp"));
    let _ = fs::remove_file(&upper);
    let _ = fs::remove_file(&lower);
    let created = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&upper)
        .is_ok();
    if !created {
        return cfg!(any(windows, target_os = "macos"));
    }
    let insensitive = fs::symlink_metadata(&lower).is_ok();
    let _ = fs::remove_file(&upper);
    insensitive
}

#[cfg(unix)]
fn file_identity(meta: &fs::Metadata) -> Option<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;
    Some((meta.dev(), meta.ino()))
}

#[cfg(not(unix))]
fn file_identity(_meta: &fs::Metadata) -> Option<(u64, u64)> {
    None
}

#[cfg(unix)]
fn entry_mode(meta: &fs::Metadata) -> Option<u32> {
    use std::os::unix::fs::PermissionsExt;
    Some(meta.permissions().mode() & 0o777)
}

#[cfg(not(unix))]
fn entry_mode(_meta: &fs::Metadata) -> Option<u32> {
    None
}

#[cfg(unix)]
fn create_private_file(path: &Path) -> io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt;
    OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
}

#[cfg(not(unix))]
fn create_private_file(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(path)
}

#[cfg(unix)]
fn set_unix_mode(path: &Path, mode: u32, label: &str) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(mode & 0o777))
        .map_err(|e| SyncError::from_io(Some(label.to_string()), e))
}

#[cfg(not(unix))]
fn set_unix_mode(_path: &Path, _mode: u32, _label: &str) -> Result<()> {
    Ok(())
}

#[cfg(unix)]
fn sync_dir_os(dir: &Path) -> Result<()> {
    let handle =
        File::open(dir).map_err(|e| SyncError::from_io(Some(dir.display().to_string()), e))?;
    handle
        .sync_all()
        .map_err(|e| SyncError::from_io(Some(dir.display().to_string()), e))
}

#[cfg(not(unix))]
fn sync_dir_os(_dir: &Path) -> Result<()> {
    // Windows NTFS/ReFS does not allow ordinary user processes to call FlushFileBuffers on a
    // directory handle without SeBackupPrivilege; directory durability on Windows relies on
    // `MOVEFILE_WRITE_THROUGH` on `MoveFileExW` and `FlushFileBuffers` (`sync_all`) on files.
    Ok(())
}

/// Exercises [`RealFs`] on `scratch_dir` across every mutating operation and checks that every
/// mutated directory has a subsequent [`RealFsDurabilityEvent::SyncDir`].
///
/// Removing a `sync_dir` call from `RealFs` (for example in `ensure_parent_dirs`, `remove_tmp`, or
/// `write_bin_meta`) causes this audit to report unsynced operations and flips the corresponding
/// durability flags to `false`.
pub fn audit_real_fs_durability(scratch_dir: &Path) -> Result<RealFsDurabilityAudit> {
    let mut fs = RealFs::open(scratch_dir)?;
    fs.enable_durability_trace();

    let mut unsynced_ops = Vec::new();
    let check_trace = |label: &str, events: &[RealFsDurabilityEvent], out: &mut Vec<String>| {
        let mut dirty: std::collections::BTreeSet<PathBuf> = std::collections::BTreeSet::new();
        for ev in events {
            match ev {
                RealFsDurabilityEvent::CreateDir { parent, .. }
                | RealFsDurabilityEvent::WriteFileSync { parent, .. }
                | RealFsDurabilityEvent::Remove { parent, .. } => {
                    dirty.insert(parent.clone());
                }
                RealFsDurabilityEvent::Rename {
                    src_parent,
                    dst_parent,
                }
                | RealFsDurabilityEvent::Exchange {
                    src_parent,
                    dst_parent,
                } => {
                    dirty.insert(src_parent.clone());
                    dirty.insert(dst_parent.clone());
                }
                RealFsDurabilityEvent::SyncDir { dir } => {
                    dirty.remove(dir);
                }
            }
        }
        for dir in dirty {
            out.push(format!("{label}: missing sync_dir({})", dir.display()));
        }
    };

    // 1. Internal dirs and marker.
    fs.ensure_internal_dirs()?;
    check_trace(
        "ensure_internal_dirs",
        &fs.take_durability_trace(),
        &mut unsynced_ops,
    );

    let marker = FolderMarker::new("audit".into(), "tower".into(), 1);
    fs.write_marker(&marker)?;
    check_trace(
        "write_marker",
        &fs.take_durability_trace(),
        &mut unsynced_ops,
    );

    // 2. Multi-level nested directory creation (`ensure_parent_dirs` must sync EVERY ancestor!).
    fs.create_dir_all_live("a/b/c")?;
    let nested_events = fs.take_durability_trace();
    let before_anc = unsynced_ops.len();
    check_trace(
        "create_dir_all_live(a/b/c)",
        &nested_events,
        &mut unsynced_ops,
    );
    let mut sync_created_ancestors = unsynced_ops.len() == before_anc;

    // 3. Staged file commit into fresh nested directory (`commit_tmp_to_live_no_replace`).
    {
        let mut w = fs.create_tmp("audit_t1")?;
        w.write_all(b"payload1")
            .map_err(|e| SyncError::from_io(Some("audit_t1".into()), e))?;
        w.finish_sync()?;
    }
    fs.commit_tmp_to_live_no_replace("audit_t1", "d1/d2/file1.txt")?;
    let commit_events = fs.take_durability_trace();
    let before_commit = unsynced_ops.len();
    check_trace(
        "commit_tmp_to_live_no_replace(d1/d2/file1.txt)",
        &commit_events,
        &mut unsynced_ops,
    );
    if unsynced_ops.len() != before_commit {
        sync_created_ancestors = false;
    }

    // 4. Deferred dir sync group-commit path: newly created ancestors MUST be synced immediately
    //    inside `ensure_parent_dirs` even before `sync_parent_dir_live` runs!
    {
        let mut w = fs.create_tmp("audit_t2")?;
        w.write_all(b"payload2")
            .map_err(|e| SyncError::from_io(Some("audit_t2".into()), e))?;
        w.finish_sync()?;
    }
    fs.commit_tmp_to_live_no_replace_deferred_dir_sync("audit_t2", "e1/e2/file2.txt")?;
    let def_events = fs.take_durability_trace();
    // Check that every CreateDir parent inside deferred commit was synced immediately.
    let mut dirty_create_parents: std::collections::BTreeSet<PathBuf> =
        std::collections::BTreeSet::new();
    for ev in &def_events {
        match ev {
            RealFsDurabilityEvent::CreateDir { parent, .. } => {
                dirty_create_parents.insert(parent.clone());
            }
            RealFsDurabilityEvent::SyncDir { dir } => {
                dirty_create_parents.remove(dir);
            }
            _ => {}
        }
    }
    if !dirty_create_parents.is_empty() {
        sync_created_ancestors = false;
        for dir in dirty_create_parents {
            unsynced_ops.push(format!(
                "commit_tmp_to_live_no_replace_deferred_dir_sync: missing immediate ancestor sync_dir({})",
                dir.display()
            ));
        }
    }
    fs.sync_parent_dir_live("e1/e2/file2.txt")?;
    fs.sync_tmp_dir()?;
    let mut combined_def = def_events;
    combined_def.extend(fs.take_durability_trace());
    check_trace(
        "commit_tmp_deferred+flush",
        &combined_def,
        &mut unsynced_ops,
    );

    // 5. `remove_tmp` must sync `.legion-sync/tmp`.
    {
        let mut w = fs.create_tmp("audit_rm")?;
        w.write_all(b"temp")
            .map_err(|e| SyncError::from_io(Some("audit_rm".into()), e))?;
        w.finish_sync()?;
    }
    let _ = fs.take_durability_trace();
    fs.remove_tmp("audit_rm")?;
    let rm_events = fs.take_durability_trace();
    let before_rm = unsynced_ops.len();
    check_trace("remove_tmp", &rm_events, &mut unsynced_ops);
    let sync_remove_tmp = unsynced_ops.len() == before_rm;

    // 6. Exchange / claim / bin / live rename / remove_empty_dir.
    {
        let mut w = fs.create_tmp("audit_ex")?;
        w.write_all(b"replacement")
            .map_err(|e| SyncError::from_io(Some("audit_ex".into()), e))?;
        w.finish_sync()?;
    }
    let _ = fs.take_durability_trace();
    if fs.exchange_tmp_with_live("audit_ex", "d1/d2/file1.txt")? {
        check_trace(
            "exchange_tmp_with_live",
            &fs.take_durability_trace(),
            &mut unsynced_ops,
        );
        fs.move_tmp_to_bin_no_replace("audit_ex", "b1.data")?;
        check_trace(
            "move_tmp_to_bin_no_replace",
            &fs.take_durability_trace(),
            &mut unsynced_ops,
        );
    } else {
        fs.remove_tmp("audit_ex")?;
        let _ = fs.take_durability_trace();
        fs.move_live_to_tmp_no_replace("d1/d2/file1.txt", "audit_claim")?;
        check_trace(
            "move_live_to_tmp_no_replace",
            &fs.take_durability_trace(),
            &mut unsynced_ops,
        );
        fs.move_tmp_to_bin_no_replace("audit_claim", "b1.data")?;
        check_trace(
            "move_tmp_to_bin_no_replace",
            &fs.take_durability_trace(),
            &mut unsynced_ops,
        );
    }

    fs.write_bin_meta("b1.json", b"{}")?;
    check_trace(
        "write_bin_meta",
        &fs.take_durability_trace(),
        &mut unsynced_ops,
    );

    fs.move_bin_to_live_no_replace("b1.data", "restored/r1/file.txt")?;
    check_trace(
        "move_bin_to_live_no_replace(restored/r1/file.txt)",
        &fs.take_durability_trace(),
        &mut unsynced_ops,
    );

    fs.remove_bin_file("b1.json")?;
    check_trace(
        "remove_bin_file",
        &fs.take_durability_trace(),
        &mut unsynced_ops,
    );

    fs.rename_live_to_live_no_replace("restored/r1/file.txt", "moved/m1/file.txt")?;
    check_trace(
        "rename_live_to_live_no_replace",
        &fs.take_durability_trace(),
        &mut unsynced_ops,
    );

    fs.remove_empty_dir_live("a/b/c")?;
    check_trace(
        "remove_empty_dir_live",
        &fs.take_durability_trace(),
        &mut unsynced_ops,
    );

    Ok(RealFsDurabilityAudit {
        sync_created_ancestors,
        sync_remove_tmp,
        unsynced_ops,
    })
}

#[cfg(unix)]
fn create_symlink_no_replace(target: &Path, dest: &Path, label: &str) -> Result<()> {
    // `std::os::unix::fs::symlink` fails atomically with `EEXIST` if `dest` already exists.
    std::os::unix::fs::symlink(target, dest)
        .map_err(|err| SyncError::from_io(Some(label.to_string()), err))
}

#[cfg(not(unix))]
fn create_symlink_no_replace(_target: &Path, _dest: &Path, label: &str) -> Result<()> {
    Err(SyncError::Unsupported {
        reason: format!("'{label}' is a symbolic link, which is held back on Windows"),
        hint: Some(
            "Symbolic links sync between Unix machines and are held back on Windows.".into(),
        ),
    })
}
