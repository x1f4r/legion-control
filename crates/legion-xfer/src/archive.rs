//! Tar stream packing and hardened atomic unpacking.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, FileTimes, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tar::{Archive, Builder, EntryType, Header};

use crate::error::{Error, Result, classify_io};
use crate::manifest::{CaseCollision, SkippedEntry, Timestamp};
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
        let mtime_secs = u64::try_from(mtime.secs).unwrap_or(0);

        let mut header = Header::new_gnu();
        header.set_mtime(mtime_secs);

        if ft.is_file() {
            let file = File::open(&full_path).map_err(|e| classify_io(&full_path, e))?;
            let file_meta = file.metadata().map_err(|e| classify_io(&full_path, e))?;
            let len = file_meta.len();
            let file_mtime = Timestamp::from_metadata(&file_meta);
            header.set_mtime(u64::try_from(file_mtime.secs).unwrap_or(0));
            header.set_entry_type(EntryType::Regular);
            header.set_size(len);
            header.set_mode(platform::entry_mode(&file_meta).unwrap_or(0o644));
            header.set_cksum();
            append_exact_reader(&mut builder, &mut header, &rel, &full_path, file, len)?;
            report.files += 1;
            report.bytes += len;
        } else if ft.is_dir() {
            header.set_entry_type(EntryType::Directory);
            header.set_size(0);
            header.set_mode(platform::entry_mode(&meta).unwrap_or(0o755));
            header.set_cksum();
            builder.append_data(&mut header, &rel, io::empty())?;
            report.dirs += 1;
        } else if ft.is_symlink() {
            let target = fs::read_link(&full_path).map_err(|e| classify_io(&full_path, e))?;
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

/// Safely extracts a tar stream from `reader` into `root`.
///
/// Refuses absolute paths, `..` traversal, backslashes, drive prefixes, unsupported special file
/// types, and any entry whose ancestor symlink would escape `root`. Regular files are streamed to
/// sibling temporary files and renamed into place so an interrupted unpack never leaves a partial
/// file at its destination path.
pub fn unpack<R: Read>(root: &Path, reader: R) -> Result<UnpackReport> {
    fs::create_dir_all(root).map_err(|e| classify_io(root, e))?;
    let canonical_root = fs::canonicalize(root).map_err(|e| classify_io(root, e))?;
    let case_insensitive = is_case_insensitive_dir(&canonical_root)?;

    let mut archive = Archive::new(reader);
    let entries = archive.entries()?;

    let mut report = UnpackReport::default();
    let mut seen_lower: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut written_exact: BTreeSet<String> = BTreeSet::new();
    let mut written_ancestors: BTreeSet<String> = BTreeSet::new();
    let mut deferred_dirs: Vec<DeferredDir> = Vec::new();

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

        let (parent, final_seg) = ensure_safe_parent_dirs(root, &canonical_root, &segments, &rel)?;
        let dest = parent.join(final_seg);

        let mtime = entry
            .header()
            .mtime()
            .ok()
            .and_then(|secs| UNIX_EPOCH.checked_add(Duration::from_secs(secs)));
        // Strip setuid/setgid/sticky bits from untrusted archive headers.
        let safe_mode = entry.header().mode().ok().map(|m| m & 0o777);

        match entry_type {
            EntryType::Directory => {
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
            EntryType::Regular | EntryType::Continuous => {
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

                record_written(&mut written_exact, &mut written_ancestors, &rel);
                report.files.push(rel);
                report.bytes += written;
            }
            EntryType::Symlink => {
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
                    record_written(&mut written_exact, &mut written_ancestors, &rel);
                    report.symlinks.push(rel);
                } else {
                    report.skipped.push(SkippedEntry {
                        path: rel,
                        reason: "symlinks are skipped on Windows".to_string(),
                    });
                }
            }
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
        }
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

/// Ensures every ancestor component exists inside `canonical_root` without following any symlink
/// that escapes `canonical_root`.
fn ensure_safe_parent_dirs<'a>(
    root: &Path,
    canonical_root: &Path,
    segments: &'a [String],
    rel: &str,
) -> Result<(PathBuf, &'a str)> {
    let Some((final_seg, parents)) = segments.split_last() else {
        return Err(Error::UnsafeArchiveEntry {
            entry: rel.to_string(),
            reason: "empty path".to_string(),
        });
    };

    let mut cur = root.to_path_buf();
    for seg in parents {
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
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&probe_upper)
        {
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
        match OpenOptions::new().write(true).create_new(true).open(&path) {
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
}
