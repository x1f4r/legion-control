//! Case/Unicode-normalization rename classification, directory folding probes, and recoverable
//! two-step rename replay.
//!
//! Interrupted two-step renames record their intent under `<state_dir>/xfer/renames/<key>.json`
//! (with a sibling `<key>.lock` flock) and use a **visible** intermediate name
//! (`legion-renaming-*`) so nothing is hidden if the process dies mid-rename. Both `fs.list` and
//! stream transfer operations call [`recover_pending_renames`] to complete or reverse any
//! interrupted rename and clean up its intent and lock files.

use std::ffi::{OsStr, OsString};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use legion_os::atomic::{FileIdentity, file_identity};
use serde::{Deserialize, Serialize};
use unicode_normalization::UnicodeNormalization;

static PROBE_SEQ: AtomicU64 = AtomicU64::new(0);

/// How a directory's file system compares entry names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FsNameBehavior {
    /// Whether names differing only in letter case refer to the same entry (e.g. NTFS, default
    /// APFS, FAT).
    pub case_insensitive: bool,
    /// Whether canonically equivalent Unicode forms (NFC vs NFD) refer to the same entry (e.g.
    /// APFS, HFS+).
    pub unicode_normalizing: bool,
}

/// Decision for a move or rename between two paths on the same machine.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SameEntryDecision {
    /// Distinct files (or destination absent): proceed as an ordinary transfer or non-replacing
    /// rename.
    DistinctEntries,
    /// Exact same directory entry (same parent directory and byte-identical entry name): no-op,
    /// must be refused.
    SameEntryNoOp,
    /// Same directory entry spelled differently (same parent directory, same underlying file,
    /// names differ byte-for-byte only by case and/or Unicode normalization under the file
    /// system's rules, and neither path is a symbolic link): perform a case/normalization rename.
    CaseOrNormalizationRename,
    /// Same underlying file through a different directory entry or path (hard link, symlink,
    /// different parent directory, or case-different hardlink on a case-sensitive file system):
    /// refuse as moving or renaming onto itself.
    RefuseSameFileAlias,
}

/// Pure decision function classifying a same-machine move or rename from parent identities, exact
/// caller entry names, file-identity equality, symlink status, and file system case/normalization
/// behavior.
pub fn classify_same_entry(
    src_parent: Option<FileIdentity>,
    dst_parent: Option<FileIdentity>,
    src_name: &OsStr,
    dst_name: &OsStr,
    same_file: bool,
    either_symlink: bool,
    fs: FsNameBehavior,
) -> SameEntryDecision {
    let same_parent = src_parent
        .zip(dst_parent)
        .is_some_and(|(a, b)| a.same_file(&b));
    if same_parent && src_name.as_encoded_bytes() == dst_name.as_encoded_bytes() {
        return SameEntryDecision::SameEntryNoOp;
    }
    if !same_file {
        return SameEntryDecision::DistinctEntries;
    }
    if same_parent && !either_symlink && names_fold_equivalent(src_name, dst_name, fs) {
        return SameEntryDecision::CaseOrNormalizationRename;
    }
    SameEntryDecision::RefuseSameFileAlias
}

/// Whether two entry names fold to the same key under `fs`'s case and Unicode normalization rules.
pub fn names_fold_equivalent(a: &OsStr, b: &OsStr, fs: FsNameBehavior) -> bool {
    if a.as_encoded_bytes() == b.as_encoded_bytes() {
        return true;
    }
    if !fs.case_insensitive && !fs.unicode_normalizing {
        return false;
    }
    let (Some(a), Some(b)) = (a.to_str(), b.to_str()) else {
        return false;
    };
    let fold = |s: &str| -> String {
        if fs.unicode_normalizing && fs.case_insensitive {
            let decomposed: String = s.nfd().collect();
            decomposed.to_lowercase().nfd().collect()
        } else if fs.unicode_normalizing {
            s.nfd().collect()
        } else {
            s.to_lowercase()
        }
    };
    fold(a) == fold(b)
}

/// Whether the file system holding `path`, or the nearest directory above it that exists,
/// ignores letter case, told without writing anything: a name in that directory is looked up again
/// with the case of its letters flipped, and the two are the same file only where case is ignored.
pub fn case_insensitive(path: &Path) -> Option<bool> {
    let dir = path
        .ancestors()
        .find(|dir| fs::metadata(dir).is_ok_and(|meta| meta.is_dir()))?;
    case_insensitive_in_dir(dir)
}

/// Read-only case-insensitivity check strictly inside `dir` (never walking to ancestors).
pub fn case_insensitive_in_dir(dir: &Path) -> Option<bool> {
    if !fs::metadata(dir).is_ok_and(|meta| meta.is_dir()) {
        return None;
    }
    let inside = fs::read_dir(dir)
        .into_iter()
        .flatten()
        .take(64)
        .filter_map(Result::ok)
        .map(|entry| entry.path());
    for candidate in inside {
        // A link and what it leads to are the same file whatever their names.
        let Ok(meta) = fs::symlink_metadata(&candidate) else {
            continue;
        };
        if meta.file_type().is_symlink() {
            continue;
        }
        let (Some(parent), Some(name)) = (
            candidate.parent(),
            candidate.file_name().and_then(|name| name.to_str()),
        ) else {
            continue;
        };
        let flipped: String = name
            .chars()
            .flat_map(|c| {
                if c.is_lowercase() {
                    c.to_uppercase().collect::<Vec<_>>()
                } else if c.is_uppercase() {
                    c.to_lowercase().collect::<Vec<_>>()
                } else {
                    vec![c]
                }
            })
            .collect();
        if flipped == name {
            continue;
        }
        let twin = parent.join(flipped);
        if fs::symlink_metadata(&twin).is_err() {
            return Some(false);
        }
        // Directories cannot be hardlinked on POSIX, and regular files with `nlink == 1` have no
        // hardlink aliases, so a same-file hit on either proves case-insensitivity without risking
        // hardlink spoofing.
        if meta.is_dir() || file_identity(&candidate).is_ok_and(|id| id.nlink == 1) {
            return Some(same_file::is_same_file(&candidate, &twin).unwrap_or(false));
        }
    }
    None
}

/// Whether the file system holding `path` folds canonically equivalent Unicode forms (NFC vs NFD),
/// told without writing anything when a decomposable entry already exists in the directory.
pub fn unicode_normalizing(path: &Path) -> Option<bool> {
    let dir = path
        .ancestors()
        .find(|dir| fs::metadata(dir).is_ok_and(|meta| meta.is_dir()))?;
    unicode_normalizing_in_dir(dir)
}

/// Read-only Unicode-normalization check strictly inside `dir` (never walking to ancestors).
pub fn unicode_normalizing_in_dir(dir: &Path) -> Option<bool> {
    if !fs::metadata(dir).is_ok_and(|meta| meta.is_dir()) {
        return None;
    }
    let inside = fs::read_dir(dir)
        .into_iter()
        .flatten()
        .take(64)
        .filter_map(Result::ok)
        .map(|entry| entry.path());
    for candidate in inside {
        let Ok(meta) = fs::symlink_metadata(&candidate) else {
            continue;
        };
        if meta.file_type().is_symlink() {
            continue;
        }
        let (Some(parent), Some(name)) = (
            candidate.parent(),
            candidate.file_name().and_then(|name| name.to_str()),
        ) else {
            continue;
        };
        let nfc: String = name.nfc().collect();
        let nfd: String = name.nfd().collect();
        if nfc == nfd {
            continue;
        }
        let other = if name == nfc { nfd } else { nfc };
        let twin = parent.join(other);
        if fs::symlink_metadata(&twin).is_err() {
            return Some(false);
        }
        if meta.is_dir() || file_identity(&candidate).is_ok_and(|id| id.nlink == 1) {
            return Some(same_file::is_same_file(&candidate, &twin).unwrap_or(false));
        }
    }
    None
}

/// Probes `dir`'s exact case-sensitivity and Unicode-normalization behavior, using read-only
/// inspection first and falling back to an ephemeral unique probe file when `dir` has no suitable
/// entry.
pub fn probe_dir_name_behavior(dir: &Path) -> FsNameBehavior {
    let ci = case_insensitive_in_dir(dir).unwrap_or_else(|| {
        probe_dir_pair(
            dir,
            |pid, seq| format!(".legion-CaseProbe-{pid}-{seq}.tmp"),
            |pid, seq| format!(".legion-caseprobe-{pid}-{seq}.tmp"),
        )
        .unwrap_or(cfg!(any(windows, target_os = "macos")))
    });
    let norm = unicode_normalizing_in_dir(dir).unwrap_or_else(|| {
        probe_dir_pair(
            dir,
            |pid, seq| format!(".legion-NormProbe-e\u{0301}-{pid}-{seq}.tmp"),
            |pid, seq| format!(".legion-NormProbe-\u{00e9}-{pid}-{seq}.tmp"),
        )
        .unwrap_or(cfg!(target_os = "macos"))
    });
    FsNameBehavior {
        case_insensitive: ci,
        unicode_normalizing: norm,
    }
}

fn probe_dir_pair(
    dir: &Path,
    make_a: impl Fn(u32, u64) -> String,
    make_b: impl Fn(u32, u64) -> String,
) -> Option<bool> {
    let pid = std::process::id();
    for _ in 0..8 {
        let seq = PROBE_SEQ.fetch_add(1, Ordering::Relaxed);
        let a = dir.join(make_a(pid, seq));
        let b = dir.join(make_b(pid, seq));
        match fs::OpenOptions::new().write(true).create_new(true).open(&a) {
            Ok(file) => {
                drop(file);
                let folded = fs::symlink_metadata(&b).is_ok()
                    && same_file::is_same_file(&a, &b).unwrap_or(false);
                let _ = fs::remove_file(&a);
                return Some(folded);
            }
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(_) => return None,
        }
    }
    None
}

/// Lists `dir` and checks whether an entry with the exact byte sequence `expected` is present,
/// rather than calling `exists()` (which folds case or Unicode normalization on folding volumes).
pub fn directory_contains_exact_name(dir: &Path, expected: &OsStr) -> io::Result<bool> {
    let expected_bytes = expected.as_encoded_bytes();
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        if entry.file_name().as_encoded_bytes() == expected_bytes {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Intent record written in `<state_dir>/xfer/renames/<key>.json` before step 1 of a two-step
/// case/normalization rename so an interrupted rename can always be completed or reversed on the
/// next transfer or listing.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PendingRenameIntent {
    /// Directory containing the entry being renamed.
    pub parent: String,
    /// Original entry name before step 1.
    pub from_name: String,
    /// Visible intermediate entry name used between step 1 and step 2.
    pub temp_name: String,
    /// Requested final entry name after step 2.
    pub to_name: String,
}

/// Directory storing pending two-step rename intents and their flock files.
pub fn pending_renames_dir(state_dir: &Path) -> PathBuf {
    state_dir.join("xfer").join("renames")
}

/// Stable state key for a pending rename intent.
pub fn rename_intent_key(parent: &Path, temp_name: &str) -> String {
    let mut data =
        Vec::with_capacity(parent.as_os_str().as_encoded_bytes().len() + 1 + temp_name.len());
    data.extend_from_slice(parent.as_os_str().as_encoded_bytes());
    data.push(0);
    data.extend_from_slice(temp_name.as_bytes());
    crate::hash::hash_bytes(&data).to_hex()
}

/// Builds a **visible** sibling intermediate filename (`legion-renaming-*`, never dot-prefixed and
/// never filtered by [`crate::is_legion_internal_name`]) so if a two-step rename is interrupted
/// mid-flight, the entry remains visible to both people and listings while recovery waits to run.
pub fn visible_rename_temp_name(from_name: &OsStr, pid: u32, seq: u64) -> String {
    let lossy = from_name.to_string_lossy();
    let safe: String = lossy
        .chars()
        .filter(|c| !matches!(c, '/' | '\\' | '\0'))
        .take(80)
        .collect();
    if safe.is_empty() {
        format!("legion-renaming-{pid}-{seq}")
    } else {
        format!("legion-renaming-{pid}-{seq}-{safe}")
    }
}

/// Recovery action decided for an interrupted two-step rename from the directory's exact entry
/// names and the filesystem's case/Unicode folding rules.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PendingRenameRecovery {
    /// Complete step 2 (`temp_name -> to_name`) without replacing anything, then remove the intent
    /// record.
    CompleteToFinal,
    /// Reverse step 1 (`temp_name -> from_name`) without replacing anything (when `to_name` is
    /// occupied on a filesystem where `from_name` is still a distinct free slot), then remove the
    /// intent record.
    ReverseToOriginal,
    /// `temp_name` is no longer present in the directory (crash before step 1, crash after step 2,
    /// or user already resolved it): remove the stale intent record and leave directory entries
    /// untouched.
    DropStaleIntent,
    /// Both `temp_name` and a conflicting entry occupying the target/original namespace exist:
    /// leave the visible `temp_name` and the conflicting entry untouched so no data is ever
    /// overwritten, and keep the intent record so recovery can finish if the conflict is cleared.
    KeepBoth,
}

/// Pure recovery decision from exact directory entry names (raw encoded bytes from `read_dir`),
/// the intent's `(from_name, temp_name, to_name)`, and the directory's case/Unicode folding rules.
pub fn decide_pending_rename_recovery(
    entries: &[OsString],
    from_name: &OsStr,
    temp_name: &OsStr,
    to_name: &OsStr,
    fs: FsNameBehavior,
) -> PendingRenameRecovery {
    let exact_present = |needle: &OsStr| {
        let want = needle.as_encoded_bytes();
        entries.iter().any(|e| e.as_encoded_bytes() == want)
    };
    let slot_occupied = |target: &OsStr| {
        let temp_bytes = temp_name.as_encoded_bytes();
        entries
            .iter()
            .any(|e| e.as_encoded_bytes() != temp_bytes && names_fold_equivalent(e, target, fs))
    };

    if !exact_present(temp_name) {
        return PendingRenameRecovery::DropStaleIntent;
    }

    let has_from = exact_present(from_name);
    let has_to = exact_present(to_name);
    let to_slot_busy = has_to || slot_occupied(to_name);
    let from_slot_busy = has_from || slot_occupied(from_name);

    match (has_from, has_to) {
        (false, false) => {
            if !to_slot_busy {
                PendingRenameRecovery::CompleteToFinal
            } else if !from_slot_busy {
                PendingRenameRecovery::ReverseToOriginal
            } else {
                PendingRenameRecovery::KeepBoth
            }
        }
        (true, false) => {
            if !to_slot_busy {
                PendingRenameRecovery::CompleteToFinal
            } else {
                PendingRenameRecovery::KeepBoth
            }
        }
        (false, true) => {
            if !from_slot_busy {
                PendingRenameRecovery::ReverseToOriginal
            } else {
                PendingRenameRecovery::KeepBoth
            }
        }
        (true, true) => PendingRenameRecovery::KeepBoth,
    }
}

fn normalize_scope_path(path: &Path) -> String {
    let raw = path.to_string_lossy();
    let stripped = raw.strip_prefix(r"\\?\").unwrap_or(&raw);
    let mut normalized = stripped.replace('\\', "/");
    while normalized.len() > 1 && normalized.ends_with('/') {
        if normalized.len() == 3 && normalized.as_bytes()[1] == b':' {
            break;
        }
        normalized.pop();
    }
    if cfg!(windows) {
        normalized.make_ascii_lowercase();
    }
    normalized
}

fn paths_overlap_scope(scope: &Path, parent: &Path) -> bool {
    if let (Ok(s_id), Ok(p_id)) = (file_identity(scope), file_identity(parent))
        && s_id.same_file(&p_id)
    {
        return true;
    }
    let s = normalize_scope_path(&fs::canonicalize(scope).unwrap_or_else(|_| scope.to_path_buf()));
    let p =
        normalize_scope_path(&fs::canonicalize(parent).unwrap_or_else(|_| parent.to_path_buf()));
    let is_prefix = |ancestor: &str, child: &str| {
        ancestor == child
            || ancestor == "/"
            || child
                .strip_prefix(ancestor)
                .is_some_and(|rest| rest.starts_with('/'))
    };
    is_prefix(&s, &p) || is_prefix(&p, &s)
}

/// Renames `src` to `dst` without replacing an existing entry when the kernel supports it.
pub fn move_no_replace_raw(src: &Path, dst: &Path) -> io::Result<()> {
    match legion_os::atomic::rename_no_replace(src, dst) {
        Ok(()) => Ok(()),
        Err(legion_os::atomic::RenameNoReplaceError::Unsupported) => fs::rename(src, dst),
        Err(legion_os::atomic::RenameNoReplaceError::AlreadyExists) => Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "destination already exists",
        )),
        Err(legion_os::atomic::RenameNoReplaceError::Io(err)) => Err(err),
    }
}

/// Removes a lock file cleanly across platforms (unlinking while locked on Unix; dropping the
/// handle first on Windows so the file can be removed).
pub fn unlink_held_lock(lock_path: &Path, lock_file: fs::File) {
    #[cfg(unix)]
    {
        let _ = fs::remove_file(lock_path);
        drop(lock_file);
    }
    #[cfg(not(unix))]
    {
        drop(lock_file);
        let _ = fs::remove_file(lock_path);
    }
}

/// Completes or reverses any interrupted two-step case/normalization renames whose intent records
/// remain under `<state_dir>/xfer/renames/`, and removes their finished or orphaned `.lock` files.
///
/// Decides purely from `read_dir(&parent)`'s exact entry names (compared byte for byte) so case
/// folding or path canonicalisation can never confuse `from_name`, `to_name`, and `temp_name`.
pub fn recover_pending_renames(state_dir: &Path, scope_dir: Option<&Path>) {
    let dir = pending_renames_dir(state_dir);
    let Ok(entries) = fs::read_dir(&dir) else {
        return;
    };
    let mut lock_candidates = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        match path.extension().and_then(OsStr::to_str) {
            Some("lock") => {
                lock_candidates.push(path);
                continue;
            }
            Some("json") => {}
            _ => continue,
        }
        let Some(stem) = path.file_stem().and_then(OsStr::to_str) else {
            continue;
        };
        let lock_path = dir.join(format!("{stem}.lock"));
        let Ok(Some(lock_file)) = crate::try_lock_state_file(&lock_path) else {
            continue;
        };
        let Ok(bytes) = fs::read(&path) else {
            unlink_held_lock(&lock_path, lock_file);
            continue;
        };
        let Ok(intent) = serde_json::from_slice::<PendingRenameIntent>(&bytes) else {
            let _ = fs::remove_file(&path);
            unlink_held_lock(&lock_path, lock_file);
            continue;
        };
        let parent = PathBuf::from(&intent.parent);
        if let Some(scope) = scope_dir
            && !paths_overlap_scope(scope, &parent)
        {
            continue;
        }
        let dir_entries: Vec<OsString> = match fs::read_dir(&parent) {
            Ok(rd) => rd.filter_map(Result::ok).map(|e| e.file_name()).collect(),
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                let _ = fs::remove_file(&path);
                unlink_held_lock(&lock_path, lock_file);
                continue;
            }
            Err(_) => continue,
        };
        let from_name = OsStr::new(&intent.from_name);
        let temp_name = OsStr::new(&intent.temp_name);
        let to_name = OsStr::new(&intent.to_name);
        // Only probe directory folding behavior when `temp_name` is actually present so cleaning
        // up a stale intent never writes a probe file.
        let has_temp = dir_entries
            .iter()
            .any(|e| e.as_encoded_bytes() == temp_name.as_encoded_bytes());
        let fs_behavior = if has_temp {
            probe_dir_name_behavior(&parent)
        } else {
            FsNameBehavior {
                case_insensitive: false,
                unicode_normalizing: false,
            }
        };
        let action = decide_pending_rename_recovery(
            &dir_entries,
            from_name,
            temp_name,
            to_name,
            fs_behavior,
        );
        let temp_path = parent.join(temp_name);
        let to_path = parent.join(to_name);
        let from_path = parent.join(from_name);
        let finished = match action {
            PendingRenameRecovery::DropStaleIntent => {
                let _ = fs::remove_file(&path);
                true
            }
            PendingRenameRecovery::CompleteToFinal => {
                if move_no_replace_raw(&temp_path, &to_path).is_ok()
                    && directory_contains_exact_name(&parent, to_name).unwrap_or(false)
                {
                    #[cfg(unix)]
                    if let Ok(dir_handle) = fs::File::open(&parent) {
                        let _ = dir_handle.sync_all();
                    }
                    let _ = fs::remove_file(&path);
                    true
                } else {
                    false
                }
            }
            PendingRenameRecovery::ReverseToOriginal => {
                if move_no_replace_raw(&temp_path, &from_path).is_ok()
                    && directory_contains_exact_name(&parent, from_name).unwrap_or(false)
                {
                    #[cfg(unix)]
                    if let Ok(dir_handle) = fs::File::open(&parent) {
                        let _ = dir_handle.sync_all();
                    }
                    let _ = fs::remove_file(&path);
                    true
                } else {
                    false
                }
            }
            PendingRenameRecovery::KeepBoth => false,
        };
        if finished {
            unlink_held_lock(&lock_path, lock_file);
        }
    }

    // Sweep any orphaned `.lock` files whose matching `.json` intent no longer exists and that are
    // not held by an active rename.
    for lock_path in lock_candidates {
        let Some(stem) = lock_path.file_stem().and_then(OsStr::to_str) else {
            continue;
        };
        if dir.join(format!("{stem}.json")).exists() {
            continue;
        }
        if let Ok(Some(lock_file)) = crate::try_lock_state_file(&lock_path) {
            unlink_held_lock(&lock_path, lock_file);
        }
    }
}
