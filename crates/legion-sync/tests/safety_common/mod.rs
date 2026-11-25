//! Shared test harness for real-disk safety tests: a `SyncFs` over `RealFs` that runs "person"
//! actions on the real disk at exact moments, plus accounting helpers.
#![allow(dead_code, clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

use legion_sync::{
    FetchOptions, FolderConfig, FolderMarker, FsCapabilities, FsErrorKind, FsMeta, FsWalkResult,
    IgnoreMatcher, IndexStore, Peer, PeerHandshake, RealFs, RecordsPage, RedbIndexStore,
    SyncEngine, SyncError, SyncFs, SyncWrite,
};
use legion_xfer::{Hash, Timestamp};

pub fn scratch() -> tempfile::TempDir {
    let base = PathBuf::from(std::env::var("HOME").unwrap()).join(".cache/legion-sync-tests");
    fs::create_dir_all(&base).unwrap();
    tempfile::Builder::new()
        .prefix("t")
        .tempdir_in(base)
        .unwrap()
}

pub struct EnginePeer<'a, F: SyncFs, S: IndexStore> {
    pub engine: &'a SyncEngine<F, S>,
    pub caller: String,
    pub member_id: String,
}

impl<'a, F: SyncFs, S: IndexStore> EnginePeer<'a, F, S> {
    pub fn new(engine: &'a SyncEngine<F, S>, caller: &str) -> Self {
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
    fn handshake(
        &self,
        caller: &legion_sync::HandshakeHello,
    ) -> legion_sync::Result<PeerHandshake> {
        self.engine.serve_handshake(&self.caller, caller)
    }
    fn records_since(
        &self,
        since_incarnation: u64,
        since_seq: u64,
        limit: legion_sync::PageLimit,
        _cancel: Option<&AtomicBool>,
    ) -> legion_sync::Result<RecordsPage> {
        self.engine
            .serve_records_since(&self.caller, since_incarnation, since_seq, limit)
    }
    fn fetch_file(
        &self,
        path: &str,
        expected_hash: Hash,
        options: &FetchOptions,
    ) -> legion_sync::Result<Box<dyn Read + Send>> {
        self.engine
            .serve_fetch_file_range(&self.caller, path, expected_hash, options.offset)
    }
    fn report_hash_mismatch(&self, path: &str) {
        self.engine.note_serve_hash_mismatch(path);
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum When {
    Before,
    After,
}

pub enum Act {
    Continue,
    Crash,
    Err(FsErrorKind),
}

pub struct Hook {
    pub op: &'static str,
    pub when: When,
    pub nth: usize,
    pub action: Box<dyn FnMut() -> Act>,
    pub fired: bool,
}

pub type ReplaceBackupHook = Box<dyn FnMut(&RealFs, &str, &str, &str) -> legion_sync::Result<bool>>;

/// `RealFs` wrapper that fires person actions on the real disk at exact operation boundaries.
pub struct HookFs {
    pub inner: RealFs,
    pub hooks: RefCell<Vec<Hook>>,
    pub counts: RefCell<BTreeMap<&'static str, usize>>,
    pub log: RefCell<Vec<String>>,
    pub no_exchange: bool,
    pub no_replace_backup: bool,
    pub simulate_replace_with_backup: bool,
    pub capabilities_override: Option<FsCapabilities>,
    pub replace_backup_override: RefCell<Option<ReplaceBackupHook>>,
}

impl HookFs {
    pub fn new(root: &Path) -> Self {
        Self {
            inner: RealFs::open(root).unwrap(),
            hooks: RefCell::new(Vec::new()),
            counts: RefCell::new(BTreeMap::new()),
            log: RefCell::new(Vec::new()),
            no_exchange: false,
            no_replace_backup: false,
            simulate_replace_with_backup: false,
            capabilities_override: None,
            replace_backup_override: RefCell::new(None),
        }
    }
    pub fn add(
        &self,
        op: &'static str,
        when: When,
        nth: usize,
        action: impl FnMut() -> Act + 'static,
    ) {
        self.hooks.borrow_mut().push(Hook {
            op,
            when,
            nth,
            action: Box::new(action),
            fired: false,
        });
    }
    pub fn set_replace_backup_override(
        &self,
        f: impl FnMut(&RealFs, &str, &str, &str) -> legion_sync::Result<bool> + 'static,
    ) {
        *self.replace_backup_override.borrow_mut() = Some(Box::new(f));
    }
    pub fn clear_replace_backup_override(&self) {
        *self.replace_backup_override.borrow_mut() = None;
    }
    pub fn clear(&self) {
        self.hooks.borrow_mut().clear();
        self.counts.borrow_mut().clear();
        *self.replace_backup_override.borrow_mut() = None;
    }
    pub fn all_fired(&self) -> bool {
        self.hooks.borrow().iter().all(|h| h.fired)
    }
    fn fire(&self, op: &'static str, when: When, n: usize) -> legion_sync::Result<()> {
        let mut hooks = self.hooks.borrow_mut();
        for h in hooks.iter_mut() {
            if h.op == op && h.when == when && h.nth == n && !h.fired {
                h.fired = true;
                self.log
                    .borrow_mut()
                    .push(format!("HOOK {op} {when:?} #{n}"));
                match (h.action)() {
                    Act::Continue => {}
                    Act::Crash => {
                        return Err(SyncError::Fs {
                            path: Some(op.to_string()),
                            detail: "review injected crash".to_string(),
                            kind: FsErrorKind::Crash,
                        });
                    }
                    Act::Err(kind) => {
                        return Err(SyncError::Fs {
                            path: Some(op.to_string()),
                            detail: format!("review injected fs error ({kind:?})"),
                            kind,
                        });
                    }
                }
            }
        }
        Ok(())
    }
    fn w<T>(
        &self,
        op: &'static str,
        arg: &str,
        f: impl FnOnce(&RealFs) -> legion_sync::Result<T>,
    ) -> legion_sync::Result<T> {
        let n = {
            let mut c = self.counts.borrow_mut();
            let e = c.entry(op).or_insert(0);
            *e += 1;
            *e
        };
        self.log.borrow_mut().push(format!("{op}({arg}) #{n}"));
        self.fire(op, When::Before, n)?;
        let r = f(&self.inner);
        if r.is_ok() {
            self.fire(op, When::After, n)?;
        }
        r
    }
    fn w_bool(
        &self,
        op: &'static str,
        arg: &str,
        f: impl FnOnce(&RealFs) -> legion_sync::Result<bool>,
    ) -> legion_sync::Result<bool> {
        let n = {
            let mut c = self.counts.borrow_mut();
            let e = c.entry(op).or_insert(0);
            *e += 1;
            *e
        };
        self.log.borrow_mut().push(format!("{op}({arg}) #{n}"));
        self.fire(op, When::Before, n)?;
        let r = f(&self.inner);
        if matches!(r, Ok(true)) {
            self.fire(op, When::After, n)?;
        }
        r
    }
}

impl SyncFs for HookFs {
    fn capabilities(&self) -> FsCapabilities {
        self.capabilities_override
            .unwrap_or_else(|| self.inner.capabilities())
    }
    fn read_marker(&self) -> legion_sync::Result<Option<FolderMarker>> {
        self.w("read_marker", "", |f| f.read_marker())
    }
    fn write_marker(&self, marker: &FolderMarker) -> legion_sync::Result<()> {
        self.w("write_marker", "", |f| f.write_marker(marker))
    }
    fn append_deletion_log(&self, bytes: &[u8]) -> legion_sync::Result<()> {
        self.inner.append_deletion_log(bytes)
    }
    fn read_deletion_log(&self) -> legion_sync::Result<Vec<u8>> {
        self.inner.read_deletion_log()
    }
    fn compact_deletion_log(&self, bytes: &[u8]) -> legion_sync::Result<()> {
        self.inner.compact_deletion_log(bytes)
    }
    fn ensure_internal_dirs(&self) -> legion_sync::Result<()> {
        self.inner.ensure_internal_dirs()
    }
    fn check_not_nested(&self) -> legion_sync::Result<()> {
        self.inner.check_not_nested()
    }
    fn walk(&self, ignore: &IgnoreMatcher) -> legion_sync::Result<FsWalkResult> {
        self.w("walk", "", |f| f.walk(ignore))
    }
    fn walk_subtree(
        &self,
        rel_root: &str,
        ignore: &IgnoreMatcher,
    ) -> legion_sync::Result<FsWalkResult> {
        self.w("walk_subtree", rel_root, |f| {
            f.walk_subtree(rel_root, ignore)
        })
    }
    fn stat_live(&self, p: &str) -> legion_sync::Result<Option<FsMeta>> {
        self.w("stat_live", p, |f| f.stat_live(p))
    }
    fn hash_live(&self, p: &str) -> legion_sync::Result<(Hash, u64)> {
        self.w("hash_live", p, |f| f.hash_live(p))
    }
    fn open_live_read_at(&self, p: &str, offset: u64) -> legion_sync::Result<Box<dyn Read + Send>> {
        self.inner.open_live_read_at(p, offset)
    }
    fn create_dir_all_live(&self, p: &str) -> legion_sync::Result<()> {
        self.w("create_dir_all_live", p, |f| f.create_dir_all_live(p))
    }
    fn list_dir_live(&self, p: &str) -> legion_sync::Result<Vec<String>> {
        self.w("list_dir_live", p, |f| f.list_dir_live(p))
    }
    fn remove_empty_dir_live(&self, p: &str) -> legion_sync::Result<()> {
        self.w("remove_empty_dir_live", p, |f| f.remove_empty_dir_live(p))
    }
    fn create_symlink_live(&self, p: &str, target: &str) -> legion_sync::Result<()> {
        self.w("create_symlink_live", p, |f| {
            f.create_symlink_live(p, target)
        })
    }
    fn remove_symlink_live(&self, p: &str) -> legion_sync::Result<()> {
        self.w("remove_symlink_live", p, |f| f.remove_symlink_live(p))
    }
    fn set_live_metadata(
        &self,
        p: &str,
        mtime: Timestamp,
        mode: Option<u32>,
    ) -> legion_sync::Result<()> {
        self.w("set_live_metadata", p, |f| {
            f.set_live_metadata(p, mtime, mode)
        })
    }
    fn rename_live_to_live_no_replace(&self, a: &str, b: &str) -> legion_sync::Result<()> {
        self.w("rename_live_to_live_no_replace", a, |f| {
            f.rename_live_to_live_no_replace(a, b)
        })
    }
    fn sync_parent_dir_live(&self, p: &str) -> legion_sync::Result<()> {
        self.inner.sync_parent_dir_live(p)
    }
    fn sync_tmp_dir(&self) -> legion_sync::Result<()> {
        self.inner.sync_tmp_dir()
    }
    fn create_tmp(&self, t: &str) -> legion_sync::Result<Box<dyn SyncWrite>> {
        self.w("create_tmp", t, |f| f.create_tmp(t))
    }
    fn open_tmp_append(&self, t: &str) -> legion_sync::Result<Box<dyn SyncWrite>> {
        self.w("open_tmp_append", t, |f| f.open_tmp_append(t))
    }
    fn stat_tmp(&self, t: &str) -> legion_sync::Result<Option<FsMeta>> {
        self.w("stat_tmp", t, |f| f.stat_tmp(t))
    }
    fn set_tmp_metadata(
        &self,
        t: &str,
        mtime: Timestamp,
        mode: Option<u32>,
    ) -> legion_sync::Result<()> {
        self.w("set_tmp_metadata", t, |f| {
            f.set_tmp_metadata(t, mtime, mode)
        })
    }
    fn hash_tmp(&self, t: &str) -> legion_sync::Result<(Hash, u64)> {
        self.w("hash_tmp", t, |f| f.hash_tmp(t))
    }
    fn remove_tmp(&self, t: &str) -> legion_sync::Result<()> {
        self.w("remove_tmp", t, |f| f.remove_tmp(t))
    }
    fn list_tmp(&self) -> legion_sync::Result<Vec<String>> {
        self.w("list_tmp", "", |f| f.list_tmp())
    }
    fn commit_tmp_to_live_no_replace(&self, t: &str, p: &str) -> legion_sync::Result<()> {
        self.w("commit_tmp_to_live_no_replace", p, |f| {
            f.commit_tmp_to_live_no_replace(t, p)
        })
    }
    fn commit_tmp_to_live_no_replace_deferred_dir_sync(
        &self,
        t: &str,
        p: &str,
    ) -> legion_sync::Result<()> {
        self.w("commit_tmp_to_live_no_replace_deferred", p, |f| {
            f.commit_tmp_to_live_no_replace_deferred_dir_sync(t, p)
        })
    }
    fn exchange_tmp_with_live(&self, t: &str, p: &str) -> legion_sync::Result<bool> {
        if self.no_exchange {
            return Ok(false);
        }
        self.w_bool("exchange_tmp_with_live", p, |f| {
            f.exchange_tmp_with_live(t, p)
        })
    }
    fn replace_live_with_tmp_backup(&self, t: &str, p: &str, b: &str) -> legion_sync::Result<bool> {
        if self.no_replace_backup {
            return Ok(false);
        }
        self.w_bool("replace_live_with_tmp_backup", p, |f| {
            if let Some(cb) = self.replace_backup_override.borrow_mut().as_mut() {
                return cb(f, t, p, b);
            }
            if self.simulate_replace_with_backup {
                f.move_live_to_tmp_no_replace(p, b)?;
                f.commit_tmp_to_live_no_replace(t, p)?;
                return Ok(true);
            }
            f.replace_live_with_tmp_backup(t, p, b)
        })
    }
    fn move_live_to_tmp_no_replace(&self, p: &str, t: &str) -> legion_sync::Result<()> {
        self.w("move_live_to_tmp_no_replace", p, |f| {
            f.move_live_to_tmp_no_replace(p, t)
        })
    }
    fn move_tmp_to_bin_no_replace(&self, t: &str, b: &str) -> legion_sync::Result<()> {
        self.w("move_tmp_to_bin_no_replace", t, |f| {
            f.move_tmp_to_bin_no_replace(t, b)
        })
    }
    fn move_live_to_bin_no_replace(&self, p: &str, b: &str) -> legion_sync::Result<()> {
        self.w("move_live_to_bin_no_replace", p, |f| {
            f.move_live_to_bin_no_replace(p, b)
        })
    }
    fn write_bin_meta(&self, m: &str, bytes: &[u8]) -> legion_sync::Result<()> {
        self.w("write_bin_meta", m, |f| f.write_bin_meta(m, bytes))
    }
    fn read_bin_meta(&self, m: &str) -> legion_sync::Result<Option<Vec<u8>>> {
        self.inner.read_bin_meta(m)
    }
    fn stat_bin(&self, b: &str) -> legion_sync::Result<Option<FsMeta>> {
        self.inner.stat_bin(b)
    }
    fn hash_bin(&self, b: &str) -> legion_sync::Result<(Hash, u64)> {
        self.inner.hash_bin(b)
    }
    fn copy_bin_to_tmp(&self, b: &str, t: &str) -> legion_sync::Result<()> {
        self.w("copy_bin_to_tmp", b, |f| f.copy_bin_to_tmp(b, t))
    }
    fn remove_bin_file(&self, b: &str) -> legion_sync::Result<()> {
        self.w("remove_bin_file", b, |f| f.remove_bin_file(b))
    }
    fn list_bin(&self) -> legion_sync::Result<Vec<String>> {
        self.inner.list_bin()
    }
    fn available_space_for(&self, rel_path: &str) -> legion_sync::Result<Option<u64>> {
        self.inner.available_space_for(rel_path)
    }
    fn needs_local_staging(&self, rel_path: &str) -> legion_sync::Result<bool> {
        self.inner.needs_local_staging(rel_path)
    }
    fn move_bin_to_live_no_replace(&self, b: &str, p: &str) -> legion_sync::Result<()> {
        self.w("move_bin_to_live_no_replace", p, |f| {
            f.move_bin_to_live_no_replace(b, p)
        })
    }
}

pub fn cfg(member: &str, members: &[&str]) -> FolderConfig {
    FolderConfig::new("docs", member, members.iter().map(|s| (*s).to_string()))
}

pub fn open_real(
    root: &Path,
    db: &Path,
    member: &str,
    members: &[&str],
) -> SyncEngine<RealFs, RedbIndexStore> {
    SyncEngine::open(
        cfg(member, members),
        RealFs::open(root).unwrap(),
        RedbIndexStore::open(db).unwrap(),
    )
    .unwrap()
}

pub fn open_hook(
    root: &Path,
    db: &Path,
    member: &str,
    members: &[&str],
) -> SyncEngine<HookFs, RedbIndexStore> {
    SyncEngine::open(
        cfg(member, members),
        HookFs::new(root),
        RedbIndexStore::open(db).unwrap(),
    )
    .unwrap()
}

/// Every regular file's bytes under `root` (live tree, bin and tmp), keyed by where it lives.
pub fn all_bytes(root: &Path) -> Vec<(String, Vec<u8>)> {
    let mut out = Vec::new();
    fn rec(base: &Path, dir: &Path, out: &mut Vec<(String, Vec<u8>)>) {
        let Ok(rd) = fs::read_dir(dir) else { return };
        for e in rd.flatten() {
            let p = e.path();
            let ft = e.file_type().unwrap();
            if ft.is_dir() {
                rec(base, &p, out);
            } else if ft.is_file() {
                let norm = p.to_string_lossy().replace('\\', "/");
                if p.extension().is_some_and(|x| x == "json") && norm.contains(".legion-sync/bin") {
                    continue;
                }
                if norm.ends_with("marker.json") {
                    continue;
                }
                let rel = p
                    .strip_prefix(base)
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/");
                out.push((rel, fs::read(&p).unwrap_or_default()));
            }
        }
    }
    rec(root, root, &mut out);
    out
}

/// Returns where `content` survives under `root` (live, bin or tmp paths).
pub fn where_is(root: &Path, content: &[u8]) -> Vec<String> {
    let mut out = Vec::new();
    let target_len = content.len() as u64;
    let prefix_len = content.len().min(64);
    let prefix = &content[..prefix_len];
    fn rec(
        base: &Path,
        dir: &Path,
        content: &[u8],
        target_len: u64,
        prefix: &[u8],
        out: &mut Vec<String>,
    ) {
        let Ok(rd) = fs::read_dir(dir) else { return };
        for e in rd.flatten() {
            let p = e.path();
            let Ok(ft) = e.file_type() else { continue };
            if ft.is_dir() {
                rec(base, &p, content, target_len, prefix, out);
            } else if ft.is_file() {
                let norm = p.to_string_lossy().replace('\\', "/");
                if p.extension().is_some_and(|x| x == "json") && norm.contains(".legion-sync/bin") {
                    continue;
                }
                if norm.ends_with("marker.json") {
                    continue;
                }
                let Ok(meta) = e.metadata() else { continue };
                if meta.len() != target_len {
                    continue;
                }
                if !prefix.is_empty() {
                    let Ok(mut f) = fs::File::open(&p) else {
                        continue;
                    };
                    let mut head = [0u8; 64];
                    if f.read_exact(&mut head[..prefix.len()]).is_err()
                        || &head[..prefix.len()] != prefix
                    {
                        continue;
                    }
                }
                if fs::read(&p).is_ok_and(|b| b == content) {
                    let rel = p
                        .strip_prefix(base)
                        .unwrap()
                        .to_string_lossy()
                        .replace('\\', "/");
                    out.push(rel);
                }
            }
        }
    }
    rec(root, root, content, target_len, prefix, &mut out);
    out
}

pub fn dump(root: &Path) -> String {
    let mut s = String::new();
    for (p, b) in all_bytes(root) {
        s.push_str(&format!(
            "  {p} = {:?}\n",
            String::from_utf8_lossy(&b[..b.len().min(60)])
        ));
    }
    s
}

pub fn write_via_rename(path: &Path, bytes: &[u8]) {
    let tmp = path.with_file_name(format!(
        ".{}.swp",
        path.file_name().unwrap().to_string_lossy()
    ));
    fs::write(&tmp, bytes).unwrap();
    fs::rename(&tmp, path).unwrap();
}

pub fn set_of(v: &[&str]) -> BTreeSet<String> {
    v.iter().map(|s| (*s).to_string()).collect()
}

pub fn ts(secs: i64) -> Timestamp {
    Timestamp::new(1_795_608_000 + secs, 0)
}
