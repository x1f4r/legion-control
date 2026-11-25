//! Platform naming, symlink, and case-sensitivity tests against a strict simulated Windows
//! and macOS [`SyncFs`] backend that runs identically on Linux, macOS, and Windows.
//!
//! Unlike a permissive in-memory map, [`StrictPlatformFs`] actively panics if the sync engine
//! ever attempts to stat, read, create, rename, or delete a live path that violates the target
//! platform's naming rules, symlink support, or case-insensitivity invariants.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::BTreeMap;
use std::io::{self, Cursor, Read, Write};
use std::sync::{Arc, Mutex};

use legion_sync::path::{
    ancestors, check_fs_compatibility, fold_case, validate_flat_internal_name, validate_sync_path,
};
use legion_sync::{
    FolderConfig, FsCapabilities, FsErrorKind, FsMeta, FsWalkEntry, FsWalkResult, IgnoreMatcher,
    IndexStore, MemberState, MemoryIndexStore, Peer, ProblemKind, RecordsPage, SyncEngine,
    SyncError, SyncFs, SyncWrite,
};
use legion_xfer::hash::hash_bytes;
use legion_xfer::{EntryKind, Hash, Timestamp};

#[derive(Debug, Clone)]
enum Node {
    File {
        bytes: Vec<u8>,
        mtime: Timestamp,
        mode: Option<u32>,
        gen_id: u64,
    },
    Dir {
        mtime: Timestamp,
    },
    Symlink {
        target: String,
        mtime: Timestamp,
    },
}

impl Node {
    fn meta(&self) -> FsMeta {
        match self {
            Node::File {
                bytes,
                mtime,
                mode,
                gen_id,
            } => FsMeta {
                kind: EntryKind::File,
                size: bytes.len() as u64,
                mtime: *mtime,
                mode: *mode,
                link_target: None,
                change_id: Some(*gen_id),
            },
            Node::Dir { mtime } => FsMeta {
                kind: EntryKind::Dir,
                size: 0,
                mtime: *mtime,
                mode: None,
                link_target: None,
                change_id: None,
            },
            Node::Symlink { target, mtime } => FsMeta {
                kind: EntryKind::Symlink,
                size: 0,
                mtime: *mtime,
                mode: None,
                link_target: Some(target.clone()),
                change_id: None,
            },
        }
    }
}

/// Strict in-memory filesystem that enforces Windows naming rules, symlink restrictions, and
/// case-insensitive collision safety on every engine call.
struct StrictPlatformFs {
    caps: FsCapabilities,
    live: Mutex<BTreeMap<String, Node>>,
    tmp: Arc<Mutex<BTreeMap<String, Vec<u8>>>>,
    bin: Mutex<BTreeMap<String, Vec<u8>>>,
    next_gen: Mutex<u64>,
}

impl StrictPlatformFs {
    fn new(caps: FsCapabilities) -> Self {
        Self {
            caps,
            live: Mutex::new(BTreeMap::new()),
            tmp: Arc::new(Mutex::new(BTreeMap::new())),
            bin: Mutex::new(BTreeMap::new()),
            next_gen: Mutex::new(1),
        }
    }

    fn alloc_gen(&self) -> u64 {
        let mut g = self.next_gen.lock().unwrap();
        let out = *g;
        *g += 1;
        out
    }

    /// Panics if the sync engine ever touches a live path the simulated platform cannot hold.
    fn assert_platform_allowed(&self, rel: &str, kind: EntryKind, op: &str) {
        if let Err((prob, msg)) = check_fs_compatibility(rel, kind, &self.caps) {
            panic!("engine attempted {op} on platform-illegal path '{rel}' ({prob:?}): {msg}");
        }
    }

    /// Panics if on a case-insensitive filesystem the engine tries to write a path whose exact
    /// path or ancestor collides case-insensitively with an existing live entry of different case.
    fn assert_no_case_collision(&self, live: &BTreeMap<String, Node>, rel: &str, op: &str) {
        if !self.caps.case_insensitive {
            return;
        }
        let mut to_check: Vec<&str> = ancestors(rel);
        to_check.push(rel);
        for candidate in to_check {
            let folded = fold_case(candidate);
            for existing in live.keys() {
                if fold_case(existing) == folded && existing != candidate {
                    panic!(
                        "engine attempted {op} on '{rel}', colliding case-insensitively with existing '{existing}'"
                    );
                }
            }
        }
    }

    /// Resolves `rel` case-insensitively when `caps.case_insensitive` is set, matching NTFS/APFS.
    fn lookup_key<'a>(
        &self,
        live: &'a BTreeMap<String, Node>,
        rel: &str,
    ) -> Option<(&'a String, &'a Node)> {
        if let Some((k, v)) = live.get_key_value(rel) {
            return Some((k, v));
        }
        if self.caps.case_insensitive {
            let folded = fold_case(rel);
            return live.iter().find(|(k, _)| fold_case(k) == folded);
        }
        None
    }

    fn user_write_file(&self, path: &str, bytes: &[u8], mtime: Timestamp) {
        let clean = validate_sync_path(path).unwrap();
        self.assert_platform_allowed(&clean, EntryKind::File, "user_write_file");
        let mut live = self.live.lock().unwrap();
        for anc in ancestors(&clean) {
            if self.lookup_key(&live, anc).is_none() {
                live.insert(anc.to_string(), Node::Dir { mtime });
            }
        }
        // Mirror real NTFS/APFS case-preserving overwrite if an entry with differing case exists.
        let target_key = self
            .lookup_key(&live, &clean)
            .map(|(k, _)| k.clone())
            .unwrap_or(clean);
        live.insert(
            target_key,
            Node::File {
                bytes: bytes.to_vec(),
                mtime,
                mode: if self.caps.windows_naming {
                    None
                } else {
                    Some(0o644)
                },
                gen_id: self.alloc_gen(),
            },
        );
    }

    fn user_symlink(&self, path: &str, target: &str, mtime: Timestamp) {
        let clean = validate_sync_path(path).unwrap();
        self.assert_platform_allowed(&clean, EntryKind::Symlink, "user_symlink");
        let mut live = self.live.lock().unwrap();
        for anc in ancestors(&clean) {
            live.entry(anc.to_string()).or_insert(Node::Dir { mtime });
        }
        live.insert(
            clean,
            Node::Symlink {
                target: target.to_string(),
                mtime,
            },
        );
    }

    fn user_remove_recursive(&self, path: &str) {
        let clean = validate_sync_path(path).unwrap();
        let mut live = self.live.lock().unwrap();
        let prefix = format!("{clean}/");
        let keys: Vec<String> = live
            .keys()
            .filter(|k| *k == &clean || k.starts_with(&prefix))
            .cloned()
            .collect();
        for k in keys {
            live.remove(&k);
        }
    }

    fn read_live_bytes(&self, path: &str) -> Option<Vec<u8>> {
        let live = self.live.lock().unwrap();
        match live.get(path) {
            Some(Node::File { bytes, .. }) => Some(bytes.clone()),
            _ => None,
        }
    }

    fn live_paths(&self) -> Vec<String> {
        self.live.lock().unwrap().keys().cloned().collect()
    }
}

struct StrictTmpWriter {
    tmp: Arc<Mutex<BTreeMap<String, Vec<u8>>>>,
    tmp_name: String,
    buf: Vec<u8>,
}

impl Write for StrictTmpWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.buf.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl SyncWrite for StrictTmpWriter {
    fn finish_sync(self: Box<Self>) -> legion_sync::Result<()> {
        self.tmp.lock().unwrap().insert(self.tmp_name, self.buf);
        Ok(())
    }
}

impl SyncFs for StrictPlatformFs {
    fn capabilities(&self) -> FsCapabilities {
        self.caps
    }

    fn walk(&self, ignore: &IgnoreMatcher) -> legion_sync::Result<FsWalkResult> {
        let live = self.live.lock().unwrap();
        let mut entries = Vec::new();
        for (path, node) in live.iter() {
            let meta = node.meta();
            if ignore.is_path_or_ancestor_ignored(path, meta.kind == EntryKind::Dir) {
                continue;
            }
            entries.push(FsWalkEntry {
                path: path.clone(),
                meta,
            });
        }
        Ok(FsWalkResult {
            entries,
            skipped: Vec::new(),
        })
    }

    fn stat_live(&self, rel_path: &str) -> legion_sync::Result<Option<FsMeta>> {
        let clean = validate_sync_path(rel_path)?;
        self.assert_platform_allowed(&clean, EntryKind::File, "stat_live");
        let live = self.live.lock().unwrap();
        for anc in ancestors(&clean) {
            match self.lookup_key(&live, anc).map(|(_, n)| n) {
                Some(Node::Symlink { .. }) => {
                    return Err(SyncError::InvalidPath {
                        path: clean,
                        reason: "ancestor is a symlink".to_string(),
                    });
                }
                Some(Node::File { .. }) => return Ok(None),
                _ => {}
            }
        }
        Ok(self.lookup_key(&live, &clean).map(|(_, n)| n.meta()))
    }

    fn hash_live(&self, rel_path: &str) -> legion_sync::Result<(Hash, u64)> {
        let clean = validate_sync_path(rel_path)?;
        self.assert_platform_allowed(&clean, EntryKind::File, "hash_live");
        let live = self.live.lock().unwrap();
        match self.lookup_key(&live, &clean).map(|(_, n)| n) {
            Some(Node::File { bytes, .. }) => Ok((hash_bytes(bytes), bytes.len() as u64)),
            Some(_) => Err(SyncError::Conflict {
                path: clean,
                reason: "not a regular file".to_string(),
            }),
            None => Err(SyncError::Fs {
                path: Some(clean),
                detail: "not found".to_string(),
                kind: FsErrorKind::NotFound,
            }),
        }
    }

    fn open_live_read(&self, rel_path: &str) -> legion_sync::Result<Box<dyn Read + Send>> {
        let clean = validate_sync_path(rel_path)?;
        self.assert_platform_allowed(&clean, EntryKind::File, "open_live_read");
        let live = self.live.lock().unwrap();
        match self.lookup_key(&live, &clean).map(|(_, n)| n) {
            Some(Node::File { bytes, .. }) => Ok(Box::new(Cursor::new(bytes.clone()))),
            Some(_) => Err(SyncError::Conflict {
                path: clean,
                reason: "not a regular file".to_string(),
            }),
            None => Err(SyncError::Fs {
                path: Some(clean),
                detail: "not found".to_string(),
                kind: FsErrorKind::NotFound,
            }),
        }
    }

    fn create_dir_all_live(&self, rel_path: &str) -> legion_sync::Result<()> {
        let clean = validate_sync_path(rel_path)?;
        self.assert_platform_allowed(&clean, EntryKind::Dir, "create_dir_all_live");
        let mut live = self.live.lock().unwrap();
        self.assert_no_case_collision(&live, &clean, "create_dir_all_live");
        let mtime = Timestamp::new(0, 0);
        for anc in ancestors(&clean) {
            match live.get(anc) {
                Some(Node::Dir { .. }) => {}
                Some(_) => {
                    return Err(SyncError::Conflict {
                        path: clean,
                        reason: "ancestor is not a directory".to_string(),
                    });
                }
                None => {
                    live.insert(anc.to_string(), Node::Dir { mtime });
                }
            }
        }
        match live.get(&clean) {
            Some(Node::Dir { .. }) => Ok(()),
            Some(_) => Err(SyncError::Conflict {
                path: clean,
                reason: "non-directory exists".to_string(),
            }),
            None => {
                live.insert(clean, Node::Dir { mtime });
                Ok(())
            }
        }
    }

    fn list_dir_live(&self, rel_path: &str) -> legion_sync::Result<Vec<String>> {
        let clean = validate_sync_path(rel_path)?;
        self.assert_platform_allowed(&clean, EntryKind::Dir, "list_dir_live");
        let live = self.live.lock().unwrap();
        let prefix = format!("{clean}/");
        let mut out = Vec::new();
        for key in live.keys() {
            if let Some(rest) = key.strip_prefix(&prefix)
                && !rest.contains('/')
            {
                out.push(rest.to_string());
            }
        }
        out.sort();
        Ok(out)
    }

    fn remove_empty_dir_live(&self, rel_path: &str) -> legion_sync::Result<()> {
        let clean = validate_sync_path(rel_path)?;
        self.assert_platform_allowed(&clean, EntryKind::Dir, "remove_empty_dir_live");
        let mut live = self.live.lock().unwrap();
        let prefix = format!("{clean}/");
        if live.keys().any(|k| k.starts_with(&prefix)) {
            return Err(SyncError::Fs {
                path: Some(clean),
                detail: "directory not empty".to_string(),
                kind: FsErrorKind::Other,
            });
        }
        match live.get(&clean) {
            Some(Node::Dir { .. }) => {
                live.remove(&clean);
                Ok(())
            }
            Some(_) => Err(SyncError::Conflict {
                path: clean,
                reason: "not a directory".to_string(),
            }),
            None => Err(SyncError::Fs {
                path: Some(clean),
                detail: "not found".to_string(),
                kind: FsErrorKind::NotFound,
            }),
        }
    }

    fn create_symlink_live(&self, rel_path: &str, target: &str) -> legion_sync::Result<()> {
        let clean = validate_sync_path(rel_path)?;
        self.assert_platform_allowed(&clean, EntryKind::Symlink, "create_symlink_live");
        let mut live = self.live.lock().unwrap();
        self.assert_no_case_collision(&live, &clean, "create_symlink_live");
        let mtime = Timestamp::new(0, 0);
        for anc in ancestors(&clean) {
            live.entry(anc.to_string()).or_insert(Node::Dir { mtime });
        }
        live.insert(
            clean,
            Node::Symlink {
                target: target.to_string(),
                mtime,
            },
        );
        Ok(())
    }

    fn remove_symlink_live(&self, rel_path: &str) -> legion_sync::Result<()> {
        let clean = validate_sync_path(rel_path)?;
        self.assert_platform_allowed(&clean, EntryKind::Symlink, "remove_symlink_live");
        let mut live = self.live.lock().unwrap();
        match live.get(&clean) {
            Some(Node::Symlink { .. }) => {
                live.remove(&clean);
                Ok(())
            }
            Some(_) => Err(SyncError::Conflict {
                path: clean,
                reason: "not a symlink".to_string(),
            }),
            None => Err(SyncError::Fs {
                path: Some(clean),
                detail: "not found".to_string(),
                kind: FsErrorKind::NotFound,
            }),
        }
    }

    fn set_live_metadata(
        &self,
        rel_path: &str,
        mtime: Timestamp,
        mode: Option<u32>,
    ) -> legion_sync::Result<()> {
        let clean = validate_sync_path(rel_path)?;
        self.assert_platform_allowed(&clean, EntryKind::File, "set_live_metadata");
        let mut live = self.live.lock().unwrap();
        if let Some(Node::File {
            mtime: m, mode: md, ..
        }) = live.get_mut(&clean)
        {
            *m = mtime;
            if !self.caps.windows_naming {
                *md = mode;
            }
        }
        Ok(())
    }

    fn rename_live_to_live(&self, from_rel: &str, to_rel: &str) -> legion_sync::Result<()> {
        let clean_from = validate_sync_path(from_rel)?;
        let clean_to = validate_sync_path(to_rel)?;
        self.assert_platform_allowed(&clean_from, EntryKind::File, "rename_live_to_live(from)");
        self.assert_platform_allowed(&clean_to, EntryKind::File, "rename_live_to_live(to)");
        let mut live = self.live.lock().unwrap();
        self.assert_no_case_collision(&live, &clean_to, "rename_live_to_live");
        let node = live.remove(&clean_from).ok_or_else(|| SyncError::Fs {
            path: Some(clean_from),
            detail: "not found".to_string(),
            kind: FsErrorKind::NotFound,
        })?;
        for anc in ancestors(&clean_to) {
            live.entry(anc.to_string()).or_insert(Node::Dir {
                mtime: Timestamp::new(0, 0),
            });
        }
        live.insert(clean_to, node);
        Ok(())
    }

    fn create_tmp(&self, tmp_name: &str) -> legion_sync::Result<Box<dyn SyncWrite>> {
        validate_flat_internal_name(tmp_name)?;
        Ok(Box::new(StrictTmpWriter {
            tmp: Arc::clone(&self.tmp),
            tmp_name: tmp_name.to_string(),
            buf: Vec::new(),
        }))
    }

    fn stat_tmp(&self, tmp_name: &str) -> legion_sync::Result<Option<FsMeta>> {
        validate_flat_internal_name(tmp_name)?;
        Ok(self.tmp.lock().unwrap().get(tmp_name).map(|b| FsMeta {
            kind: EntryKind::File,
            size: b.len() as u64,
            mtime: Timestamp::new(0, 0),
            mode: None,
            link_target: None,
            change_id: None,
        }))
    }

    fn hash_tmp(&self, tmp_name: &str) -> legion_sync::Result<(Hash, u64)> {
        validate_flat_internal_name(tmp_name)?;
        let tmp = self.tmp.lock().unwrap();
        let bytes = tmp.get(tmp_name).ok_or_else(|| SyncError::Fs {
            path: Some(tmp_name.to_string()),
            detail: "not found".to_string(),
            kind: FsErrorKind::NotFound,
        })?;
        Ok((hash_bytes(bytes), bytes.len() as u64))
    }

    fn remove_tmp(&self, tmp_name: &str) -> legion_sync::Result<()> {
        validate_flat_internal_name(tmp_name)?;
        self.tmp.lock().unwrap().remove(tmp_name);
        Ok(())
    }

    fn list_tmp(&self) -> legion_sync::Result<Vec<String>> {
        Ok(self.tmp.lock().unwrap().keys().cloned().collect())
    }

    fn commit_tmp_to_live(&self, tmp_name: &str, live_rel: &str) -> legion_sync::Result<()> {
        validate_flat_internal_name(tmp_name)?;
        let clean = validate_sync_path(live_rel)?;
        self.assert_platform_allowed(&clean, EntryKind::File, "commit_tmp_to_live");
        let bytes = self
            .tmp
            .lock()
            .unwrap()
            .remove(tmp_name)
            .ok_or_else(|| SyncError::Fs {
                path: Some(tmp_name.to_string()),
                detail: "tmp not found".to_string(),
                kind: FsErrorKind::NotFound,
            })?;
        let mut live = self.live.lock().unwrap();
        self.assert_no_case_collision(&live, &clean, "commit_tmp_to_live");
        let mtime = Timestamp::new(0, 0);
        for anc in ancestors(&clean) {
            live.entry(anc.to_string()).or_insert(Node::Dir { mtime });
        }
        live.insert(
            clean,
            Node::File {
                bytes,
                mtime,
                mode: if self.caps.windows_naming {
                    None
                } else {
                    Some(0o644)
                },
                gen_id: self.alloc_gen(),
            },
        );
        Ok(())
    }

    fn move_live_to_bin(&self, live_rel: &str, bin_data_name: &str) -> legion_sync::Result<()> {
        let clean = validate_sync_path(live_rel)?;
        validate_flat_internal_name(bin_data_name)?;
        self.assert_platform_allowed(&clean, EntryKind::File, "move_live_to_bin");
        let mut live = self.live.lock().unwrap();
        match live.remove(&clean) {
            Some(Node::File { bytes, .. }) => {
                self.bin
                    .lock()
                    .unwrap()
                    .insert(bin_data_name.to_string(), bytes);
                Ok(())
            }
            Some(_) => Err(SyncError::Conflict {
                path: clean,
                reason: "not a regular file".to_string(),
            }),
            None => Err(SyncError::Fs {
                path: Some(clean),
                detail: "not found".to_string(),
                kind: FsErrorKind::NotFound,
            }),
        }
    }

    fn write_bin_meta(&self, meta_name: &str, bytes: &[u8]) -> legion_sync::Result<()> {
        validate_flat_internal_name(meta_name)?;
        self.bin
            .lock()
            .unwrap()
            .insert(meta_name.to_string(), bytes.to_vec());
        Ok(())
    }

    fn read_bin_meta(&self, meta_name: &str) -> legion_sync::Result<Option<Vec<u8>>> {
        validate_flat_internal_name(meta_name)?;
        Ok(self.bin.lock().unwrap().get(meta_name).cloned())
    }

    fn stat_bin(&self, bin_name: &str) -> legion_sync::Result<Option<FsMeta>> {
        validate_flat_internal_name(bin_name)?;
        Ok(self.bin.lock().unwrap().get(bin_name).map(|b| FsMeta {
            kind: EntryKind::File,
            size: b.len() as u64,
            mtime: Timestamp::new(0, 0),
            mode: None,
            link_target: None,
            change_id: None,
        }))
    }

    fn hash_bin(&self, bin_name: &str) -> legion_sync::Result<(Hash, u64)> {
        validate_flat_internal_name(bin_name)?;
        let bin = self.bin.lock().unwrap();
        let bytes = bin.get(bin_name).ok_or_else(|| SyncError::Fs {
            path: Some(bin_name.to_string()),
            detail: "not found".to_string(),
            kind: FsErrorKind::NotFound,
        })?;
        Ok((hash_bytes(bytes), bytes.len() as u64))
    }

    fn copy_bin_to_tmp(&self, bin_data_name: &str, tmp_name: &str) -> legion_sync::Result<()> {
        validate_flat_internal_name(bin_data_name)?;
        validate_flat_internal_name(tmp_name)?;
        let bytes = self
            .bin
            .lock()
            .unwrap()
            .get(bin_data_name)
            .cloned()
            .ok_or_else(|| SyncError::Fs {
                path: Some(bin_data_name.to_string()),
                detail: "not found".to_string(),
                kind: FsErrorKind::NotFound,
            })?;
        self.tmp.lock().unwrap().insert(tmp_name.to_string(), bytes);
        Ok(())
    }

    fn remove_bin_file(&self, bin_name: &str) -> legion_sync::Result<()> {
        validate_flat_internal_name(bin_name)?;
        self.bin.lock().unwrap().remove(bin_name);
        Ok(())
    }

    fn list_bin(&self) -> legion_sync::Result<Vec<String>> {
        Ok(self.bin.lock().unwrap().keys().cloned().collect())
    }
}

struct EnginePeer<'a, F: SyncFs, S: IndexStore> {
    engine: &'a SyncEngine<F, S>,
    caller: String,
    member_id: String,
}

impl<'a, F: SyncFs, S: IndexStore> EnginePeer<'a, F, S> {
    fn new(engine: &'a SyncEngine<F, S>, caller: &str) -> Self {
        let member_id = engine.meta().unwrap().local_member;
        Self {
            engine,
            caller: caller.to_string(),
            member_id,
        }
    }
}

impl<F: SyncFs, S: IndexStore> Peer for EnginePeer<'_, F, S> {
    fn member_id(&self) -> &str {
        &self.member_id
    }

    fn records_since(&self, since_seq: u64, limit: usize) -> legion_sync::Result<RecordsPage> {
        self.engine
            .serve_records_since(&self.caller, since_seq, limit)
    }

    fn fetch_file(
        &self,
        path: &str,
        expected_hash: Hash,
    ) -> legion_sync::Result<Box<dyn Read + Send>> {
        self.engine
            .serve_fetch_file(&self.caller, path, expected_hash)
    }
}

fn open_sim_engine(
    member: &str,
    members: &[&str],
    caps: FsCapabilities,
) -> SyncEngine<StrictPlatformFs, MemoryIndexStore> {
    let cfg = FolderConfig::new("docs", member, members.iter().map(|s| (*s).to_string()));
    SyncEngine::open(cfg, StrictPlatformFs::new(caps), MemoryIndexStore::new()).unwrap()
}

#[test]
fn simulated_windows_fs_holds_back_all_windows_rules_and_unblocks_cleanly() {
    let t0 = Timestamp::new(1_795_608_000, 0);
    let mut linux = open_sim_engine("tower", &["tower", "winbox"], FsCapabilities::unix());
    let mut winbox = open_sim_engine("winbox", &["tower", "winbox"], FsCapabilities::windows());

    // Safe entries that must sync to Windows.
    linux.fs().user_write_file("ok.txt", b"ok", t0);
    linux.fs().user_write_file("README.md", b"upper", t0);
    linux.fs().user_write_file("Docs/a.txt", b"docs-a", t0);

    // Every Windows naming, length, symlink, and case-clash hazard on the Unix peer.
    // `FsCapabilities::windows()` sets `max_path_len = 240`: parent dirs total 231 bytes (valid)
    // while the leaf path totals 245 bytes (> 240 -> PathTooLong).
    let long_path = format!("{}/{}/long-name.txt", "d".repeat(115), "e".repeat(115));
    let exact_max_path = "x".repeat(240); // exactly at Windows `max_path_len` (representable)
    let mut expected_problems: BTreeMap<String, ProblemKind> = BTreeMap::new();

    for (path, kind) in [
        // Reserved DOS device names (bare, with extension, nested, trailing-space stem)
        ("CON", ProblemKind::ReservedWindowsName),
        ("CON.txt", ProblemKind::ReservedWindowsName),
        ("nul.tar.gz", ProblemKind::ReservedWindowsName),
        ("sub/LPT1.log", ProblemKind::ReservedWindowsName),
        ("com9 .txt", ProblemKind::ReservedWindowsName),
        // Forbidden Windows characters + control character
        ("a:b.txt", ProblemKind::ForbiddenWindowsChar),
        ("a?b.txt", ProblemKind::ForbiddenWindowsChar),
        ("a*b.txt", ProblemKind::ForbiddenWindowsChar),
        ("a<b.txt", ProblemKind::ForbiddenWindowsChar),
        ("a>b.txt", ProblemKind::ForbiddenWindowsChar),
        ("a\"b.txt", ProblemKind::ForbiddenWindowsChar),
        ("a|b.txt", ProblemKind::ForbiddenWindowsChar),
        ("ctrl\x07.txt", ProblemKind::ForbiddenWindowsChar),
        // Trailing dot or space on leaf and on ancestor directory
        ("trailing.", ProblemKind::TrailingDotOrSpace),
        ("trailing ", ProblemKind::TrailingDotOrSpace),
        ("dir./nested.txt", ProblemKind::TrailingDotOrSpace),
        ("dir /nested.txt", ProblemKind::TrailingDotOrSpace),
        // Case clashes (leaf and ancestor directory)
        ("readme.md", ProblemKind::CaseClash),
        ("docs/b.txt", ProblemKind::CaseClash),
    ] {
        linux
            .fs()
            .user_write_file(path, format!("payload:{path}").as_bytes(), t0);
        expected_problems.insert(path.to_string(), kind);
    }

    // Ancestor directories created by `dir./nested.txt`, `dir /nested.txt`, `docs/b.txt`, and
    // `long_path` are scanned as Dir entries too; record their exact expected ProblemKinds.
    expected_problems.insert("dir.".to_string(), ProblemKind::TrailingDotOrSpace);
    expected_problems.insert("dir ".to_string(), ProblemKind::TrailingDotOrSpace);
    expected_problems.insert("docs".to_string(), ProblemKind::CaseClash);

    linux.fs().user_write_file(&long_path, b"too-long", t0);
    expected_problems.insert(long_path.clone(), ProblemKind::PathTooLong);

    linux.fs().user_symlink("sym", "ok.txt", t0);
    expected_problems.insert("sym".to_string(), ProblemKind::UnsupportedSymlink);

    // Also verify a 240-byte name (at `max_path_len`) is representable on Windows.
    linux.fs().user_write_file(&exact_max_path, b"max-ok", t0);

    linux.scan(t0).unwrap();

    let pull = winbox
        .pull_from_peer(&EnginePeer::new(&linux, "winbox"), t0)
        .unwrap();
    assert_eq!(pull.held_back, expected_problems.len());

    // Only representable entries were materialized on the strict simulated Windows filesystem.
    assert_eq!(
        winbox.fs().read_live_bytes("ok.txt").as_deref(),
        Some(b"ok".as_slice())
    );
    assert_eq!(
        winbox.fs().read_live_bytes("README.md").as_deref(),
        Some(b"upper".as_slice())
    );
    assert_eq!(
        winbox.fs().read_live_bytes("Docs/a.txt").as_deref(),
        Some(b"docs-a".as_slice())
    );
    assert_eq!(
        winbox.fs().read_live_bytes(&exact_max_path).as_deref(),
        Some(b"max-ok".as_slice())
    );
    for held_path in expected_problems.keys() {
        assert!(
            !winbox.fs().live_paths().contains(held_path),
            "held-back path '{held_path}' must not exist on simulated Windows FS"
        );
    }

    match winbox.status().unwrap().state {
        MemberState::Problem { sentence, problems } => {
            assert!(!sentence.is_empty());
            let actual: BTreeMap<String, ProblemKind> =
                problems.into_iter().map(|p| (p.path, p.kind)).collect();
            assert_eq!(actual, expected_problems);
        }
        other => panic!("expected Problem state, got {other:?}"),
    }

    // Delete every Windows-illegal entry and the clashing uppercase entries on Linux; Windows must
    // process every tombstone without ever touching illegal Win32 paths and unblock `readme.md`
    // and `docs/b.txt`.
    let t1 = Timestamp::new(t0.secs + 60, 0);
    for path in [
        "README.md",
        "Docs",
        "CON",
        "CON.txt",
        "nul.tar.gz",
        "sub/LPT1.log",
        "com9 .txt",
        "a:b.txt",
        "a?b.txt",
        "a*b.txt",
        "a<b.txt",
        "a>b.txt",
        "a\"b.txt",
        "a|b.txt",
        "ctrl\x07.txt",
        "trailing.",
        "trailing ",
        "dir.",
        "dir ",
        "sym",
        &long_path,
    ] {
        linux.fs().user_remove_recursive(path);
    }
    linux.scan(t1).unwrap();

    let pull2 = winbox
        .pull_from_peer(&EnginePeer::new(&linux, "winbox"), t1)
        .unwrap();
    assert_eq!(pull2.held_back, 0);
    assert_eq!(pull2.held_back_resolved, 3); // readme.md, docs, docs/b.txt
    assert_eq!(winbox.status().unwrap().state, MemberState::UpToDate);
    assert_eq!(
        winbox.fs().read_live_bytes("readme.md").as_deref(),
        Some(b"payload:readme.md".as_slice())
    );
    assert_eq!(
        winbox.fs().read_live_bytes("docs/b.txt").as_deref(),
        Some(b"payload:docs/b.txt".as_slice())
    );
}

#[test]
fn simulated_windows_fs_conflicts_and_macos_case_insensitive_rules() {
    let t0 = Timestamp::new(1_795_608_000, 0);
    let t1 = Timestamp::new(t0.secs + 60, 0);

    // 1. Concurrent edits where the conflict copy name exceeds Windows `max_path_len`: held back
    //    as `PathTooLong` rather than failing mid-apply on Win32.
    let tight_win_caps = FsCapabilities {
        max_path_len: 40,
        ..FsCapabilities::windows()
    };
    let mut linux = open_sim_engine("tower", &["tower", "winbox"], FsCapabilities::unix());
    let mut winbox = open_sim_engine("winbox", &["tower", "winbox"], tight_win_caps);

    let near_limit = "alpha/beta/report-2026.txt"; // 26 bytes (< 40), conflict copy is 55 bytes (> 40)
    linux.fs().user_write_file(near_limit, b"from-linux", t0);
    linux.scan(t0).unwrap();
    winbox.fs().user_write_file(near_limit, b"from-windows", t1);
    winbox.scan(t1).unwrap();

    let pull = winbox
        .pull_from_peer(&EnginePeer::new(&linux, "winbox"), t1)
        .unwrap();
    assert_eq!(pull.held_back, 1);
    assert_eq!(
        winbox.fs().read_live_bytes(near_limit).as_deref(),
        Some(b"from-windows".as_slice())
    );
    match winbox.status().unwrap().state {
        MemberState::Problem { problems, .. } => {
            assert_eq!(problems.len(), 1);
            assert_eq!(problems[0].kind, ProblemKind::PathTooLong);
        }
        other => panic!("expected Problem(PathTooLong), got {other:?}"),
    }

    // 2. Local Windows file `Notes.txt` collides with incoming Unix `notes.txt`: `notes.txt` is
    //    held back as `CaseClash` and never overwrites `Notes.txt`.
    winbox.fs().user_write_file("Notes.txt", b"win-notes", t1);
    winbox.scan(t1).unwrap();
    linux.fs().user_write_file("notes.txt", b"unix-notes", t1);
    linux.scan(t1).unwrap();

    let pull_case = winbox
        .pull_from_peer(&EnginePeer::new(&linux, "winbox"), t1)
        .unwrap();
    assert_eq!(pull_case.held_back, 1);
    assert_eq!(
        winbox.fs().read_live_bytes("Notes.txt").as_deref(),
        Some(b"win-notes".as_slice())
    );
    assert!(!winbox.fs().live_paths().contains(&"notes.txt".to_string()));

    // 3. Segment exceeding `max_segment_len` while total path stays within `max_path_len`: held
    //    back as `SegmentTooLong` without touching the simulated Windows filesystem.
    let seg_win_caps = FsCapabilities {
        max_segment_len: 32,
        max_path_len: 240,
        ..FsCapabilities::windows()
    };
    let mut seg_src = open_sim_engine("tower", &["tower", "winbox"], FsCapabilities::unix());
    let mut seg_win = open_sim_engine("winbox", &["tower", "winbox"], seg_win_caps);
    let long_seg_name = "s".repeat(40);
    seg_src.fs().user_write_file(&long_seg_name, b"seg", t0);
    seg_src.scan(t0).unwrap();
    let seg_pull = seg_win
        .pull_from_peer(&EnginePeer::new(&seg_src, "winbox"), t0)
        .unwrap();
    assert_eq!(seg_pull.held_back, 1);
    assert!(!seg_win.fs().live_paths().contains(&long_seg_name));
    match seg_win.status().unwrap().state {
        MemberState::Problem { problems, .. } => {
            assert_eq!(problems.len(), 1);
            assert_eq!(problems[0].path, long_seg_name);
            assert_eq!(problems[0].kind, ProblemKind::SegmentTooLong);
        }
        other => panic!("expected Problem(SegmentTooLong), got {other:?}"),
    }

    // 4. Case-insensitive macOS allows Windows-forbidden names, reserved names, trailing dots,
    //    and symlinks, while still holding back case clashes.
    let mut mac_source = open_sim_engine("tower", &["tower", "mac"], FsCapabilities::unix());
    let mut mac = open_sim_engine(
        "mac",
        &["tower", "mac"],
        FsCapabilities::macos_case_insensitive(),
    );
    mac_source.fs().user_write_file("CON.txt", b"con", t0);
    mac_source.fs().user_write_file("a:b.txt", b"colon", t0);
    mac_source.fs().user_write_file("trailing.", b"dot", t0);
    mac_source.fs().user_symlink("sym", "CON.txt", t0);
    mac_source.fs().user_write_file("README.md", b"upper", t0);
    mac_source.fs().user_write_file("readme.md", b"lower", t0);
    mac_source.scan(t0).unwrap();

    let mac_pull = mac
        .pull_from_peer(&EnginePeer::new(&mac_source, "mac"), t0)
        .unwrap();
    assert_eq!(mac_pull.held_back, 1);
    assert_eq!(
        mac.fs().read_live_bytes("CON.txt").as_deref(),
        Some(b"con".as_slice())
    );
    assert_eq!(
        mac.fs().read_live_bytes("a:b.txt").as_deref(),
        Some(b"colon".as_slice())
    );
    assert_eq!(
        mac.fs().read_live_bytes("trailing.").as_deref(),
        Some(b"dot".as_slice())
    );
    assert!(mac.fs().live_paths().contains(&"sym".to_string()));
    match mac.status().unwrap().state {
        MemberState::Problem { problems, .. } => {
            assert_eq!(problems.len(), 1);
            assert_eq!(problems[0].path, "readme.md");
            assert_eq!(problems[0].kind, ProblemKind::CaseClash);
        }
        other => panic!("expected Problem(CaseClash), got {other:?}"),
    }
}
