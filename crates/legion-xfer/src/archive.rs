//! Tar stream packing and hardened atomic unpacking.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, FileTimes};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tar::{Archive, Builder, EntryType, Header};

use crate::error::{Error, Result, classify_io};
use crate::manifest::{CaseCollision, EntryKind, SkippedEntry, Timestamp};
use crate::path::parse_relative_path_bytes;
use crate::platform;

/// Summary returned by [`pack`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PackReport {
    /// Regular files written into the tar stream.
    pub files: u64,
    /// Directory headers written into the tar stream.
    pub dirs: u64,
    /// Symlink headers written into the tar stream.
    pub symlinks: u64,
    /// Total file payload bytes written into the tar stream.
    pub bytes: u64,
}

/// Summary returned by [`unpack`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnpackReport {
    /// Relative paths of regular files (and hard links) written.
    pub files: Vec<String>,
    /// Relative paths of directories created or updated.
    pub dirs: Vec<String>,
    /// Relative paths of symlinks created.
    pub symlinks: Vec<String>,
    /// Total file payload bytes written.
    pub bytes: u64,
    /// Entries skipped without failing the unpack (such as symlinks on Windows).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skipped: Vec<SkippedEntry>,
    /// Entries in the archive whose paths differed only in letter case.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub case_collisions: Vec<CaseCollision>,
}

/// Writes a tar stream containing the exact relative `names` under `root` into `writer`.
///
/// Directory entries are packed as directory headers without recursing into their children so
/// incremental plans can send only newly added or changed paths.
pub fn pack<S, W>(root: &Path, names: &[S], writer: W) -> Result<PackReport>
where
    S: AsRef<str>,
    W: Write,
{
    let root_meta = fs::metadata(root).map_err(|e| classify_io(root, e))?;
    if !root_meta.is_dir() {
        return Err(Error::Conflict {
            path: root.to_path_buf(),
            reason: "pack root is not a directory".to_string(),
        });
    }

    let mut builder = Builder::new(writer);
    builder.follow_symlinks(false);

    let mut seen = BTreeSet::new();
    let mut report = PackReport::default();

    for raw_name in names {
        let name = raw_name.as_ref();
        let Some((rel, segments)) = parse_relative_path_bytes(name.as_bytes())? else {
            return Err(Error::UnsafeArchiveEntry {
                entry: name.to_string(),
                reason: "empty or current-directory path cannot be packed".to_string(),
            });
        };
        if !seen.insert(rel.clone()) {
            continue;
        }

        let mut full_path = root.to_path_buf();
        for seg in &segments {
            full_path.push(seg);
        }

        let meta = fs::symlink_metadata(&full_path).map_err(|e| classify_io(&full_path, e))?;
        let ft = meta.file_type();
        let mtime = Timestamp::from_metadata(&meta);

        let mut header = Header::new_gnu();

        if ft.is_file() {
            let file = File::open(&full_path).map_err(|e| classify_io(&full_path, e))?;
            let file_meta = file.metadata().map_err(|e| classify_io(&full_path, e))?;
            let len = file_meta.len();
            let file_mtime = Timestamp::from_metadata(&file_meta);
            set_header_mtime(&mut builder, &mut header, file_mtime)?;
            header.set_entry_type(EntryType::Regular);
            header.set_size(len);
            header.set_mode(platform::entry_mode(&file_meta).unwrap_or(0o644));
            header.set_cksum();
            append_exact_reader(&mut builder, &mut header, &rel, &full_path, file, len)?;
            report.files += 1;
            report.bytes += len;
        } else if ft.is_dir() {
            set_header_mtime(&mut builder, &mut header, mtime)?;
            header.set_entry_type(EntryType::Directory);
            header.set_size(0);
            header.set_mode(platform::entry_mode(&meta).unwrap_or(0o755));
            header.set_cksum();
            builder.append_data(&mut header, &rel, io::empty())?;
            report.dirs += 1;
        } else if ft.is_symlink() {
            let target = fs::read_link(&full_path).map_err(|e| classify_io(&full_path, e))?;
            set_header_mtime(&mut builder, &mut header, mtime)?;
            header.set_entry_type(EntryType::Symlink);
            header.set_size(0);
            header.set_mode(platform::entry_mode(&meta).unwrap_or(0o777));
            header.set_cksum();
            builder.append_link(&mut header, &rel, &target)?;
            report.symlinks += 1;
        } else {
            return Err(Error::Conflict {
                path: full_path,
                reason: "unsupported file type".to_string(),
            });
        }
    }

    builder.finish()?;
    let mut writer = builder.into_inner()?;
    writer.flush()?;

    Ok(report)
}

/// Safely extracts all entries from a tar stream in `reader` into `root`.
///
/// Convenience wrapper over [`unpack_filtered`] that accepts every valid entry.
pub fn unpack<R: Read>(root: &Path, reader: R) -> Result<UnpackReport> {
    unpack_impl(root, reader, false, |_, _| true)
}

/// Safely extracts a tar stream from `reader` into `root`, consulting `filter` before writing each
/// entry.
///
/// Refuses absolute paths, `..` traversal, backslashes, colons, Windows reserved names, hard links,
/// unsupported special file types, any entry whose ancestor symlink would escape `root`, and any
/// entry traversing a symlink created earlier in the same archive. `filter` receives the true
/// destination path relative to `root` after resolving existing ancestor symlinks so symlinks
/// inside `root` cannot bypass path filters. When `filter` returns `false`, the entry's bytes are
/// drained to keep the stream aligned, nothing is written for it, and it is recorded in
/// [`UnpackReport::skipped`]. Regular files are streamed to sibling temporary files, synced, and
/// renamed into place so an interrupted unpack never leaves a partial file at its destination path.
pub fn unpack_filtered<R, F>(root: &Path, reader: R, filter: F) -> Result<UnpackReport>
where
    R: Read,
    F: FnMut(&str, EntryKind) -> bool,
{
    unpack_impl(root, reader, true, filter)
}

fn unpack_impl<R, F>(root: &Path, reader: R, filtered: bool, mut filter: F) -> Result<UnpackReport>
where
    R: Read,
    F: FnMut(&str, EntryKind) -> bool,
{
    fs::create_dir_all(root).map_err(|e| classify_io(root, e))?;
    let canonical_root = fs::canonicalize(root).map_err(|e| classify_io(root, e))?;
    let case_insensitive = is_case_insensitive_dir(&canonical_root)?;

    let mut archive = Archive::new(reader);
    let entries = archive.entries()?;

    let mut report = UnpackReport::default();
    let mut seen_lower: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut written_exact: BTreeSet<String> = BTreeSet::new();
    let mut written_ancestors: BTreeSet<String> = BTreeSet::new();
    let mut written_symlinks: BTreeSet<String> = BTreeSet::new();
    let mut deferred_dirs: Vec<DeferredDir> = Vec::new();
    // Batch parent directory syncs per unique touched directory rather than calling fsync after
    // every small file, keeping durability before unpack returns while avoiding O(files) redundant
    // directory syncs.
    let mut dirs_to_sync: BTreeSet<PathBuf> = BTreeSet::new();

    for item in entries {
        let mut entry = item?;
        let entry_type = entry.header().entry_type();

        if matches!(entry_type, EntryType::XGlobalHeader | EntryType::XHeader) {
            continue;
        }

        let raw_path = entry.path_bytes();
        let parsed = parse_relative_path_bytes(raw_path.as_ref())?;
        let Some((rel, segments)) = parsed else {
            if entry_type == EntryType::Directory {
                continue;
            }
            return Err(Error::UnsafeArchiveEntry {
                entry: String::from_utf8_lossy(raw_path.as_ref()).into_owned(),
                reason: "non-directory entry targets archive root".to_string(),
            });
        };

        let kind = match entry_type {
            EntryType::Regular | EntryType::Continuous => EntryKind::File,
            EntryType::Directory => EntryKind::Dir,
            EntryType::Symlink => EntryKind::Symlink,
            EntryType::Link => {
                return Err(Error::UnsafeArchiveEntry {
                    entry: rel,
                    reason: "hard link entries are not allowed".to_string(),
                });
            }
            other => {
                return Err(Error::UnsafeArchiveEntry {
                    entry: rel,
                    reason: format!("unsupported archive entry type {other:?}"),
                });
            }
        };

        if filtered && has_ancestor_in(&segments, &written_symlinks) {
            return Err(Error::UnsafeArchiveEntry {
                entry: rel,
                reason: "entry traverses a symlink created earlier in this archive".to_string(),
            });
        }

        let effective_rel = resolve_effective_rel_path(root, &canonical_root, &segments, &rel)?;
        if filtered && has_path_ancestor_in(&effective_rel, &written_symlinks) {
            return Err(Error::UnsafeArchiveEntry {
                entry: rel,
                reason: "entry traverses a symlink created earlier in this archive".to_string(),
            });
        }

        if filtered && !filter(&effective_rel, kind) {
            let expected_size = entry.size();
            let drained = io::copy(&mut entry, &mut io::sink())?;
            if kind == EntryKind::File && drained != expected_size {
                return Err(Error::Io {
                    path: None,
                    source: io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        format!(
                            "truncated archive entry {rel:?}: expected {expected_size} bytes, got {drained}"
                        ),
                    ),
                });
            }
            report.skipped.push(SkippedEntry {
                path: effective_rel,
                reason: "skipped by entry filter".to_string(),
            });
            continue;
        }

        // Detect case-only collisions against entries already seen in this archive.
        let lower_key = rel.to_lowercase();
        let collides_existing = seen_lower
            .get(&lower_key)
            .is_some_and(|existing| !existing.contains(&rel));
        record_seen_with_ancestors(&mut seen_lower, &rel);

        if collides_existing && case_insensitive {
            report.skipped.push(SkippedEntry {
                path: rel,
                reason: "case collision with another entry on a case-insensitive filesystem"
                    .to_string(),
            });
            continue;
        }

        let (parent, final_seg) =
            ensure_safe_parent_dirs(root, &canonical_root, &segments, &rel, &mut dirs_to_sync)?;
        let dest = parent.join(final_seg);

        let mtime = entry_mtime(&mut entry);
        // Strip setuid/setgid/sticky bits from untrusted archive headers.
        let safe_mode = entry.header().mode().ok().map(|m| m & 0o777);

        match kind {
            EntryKind::Dir => {
                match fs::symlink_metadata(&dest) {
                    Ok(meta) if meta.file_type().is_dir() => {}
                    Ok(meta) if meta.file_type().is_symlink() => {
                        return Err(Error::UnsafeArchiveEntry {
                            entry: rel,
                            reason: "directory entry targets an existing symlink".to_string(),
                        });
                    }
                    Ok(_) => {
                        return Err(Error::Conflict {
                            path: dest,
                            reason: "non-directory exists at directory path".to_string(),
                        });
                    }
                    Err(err) if err.kind() == io::ErrorKind::NotFound => {
                        fs::create_dir(&dest).map_err(|e| classify_io(&dest, e))?;
                        dirs_to_sync.insert(parent.clone());
                    }
                    Err(err) => return Err(classify_io(&dest, err)),
                }
                deferred_dirs.push(DeferredDir {
                    segments,
                    mtime,
                    mode: safe_mode,
                });
                record_written(&mut written_exact, &mut written_ancestors, &rel);
                report.dirs.push(rel);
            }
            EntryKind::File => {
                if let Ok(meta) = fs::symlink_metadata(&dest)
                    && meta.file_type().is_dir()
                {
                    return Err(Error::Conflict {
                        path: dest,
                        reason: "directory exists at file path".to_string(),
                    });
                }

                let expected_size = entry.size();
                let (mut tmp_file, mut guard) = create_temp_file(&parent)?;
                let written = io::copy(&mut entry, &mut tmp_file)?;
                if written != expected_size {
                    return Err(Error::Io {
                        path: Some(dest),
                        source: io::Error::new(
                            io::ErrorKind::UnexpectedEof,
                            format!(
                                "truncated archive entry {rel:?}: expected {expected_size} bytes, got {written}"
                            ),
                        ),
                    });
                }
                tmp_file.flush().map_err(|e| classify_io(guard.path(), e))?;
                tmp_file
                    .sync_all()
                    .map_err(|e| classify_io(guard.path(), e))?;
                drop(tmp_file);

                if let Some(mode) = safe_mode {
                    platform::set_mode(guard.path(), mode)?;
                }
                if let Some(mtime) = mtime {
                    fs::set_times(guard.path(), FileTimes::new().set_modified(mtime))
                        .map_err(|e| classify_io(guard.path(), e))?;
                }

                platform::replace_file(guard.path(), &dest)?;
                guard.disarm();
                remove_stale_part_file(&dest);
                dirs_to_sync.insert(parent);

                record_written(&mut written_exact, &mut written_ancestors, &rel);
                report.files.push(rel);
                report.bytes += written;
            }
            EntryKind::Symlink => {
                if written_exact.contains(&rel) || written_ancestors.contains(&rel) {
                    return Err(Error::UnsafeArchiveEntry {
                        entry: rel,
                        reason: "symlink entry overwrites or shadows an already-written entry"
                            .to_string(),
                    });
                }

                let raw_target =
                    entry
                        .link_name_bytes()
                        .ok_or_else(|| Error::UnsafeArchiveEntry {
                            entry: rel.clone(),
                            reason: "symlink entry missing link target".to_string(),
                        })?;
                if raw_target.contains(&0) {
                    return Err(Error::UnsafeArchiveEntry {
                        entry: rel,
                        reason: "symlink target contains NUL byte".to_string(),
                    });
                }
                let target_str = std::str::from_utf8(raw_target.as_ref()).map_err(|_| {
                    Error::UnsafeArchiveEntry {
                        entry: rel.clone(),
                        reason: "symlink target is not valid UTF-8".to_string(),
                    }
                })?;

                if let Ok(meta) = fs::symlink_metadata(&dest)
                    && meta.file_type().is_dir()
                {
                    return Err(Error::Conflict {
                        path: dest,
                        reason: "directory exists at symlink path".to_string(),
                    });
                }

                if platform::create_symlink_atomic(Path::new(target_str), &dest)? {
                    remove_stale_part_file(&dest);
                    dirs_to_sync.insert(parent);
                    record_written(&mut written_exact, &mut written_ancestors, &rel);
                    written_symlinks.insert(rel.clone());
                    written_symlinks.insert(effective_rel);
                    report.symlinks.push(rel);
                } else {
                    report.skipped.push(SkippedEntry {
                        path: rel,
                        reason: "symlinks are skipped on Windows".to_string(),
                    });
                }
            }
        }
    }

    // Sync each unique modified parent directory once before applying restrictive directory modes.
    for dir in dirs_to_sync {
        platform::sync_dir(&dir)?;
    }

    // Apply directory mtimes and modes deepest-first so writing children never clobbers parent
    // timestamps and read-only parent directories do not block child extraction. Re-verify every
    // path component as a real directory inside `canonical_root` before touching metadata so a
    // symlink cannot redirect chmod or set_times outside the root.
    deferred_dirs.sort_by_key(|a| std::cmp::Reverse(a.segments.len()));
    for deferred in deferred_dirs {
        let Some(dir_path) = verify_real_dir_inside_root(root, &canonical_root, &deferred.segments)
        else {
            continue;
        };
        if let Some(mtime) = deferred.mtime {
            fs::set_times(&dir_path, FileTimes::new().set_modified(mtime))
                .map_err(|e| classify_io(&dir_path, e))?;
        }
        if let Some(m) = deferred.mode {
            platform::set_mode(&dir_path, m)?;
        }
    }

    report.case_collisions = seen_lower
        .into_iter()
        .filter_map(|(canonical, paths)| {
            if paths.len() > 1 {
                Some(CaseCollision {
                    canonical,
                    paths: paths.into_iter().collect(),
                })
            } else {
                None
            }
        })
        .collect();
    report.skipped.sort();

    Ok(report)
}

struct ExactRead<R> {
    inner: io::Take<R>,
    expected: u64,
    read: u64,
    shrank: bool,
}

impl<R: Read> Read for ExactRead<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read(buf)?;
        if n == 0 {
            if self.read < self.expected {
                self.shrank = true;
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    format!(
                        "file shrank while packing: expected {} bytes, read {}",
                        self.expected, self.read
                    ),
                ));
            }
            return Ok(0);
        }
        self.read += n as u64;
        Ok(n)
    }
}

/// Appends exactly `expected` bytes from `reader` into `builder`.
///
/// Caps reads at `expected` bytes so a growing file cannot desynchronize the tar stream, and
/// returns [`Error::Conflict`] if `reader` hits EOF before `expected` bytes are produced.
fn append_exact_reader<R: Read, W: Write>(
    builder: &mut Builder<W>,
    header: &mut Header,
    rel: &str,
    full_path: &Path,
    reader: R,
    expected: u64,
) -> Result<()> {
    let mut exact = ExactRead {
        inner: reader.take(expected),
        expected,
        read: 0,
        shrank: false,
    };
    match builder.append_data(header, rel, &mut exact) {
        Ok(()) => {
            if exact.read < expected {
                return Err(Error::Conflict {
                    path: full_path.to_path_buf(),
                    reason: format!(
                        "file shrank while packing: expected {expected} bytes, read {}",
                        exact.read
                    ),
                });
            }
            Ok(())
        }
        Err(_) if exact.shrank => Err(Error::Conflict {
            path: full_path.to_path_buf(),
            reason: format!(
                "file shrank while packing: expected {expected} bytes, read {}",
                exact.read
            ),
        }),
        Err(err) => Err(Error::from(err)),
    }
}

/// Writes `mtime` into `header`, emitting a PAX `mtime` record for pre-1970 timestamps that the
/// unsigned GNU tar header field cannot represent.
fn set_header_mtime<W: Write>(
    builder: &mut Builder<W>,
    header: &mut Header,
    mtime: Timestamp,
) -> Result<()> {
    if mtime.secs < 0 {
        let value = mtime.secs.to_string();
        builder.append_pax_extensions([("mtime", value.as_bytes())])?;
        header.set_mtime(0);
    } else {
        header.set_mtime(mtime.secs as u64);
    }
    Ok(())
}

/// Extracts an entry's modification time, preferring a PAX `mtime` extension (which carries signed
/// pre-1970 values) over the legacy unsigned header field.
fn entry_mtime<R: Read>(entry: &mut tar::Entry<'_, R>) -> Option<SystemTime> {
    if let Ok(Some(exts)) = entry.pax_extensions() {
        for ext in exts.flatten() {
            if ext.key() == Ok("mtime")
                && let Ok(val) = ext.value()
                && let Some(time) = parse_pax_mtime(val)
            {
                return Some(time);
            }
        }
    }
    entry
        .header()
        .mtime()
        .ok()
        .and_then(|secs| UNIX_EPOCH.checked_add(Duration::from_secs(secs)))
}

fn parse_pax_mtime(raw: &str) -> Option<SystemTime> {
    let trimmed = raw.trim();
    let (negative, unsigned) = match trimmed.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, trimmed),
    };
    let (secs_str, frac_str) = match unsigned.split_once('.') {
        Some((s, f)) => (s, Some(f)),
        None => (unsigned, None),
    };
    let secs: u64 = secs_str.parse().ok()?;
    let nanos: u32 = match frac_str {
        Some(f) if !f.is_empty() => {
            let mut padded = [b'0'; 9];
            for (slot, byte) in padded.iter_mut().zip(f.bytes()) {
                if !byte.is_ascii_digit() {
                    return None;
                }
                *slot = byte;
            }
            std::str::from_utf8(&padded).ok()?.parse().ok()?
        }
        _ => 0,
    };
    let delta = Duration::new(secs, nanos);
    if negative {
        UNIX_EPOCH.checked_sub(delta)
    } else {
        UNIX_EPOCH.checked_add(delta)
    }
}

/// Removes a stale `<name>.legion-part` file beside `dest` when `unpack` replaces `dest`.
fn remove_stale_part_file(dest: &Path) {
    if let Ok(partial) = crate::file::part_path(dest)
        && let Ok(meta) = fs::symlink_metadata(&partial)
        && meta.file_type().is_file()
    {
        let _ = fs::remove_file(&partial);
    }
}

struct DeferredDir {
    segments: Vec<String>,
    mtime: Option<SystemTime>,
    mode: Option<u32>,
}

fn record_written(exact: &mut BTreeSet<String>, ancestors: &mut BTreeSet<String>, rel: &str) {
    exact.insert(rel.to_string());
    let mut current = rel;
    while let Some((parent, _)) = current.rsplit_once('/') {
        if parent.is_empty() {
            break;
        }
        ancestors.insert(parent.to_string());
        current = parent;
    }
}

/// Re-verifies that every segment of `segments` beneath `root` resolves through real directories
/// (never symlinks) and that the canonical path stays inside `canonical_root`.
fn verify_real_dir_inside_root(
    root: &Path,
    canonical_root: &Path,
    segments: &[String],
) -> Option<PathBuf> {
    let mut cur = root.to_path_buf();
    for seg in segments {
        cur.push(seg);
        let meta = fs::symlink_metadata(&cur).ok()?;
        if !meta.file_type().is_dir() {
            return None;
        }
    }
    let canonical = fs::canonicalize(&cur).ok()?;
    if !canonical.starts_with(canonical_root) {
        return None;
    }
    Some(cur)
}

fn record_seen_with_ancestors(seen_lower: &mut BTreeMap<String, BTreeSet<String>>, path: &str) {
    let mut current = path;
    loop {
        seen_lower
            .entry(current.to_lowercase())
            .or_default()
            .insert(current.to_string());
        match current.rsplit_once('/') {
            Some((parent, _)) if !parent.is_empty() => current = parent,
            _ => break,
        }
    }
}

fn has_ancestor_in(segments: &[String], set: &BTreeSet<String>) -> bool {
    if segments.len() < 2 || set.is_empty() {
        return false;
    }
    let mut prefix = String::new();
    for seg in &segments[..segments.len() - 1] {
        if !prefix.is_empty() {
            prefix.push('/');
        }
        prefix.push_str(seg);
        if set.contains(&prefix) {
            return true;
        }
    }
    false
}

fn has_path_ancestor_in(path: &str, set: &BTreeSet<String>) -> bool {
    if set.is_empty() {
        return false;
    }
    let mut current = path;
    while let Some((parent, _)) = current.rsplit_once('/') {
        if parent.is_empty() {
            break;
        }
        if set.contains(parent) {
            return true;
        }
        current = parent;
    }
    false
}

/// Resolves the true relative path under `canonical_root` that `segments` will write to, following
/// any existing in-root ancestor symlinks without creating directories on disk.
fn resolve_effective_rel_path(
    root: &Path,
    canonical_root: &Path,
    segments: &[String],
    rel: &str,
) -> Result<String> {
    let Some((final_seg, parents)) = segments.split_last() else {
        return Err(Error::UnsafeArchiveEntry {
            entry: rel.to_string(),
            reason: "empty path".to_string(),
        });
    };

    let mut cur = root.to_path_buf();
    let mut uncreated_start = parents.len();

    for (idx, seg) in parents.iter().enumerate() {
        let candidate = cur.join(seg);
        match fs::symlink_metadata(&candidate) {
            Ok(meta) if meta.file_type().is_dir() => {
                cur = candidate;
            }
            Ok(meta) if meta.file_type().is_symlink() => {
                let resolved =
                    fs::canonicalize(&candidate).map_err(|_| Error::UnsafeArchiveEntry {
                        entry: rel.to_string(),
                        reason: "ancestor symlink cannot be resolved inside destination root"
                            .to_string(),
                    })?;
                if !resolved.starts_with(canonical_root) {
                    return Err(Error::UnsafeArchiveEntry {
                        entry: rel.to_string(),
                        reason: "ancestor symlink escapes destination root".to_string(),
                    });
                }
                if !resolved.is_dir() {
                    return Err(Error::UnsafeArchiveEntry {
                        entry: rel.to_string(),
                        reason: "ancestor symlink does not point to a directory".to_string(),
                    });
                }
                cur = resolved;
            }
            Ok(_) => {
                return Err(Error::Conflict {
                    path: candidate,
                    reason: "non-directory exists at ancestor path".to_string(),
                });
            }
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                uncreated_start = idx;
                break;
            }
            Err(err) => return Err(classify_io(&candidate, err)),
        }
    }

    let canonical_existing = fs::canonicalize(&cur).map_err(|e| classify_io(&cur, e))?;
    let rel_existing = canonical_existing
        .strip_prefix(canonical_root)
        .map_err(|_| Error::UnsafeArchiveEntry {
            entry: rel.to_string(),
            reason: "resolved parent directory escapes destination root".to_string(),
        })?;

    let mut effective_parts: Vec<String> = Vec::new();
    for comp in rel_existing.components() {
        let std::path::Component::Normal(os) = comp else {
            continue;
        };
        let s = os.to_str().ok_or_else(|| Error::UnsafeArchiveEntry {
            entry: rel.to_string(),
            reason: "resolved ancestor path is not valid UTF-8".to_string(),
        })?;
        effective_parts.push(s.to_string());
    }
    for seg in &parents[uncreated_start..] {
        effective_parts.push(seg.clone());
    }
    effective_parts.push(final_seg.clone());
    Ok(effective_parts.join("/"))
}

/// Ensures every ancestor component exists inside `canonical_root` without following any symlink
/// that escapes `canonical_root`.
fn ensure_safe_parent_dirs<'a>(
    root: &Path,
    canonical_root: &Path,
    segments: &'a [String],
    rel: &str,
    dirs_to_sync: &mut BTreeSet<PathBuf>,
) -> Result<(PathBuf, &'a str)> {
    let Some((final_seg, parents)) = segments.split_last() else {
        return Err(Error::UnsafeArchiveEntry {
            entry: rel.to_string(),
            reason: "empty path".to_string(),
        });
    };

    let mut cur = root.to_path_buf();
    for seg in parents {
        let parent_before = cur.clone();
        cur.push(seg);
        match fs::symlink_metadata(&cur) {
            Ok(meta) if meta.file_type().is_dir() => {}
            Ok(meta) if meta.file_type().is_symlink() => {
                let resolved = fs::canonicalize(&cur).map_err(|_| Error::UnsafeArchiveEntry {
                    entry: rel.to_string(),
                    reason: "ancestor symlink cannot be resolved inside destination root"
                        .to_string(),
                })?;
                if !resolved.starts_with(canonical_root) {
                    return Err(Error::UnsafeArchiveEntry {
                        entry: rel.to_string(),
                        reason: "ancestor symlink escapes destination root".to_string(),
                    });
                }
                if !resolved.is_dir() {
                    return Err(Error::UnsafeArchiveEntry {
                        entry: rel.to_string(),
                        reason: "ancestor symlink does not point to a directory".to_string(),
                    });
                }
            }
            Ok(_) => {
                return Err(Error::Conflict {
                    path: cur,
                    reason: "non-directory exists at ancestor path".to_string(),
                });
            }
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                fs::create_dir(&cur).map_err(|e| classify_io(&cur, e))?;
                dirs_to_sync.insert(parent_before);
            }
            Err(err) => return Err(classify_io(&cur, err)),
        }
    }

    let canonical_parent = fs::canonicalize(&cur).map_err(|e| classify_io(&cur, e))?;
    if !canonical_parent.starts_with(canonical_root) {
        return Err(Error::UnsafeArchiveEntry {
            entry: rel.to_string(),
            reason: "resolved parent directory escapes destination root".to_string(),
        });
    }

    Ok((cur, final_seg.as_str()))
}

/// Probes whether `dir` lives on a case-insensitive filesystem using an ephemeral mixed-case file.
fn is_case_insensitive_dir(dir: &Path) -> Result<bool> {
    let pid = std::process::id();
    loop {
        let seq = platform::next_temp_id();
        let probe_upper = dir.join(format!(".legion-CaseProbe-{pid}-{seq}.tmp"));
        let probe_lower = dir.join(format!(".legion-caseprobe-{pid}-{seq}.tmp"));
        match platform::create_new_private(&probe_upper) {
            Ok(file) => {
                drop(file);
                let insensitive = fs::symlink_metadata(&probe_lower).is_ok();
                let _ = fs::remove_file(&probe_upper);
                return Ok(insensitive);
            }
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(err) => return Err(classify_io(&probe_upper, err)),
        }
    }
}

fn unique_temp_path(parent: &Path) -> PathBuf {
    let pid = std::process::id();
    let seq = platform::next_temp_id();
    parent.join(format!(".legion-unpack-{pid}-{seq}.tmp"))
}

fn create_temp_file(parent: &Path) -> Result<(File, TempFileGuard)> {
    loop {
        let path = unique_temp_path(parent);
        match platform::create_new_private(&path) {
            Ok(file) => return Ok((file, TempFileGuard::new(path))),
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(err) => return Err(classify_io(&path, err)),
        }
    }
}

/// Deletes a temporary unpack file on drop unless disarmed after a successful rename.
struct TempFileGuard {
    path: Option<PathBuf>,
}

impl TempFileGuard {
    fn new(path: PathBuf) -> Self {
        Self { path: Some(path) }
    }

    fn path(&self) -> &Path {
        self.path.as_deref().unwrap_or_else(|| Path::new(""))
    }

    fn disarm(&mut self) {
        self.path = None;
    }
}

impl Drop for TempFileGuard {
    fn drop(&mut self) {
        if let Some(path) = self.path.take() {
            let _ = fs::remove_file(path);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;

    /// Writes a raw tar header with an arbitrary path string (bypassing `tar::Builder` safety
    /// checks so hostile archives can be tested directly).
    fn build_raw_tar(entries: &[(&str, EntryType, &[u8], Option<&str>)]) -> Vec<u8> {
        let mut out = Vec::new();
        for (path, entry_type, data, link_target) in entries {
            let mut header = Header::new_gnu();
            header.set_entry_type(*entry_type);
            header.set_size(data.len() as u64);
            header.set_mode(0o644);
            header.set_mtime(1_700_000_000);
            let raw_header = header.as_mut_bytes();
            let path_bytes = path.as_bytes();
            raw_header[..path_bytes.len()].copy_from_slice(path_bytes);
            if let Some(target) = link_target {
                let target_bytes = target.as_bytes();
                raw_header[157..157 + target_bytes.len()].copy_from_slice(target_bytes);
            }
            header.set_cksum();
            out.extend_from_slice(header.as_bytes());
            out.extend_from_slice(data);
            let rem = data.len() % 512;
            if rem != 0 {
                out.resize(out.len() + (512 - rem), 0);
            }
        }
        // Two 512-byte zero blocks terminate a tar stream.
        out.resize(out.len() + 1024, 0);
        out
    }

    #[test]
    fn pack_and_unpack_round_trip_with_unusual_names() {
        let src = tempfile::tempdir().unwrap();
        let dst = tempfile::tempdir().unwrap();

        let long_name = format!("deep/{}/file.txt", "a".repeat(120));
        fs::create_dir_all(src.path().join("empty dir")).unwrap();
        fs::create_dir_all(src.path().join("café/日本語")).unwrap();
        fs::create_dir_all(src.path().join("deep").join("a".repeat(120))).unwrap();
        fs::write(src.path().join("empty.txt"), b"").unwrap();
        fs::write(src.path().join("-leading-dash.txt"), b"dash content").unwrap();
        fs::write(src.path().join("café/日本語/hello world.txt"), b"unicode").unwrap();
        fs::write(src.path().join(&long_name), b"long path payload").unwrap();

        // Set a deterministic historical mtime so we can verify preservation.
        let fixed_time = UNIX_EPOCH + Duration::from_secs(1_700_000_123);
        fs::set_times(
            src.path().join("-leading-dash.txt"),
            FileTimes::new().set_modified(fixed_time),
        )
        .unwrap();

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(
                src.path().join("-leading-dash.txt"),
                fs::Permissions::from_mode(0o751),
            )
            .unwrap();
            std::os::unix::fs::symlink("-leading-dash.txt", src.path().join("sym")).unwrap();
        }

        let base_names = [
            "-leading-dash.txt".to_string(),
            "café".to_string(),
            "café/日本語".to_string(),
            "café/日本語/hello world.txt".to_string(),
            "deep".to_string(),
            format!("deep/{}", "a".repeat(120)),
            long_name.clone(),
            "empty dir".to_string(),
            "empty.txt".to_string(),
        ];
        #[cfg(unix)]
        let names: Vec<String> = base_names.into_iter().chain(["sym".to_string()]).collect();
        #[cfg(not(unix))]
        let names = base_names;

        let mut buf = Vec::new();
        let packed = pack(src.path(), &names, &mut buf).unwrap();
        assert_eq!(packed.files, 4);
        assert_eq!(packed.dirs, 5);
        #[cfg(unix)]
        assert_eq!(packed.symlinks, 1);

        let unpacked = unpack(dst.path(), Cursor::new(&buf)).unwrap();
        assert_eq!(unpacked.files.len(), 4);
        assert_eq!(unpacked.dirs.len(), 5);
        assert!(dst.path().join("empty dir").is_dir());
        assert_eq!(fs::read(dst.path().join("empty.txt")).unwrap(), b"");
        assert_eq!(
            fs::read(dst.path().join("-leading-dash.txt")).unwrap(),
            b"dash content"
        );
        assert_eq!(
            fs::read(dst.path().join("café/日本語/hello world.txt")).unwrap(),
            b"unicode"
        );
        assert_eq!(
            fs::read(dst.path().join(&long_name)).unwrap(),
            b"long path payload"
        );

        let unpacked_meta = fs::symlink_metadata(dst.path().join("-leading-dash.txt")).unwrap();
        assert_eq!(Timestamp::from_metadata(&unpacked_meta).secs, 1_700_000_123);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(unpacked_meta.permissions().mode() & 0o777, 0o751);
            assert_eq!(
                fs::read_link(dst.path().join("sym")).unwrap(),
                Path::new("-leading-dash.txt")
            );
        }
    }

    #[test]
    fn refuses_absolute_path_in_archive() {
        let dst = tempfile::tempdir().unwrap();
        let tar_bytes = build_raw_tar(&[("/etc/evil.txt", EntryType::Regular, b"nope", None)]);
        let err = unpack(dst.path(), Cursor::new(tar_bytes)).unwrap_err();
        assert!(matches!(err, Error::UnsafeArchiveEntry { .. }), "{err:?}");
    }

    #[test]
    fn refuses_parent_traversal_in_archive() {
        let dst = tempfile::tempdir().unwrap();
        for bad in ["../escape.txt", "a/../../escape.txt", "..\\escape.txt"] {
            let tar_bytes = build_raw_tar(&[(bad, EntryType::Regular, b"nope", None)]);
            let err = unpack(dst.path(), Cursor::new(tar_bytes)).unwrap_err();
            assert!(
                matches!(err, Error::UnsafeArchiveEntry { .. }),
                "expected UnsafeArchiveEntry for {bad:?}, got {err:?}"
            );
        }
    }

    #[test]
    fn refuses_colon_and_drive_prefixes_in_any_segment() {
        let dst = tempfile::tempdir().unwrap();
        let src = tempfile::tempdir().unwrap();
        for bad in ["C:evil.txt", "sub/C:evil.txt", "a/b:stream"] {
            let tar_bytes = build_raw_tar(&[(bad, EntryType::Regular, b"nope", None)]);
            let err = unpack(dst.path(), Cursor::new(tar_bytes)).unwrap_err();
            assert!(
                matches!(err, Error::UnsafeArchiveEntry { .. }),
                "expected unpack rejection for {bad:?}, got {err:?}"
            );

            let mut out = Vec::new();
            let pack_err = pack(src.path(), &[bad], &mut out).unwrap_err();
            assert!(
                matches!(pack_err, Error::UnsafeArchiveEntry { .. }),
                "expected pack rejection for {bad:?}, got {pack_err:?}"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn refuses_writing_through_escaping_symlink() {
        let outside = tempfile::tempdir().unwrap();
        let dst = tempfile::tempdir().unwrap();
        let target_str = outside.path().to_str().unwrap();

        let tar_bytes = build_raw_tar(&[
            ("escape", EntryType::Symlink, b"", Some(target_str)),
            ("escape/pwned.txt", EntryType::Regular, b"bad", None),
        ]);
        let err = unpack(dst.path(), Cursor::new(tar_bytes)).unwrap_err();
        assert!(matches!(err, Error::UnsafeArchiveEntry { .. }), "{err:?}");
        assert!(!outside.path().join("pwned.txt").exists());
    }

    #[cfg(unix)]
    #[test]
    fn refuses_writing_through_preexisting_escaping_symlink() {
        let outside = tempfile::tempdir().unwrap();
        let dst = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path(), dst.path().join("out")).unwrap();

        let tar_bytes = build_raw_tar(&[("out/pwned.txt", EntryType::Regular, b"bad", None)]);
        let err = unpack(dst.path(), Cursor::new(tar_bytes)).unwrap_err();
        assert!(matches!(err, Error::UnsafeArchiveEntry { .. }), "{err:?}");
        assert!(!outside.path().join("pwned.txt").exists());
    }

    #[test]
    fn interrupted_unpack_leaves_no_partial_target_file() {
        let src = tempfile::tempdir().unwrap();
        let dst = tempfile::tempdir().unwrap();
        let payload = vec![b'Z'; 128 * 1024];
        fs::write(src.path().join("big.bin"), &payload).unwrap();

        let mut tar_bytes = Vec::new();
        pack(src.path(), &["big.bin"], &mut tar_bytes).unwrap();

        // Truncate right in the middle of the file payload.
        tar_bytes.truncate(4096);
        let err = unpack(dst.path(), Cursor::new(tar_bytes)).unwrap_err();
        assert!(matches!(err, Error::Io { .. }), "{err:?}");
        assert!(!dst.path().join("big.bin").exists());
        // No leftover temp files either.
        assert_eq!(fs::read_dir(dst.path()).unwrap().count(), 0);
    }

    #[test]
    fn unpack_reports_case_collisions() {
        let dst = tempfile::tempdir().unwrap();
        let tar_bytes = build_raw_tar(&[
            ("Readme.md", EntryType::Regular, b"first", None),
            ("README.md", EntryType::Regular, b"second", None),
        ]);
        let report = unpack(dst.path(), Cursor::new(tar_bytes)).unwrap();
        assert_eq!(
            report.case_collisions,
            vec![CaseCollision {
                canonical: "readme.md".to_string(),
                paths: vec!["README.md".to_string(), "Readme.md".to_string()],
            }]
        );
        // First file's content must never be silently lost even on case-insensitive filesystems.
        assert_eq!(fs::read(dst.path().join("Readme.md")).unwrap(), b"first");
    }

    #[cfg(unix)]
    #[test]
    fn refuses_symlink_retargeting_deferred_directory_outside_root() {
        use std::os::unix::fs::PermissionsExt;

        let outside = tempfile::tempdir().unwrap();
        let outside_secret = outside.path().join("secret");
        fs::create_dir(&outside_secret).unwrap();
        fs::set_permissions(&outside_secret, fs::Permissions::from_mode(0o700)).unwrap();
        let original_time = UNIX_EPOCH + Duration::from_secs(1_650_000_000);
        fs::set_times(
            &outside_secret,
            FileTimes::new().set_modified(original_time),
        )
        .unwrap();

        let dst = tempfile::tempdir().unwrap();
        let outside_str = outside.path().to_str().unwrap();

        let mut tar_bytes = Vec::new();
        for (path, entry_type, mode, link_target) in [
            ("inner", EntryType::Directory, 0o755, None),
            ("s", EntryType::Symlink, 0o777, Some("inner")),
            ("s/secret", EntryType::Directory, 0o777, None),
            ("s", EntryType::Symlink, 0o777, Some(outside_str)),
        ] {
            let mut header = Header::new_gnu();
            header.set_entry_type(entry_type);
            header.set_size(0);
            header.set_mode(mode);
            header.set_mtime(1_700_000_999);
            let raw = header.as_mut_bytes();
            raw[..path.len()].copy_from_slice(path.as_bytes());
            if let Some(target) = link_target {
                raw[157..157 + target.len()].copy_from_slice(target.as_bytes());
            }
            header.set_cksum();
            tar_bytes.extend_from_slice(header.as_bytes());
        }
        tar_bytes.resize(tar_bytes.len() + 1024, 0);

        let err = unpack(dst.path(), Cursor::new(tar_bytes)).unwrap_err();
        assert!(matches!(err, Error::UnsafeArchiveEntry { .. }), "{err:?}");

        let meta = fs::symlink_metadata(&outside_secret).unwrap();
        assert_eq!(meta.permissions().mode() & 0o777, 0o700);
        assert_eq!(Timestamp::from_metadata(&meta).secs, 1_650_000_000);
    }

    #[cfg(unix)]
    #[test]
    fn refuses_symlink_overwriting_written_entry_or_ancestor() {
        let outside = tempfile::tempdir().unwrap();
        let outside_str = outside.path().to_str().unwrap();

        // Symlink replacing an already-written file.
        let dst1 = tempfile::tempdir().unwrap();
        let tar1 = build_raw_tar(&[
            ("file.txt", EntryType::Regular, b"hello", None),
            ("file.txt", EntryType::Symlink, b"", Some(outside_str)),
        ]);
        let err1 = unpack(dst1.path(), Cursor::new(tar1)).unwrap_err();
        assert!(matches!(err1, Error::UnsafeArchiveEntry { .. }), "{err1:?}");

        // Symlink replacing an ancestor of an already-written file.
        let dst2 = tempfile::tempdir().unwrap();
        let tar2 = build_raw_tar(&[
            ("sub/file.txt", EntryType::Regular, b"hello", None),
            ("sub", EntryType::Symlink, b"", Some(outside_str)),
        ]);
        let err2 = unpack(dst2.path(), Cursor::new(tar2)).unwrap_err();
        assert!(matches!(err2, Error::UnsafeArchiveEntry { .. }), "{err2:?}");
    }

    #[cfg(unix)]
    #[test]
    fn deferred_dir_update_skips_paths_traversing_symlinked_ancestors() {
        use std::os::unix::fs::PermissionsExt;

        let dst = tempfile::tempdir().unwrap();
        let mut tar_bytes = Vec::new();
        for (path, entry_type, mode, link_target) in [
            ("inner", EntryType::Directory, 0o755, None),
            ("s", EntryType::Symlink, 0o777, Some("inner")),
            ("s/secret", EntryType::Directory, 0o777, None),
        ] {
            let mut header = Header::new_gnu();
            header.set_entry_type(entry_type);
            header.set_size(0);
            header.set_mode(mode);
            header.set_mtime(1_600_000_000);
            let raw = header.as_mut_bytes();
            raw[..path.len()].copy_from_slice(path.as_bytes());
            if let Some(target) = link_target {
                raw[157..157 + target.len()].copy_from_slice(target.as_bytes());
            }
            header.set_cksum();
            tar_bytes.extend_from_slice(header.as_bytes());
        }
        tar_bytes.resize(tar_bytes.len() + 1024, 0);

        unpack(dst.path(), Cursor::new(tar_bytes)).unwrap();

        // `inner/secret` was reached via symlink `s`, so deferred chmod/set_times must skip it.
        let meta = fs::symlink_metadata(dst.path().join("inner/secret")).unwrap();
        assert_ne!(Timestamp::from_metadata(&meta).secs, 1_600_000_000);
        // `inner` itself is a real directory and receives its deferred mtime.
        let inner_meta = fs::symlink_metadata(dst.path().join("inner")).unwrap();
        assert_eq!(Timestamp::from_metadata(&inner_meta).secs, 1_600_000_000);
        assert_eq!(inner_meta.permissions().mode() & 0o777, 0o755);
    }

    #[test]
    fn append_exact_reader_caps_growing_file_and_fails_on_shrinking_file() {
        // Growing file: 200 bytes available when header declares 100. Tar stream must stay
        // synchronized so a following entry unpacks cleanly.
        let mut tar_bytes = Vec::new();
        {
            let mut builder = Builder::new(&mut tar_bytes);

            let mut h1 = Header::new_gnu();
            h1.set_entry_type(EntryType::Regular);
            h1.set_size(100);
            h1.set_mode(0o644);
            h1.set_mtime(1_700_000_000);
            h1.set_cksum();
            let growing_data = vec![b'A'; 200];
            append_exact_reader(
                &mut builder,
                &mut h1,
                "grew.bin",
                Path::new("grew.bin"),
                Cursor::new(growing_data),
                100,
            )
            .unwrap();

            let mut h2 = Header::new_gnu();
            h2.set_entry_type(EntryType::Regular);
            h2.set_size(5);
            h2.set_mode(0o644);
            h2.set_mtime(1_700_000_000);
            h2.set_cksum();
            append_exact_reader(
                &mut builder,
                &mut h2,
                "after.txt",
                Path::new("after.txt"),
                Cursor::new(b"hello".as_slice()),
                5,
            )
            .unwrap();
            builder.finish().unwrap();
        }

        let dst = tempfile::tempdir().unwrap();
        unpack(dst.path(), Cursor::new(&tar_bytes)).unwrap();
        assert_eq!(
            fs::read(dst.path().join("grew.bin")).unwrap(),
            vec![b'A'; 100]
        );
        assert_eq!(fs::read(dst.path().join("after.txt")).unwrap(), b"hello");

        // Shrinking file: only 50 bytes arrive when header declares 100 -> Conflict error.
        let mut bad_buf = Vec::new();
        let mut builder = Builder::new(&mut bad_buf);
        let mut h = Header::new_gnu();
        h.set_entry_type(EntryType::Regular);
        h.set_size(100);
        h.set_mode(0o644);
        h.set_mtime(1_700_000_000);
        h.set_cksum();
        let err = append_exact_reader(
            &mut builder,
            &mut h,
            "shrank.bin",
            Path::new("shrank.bin"),
            Cursor::new(vec![b'B'; 50]),
            100,
        )
        .unwrap_err();
        assert!(matches!(err, Error::Conflict { .. }), "{err:?}");
    }

    #[test]
    fn refuses_hard_link_even_to_existing_file_inside_root() {
        let dst = tempfile::tempdir().unwrap();
        fs::write(dst.path().join("existing.txt"), b"sensitive").unwrap();

        let tar_bytes = build_raw_tar(&[("alias.txt", EntryType::Link, b"", Some("existing.txt"))]);
        let err = unpack(dst.path(), Cursor::new(tar_bytes)).unwrap_err();
        assert!(matches!(err, Error::UnsafeArchiveEntry { .. }), "{err:?}");
        assert!(!dst.path().join("alias.txt").exists());
    }

    #[test]
    fn unpack_syncs_modified_parent_directories_including_readonly_dirs() {
        let dst = tempfile::tempdir().unwrap();
        let before = platform::dir_sync_calls();

        let mut tar_bytes = Vec::new();
        for (path, entry_type, data, mode) in [
            ("ro", EntryType::Directory, b"".as_slice(), 0o555),
            ("ro/a.txt", EntryType::Regular, b"one".as_slice(), 0o644),
            ("ro/b.txt", EntryType::Regular, b"two".as_slice(), 0o644),
        ] {
            let mut h = Header::new_gnu();
            h.set_entry_type(entry_type);
            h.set_size(data.len() as u64);
            h.set_mode(mode);
            h.set_mtime(1_700_000_000);
            let raw = h.as_mut_bytes();
            raw[..path.len()].copy_from_slice(path.as_bytes());
            h.set_cksum();
            tar_bytes.extend_from_slice(h.as_bytes());
            tar_bytes.extend_from_slice(data);
            let rem = data.len() % 512;
            if rem != 0 {
                tar_bytes.resize(tar_bytes.len() + (512 - rem), 0);
            }
        }
        tar_bytes.resize(tar_bytes.len() + 1024, 0);

        unpack(dst.path(), Cursor::new(tar_bytes)).unwrap();
        assert!(platform::dir_sync_calls() > before);
        assert_eq!(fs::read(dst.path().join("ro/a.txt")).unwrap(), b"one");
        assert_eq!(fs::read(dst.path().join("ro/b.txt")).unwrap(), b"two");

        // Restore write permission so tempdir cleanup succeeds on Unix.
        #[cfg(unix)]
        platform::set_mode(&dst.path().join("ro"), 0o755).unwrap();
    }

    #[test]
    fn unpack_filtered_skips_refused_entries_and_drains_bytes() {
        let src = tempfile::tempdir().unwrap();
        let dst = tempfile::tempdir().unwrap();

        fs::create_dir_all(src.path().join("blocked")).unwrap();
        fs::write(src.path().join("keep-first.txt"), b"first").unwrap();
        fs::write(src.path().join("blocked/secret.bin"), vec![b'S'; 16 * 1024]).unwrap();
        fs::write(src.path().join("keep-last.txt"), b"last").unwrap();

        let mut tar_bytes = Vec::new();
        pack(
            src.path(),
            &[
                "keep-first.txt",
                "blocked",
                "blocked/secret.bin",
                "keep-last.txt",
            ],
            &mut tar_bytes,
        )
        .unwrap();

        let report = unpack_filtered(dst.path(), Cursor::new(&tar_bytes), |path, kind| {
            !(path == "blocked" || path.starts_with("blocked/") || kind == EntryKind::Symlink)
        })
        .unwrap();

        assert_eq!(report.files, vec!["keep-first.txt", "keep-last.txt"]);
        assert!(report.dirs.is_empty());
        assert_eq!(
            report
                .skipped
                .iter()
                .map(|s| s.path.as_str())
                .collect::<Vec<_>>(),
            vec!["blocked", "blocked/secret.bin"]
        );
        assert_eq!(
            fs::read(dst.path().join("keep-first.txt")).unwrap(),
            b"first"
        );
        assert_eq!(fs::read(dst.path().join("keep-last.txt")).unwrap(), b"last");
        assert!(!dst.path().join("blocked").exists());

        // Unsafe entries are still rejected even if the filter would refuse them.
        let hostile = build_raw_tar(&[("../escape.txt", EntryType::Regular, b"nope", None)]);
        let err = unpack_filtered(dst.path(), Cursor::new(hostile), |_, _| false).unwrap_err();
        assert!(matches!(err, Error::UnsafeArchiveEntry { .. }), "{err:?}");
    }

    #[test]
    fn unpack_cleans_up_stale_partial_file_for_unpacked_target() {
        let src = tempfile::tempdir().unwrap();
        let dst = tempfile::tempdir().unwrap();

        fs::write(src.path().join("data.bin"), b"fresh").unwrap();
        let stale_part = crate::file::part_path(&dst.path().join("data.bin")).unwrap();
        fs::write(&stale_part, b"abandoned partial upload").unwrap();

        let mut tar_bytes = Vec::new();
        pack(src.path(), &["data.bin"], &mut tar_bytes).unwrap();
        unpack(dst.path(), Cursor::new(&tar_bytes)).unwrap();

        assert_eq!(fs::read(dst.path().join("data.bin")).unwrap(), b"fresh");
        assert!(!stale_part.exists());
    }

    #[test]
    fn pre_1970_mtimes_round_trip_and_produce_empty_second_plan() {
        let src = tempfile::tempdir().unwrap();
        let dst = tempfile::tempdir().unwrap();

        let dir_path = src.path().join("vintage");
        let file_path = dir_path.join("old.txt");
        fs::create_dir(&dir_path).unwrap();
        fs::write(&file_path, b"1969 data").unwrap();

        let pre_epoch_secs = 86_400u64 * 365;
        let pre_epoch = UNIX_EPOCH - Duration::from_secs(pre_epoch_secs);
        fs::set_times(&file_path, FileTimes::new().set_modified(pre_epoch)).unwrap();
        fs::set_times(&dir_path, FileTimes::new().set_modified(pre_epoch)).unwrap();

        let src_m = crate::manifest::build(src.path(), &crate::ManifestOptions::default()).unwrap();
        let src_file = src_m
            .entries
            .iter()
            .find(|e| e.path == "vintage/old.txt")
            .unwrap();
        assert_eq!(src_file.mtime.secs, -(pre_epoch_secs as i64));

        let mut tar_bytes = Vec::new();
        pack(src.path(), &["vintage", "vintage/old.txt"], &mut tar_bytes).unwrap();
        unpack(dst.path(), Cursor::new(&tar_bytes)).unwrap();

        let dst_m = crate::manifest::build(dst.path(), &crate::ManifestOptions::default()).unwrap();
        let dst_file = dst_m
            .entries
            .iter()
            .find(|e| e.path == "vintage/old.txt")
            .unwrap();
        assert_eq!(dst_file.mtime.secs, -(pre_epoch_secs as i64));

        let second_plan = crate::plan::plan(&src_m, &dst_m, &crate::PlanOptions::default());
        assert!(
            second_plan.is_empty(),
            "expected empty second plan for pre-1970 file, got {second_plan:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn unpack_filtered_blocks_symlink_bypass_into_disallowed_subdir() {
        // Case 1: archive creates symlink `public/l -> ../private` and then writes `public/l/owned`.
        let dst1 = tempfile::tempdir().unwrap();
        fs::create_dir_all(dst1.path().join("public")).unwrap();
        fs::create_dir_all(dst1.path().join("private")).unwrap();

        let tar1 = build_raw_tar(&[
            ("public/l", EntryType::Symlink, b"", Some("../private")),
            ("public/l/owned", EntryType::Regular, b"pwned", None),
        ]);
        let err = unpack_filtered(dst1.path(), Cursor::new(tar1), |path, _| {
            path == "public" || path.starts_with("public/")
        })
        .unwrap_err();
        assert!(matches!(err, Error::UnsafeArchiveEntry { .. }), "{err:?}");
        assert!(!dst1.path().join("private/owned").exists());

        // Case 2: pre-existing symlink `public/l -> ../private` in root. Filter must see the real
        // resolved path `private/owned` (and `private/sub/owned`) and reject it without creating
        // anything under `private/`.
        let dst2 = tempfile::tempdir().unwrap();
        fs::create_dir_all(dst2.path().join("public")).unwrap();
        fs::create_dir_all(dst2.path().join("private")).unwrap();
        std::os::unix::fs::symlink("../private", dst2.path().join("public/l")).unwrap();

        let tar2 = build_raw_tar(&[
            ("public/l/owned", EntryType::Regular, b"nope", None),
            ("public/l/sub/deep", EntryType::Regular, b"nope", None),
            ("public/ok.txt", EntryType::Regular, b"yes", None),
        ]);
        let mut seen_by_filter = Vec::new();
        let report = unpack_filtered(dst2.path(), Cursor::new(tar2), |path, _| {
            seen_by_filter.push(path.to_string());
            path == "public" || path.starts_with("public/")
        })
        .unwrap();

        assert_eq!(
            seen_by_filter,
            vec!["private/owned", "private/sub/deep", "public/ok.txt"]
        );
        assert_eq!(report.files, vec!["public/ok.txt"]);
        assert!(!dst2.path().join("private/owned").exists());
        assert!(!dst2.path().join("private/sub").exists());
        assert_eq!(fs::read(dst2.path().join("public/ok.txt")).unwrap(), b"yes");
    }

    #[cfg(unix)]
    #[test]
    fn unpack_temp_files_are_created_with_0600_permissions() {
        use std::os::unix::fs::PermissionsExt;

        struct InspectingReader {
            data: Vec<u8>,
            pos: usize,
            dir: PathBuf,
            observed_tmp_mode: Option<u32>,
        }

        impl Read for InspectingReader {
            fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
                if self.observed_tmp_mode.is_none() {
                    for entry in fs::read_dir(&self.dir)?.flatten() {
                        let name = entry.file_name();
                        if name.to_string_lossy().starts_with(".legion-unpack-") {
                            let mode =
                                fs::symlink_metadata(entry.path())?.permissions().mode() & 0o777;
                            self.observed_tmp_mode = Some(mode);
                        }
                    }
                }
                let remaining = self.data.len() - self.pos;
                if remaining == 0 {
                    return Ok(0);
                }
                let take = remaining.min(buf.len()).min(256);
                buf[..take].copy_from_slice(&self.data[self.pos..self.pos + take]);
                self.pos += take;
                Ok(take)
            }
        }

        let dst = tempfile::tempdir().unwrap();
        let (_file, guard) = create_temp_file(dst.path()).unwrap();
        let direct_mode = fs::symlink_metadata(guard.path())
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(direct_mode, 0o600);
        drop(guard);

        let payload = vec![b'K'; 4096];
        let mut tar_bytes = Vec::new();
        let mut h = Header::new_gnu();
        h.set_entry_type(EntryType::Regular);
        h.set_size(payload.len() as u64);
        h.set_mode(0o600);
        h.set_mtime(1_700_000_000);
        h.set_cksum();
        let mut builder = Builder::new(&mut tar_bytes);
        builder
            .append_data(&mut h, "secret.key", Cursor::new(&payload))
            .unwrap();
        builder.finish().unwrap();
        drop(builder);

        let mut reader = InspectingReader {
            data: tar_bytes,
            pos: 0,
            dir: dst.path().to_path_buf(),
            observed_tmp_mode: None,
        };
        unpack(dst.path(), &mut reader).unwrap();
        assert_eq!(reader.observed_tmp_mode, Some(0o600));
        assert_eq!(
            fs::metadata(dst.path().join("secret.key"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }
}
