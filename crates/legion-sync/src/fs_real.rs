//! Real-disk [`SyncFs`] implementation over `std::fs`, `legion-xfer`, and `legion-os`.

use std::fs::{self, File, FileTimes, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use legion_os::atomic::{WriteOptions, replace_file, write_atomic};
use legion_xfer::hash::hash_reader;
use legion_xfer::{EntryKind, Hash, Timestamp};

use crate::error::{FsErrorKind, Result, SyncError};
use crate::path::{
    IgnoreMatcher, SYNC_BIN_DIR, SYNC_META_DIR, SYNC_TMP_DIR, ancestors,
    validate_flat_internal_name, validate_sync_path,
};
use crate::traits::{FsCapabilities, FsMeta, FsWalkEntry, FsWalkResult, SyncFs, SyncWrite};

static REAL_TMP_SEQ: AtomicU64 = AtomicU64::new(0);

/// Real filesystem backend rooted at one local sync folder directory.
///
/// Enforces root containment on every live operation: paths are validated, intermediate symlinks
/// are refused so nothing ever traverses outside the sync root, and `.legion-sync/tmp` and
/// `.legion-sync/bin` are accessed only through flat internal names.
#[derive(Debug, Clone)]
pub struct RealFs {
    root: PathBuf,
    caps: FsCapabilities,
}

impl RealFs {
    /// Opens `root` as a sync folder, creating `root`, `.legion-sync/tmp`, and `.legion-sync/bin`
    /// if missing and probing the filesystem's case sensitivity.
    pub fn open(root: impl AsRef<Path>) -> Result<Self> {
        let root = root.as_ref().to_path_buf();
        fs::create_dir_all(&root)
            .map_err(|err| SyncError::from_io(Some(root.display().to_string()), err))?;
        let tmp_dir = root.join(SYNC_TMP_DIR);
        let bin_dir = root.join(SYNC_BIN_DIR);
        fs::create_dir_all(&tmp_dir)
            .map_err(|err| SyncError::from_io(Some(tmp_dir.display().to_string()), err))?;
        fs::create_dir_all(&bin_dir)
            .map_err(|err| SyncError::from_io(Some(bin_dir.display().to_string()), err))?;

        let caps = detect_capabilities(&root);
        Ok(Self { root, caps })
    }

    /// Opens `root` with explicitly supplied [`FsCapabilities`] (useful for testing Windows or
    /// case-insensitive rules against a temporary Linux directory).
    pub fn open_with_capabilities(root: impl AsRef<Path>, caps: FsCapabilities) -> Result<Self> {
        let mut fs_handle = Self::open(root)?;
        fs_handle.caps = caps;
        Ok(fs_handle)
    }

    /// Returns the root directory path of this sync folder.
    pub fn root(&self) -> &Path {
        &self.root
    }

    fn tmp_dir(&self) -> PathBuf {
        self.root.join(SYNC_TMP_DIR)
    }

    fn bin_dir(&self) -> PathBuf {
        self.root.join(SYNC_BIN_DIR)
    }

    fn resolve_live(&self, rel_path: &str) -> Result<PathBuf> {
        let clean = validate_sync_path(rel_path)?;
        let mut out = self.root.clone();
        for seg in clean.split('/') {
            out.push(seg);
        }
        Ok(out)
    }

    /// Verifies that no strict ancestor of `rel_path` is a symbolic link.
    ///
    /// Returns `Ok(true)` if all existing ancestors are directories (so `rel_path` could exist),
    /// or `Ok(false)` if an existing ancestor is a regular file (meaning `rel_path` cannot exist).
    fn check_ancestors_not_symlink(&self, rel_path: &str) -> Result<bool> {
        let clean = validate_sync_path(rel_path)?;
        let mut current = self.root.clone();
        for anc in ancestors(&clean) {
            let seg = anc.rsplit_once('/').map_or(anc, |(_, s)| s);
            current.push(seg);
            match fs::symlink_metadata(&current) {
                Ok(meta) => {
                    if meta.file_type().is_symlink() {
                        let reason =
                            format!("ancestor '{anc}' is a symbolic link and cannot be traversed");
                        return Err(SyncError::InvalidPath {
                            path: clean,
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

    /// Verifies that no strict ancestor directory of `rel_path` is a symlink or non-directory.
    fn ensure_no_symlink_ancestors(&self, rel_path: &str) -> Result<()> {
        let clean = validate_sync_path(rel_path)?;
        let mut current = self.root.clone();
        for anc in ancestors(&clean) {
            let seg = anc.rsplit_once('/').map_or(anc, |(_, s)| s);
            current.push(seg);
            match fs::symlink_metadata(&current) {
                Ok(meta) => {
                    if meta.file_type().is_symlink() {
                        let reason =
                            format!("ancestor '{anc}' is a symbolic link and cannot be traversed");
                        return Err(SyncError::InvalidPath {
                            path: clean,
                            reason,
                        });
                    }
                    if !meta.file_type().is_dir() {
                        let reason = format!("ancestor '{anc}' is not a directory");
                        return Err(SyncError::Conflict {
                            path: clean,
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

    fn ensure_parent_dirs(&self, rel_path: &str) -> Result<PathBuf> {
        let clean = validate_sync_path(rel_path)?;
        self.ensure_no_symlink_ancestors(&clean)?;
        let mut current = self.root.clone();
        for anc in ancestors(&clean) {
            let seg = anc.rsplit_once('/').map_or(anc, |(_, s)| s);
            current.push(seg);
            match fs::symlink_metadata(&current) {
                Ok(meta) => {
                    if !meta.file_type().is_dir() {
                        let reason = format!("ancestor '{anc}' is not a directory");
                        return Err(SyncError::Conflict {
                            path: clean,
                            reason,
                        });
                    }
                }
                Err(err) if err.kind() == io::ErrorKind::NotFound => {
                    fs::create_dir(&current)
                        .map_err(|e| SyncError::from_io(Some(anc.to_string()), e))?;
                }
                Err(err) => return Err(SyncError::from_io(Some(anc.to_string()), err)),
            }
        }
        Ok(current)
    }

    fn resolve_tmp(&self, tmp_name: &str) -> Result<PathBuf> {
        validate_flat_internal_name(tmp_name)?;
        let dir = self.tmp_dir();
        fs::create_dir_all(&dir)
            .map_err(|err| SyncError::from_io(Some(SYNC_TMP_DIR.to_string()), err))?;
        Ok(dir.join(tmp_name))
    }

    fn resolve_bin(&self, bin_name: &str) -> Result<PathBuf> {
        validate_flat_internal_name(bin_name)?;
        let dir = self.bin_dir();
        fs::create_dir_all(&dir)
            .map_err(|err| SyncError::from_io(Some(SYNC_BIN_DIR.to_string()), err))?;
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
}

impl SyncFs for RealFs {
    fn capabilities(&self) -> FsCapabilities {
        self.caps
    }

    fn walk(&self, ignore: &IgnoreMatcher) -> Result<FsWalkResult> {
        let mut entries = Vec::new();
        let mut skipped = Vec::new();
        walk_dir(&self.root, "", ignore, &mut entries, &mut skipped)?;
        entries.sort_by(|a, b| a.path.cmp(&b.path));
        skipped.sort();
        Ok(FsWalkResult { entries, skipped })
    }

    fn stat_live(&self, rel_path: &str) -> Result<Option<FsMeta>> {
        let clean = validate_sync_path(rel_path)?;
        if !self.check_ancestors_not_symlink(&clean)? {
            return Ok(None);
        }
        let full = self.resolve_live(&clean)?;
        stat_path(&full, &clean)
    }

    fn hash_live(&self, rel_path: &str) -> Result<(Hash, u64)> {
        let clean = validate_sync_path(rel_path)?;
        self.ensure_no_symlink_ancestors(&clean)?;
        let full = self.resolve_live(&clean)?;
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

    fn open_live_read(&self, rel_path: &str) -> Result<Box<dyn Read + Send>> {
        let clean = validate_sync_path(rel_path)?;
        self.ensure_no_symlink_ancestors(&clean)?;
        let full = self.resolve_live(&clean)?;
        let meta =
            fs::symlink_metadata(&full).map_err(|e| SyncError::from_io(Some(clean.clone()), e))?;
        if !meta.file_type().is_file() {
            return Err(SyncError::Conflict {
                path: clean,
                reason: "expected a regular file to stream to peer".to_string(),
            });
        }
        let file = File::open(&full).map_err(|e| SyncError::from_io(Some(clean), e))?;
        Ok(Box::new(file))
    }

    fn create_dir_all_live(&self, rel_path: &str) -> Result<()> {
        let clean = validate_sync_path(rel_path)?;
        let parent = self.ensure_parent_dirs(&clean)?;
        let full = self.resolve_live(&clean)?;
        match fs::symlink_metadata(&full) {
            Ok(meta) if meta.file_type().is_dir() => Ok(()),
            Ok(_) => Err(SyncError::Conflict {
                path: clean,
                reason: "a non-directory entry already exists at this path".to_string(),
            }),
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                fs::create_dir(&full).map_err(|e| SyncError::from_io(Some(clean), e))?;
                sync_dir(&parent);
                Ok(())
            }
            Err(err) => Err(SyncError::from_io(Some(clean), err)),
        }
    }

    fn list_dir_live(&self, rel_path: &str) -> Result<Vec<String>> {
        let clean = validate_sync_path(rel_path)?;
        self.ensure_no_symlink_ancestors(&clean)?;
        let full = self.resolve_live(&clean)?;
        let mut names = Vec::new();
        for item in fs::read_dir(&full).map_err(|e| SyncError::from_io(Some(clean.clone()), e))? {
            let entry = item.map_err(|e| SyncError::from_io(Some(clean.clone()), e))?;
            names.push(entry.file_name().to_string_lossy().into_owned());
        }
        names.sort();
        Ok(names)
    }

    fn remove_empty_dir_live(&self, rel_path: &str) -> Result<()> {
        let clean = validate_sync_path(rel_path)?;
        self.ensure_no_symlink_ancestors(&clean)?;
        let full = self.resolve_live(&clean)?;
        fs::remove_dir(&full).map_err(|e| SyncError::from_io(Some(clean), e))?;
        if let Some(parent) = full.parent() {
            sync_dir(parent);
        }
        Ok(())
    }

    fn create_symlink_live(&self, rel_path: &str, target: &str) -> Result<()> {
        let clean = validate_sync_path(rel_path)?;
        let parent = self.ensure_parent_dirs(&clean)?;
        let full = self.resolve_live(&clean)?;
        create_symlink_atomic(Path::new(target), &full, &clean)?;
        sync_dir(&parent);
        Ok(())
    }

    fn remove_symlink_live(&self, rel_path: &str) -> Result<()> {
        let clean = validate_sync_path(rel_path)?;
        self.ensure_no_symlink_ancestors(&clean)?;
        let full = self.resolve_live(&clean)?;
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
            sync_dir(parent);
        }
        Ok(())
    }

    fn set_live_metadata(&self, rel_path: &str, mtime: Timestamp, mode: Option<u32>) -> Result<()> {
        let clean = validate_sync_path(rel_path)?;
        self.ensure_no_symlink_ancestors(&clean)?;
        let full = self.resolve_live(&clean)?;
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

    fn rename_live_to_live(&self, from_rel: &str, to_rel: &str) -> Result<()> {
        let clean_from = validate_sync_path(from_rel)?;
        let clean_to = validate_sync_path(to_rel)?;
        self.ensure_no_symlink_ancestors(&clean_from)?;
        let parent_to = self.ensure_parent_dirs(&clean_to)?;
        let full_from = self.resolve_live(&clean_from)?;
        let full_to = self.resolve_live(&clean_to)?;
        replace_file(&full_from, &full_to).map_err(|err| SyncError::Fs {
            path: Some(clean_to),
            detail: err.to_string(),
            kind: FsErrorKind::Other,
        })?;
        sync_dir(&parent_to);
        if let Some(parent_from) = full_from.parent() {
            sync_dir(parent_from);
        }
        Ok(())
    }

    fn create_tmp(&self, tmp_name: &str) -> Result<Box<dyn SyncWrite>> {
        let full = self.resolve_tmp(tmp_name)?;
        let file = create_private_file(&full)
            .map_err(|e| SyncError::from_io(Some(format!("{SYNC_TMP_DIR}/{tmp_name}")), e))?;
        Ok(Box::new(RealSyncWriter { path: full, file }))
    }

    fn stat_tmp(&self, tmp_name: &str) -> Result<Option<FsMeta>> {
        let full = self.resolve_tmp(tmp_name)?;
        stat_path(&full, &format!("{SYNC_TMP_DIR}/{tmp_name}"))
    }

    fn hash_tmp(&self, tmp_name: &str) -> Result<(Hash, u64)> {
        let full = self.resolve_tmp(tmp_name)?;
        let mut file = File::open(&full)
            .map_err(|e| SyncError::from_io(Some(format!("{SYNC_TMP_DIR}/{tmp_name}")), e))?;
        hash_reader(&mut file).map_err(SyncError::from)
    }

    fn remove_tmp(&self, tmp_name: &str) -> Result<()> {
        let full = self.resolve_tmp(tmp_name)?;
        match fs::remove_file(&full) {
            Ok(()) => Ok(()),
            Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(err) => Err(SyncError::from_io(
                Some(format!("{SYNC_TMP_DIR}/{tmp_name}")),
                err,
            )),
        }
    }

    fn list_tmp(&self) -> Result<Vec<String>> {
        list_flat_dir(&self.tmp_dir(), SYNC_TMP_DIR)
    }

    fn commit_tmp_to_live(&self, tmp_name: &str, live_rel: &str) -> Result<()> {
        let clean = validate_sync_path(live_rel)?;
        let parent = self.ensure_parent_dirs(&clean)?;
        let src = self.resolve_tmp(tmp_name)?;
        let dst = self.resolve_live(&clean)?;
        // Default readable permissions before publishing on Unix; caller may refine with set_live_metadata.
        set_unix_mode(&src, 0o644, &clean)?;
        replace_file(&src, &dst).map_err(|err| SyncError::Fs {
            path: Some(clean),
            detail: err.to_string(),
            kind: FsErrorKind::Other,
        })?;
        sync_dir(&parent);
        sync_dir(&self.tmp_dir());
        Ok(())
    }

    fn move_live_to_bin(&self, live_rel: &str, bin_data_name: &str) -> Result<()> {
        let clean = validate_sync_path(live_rel)?;
        self.ensure_no_symlink_ancestors(&clean)?;
        let src = self.resolve_live(&clean)?;
        let dst = self.resolve_bin(bin_data_name)?;
        replace_file(&src, &dst).map_err(|err| SyncError::Fs {
            path: Some(clean),
            detail: err.to_string(),
            kind: FsErrorKind::Other,
        })?;
        sync_dir(&self.bin_dir());
        if let Some(parent) = src.parent() {
            sync_dir(parent);
        }
        Ok(())
    }

    fn write_bin_meta(&self, meta_name: &str, bytes: &[u8]) -> Result<()> {
        let dst = self.resolve_bin(meta_name)?;
        write_atomic(&dst, bytes, WriteOptions::with_mode(0o600)).map_err(|err| SyncError::Fs {
            path: Some(format!("{SYNC_BIN_DIR}/{meta_name}")),
            detail: err.to_string(),
            kind: FsErrorKind::Other,
        })?;
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
        let dst_path = self.resolve_tmp(tmp_name)?;
        let mut src = File::open(&src_path)
            .map_err(|e| SyncError::from_io(Some(format!("{SYNC_BIN_DIR}/{bin_data_name}")), e))?;
        let mut dst = create_private_file(&dst_path)
            .map_err(|e| SyncError::from_io(Some(format!("{SYNC_TMP_DIR}/{tmp_name}")), e))?;
        io::copy(&mut src, &mut dst)
            .map_err(|e| SyncError::from_io(Some(format!("{SYNC_TMP_DIR}/{tmp_name}")), e))?;
        dst.flush()
            .map_err(|e| SyncError::from_io(Some(format!("{SYNC_TMP_DIR}/{tmp_name}")), e))?;
        dst.sync_all()
            .map_err(|e| SyncError::from_io(Some(format!("{SYNC_TMP_DIR}/{tmp_name}")), e))?;
        Ok(())
    }

    fn remove_bin_file(&self, bin_name: &str) -> Result<()> {
        let full = self.resolve_bin(bin_name)?;
        match fs::remove_file(&full) {
            Ok(()) => {
                sync_dir(&self.bin_dir());
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
        if prefix.is_empty() && name.eq_ignore_ascii_case(SYNC_META_DIR) {
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
                    change_id: disk_change_id(&meta),
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
                            change_id: disk_change_id(&meta),
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
                    change_id: disk_change_id(&meta),
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
            change_id: disk_change_id(&meta),
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
            change_id: disk_change_id(&meta),
        }))
    } else if ft.is_file() {
        Ok(Some(FsMeta {
            kind: EntryKind::File,
            size: meta.len(),
            mtime: metadata_mtime(&meta),
            mode: entry_mode(&meta),
            link_target: None,
            change_id: disk_change_id(&meta),
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
    let case_insensitive = probe_case_insensitive(&root.join(SYNC_TMP_DIR));
    #[cfg(windows)]
    {
        let root_len = root.to_string_lossy().len();
        FsCapabilities {
            case_insensitive,
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
            windows_naming: false,
            supports_symlinks: true,
            max_segment_len: 255,
            max_path_len,
        }
    }
}

fn probe_case_insensitive(tmp_dir: &Path) -> bool {
    let pid = std::process::id();
    let seq = REAL_TMP_SEQ.fetch_add(1, Ordering::Relaxed);
    let upper = tmp_dir.join(format!(".legion-CaseProbe-{pid}-{seq}.tmp"));
    let lower = tmp_dir.join(format!(".legion-caseprobe-{pid}-{seq}.tmp"));
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
fn entry_mode(meta: &fs::Metadata) -> Option<u32> {
    use std::os::unix::fs::PermissionsExt;
    Some(meta.permissions().mode() & 0o777)
}

#[cfg(not(unix))]
fn entry_mode(_meta: &fs::Metadata) -> Option<u32> {
    None
}

#[cfg(unix)]
fn disk_change_id(meta: &fs::Metadata) -> Option<u64> {
    use std::os::unix::fs::MetadataExt;
    let mut h = 0xcbf2_9ce4_8422_2325u64;
    for v in [
        meta.dev(),
        meta.ino(),
        meta.ctime() as u64,
        meta.ctime_nsec() as u64,
    ] {
        h ^= v;
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    Some(h)
}

#[cfg(not(unix))]
fn disk_change_id(_meta: &fs::Metadata) -> Option<u64> {
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
fn sync_dir(dir: &Path) {
    if let Ok(handle) = File::open(dir) {
        let _ = handle.sync_all();
    }
}

#[cfg(not(unix))]
fn sync_dir(_dir: &Path) {}

#[cfg(unix)]
fn create_symlink_atomic(target: &Path, dest: &Path, label: &str) -> Result<()> {
    let parent = dest.parent().unwrap_or_else(|| Path::new("."));
    let pid = std::process::id();
    loop {
        let seq = REAL_TMP_SEQ.fetch_add(1, Ordering::Relaxed);
        let tmp = parent.join(format!(".legion-link-{pid}-{seq}.tmp"));
        match std::os::unix::fs::symlink(target, &tmp) {
            Ok(()) => {
                if let Err(err) = replace_file(&tmp, dest) {
                    let _ = fs::remove_file(&tmp);
                    return Err(SyncError::Fs {
                        path: Some(label.to_string()),
                        detail: err.to_string(),
                        kind: FsErrorKind::Other,
                    });
                }
                return Ok(());
            }
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(err) => return Err(SyncError::from_io(Some(label.to_string()), err)),
        }
    }
}

#[cfg(not(unix))]
fn create_symlink_atomic(_target: &Path, _dest: &Path, label: &str) -> Result<()> {
    Err(SyncError::Unsupported {
        reason: format!("'{label}' is a symbolic link, which is held back on Windows"),
        hint: Some(
            "Symbolic links sync between Unix machines and are held back on Windows.".into(),
        ),
    })
}
