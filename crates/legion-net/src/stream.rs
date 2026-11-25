//! `legion stream`: the pipe bulk file data moves over.
//!
//! JSON carries control and never bulk bytes, so a transfer opens a second pipe per operation. Its
//! first line on stdin is one JSON object, the header, saying what to do; raw bytes then move in the
//! direction the operation needs. The remote command line is always the fixed `legion stream`, and
//! every path and value rides inside the header, so nothing a caller supplies is ever quoted for a
//! remote shell.
//!
//! Every header carries `protocol`, the version of this exchange it speaks, and a node's first
//! line, before any reply or refusal, is `{"protocol": N}`, the version it speaks. A node that
//! does not speak a header's version, or does not know the operation, answers `unsupported` and
//! names what it speaks; a reader refuses a node whose first line does not say, before it sends
//! anything that relies on the node holding to this version. Two nodes of different ages fail
//! plainly rather than misread each other.
//!
//! After that line every reply starts with one JSON line, so a client can skip a login banner
//! before it and tell a refusal (`{"error": …}`) from data. While an operation works on something
//! long (hashing a large file, listing a large tree, verifying what it received), it writes
//! `{"busy": true}` every [`HEARTBEAT`] where a line is awaited, so a reader can give up on
//! silence without giving up on work:
//!
//! | `op` | Header | Reply |
//! | --- | --- | --- |
//! | `manifest` | `{root, checksum?, exclude?}` | one line per entry, then `{done, entries, skipped}` |
//! | `stat` | `{path, checksum?, if_size?}` | `{kind, size, mode?, hash?, mtime?, partial?, …}` |
//! | `get` | `{path, offset?}` | `{size}`, then the bytes from `offset` to the end |
//! | `put` | `{path, size, hash, offset?, mode?, mtime?}` | `{offset}`; reads the rest; `{ok, size, hash}` |
//! | `pack` | `{root, names}` | `{ready, skipped}`, then a tar stream of those names |
//! | `unpack` | `{root, entries}` | `{ready}`; reads a tar stream to its end; `{ok, files, …}` |
//! | `delete` | `{root, names}` | `{removed, skipped}` |
//!
//! Each operation is authorised as the file tool it amounts to would be (`fs.read` for the first
//! four reads, `fs.write` for `put` and `unpack`, `fs.remove` for `delete`), every path goes through
//! [`Grant::check_path`], and each is written to the audit log with the door `stream`. The same
//! code serves the ends of a transfer that are on the node running it, so a local end is held to
//! exactly what a remote one is.
//!
//! A failure before any raw bytes were written is one `{"error": …}` line; after that the pipe just
//! ends and the reason goes to stderr. A receiver that refuses replies and returns at once, which
//! closes its input, so a sender writing to it is never left blocked.

use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::ffi::{OsStr, OsString};
use std::fs::{self, FileTimes};
use std::io::{self, BufRead, BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::guard::{Expected, Guarded, Watch};
use legion_core::audit::{self, AuditLog, Entry as AuditEntry, Outcome};
use legion_core::config::ConfigStore;
use legion_core::policy::Policy;
use legion_core::{Access, CancellationToken, CreateKind, Door, Grant, Paths};
use legion_os::atomic::{
    FileIdentity, WriteOptions, file_identity, symlink_file_identity, write_atomic,
};
use legion_proto::{Effect, ErrorBody, ErrorCode, ToolSpec};
use legion_tools::device::registry::DevicesFile;
use legion_tools::xfer::{error as xfer_error, manifest, resolve_root};
pub use legion_xfer::recover_pending_renames;
use legion_xfer::{
    CaseCollision, Entry, EntryKind, Hash, IfExists, ManifestOptions, PlacedFileMeta, PutSession,
    RenameRecoveryScope, RenamedEntry, SkippedEntry, Timestamp, case_insensitive,
    case_insensitive_in_dir, directory_contains_exact_name, move_no_replace_safe,
    pending_renames_dir, probe_dir_name_behavior, recover_pending_renames_checked,
    rename_intent_key, resolve_relative_path, unicode_normalizing, unicode_normalizing_in_dir,
    visible_rename_temp_name,
};
pub(crate) use legion_xfer::{
    FsNameBehavior, PendingRenameIntent, SameEntryDecision, classify_same_entry,
    names_fold_equivalent,
};
#[cfg(test)]
use legion_xfer::{PendingRenameRecovery, decide_pending_rename_recovery};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// The version of this exchange: a header names it, and a node refuses one it does not speak.
pub const STREAM_PROTOCOL: u32 = 1;

/// Stream capability: destination honours `if_exists` (`fail`, `skip`, `replace`, `rename`).
pub const CAP_IF_EXISTS: &str = "if_exists";
/// Stream capability: node supports `lock` (ordered exclusive move locks in the node's state dir).
pub const CAP_LOCK: &str = "lock";
/// Stream capability: node supports safe `move` (`delete` with `dry_run`, `remove_root`,
/// `expect_hashes`, `expect_root_hash`, `forbid_file_id`, stored-file hash verification in
/// `unpack`, and `file_id` reporting).
pub const CAP_MOVE: &str = "move";
/// Stream capability: node supports `probe_write` / `probe_read` directory distinctness probes.
pub const CAP_PROBE: &str = "probe";
/// Stream capability: node supports recoverable same-entry case/normalization `rename`.
pub const CAP_RENAME: &str = "rename";

/// Capabilities advertised in this node's stream greeting line.
pub const STREAM_CAPABILITIES: [&str; 5] =
    [CAP_IF_EXISTS, CAP_LOCK, CAP_MOVE, CAP_PROBE, CAP_RENAME];

/// How often an operation at work on something long says so, where a reply line is awaited.
pub const HEARTBEAT: Duration = Duration::from_secs(10);

/// How many streamed bytes pass between live permission and device-revocation re-checks.
const RECHECK_INTERVAL_BYTES: u64 = 4 * 1024 * 1024;

/// Maximum wall-clock time between live permission and device-revocation re-checks, so slow or
/// stalled streams also stop promptly after revocation.
const RECHECK_INTERVAL_TIME: Duration = Duration::from_secs(2);

/// Watchdog polling cadence while waiting on stalled stream I/O.
const RECHECK_POLL_INTERVAL: Duration = Duration::from_millis(250);

/// Monotonic sequence number for unique sibling probe/rename temp names.
static STREAM_TEMP_SEQ: AtomicU64 = AtomicU64::new(0);

/// The operations this node knows, as `unsupported` names them.
const OPS: [&str; 11] = [
    "manifest",
    "stat",
    "get",
    "put",
    "pack",
    "unpack",
    "delete",
    "probe_write",
    "probe_read",
    "rename",
    "lock",
];

/// The longest header read. A header carries the names a `pack` or `delete` acts on, which a
/// transfer sends in batches well below this; anything longer is a client gone wrong.
pub const LONGEST_HEADER: usize = 64 * 1024 * 1024;

/// How much output is gathered before it is written: a file's bytes go out in large writes, and
/// a listing's lines are not written one by one.
const OUTPUT_BUFFER: usize = 64 * 1024;

/// How much of the input an operation reads past what it took before it stops: the padding a tar
/// writer adds after an archive is a few blocks, and a sender that goes on past this has its pipe
/// closed on it rather than being read forever.
const MOST_TRAILING: u64 = 1024 * 1024;

/// How many chunks of input wait for an operation that reads it on its own thread.
const QUEUED_CHUNKS: usize = 4;

/// The node a stream is served for: where its files are, and the name its errors and audit lines
/// carry.
#[derive(Clone, Debug)]
pub struct Host {
    paths: Paths,
    name: String,
}

impl Host {
    /// A host over these directories, called `name`.
    pub fn new(paths: Paths, name: impl Into<String>) -> Host {
        Host {
            paths,
            name: name.into(),
        }
    }

    /// The node's name.
    pub fn name(&self) -> &str {
        &self.name
    }
}

/// The header: what to do, and optionally whom to do it as.
#[derive(Debug, Deserialize)]
struct Header {
    #[serde(rename = "as", default)]
    as_principal: Option<String>,
    #[serde(flatten)]
    op: Op,
}

/// One operation, as its header names it.
#[derive(Debug, Deserialize)]
#[serde(tag = "op", rename_all = "lowercase")]
pub(crate) enum Op {
    Manifest {
        root: String,
        #[serde(default)]
        checksum: bool,
        #[serde(default)]
        exclude: Vec<String>,
    },
    Stat {
        path: String,
        #[serde(default)]
        checksum: bool,
        #[serde(default)]
        if_size: Option<u64>,
        #[serde(default)]
        if_hash: Option<String>,
    },
    Get {
        path: String,
        #[serde(default)]
        offset: u64,
    },
    /// A sender's `offset` is not read: what is on disk decides where it starts. `mtime`, the
    /// source's modification time, is given to the file once it is in place, so a tree that holds
    /// it later finds it unchanged.
    Put {
        path: String,
        size: u64,
        hash: String,
        #[serde(default)]
        mode: Option<u32>,
        #[serde(default)]
        mtime: Option<Timestamp>,
        #[serde(default)]
        if_exists: IfExists,
    },
    Pack {
        root: String,
        names: Vec<String>,
    },
    /// `names` are the entries planned for this archive; anything else in it is refused, so a
    /// source can only deliver what the destination agreed to take.
    /// `entries` are what the destination agreed to take, as the source listed them; the
    /// archive must carry each of them once, as listed, and nothing else.
    Unpack {
        root: String,
        entries: Vec<Planned>,
        #[serde(default)]
        if_exists: IfExists,
    },
    Delete {
        root: String,
        #[serde(default)]
        names: Vec<String>,
        /// Check permissions and protected-tree rules without removing anything.
        #[serde(default)]
        dry_run: bool,
        /// Also remove `root` itself (a file, symlink, or now-empty directory) after `names`.
        #[serde(default)]
        remove_root: bool,
        /// When present, unlink a named regular file only if its current BLAKE3 hash still matches.
        #[serde(default)]
        expect_hashes: HashMap<String, Hash>,
        /// When present with `remove_root`, unlink `root` only if its current BLAKE3 hash matches.
        #[serde(default)]
        expect_root_hash: Option<Hash>,
        /// When present with `remove_root`, refuse to unlink `root` if its current `(dev, ino)`
        /// matches this destination file identity.
        #[serde(default)]
        forbid_file_id: Option<(u64, u64)>,
    },
    /// Create or remove a uniquely named empty marker file in a directory to prove whether two
    /// move endpoints refer to distinct directories (even across fleet aliases or network shares).
    #[serde(rename = "probe_write")]
    ProbeWrite {
        action: ProbeWriteAction,
        dir: String,
        #[serde(default)]
        marker: Option<String>,
        /// When `true` with `action = "create"`, walk up to the nearest existing ancestor
        /// directory of `dir` when `dir` itself does not exist yet.
        #[serde(default)]
        nearest_ancestor: bool,
        /// When `true` with `action = "create"`, keep the stream open after replying, hold an
        /// ownership flock under `<state_dir>/xfer/temps/`, and delete the marker automatically
        /// when the caller closes stdin, sends `"remove"`, or times out.
        #[serde(default)]
        hold: bool,
    },
    /// Check whether a probe marker file appears in `dir` or any of its named subdirectories.
    #[serde(rename = "probe_read")]
    ProbeRead {
        dir: String,
        marker: String,
        #[serde(default)]
        subdirs: Vec<String>,
    },
    /// Rename an entry on this node ONLY when `from` and `to` already refer to the exact same file
    /// or directory in the same parent directory (case-only or Unicode-normalization rename).
    Rename {
        from: String,
        to: String,
    },
    /// Acquire exclusive per-path move locks in the node's state directory in sorted path order,
    /// inspect every locked path the caller may read, reply with [`LockReady`], and hold the locks
    /// for a short renewable lease until released or expired.
    Lock {
        paths: Vec<String>,
        /// Optional lease duration in milliseconds (clamped to `[50, MOVE_LOCK_DEFAULT_LEASE]`).
        #[serde(default)]
        lease_ms: Option<u64>,
        /// Optional transfer job id owning this lock, disclosed on busy errors only to callers
        /// allowed to see that job.
        #[serde(default)]
        job_id: Option<String>,
    },
}

/// Action for [`Op::ProbeWrite`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ProbeWriteAction {
    Create,
    Remove,
    /// Probe the directory's exact case-sensitivity and Unicode-normalization behavior (falling
    /// back to an ephemeral unique probe file when read-only inspection is inconclusive).
    Behavior,
}

impl Op {
    fn name(&self) -> &'static str {
        match self {
            Op::Manifest { .. } => "manifest",
            Op::Stat { .. } => "stat",
            Op::Get { .. } => "get",
            Op::Put { .. } => "put",
            Op::Pack { .. } => "pack",
            Op::Unpack { .. } => "unpack",
            Op::Delete { .. } => "delete",
            Op::ProbeWrite { .. } => "probe_write",
            Op::ProbeRead { .. } => "probe_read",
            Op::Rename { .. } => "rename",
            Op::Lock { .. } => "lock",
        }
    }

    /// The file tool whose permission the operation needs: what it amounts to on this node.
    fn authorised_as(&self) -> (&'static str, Effect) {
        op_tool(self.name()).unwrap_or(("fs.write", Effect::Write))
    }
}

/// Maps a stream header's `op` name to the file tool and effect it corresponds to.
pub(crate) fn op_tool(op: &str) -> Option<(&'static str, Effect)> {
    match op {
        "manifest" | "stat" | "get" | "pack" | "probe_read" => Some(("fs.read", Effect::Read)),
        "put" | "unpack" | "probe_write" | "lock" => Some(("fs.write", Effect::Write)),
        "rename" => Some(("fs.move", Effect::Write)),
        "delete" => Some(("fs.remove", Effect::Destroy)),
        _ => None,
    }
}

/// One entry an `unpack` agrees to take: its name and kind, a file's size and, with `checksum`,
/// its hash, a link's target.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct Planned {
    pub path: String,
    pub kind: EntryKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hash: Option<Hash>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
}

/// What `stat` says about a path.
#[derive(Debug, Default, Serialize, Deserialize)]
pub(crate) struct StatReply {
    /// `file`, `dir`, `symlink` (one that leads nowhere), `other` or `missing`, after following a
    /// last part that is a symbolic link.
    pub kind: String,
    #[serde(default)]
    pub size: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hash: Option<Hash>,
    /// A file's modification time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mtime: Option<Timestamp>,
    /// The length of a partial file a `put` left beside it, which the next one continues from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub partial: Option<u64>,
    /// For a directory, or a path that does not exist yet, whether the file system it is or
    /// would be on ignores letter case, so two names that differ only in case are one there.
    /// Absent when that cannot be told without writing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub case_insensitive: Option<bool>,
    /// Whether the file system folds canonically equivalent Unicode forms (NFC vs NFD) into one
    /// entry. Absent when that cannot be told without writing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unicode_normalizing: Option<bool>,
    /// Case-folding behavior of `path`'s parent directory (where `path`'s own entry name lives),
    /// never inspecting inside `path` itself. Absent when inconclusive without writing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_case_insensitive: Option<bool>,
    /// Unicode-normalization behavior of `path`'s parent directory (where `path`'s own entry name
    /// lives), never inspecting inside `path` itself. Absent when inconclusive without writing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_unicode_normalizing: Option<bool>,
    /// The resolved filesystem path on this node (with `~` and symlinks resolved on existing
    /// ancestors), used to detect moving a file onto itself or a tree into its own subtree.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolved_path: Option<String>,
    /// Filesystem identity `(dev, ino, nlink)` of `path` when it exists.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_id: Option<FileIdentity>,
    /// Filesystem identity of `path`'s parent directory when it exists.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<FileIdentity>,
    /// Filesystem identities of existing ancestor directories of `path` (for subtree detection).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ancestor_ids: Vec<FileIdentity>,
    /// Whether the path as given (before following the final link) is a symbolic link.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub is_symlink: bool,
}

/// The line that ends a `manifest`.
#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct ManifestDone {
    pub done: bool,
    pub entries: u64,
    #[serde(default)]
    pub skipped: Vec<SkippedEntry>,
    /// How many entries could not be read, when `skipped` names only the first of them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skipped_total: Option<u64>,
}

/// The line that answers a `get` before its bytes.
#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct GetReady {
    pub size: u64,
}

/// The line that answers a `put` before its bytes: where the sender is to start.
#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct PutReady {
    pub offset: u64,
}

/// The line that ends a `put` that verified and renamed the file.
#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct PutDone {
    pub ok: bool,
    pub size: u64,
    pub hash: Hash,
    /// Actual path written on this node (populated when `if_exists = "rename"` chooses a numbered
    /// name, or in general when a file is placed).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// `true` when `if_exists = "skip"` left an existing file untouched.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub skipped: bool,
    /// Filesystem identity `(dev, ino, nlink)` of the placed file after commit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_id: Option<FileIdentity>,
    /// Modification time of the placed file after commit (after `set_times`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mtime: Option<Timestamp>,
}

/// The line that answers a `pack` before its archive: the names it left out, and why.
#[derive(Debug, Default, Serialize, Deserialize)]
pub(crate) struct PackReady {
    pub ready: bool,
    #[serde(default)]
    pub skipped: Vec<SkippedEntry>,
}

/// The line that answers an `unpack` before it reads.
#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct UnpackReady {
    pub ready: bool,
}

/// The line that ends an `unpack`.
#[derive(Debug, Default, Serialize, Deserialize)]
pub(crate) struct UnpackDone {
    pub ok: bool,
    pub files: u64,
    pub dirs: u64,
    pub symlinks: u64,
    pub bytes: u64,
    #[serde(default)]
    pub files_skipped: u64,
    #[serde(default)]
    pub skipped: Vec<SkippedEntry>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub renamed: Vec<RenamedEntry>,
    /// Regular files unpacked and verified in this round (by their planned relative path).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub verified_files: Vec<String>,
    /// Symlinks unpacked in this round (by their planned relative path).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub verified_symlinks: Vec<String>,
    /// Post-commit metadata of each verified regular file placed in this round, keyed by its
    /// planned relative path.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub placed_meta: BTreeMap<String, PlacedFileMeta>,
    /// Entries of the archive whose names differ only in letter case.
    #[serde(default)]
    pub case_collisions: Vec<CaseCollision>,
}

/// The line that ends a `delete`.
#[derive(Debug, Default, Serialize, Deserialize)]
pub(crate) struct DeleteDone {
    pub removed: u64,
    #[serde(default)]
    pub skipped: Vec<SkippedEntry>,
}

/// Serialized three-valued filesystem locality reported by `probe_write` and `probe_read`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "detail", rename_all = "snake_case")]
pub(crate) enum ProbeFsLocality {
    Local,
    Network(String),
    Unknown(String),
}

impl From<legion_os::fs_kind::FsLocality> for ProbeFsLocality {
    fn from(value: legion_os::fs_kind::FsLocality) -> Self {
        match value {
            legion_os::fs_kind::FsLocality::Local => Self::Local,
            legion_os::fs_kind::FsLocality::Network(name) => Self::Network(name),
            legion_os::fs_kind::FsLocality::Unknown(reason) => Self::Unknown(reason),
        }
    }
}

/// The line that answers a `probe_write` or `probe_read`.
#[derive(Debug, Default, Serialize, Deserialize)]
pub(crate) struct ProbeReply {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dir: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub marker: Option<String>,
    #[serde(default)]
    pub exists: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub case_insensitive: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unicode_normalizing: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub network_fs: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fs_locality: Option<ProbeFsLocality>,
}

impl ProbeReply {
    /// Returns the effective three-valued filesystem locality reported by this probe reply.
    ///
    /// Errs toward [`ProbeFsLocality::Unknown`] when neither `fs_locality` nor `network_fs` was
    /// supplied so an unverified or older peer can never be mistaken for provably local.
    pub(crate) fn effective_locality(&self) -> ProbeFsLocality {
        if let Some(ref net) = self.network_fs {
            return ProbeFsLocality::Network(net.clone());
        }
        match self.fs_locality {
            Some(ref loc) => loc.clone(),
            None => ProbeFsLocality::Unknown(
                "peer did not report filesystem locality".to_owned(),
            ),
        }
    }
}

/// Confirmation reply sent by a held `probe_write` (`hold: true`) when the caller sends
/// `"confirm\n"` after completing its `probe_read`.
#[derive(Debug, Default, Serialize, Deserialize)]
pub(crate) struct ProbeConfirm {
    pub held: bool,
}

/// The line that answers a same-node `rename`.
#[derive(Debug, Default, Serialize, Deserialize)]
pub(crate) struct RenameDone {
    pub ok: bool,
}

/// Live metadata observed for one path while holding its exclusive move lock.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub(crate) struct LockEntry {
    pub path: String,
    pub exists: bool,
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_id: Option<FileIdentity>,
    #[serde(default)]
    pub size: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mtime: Option<Timestamp>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub link_target: Option<String>,
}

/// The line that answers a `lock` once all requested paths are locked exclusively.
#[derive(Debug, Default, Serialize, Deserialize)]
pub(crate) struct LockReady {
    pub locked: bool,
    #[serde(default)]
    pub entries: Vec<LockEntry>,
}

/// How an operation failed: in time to say so on stdout, or after its raw bytes had started.
#[derive(Debug)]
enum Failure {
    Reply(ErrorBody),
    Raw(ErrorBody),
}

impl From<ErrorBody> for Failure {
    fn from(error: ErrorBody) -> Self {
        Failure::Reply(error)
    }
}

/// Serves one operation: reads its header from `input`, checks it, runs it, writes its replies to
/// `output` and its line to the audit log.
///
/// `pinned` is the principal a door fixed (`legion stream --as`, a forced ssh key, or the caller
/// of a transfer this node runs); a header's `as` is then honoured only when that principal's role
/// delegates. The error says why the operation failed, for stderr and the exit code; when it could
/// still be said on `output`, it already has been.
pub fn serve<R: Read + Send + 'static, W: Write>(
    host: &Host,
    pinned: Option<&str>,
    input: R,
    output: W,
) -> Result<(), ErrorBody> {
    serve_with_cancel(host, pinned, Door::Stream, None, None, None, input, output)
}

/// Like [`serve`], allowing the caller to specify the audit [`Door`] (for example [`Door::Ssh`]
/// when served behind `legion gate`), optional paired `device` name, and optional `pairing_id`
/// (checked live during streaming so revoking a device stops an in-flight transfer promptly).
pub fn serve_with_door<R: Read + Send + 'static, W: Write>(
    host: &Host,
    pinned: Option<&str>,
    door: Door,
    device: Option<&str>,
    pairing_id: Option<&str>,
    input: R,
    output: W,
) -> Result<(), ErrorBody> {
    serve_with_cancel(host, pinned, door, device, pairing_id, None, input, output)
}

/// Like [`serve_with_door`], accepting an optional [`CancellationToken`] that is cancelled by the
/// mid-stream auth watchdog if policy or device access is revoked while output is stalled.
#[allow(clippy::too_many_arguments)]
pub fn serve_with_cancel<R: Read + Send + 'static, W: Write>(
    host: &Host,
    pinned: Option<&str>,
    door: Door,
    device: Option<&str>,
    pairing_id: Option<&str>,
    abort_on_revoke: Option<CancellationToken>,
    input: R,
    output: W,
) -> Result<(), ErrorBody> {
    let started = Instant::now();
    let mut reader = BufReader::new(input);
    let mut output = BufWriter::with_capacity(OUTPUT_BUFFER, output);
    // Said before anything else, refusals included, so a reader can tell this node's replies
    // and advertised capabilities from those of an older node.
    let _ = line(
        &mut output,
        &json!({ "protocol": STREAM_PROTOCOL, "capabilities": STREAM_CAPABILITIES }),
    )
    .and_then(|()| flush_retry(&mut output));
    let (header, recorded) = match read_header(&mut reader) {
        Ok(read) => read,
        Err(error) => {
            let error = error.with_machine(host.name.clone());
            let _ = reply_error(&mut output, &error);
            return Err(error);
        }
    };
    let mut input = Some(reader);
    let tool = format!("stream.{}", header.op.name());
    let config = ConfigStore::new(host.paths.config_dir());
    let (principal, result) = match authorise(host, &config, pinned, pairing_id, &header) {
        Ok(grant) => {
            let principal = grant.principal().to_owned();
            (
                Some(principal),
                run(
                    host,
                    &grant,
                    pairing_id,
                    abort_on_revoke,
                    header.op,
                    &mut input,
                    &mut output,
                ),
            )
        }
        Err((principal, error)) => (principal, Err(Failure::Reply(error))),
    };
    let result = match result {
        Ok(()) => flush_retry(&mut output).map_err(|error| {
            ErrorBody::new(
                ErrorCode::Failed,
                format!("The reader of the stream went away: {error}."),
            )
        }),
        Err(Failure::Reply(error)) => {
            let error = machine(error, host);
            let _ = reply_error(&mut output, &error);
            Err(error)
        }
        Err(Failure::Raw(error)) => Err(machine(error, host)),
    };
    record(
        host, principal, door, device, &tool, recorded, started, &result,
    );
    result
}

fn machine(error: ErrorBody, host: &Host) -> ErrorBody {
    if error.machine.is_some() {
        error
    } else {
        error.with_machine(host.name.clone())
    }
}

/// Reads and parses the header line, returning it and the arguments the audit log records.
fn read_header<R: BufRead>(input: &mut R) -> Result<(Header, Value), ErrorBody> {
    let mut line = Vec::new();
    let read = input
        .by_ref()
        .take(LONGEST_HEADER as u64 + 1)
        .read_until(b'\n', &mut line)
        .map_err(|error| {
            ErrorBody::new(
                ErrorCode::Failed,
                format!("The stream's header could not be read: {error}."),
            )
        })?;
    if read == 0 {
        return Err(ErrorBody::new(
            ErrorCode::InvalidArgs,
            "The stream ended before its header.",
        )
        .with_hint("Send one JSON object on the first line, such as {\"op\":\"stat\",\"path\":\"~/notes.txt\"}."));
    }
    if line.len() > LONGEST_HEADER {
        return Err(ErrorBody::new(
            ErrorCode::InvalidArgs,
            format!("The stream's header is longer than {LONGEST_HEADER} bytes."),
        )
        .with_hint("Send the names of a big tree in several operations."));
    }
    let value: Value = serde_json::from_slice(&line).map_err(|error| {
        ErrorBody::new(
            ErrorCode::InvalidArgs,
            format!("The stream's header is not JSON: {error}."),
        )
    })?;
    let Value::Object(fields) = &value else {
        return Err(ErrorBody::new(
            ErrorCode::InvalidArgs,
            "The stream's header is not a JSON object.",
        ));
    };
    match fields.get("protocol") {
        Some(Value::Number(number)) if number.as_u64() == Some(u64::from(STREAM_PROTOCOL)) => {}
        Some(Value::Number(number)) => {
            return Err(unsupported(format!(
                "The stream asks for stream protocol {number}, and this node speaks stream protocol {STREAM_PROTOCOL}."
            )));
        }
        _ => {
            return Err(unsupported(format!(
                "The stream's header names no protocol, and this node speaks stream protocol {STREAM_PROTOCOL}."
            )));
        }
    }
    match fields.get("op").and_then(Value::as_str) {
        Some(op) if OPS.contains(&op) => {}
        Some(op) => {
            return Err(unsupported(format!(
                "This node does not know the stream operation {op}; it knows {}.",
                OPS.join(", ")
            )));
        }
        None => {
            return Err(unsupported(format!(
                "The stream's header names no operation; this node knows {}.",
                OPS.join(", ")
            )));
        }
    }
    let header: Header = serde_json::from_value(value.clone()).map_err(|error| {
        ErrorBody::new(
            ErrorCode::InvalidArgs,
            format!("The stream's header does not name an operation it can do: {error}."),
        )
        .with_hint("op is one of manifest, stat, get, put, pack, unpack and delete.")
    })?;
    let mut recorded = value;
    if let Value::Object(map) = &mut recorded {
        map.remove("as");
    }
    Ok((header, recorded))
}

/// What a node answers a header it cannot serve: what it speaks instead.
fn unsupported(message: String) -> ErrorBody {
    ErrorBody::new(ErrorCode::Unsupported, message)
        .with_hint(
            "Update legion on whichever machine is older, so both speak the same stream protocol.",
        )
        .with_details(json!({"protocol": [STREAM_PROTOCOL], "ops": OPS}))
}

/// Decides whom the operation acts as and whether that principal may do it, as a tool call would
/// be decided. On failure, says whom it was recorded as.
fn authorise(
    host: &Host,
    config: &ConfigStore,
    pinned: Option<&str>,
    pairing_id: Option<&str>,
    header: &Header,
) -> Result<Grant, (Option<String>, ErrorBody)> {
    if let Some(pid) = pairing_id
        && let Err(error) = check_pairing_not_revoked(config, pid)
    {
        return Err((pinned.map(str::to_owned), error));
    }
    let policy = config
        .get::<Policy>()
        .map_err(|error| (pinned.map(str::to_owned), error))?;
    let principal = policy
        .resolve(header.as_principal.as_deref(), pinned)
        .map_err(|error| {
            let caller =
                pinned.map_or_else(|| policy.default_principal().to_owned(), str::to_owned);
            (Some(caller), error)
        })?;
    let (tool, effect) = header.op.authorised_as();
    let spec = ToolSpec {
        name: tool.to_owned(),
        description: String::new(),
        effect,
        input_schema: json!({"type": "object"}),
    };
    policy
        .authorize(&principal, &spec, &host.paths)
        .map_err(|error| (Some(principal), error))
}

/// Checks whether a paired device (`pairing_id`) has been removed in `devices.toml`.
fn check_pairing_not_revoked(config: &ConfigStore, pairing_id: &str) -> Result<(), ErrorBody> {
    if !config.path_of::<DevicesFile>().exists() {
        return Ok(());
    }
    let devices = config.get::<DevicesFile>()?;
    if devices.removed.contains_key(pairing_id) || !devices.devices.contains_key(pairing_id) {
        return Err(ErrorBody::new(
            ErrorCode::Denied,
            "This paired device has been removed, so the transfer was stopped.",
        )
        .with_hint("Pair the device again if it should still have access."));
    }
    Ok(())
}

/// Live permission and device-revocation re-checker run periodically while streaming large files
/// (every [`RECHECK_INTERVAL_BYTES`] or [`RECHECK_INTERVAL_TIME`], whichever comes first, and even
/// while a stream is stalled).
#[derive(Clone)]
struct StreamAuthCheck {
    paths: Paths,
    principal: String,
    pairing_id: Option<String>,
    tool: &'static str,
    effect: Effect,
    path: PathBuf,
    access: Access,
    create_kind: Option<CreateKind>,
    last_check: Arc<Mutex<Instant>>,
    denied: Arc<Mutex<Option<ErrorBody>>>,
}

impl StreamAuthCheck {
    #[allow(clippy::too_many_arguments)]
    fn new(
        host: &Host,
        grant: &Grant,
        pairing_id: Option<&str>,
        tool: &'static str,
        effect: Effect,
        path: PathBuf,
        access: Access,
        create_kind: Option<CreateKind>,
    ) -> Self {
        Self {
            paths: host.paths.clone(),
            principal: grant.principal().to_owned(),
            pairing_id: pairing_id.map(str::to_owned),
            tool,
            effect,
            path,
            access,
            create_kind,
            last_check: Arc::new(Mutex::new(Instant::now())),
            denied: Arc::new(Mutex::new(None)),
        }
    }

    fn verify_once(&self) -> Result<(), ErrorBody> {
        let config = ConfigStore::new(self.paths.config_dir());
        if let Some(ref pid) = self.pairing_id {
            check_pairing_not_revoked(&config, pid)?;
        }
        let policy = config.get::<Policy>()?;
        let spec = ToolSpec {
            name: self.tool.to_owned(),
            description: String::new(),
            effect: self.effect,
            input_schema: json!({"type": "object"}),
        };
        let live_grant = policy.authorize(&self.principal, &spec, &self.paths)?;
        let checked = live_grant.check_path(&self.path, self.access)?;
        if let Some(kind) = self.create_kind {
            live_grant.check_create(&checked, kind, true)?;
        }
        Ok(())
    }

    fn verify(&self) -> io::Result<()> {
        const RETRY_DELAY: Duration = Duration::from_millis(200);
        const RETRIES: usize = 2;

        let mut res = self.verify_once();
        if res.is_err() {
            // Confirm a negative read twice after a short pause so an in-place editor save or
            // momentary unreadable file never cancels an active stream spuriously.
            for _ in 0..RETRIES {
                std::thread::sleep(RETRY_DELAY);
                res = self.verify_once();
                if res.is_ok() {
                    break;
                }
            }
        }
        if let Ok(mut last) = self.last_check.lock() {
            *last = Instant::now();
        }
        match res {
            Ok(()) => Ok(()),
            Err(error) => {
                let message = error.message.clone();
                if let Ok(mut slot) = self.denied.lock() {
                    *slot = Some(error);
                }
                Err(io::Error::new(io::ErrorKind::PermissionDenied, message))
            }
        }
    }

    /// Re-verifies authorization whenever either [`RECHECK_INTERVAL_BYTES`] have flowed or
    /// [`RECHECK_INTERVAL_TIME`] has elapsed since the previous check.
    fn maybe_recheck(&self, bytes_since: &mut u64) -> io::Result<()> {
        if self.is_denied() {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "permissions or device access were revoked mid-stream",
            ));
        }
        let due_by_time = self
            .last_check
            .lock()
            .map(|last| last.elapsed() >= RECHECK_INTERVAL_TIME)
            .unwrap_or(true);
        if *bytes_since >= RECHECK_INTERVAL_BYTES || due_by_time {
            *bytes_since = 0;
            self.verify()?;
        }
        Ok(())
    }

    fn is_denied(&self) -> bool {
        self.denied
            .lock()
            .map(|slot| slot.is_some())
            .unwrap_or(true)
    }

    fn take_denied(&self) -> Option<ErrorBody> {
        self.denied.lock().ok().and_then(|mut slot| slot.take())
    }
}

/// Background watchdog for outbound (`get`/`pack`) streams so a completely stalled pipe (0 B/s)
/// still stops within [`RECHECK_INTERVAL_TIME`] when permissions or device access are revoked.
struct RevokeWatchdog {
    done: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl RevokeWatchdog {
    fn start(check: StreamAuthCheck, abort: Option<CancellationToken>) -> Self {
        let done = Arc::new(AtomicBool::new(false));
        let Some(token) = abort else {
            return Self { done, handle: None };
        };
        let done_bg = Arc::clone(&done);
        let handle = std::thread::spawn(move || {
            while !done_bg.load(Ordering::Acquire) {
                std::thread::sleep(RECHECK_POLL_INTERVAL);
                if done_bg.load(Ordering::Acquire) {
                    break;
                }
                let mut zero = 0u64;
                if check.maybe_recheck(&mut zero).is_err() {
                    token.cancel();
                    break;
                }
            }
        });
        Self {
            done,
            handle: Some(handle),
        }
    }
}

impl Drop for RevokeWatchdog {
    fn drop(&mut self) {
        self.done.store(true, Ordering::Release);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// Maximum slice written per underlying call when a slow link is detected, so slow transfers
/// (a few KB/s) never block inside a single 64 KiB write for more than a couple of seconds.
const SLOW_WRITE_CHUNK: usize = 4 * 1024;
const SLOW_WRITE_THRESHOLD: Duration = Duration::from_millis(100);

/// Wraps a writer so live policy and pairing status are re-checked every [`RECHECK_INTERVAL_BYTES`]
/// or [`RECHECK_INTERVAL_TIME`], whichever comes first.
struct RecheckingWriter<W> {
    inner: W,
    check: StreamAuthCheck,
    since_check: u64,
    slow_mode: bool,
}

impl<W: Write> RecheckingWriter<W> {
    fn new(inner: W, check: StreamAuthCheck) -> Self {
        Self {
            inner,
            check,
            since_check: 0,
            slow_mode: false,
        }
    }
}

impl<W: Write> Write for RecheckingWriter<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.check.maybe_recheck(&mut self.since_check)?;
        if buf.is_empty() {
            return Ok(0);
        }
        let cap = if self.slow_mode {
            SLOW_WRITE_CHUNK
        } else {
            buf.len()
        };
        let slice = &buf[..buf.len().min(cap)];
        let started = Instant::now();
        let n = self.inner.write(slice)?;
        self.slow_mode = started.elapsed() >= SLOW_WRITE_THRESHOLD;
        self.since_check = self.since_check.saturating_add(n as u64);
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        loop {
            self.check.maybe_recheck(&mut self.since_check)?;
            match self.inner.flush() {
                Ok(()) => return Ok(()),
                Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
                Err(err) => return Err(err),
            }
        }
    }
}

/// Wraps a reader so live policy and pairing status are re-checked every [`RECHECK_INTERVAL_BYTES`]
/// or [`RECHECK_INTERVAL_TIME`], whichever comes first.
struct RecheckingReader<R> {
    inner: R,
    check: StreamAuthCheck,
    since_check: u64,
}

impl<R: Read> RecheckingReader<R> {
    fn new(inner: R, check: StreamAuthCheck) -> Self {
        Self {
            inner,
            check,
            since_check: 0,
        }
    }
}

impl<R: Read> Read for RecheckingReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.check.maybe_recheck(&mut self.since_check)?;
        let n = self.inner.read(buf)?;
        self.since_check = self.since_check.saturating_add(n as u64);
        Ok(n)
    }
}

#[allow(clippy::too_many_arguments)]
fn record(
    host: &Host,
    principal: Option<String>,
    door: Door,
    device: Option<&str>,
    tool: &str,
    args: Value,
    started: Instant,
    result: &Result<(), ErrorBody>,
) {
    let entry = AuditEntry {
        time: audit::now(),
        principal,
        door,
        device: device.map(str::to_owned),
        tool: tool.to_owned(),
        target: host.name.clone(),
        outcome: match result {
            Ok(()) => Outcome::Ok,
            Err(error) => Outcome::Error(error.code),
        },
        duration: (started.elapsed().as_secs_f64() * 1e6).round() / 1e6,
        args: audit::redact(&args),
        message: result
            .as_ref()
            .err()
            .map(|error| audit::redact_message(&error.message)),
    };
    // stderr is for people; an operation whose audit line is lost still happened.
    if let Err(error) = AuditLog::new(&host.paths).append(&entry) {
        let _ = writeln!(
            io::stderr(),
            "legion stream: the operation ran, but it could not be written to the audit log: {error}"
        );
    }
}

fn run<R: Read + Send + 'static, W: Write>(
    host: &Host,
    grant: &Grant,
    pairing_id: Option<&str>,
    abort_on_revoke: Option<CancellationToken>,
    op: Op,
    input: &mut Option<BufReader<R>>,
    output: &mut BufWriter<W>,
) -> Result<(), Failure> {
    match op {
        Op::Manifest {
            root,
            checksum,
            exclude,
        } => list(host, grant, &root, checksum, exclude, output),
        Op::Stat {
            path,
            checksum,
            if_size,
            if_hash,
        } => stat(
            host,
            grant,
            &path,
            checksum,
            if_size,
            if_hash.as_deref(),
            output,
        ),
        Op::Get { path, offset } => get(
            host,
            grant,
            pairing_id,
            abort_on_revoke,
            &path,
            offset,
            output,
        ),
        Op::Put {
            path,
            size,
            hash,
            mode,
            mtime,
            if_exists,
        } => put(
            host, grant, pairing_id, &path, size, &hash, mode, mtime, if_exists, input, output,
        ),
        Op::Pack { root, names } => pack(
            host,
            grant,
            pairing_id,
            abort_on_revoke,
            &root,
            &names,
            output,
        ),
        Op::Unpack {
            root,
            entries,
            if_exists,
        } => unpack(
            host, grant, pairing_id, &root, &entries, if_exists, input, output,
        ),
        Op::Delete {
            root,
            names,
            dry_run,
            remove_root,
            expect_hashes,
            expect_root_hash,
            forbid_file_id,
        } => {
            legion_xfer::sweep_abandoned_temps(
                host.paths.state_dir(),
                legion_xfer::PROBE_SWEEP_AGE,
            );
            delete(
                grant,
                &root,
                &names,
                dry_run,
                remove_root,
                &expect_hashes,
                expect_root_hash,
                forbid_file_id,
                output,
            )
        }
        Op::ProbeWrite {
            action,
            dir,
            marker,
            nearest_ancestor,
            hold,
        } => probe_write(
            host,
            grant,
            action,
            &dir,
            marker.as_deref(),
            nearest_ancestor,
            hold,
            input,
            output,
        ),
        Op::ProbeRead {
            dir,
            marker,
            subdirs,
        } => probe_read(grant, &dir, &marker, &subdirs, output),
        Op::Rename { from, to } => rename_same_file(host, grant, &from, &to, output),
        Op::Lock {
            paths,
            lease_ms,
            job_id,
        } => lock_paths(
            host,
            grant,
            pairing_id,
            &paths,
            lease_ms,
            job_id.as_deref(),
            input,
            output,
        ),
    }
}

fn flush_retry<W: Write>(output: &mut W) -> io::Result<()> {
    loop {
        match output.flush() {
            Ok(()) => return Ok(()),
            Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
            Err(err) => return Err(err),
        }
    }
}

fn line<W: Write, T: Serialize>(output: &mut W, value: &T) -> io::Result<()> {
    serde_json::to_writer(&mut *output, value).map_err(io::Error::other)?;
    output.write_all(b"\n")
}

fn reply<W: Write, T: Serialize>(output: &mut W, value: &T) -> Result<(), Failure> {
    line(output, value)
        .and_then(|()| flush_retry(output))
        .map_err(|error| Failure::Raw(gone(&error)))
}

fn reply_error<W: Write>(output: &mut W, error: &ErrorBody) -> io::Result<()> {
    line(output, &json!({ "error": error }))?;
    flush_retry(output)
}

fn gone(error: &io::Error) -> ErrorBody {
    ErrorBody::new(
        ErrorCode::Failed,
        format!("The other end of the stream went away: {error}."),
    )
}

fn write_grant_for(host: &Host, principal: &str) -> Option<Grant> {
    ConfigStore::new(host.paths.config_dir())
        .get::<Policy>()
        .ok()
        .and_then(|policy| {
            policy
                .authorize_name(principal, "fs.write", Some(Effect::Write), &host.paths)
                .ok()
        })
}

fn can_write_rename_intent(grant: &Grant, parent: &Path, intent: &PendingRenameIntent) -> bool {
    grant.check_path(parent, Access::Write).is_ok()
        && legion_tools::fs::check_not_protected_tree(grant, parent).is_ok()
        && [
            intent.from_name.as_str(),
            intent.temp_name.as_str(),
            intent.to_name.as_str(),
        ]
        .iter()
        .all(|name| {
            let entry = parent.join(name);
            grant.check_path(&entry, Access::Write).is_ok()
                && legion_tools::fs::check_not_protected_tree(grant, &entry).is_ok()
        })
}

fn list<W: Write>(
    host: &Host,
    grant: &Grant,
    root: &str,
    checksum: bool,
    exclude: Vec<String>,
    output: &mut W,
) -> Result<(), Failure> {
    let root = resolve_root(grant, root, Access::Read)?;
    let state_dir = host.paths.state_dir();
    if let Some(wg) = write_grant_for(host, grant.principal()) {
        recover_pending_renames_checked(
            state_dir,
            Some(RenameRecoveryScope::WithinDir(&root)),
            |parent, intent| can_write_rename_intent(&wg, parent, intent),
        );
    }
    let options = ManifestOptions { checksum, exclude };
    let listing = busy(output, || manifest(grant, &host.paths, &root, &options))??;
    write_listing(output, listing.entries, listing.skipped)
        .map_err(|error| Failure::Raw(gone(&error)))
}

/// How much of the line that ends a listing may go to naming what could not be read, so the line
/// stays well inside what a reader takes.
const MOST_SKIPPED_BYTES: usize = 256 * 1024;

fn write_listing<W: Write>(
    output: &mut W,
    entries: Vec<Entry>,
    mut skipped: Vec<SkippedEntry>,
) -> io::Result<()> {
    for entry in &entries {
        line(output, entry)?;
    }
    let total = skipped.len();
    let mut used = 0;
    let kept = skipped
        .iter()
        .take_while(|entry| {
            used += entry.path.len() + entry.reason.len() + 32;
            used <= MOST_SKIPPED_BYTES
        })
        .count();
    skipped.truncate(kept);
    line(
        output,
        &ManifestDone {
            done: true,
            entries: entries.len() as u64,
            skipped,
            skipped_total: (kept < total).then_some(total as u64),
        },
    )?;
    flush_retry(output)
}

fn stat<W: Write>(
    host: &Host,
    grant: &Grant,
    path: &str,
    checksum: bool,
    if_size: Option<u64>,
    if_hash: Option<&str>,
    output: &mut W,
) -> Result<(), Failure> {
    // The partial file a `put` to this path would continue sits beside the path as given, a link
    // kept as a link, and is only looked at when it may be read itself and matches the expected
    // source size and hash. Sidecar metadata/locks live in the node's state directory.
    let given = grant.check_path(path, Access::Read)?;
    let state_dir = host.paths.state_dir();
    if let Some(wg) = write_grant_for(host, grant.principal()) {
        if let Some(parent) = given.parent() {
            recover_pending_renames_checked(
                state_dir,
                Some(RenameRecoveryScope::ExactDir(parent)),
                |p, intent| can_write_rename_intent(&wg, p, intent),
            );
        }
        if wg.check_path(&given, Access::Write).is_ok()
            && legion_tools::fs::check_not_protected_tree(&wg, &given).is_ok()
        {
            legion_xfer::reclaim_stale_claim(state_dir, &given);
        }
    }
    let is_symlink = fs::symlink_metadata(&given).is_ok_and(|meta| meta.file_type().is_symlink());
    let expected_hash = if_hash.and_then(|s| s.parse::<Hash>().ok());
    let partial = || {
        legion_xfer::part_path(&given)
            .ok()
            .filter(|part| grant.check_path(part, Access::Read).is_ok())
            .and_then(|_| match if_size {
                Some(expected_size) => legion_xfer::resume_offset_for_with_state(
                    state_dir,
                    &given,
                    expected_size,
                    expected_hash,
                )
                .ok(),
                None => legion_xfer::resume_offset_with_state(state_dir, &given).ok(),
            })
            .filter(|len| *len > 0)
    };
    let path = resolve_root(grant, path, Access::Read)?;
    let resolved_path = Some(path.to_string_lossy().into_owned());
    let parent_dir = path.parent().filter(|p| !p.as_os_str().is_empty());
    let parent_id = parent_dir.and_then(|p| file_identity(p).ok());
    let parent_case_insensitive = parent_dir.and_then(case_insensitive_in_dir);
    let parent_unicode_normalizing = parent_dir.and_then(unicode_normalizing_in_dir);
    let ancestor_ids = || {
        let mut ids = Vec::new();
        for anc in path.ancestors().skip(1) {
            if anc.as_os_str().is_empty() {
                continue;
            }
            if let Ok(id) = file_identity(anc) {
                ids.push(id);
            }
        }
        ids
    };
    let found = match fs::symlink_metadata(&path) {
        Ok(meta) => meta,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return reply(
                output,
                &StatReply {
                    kind: "missing".to_owned(),
                    partial: partial(),
                    case_insensitive: case_insensitive(&path),
                    unicode_normalizing: unicode_normalizing(&path),
                    parent_case_insensitive,
                    parent_unicode_normalizing,
                    resolved_path,
                    file_id: None,
                    parent_id,
                    ancestor_ids: ancestor_ids(),
                    ..StatReply::default()
                },
            );
        }
        Err(error) => return Err(Failure::Reply(io_error("look at", &path, &error))),
    };
    let fid = file_identity(&path).ok();
    let kind = found.file_type();
    let answer = if kind.is_dir() {
        StatReply {
            kind: "dir".to_owned(),
            case_insensitive: case_insensitive(&path),
            unicode_normalizing: unicode_normalizing(&path),
            parent_case_insensitive,
            parent_unicode_normalizing,
            resolved_path,
            file_id: fid,
            parent_id,
            ancestor_ids: ancestor_ids(),
            is_symlink,
            ..StatReply::default()
        }
    } else if kind.is_file() {
        let hash = checksum && if_size.is_none_or(|size| size == found.len());
        let options = ManifestOptions {
            checksum: hash,
            exclude: Vec::new(),
        };
        let listed =
            busy(output, || legion_xfer::manifest::build(&path, &options))?.map_err(xfer_error)?;
        let entry = listed.entries.into_iter().next();
        StatReply {
            kind: "file".to_owned(),
            size: entry.as_ref().map_or(found.len(), |entry| entry.size),
            mode: entry.as_ref().and_then(|entry| entry.mode),
            mtime: entry.as_ref().map(|entry| entry.mtime),
            hash: entry.and_then(|entry| entry.hash),
            partial: partial(),
            case_insensitive: case_insensitive(&path),
            unicode_normalizing: unicode_normalizing(&path),
            parent_case_insensitive,
            parent_unicode_normalizing,
            resolved_path,
            file_id: fid,
            parent_id,
            ancestor_ids: Vec::new(),
            is_symlink,
        }
    } else {
        StatReply {
            kind: if kind.is_symlink() {
                "symlink"
            } else {
                "other"
            }
            .to_owned(),
            parent_case_insensitive,
            parent_unicode_normalizing,
            resolved_path,
            file_id: fid,
            parent_id,
            is_symlink: is_symlink || kind.is_symlink(),
            ..StatReply::default()
        }
    };
    reply(output, &answer)
}

fn get<W: Write>(
    host: &Host,
    grant: &Grant,
    pairing_id: Option<&str>,
    abort_on_revoke: Option<CancellationToken>,
    path: &str,
    offset: u64,
    output: &mut BufWriter<W>,
) -> Result<(), Failure> {
    let path = grant.check_path(path, Access::Read)?;
    let source = legion_xfer::open_get(&path, offset).map_err(xfer_error)?;
    reply(
        output,
        &GetReady {
            size: source.size(),
        },
    )?;
    let check = StreamAuthCheck::new(
        host,
        grant,
        pairing_id,
        "fs.read",
        Effect::Read,
        path,
        Access::Read,
        None,
    );
    let _watchdog = RevokeWatchdog::start(check.clone(), abort_on_revoke);
    // Write raw file bytes directly to the underlying writer (already flushed above) so slow links
    // never block inside a 64 KiB BufWriter drain before the 2-second re-auth timer fires.
    let mut guarded = RecheckingWriter::new(output.get_mut(), check.clone());
    if let Err(error) = source.stream_to(&mut guarded) {
        if let Some(denied) = check.take_denied() {
            return Err(Failure::Raw(denied));
        }
        return Err(Failure::Raw(xfer_error(error)));
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn put<R: Read + Send + 'static, W: Write>(
    host: &Host,
    grant: &Grant,
    pairing_id: Option<&str>,
    path: &str,
    size: u64,
    hash: &str,
    mode: Option<u32>,
    mtime: Option<Timestamp>,
    if_exists: IfExists,
    input: &mut Option<BufReader<R>>,
    output: &mut BufWriter<W>,
) -> Result<(), Failure> {
    let target = grant.check_path(path, Access::Write)?;
    legion_tools::fs::check_not_protected_tree(grant, &target)?;
    grant.check_create(&target, CreateKind::File, true)?;
    let state_dir = host.paths.state_dir().to_path_buf();
    if let Some(parent) = target.parent() {
        recover_pending_renames_checked(
            &state_dir,
            Some(RenameRecoveryScope::ExactDir(parent)),
            |p, intent| can_write_rename_intent(grant, p, intent),
        );
    }
    legion_xfer::reclaim_stale_claim(&state_dir, &target);
    // Only the resumable `.legion-part` file sits beside the target; lock and metadata sidecars
    // live under the node's state directory.
    let part = legion_xfer::part_path(&target).map_err(xfer_error)?;
    grant.check_path(&part, Access::Write)?;
    grant.check_create(&part, CreateKind::File, true)?;
    let hash: Hash = hash.parse().map_err(xfer_error)?;
    if let Ok(meta) = fs::symlink_metadata(&target) {
        if if_exists == IfExists::Fail {
            return Err(Failure::Reply(
                ErrorBody::new(
                    ErrorCode::Exists,
                    format!("{} already exists.", target.display()),
                )
                .with_hint(
                    "Pass if_exists=\"replace\", \"skip\", or \"rename\", or choose another path.",
                ),
            ));
        }
        if if_exists == IfExists::Skip {
            reply(output, &PutReady { offset: size })?;
            return reply(
                output,
                &PutDone {
                    ok: true,
                    size: 0,
                    hash,
                    path: None,
                    skipped: true,
                    file_id: None,
                    mtime: None,
                },
            );
        }
        if if_exists == IfExists::Replace && meta.is_dir() {
            return Err(Failure::Reply(
                ErrorBody::new(
                    ErrorCode::Conflict,
                    format!(
                        "{} is a directory, and a file cannot replace it.",
                        target.display()
                    ),
                )
                .with_hint("Name the file inside it instead."),
            ));
        }
    }
    if let Some(parent) = target.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent)
            .map_err(|error| Failure::Reply(io_error("create", parent, &error)))?;
    }
    // The sender's offset is only a hint: what is on disk decides, so a sender that lost track
    // of an earlier attempt still continues it rather than starting over, while a concurrent
    // transfer to the same target falls back to its own private partial file.
    let session = PutSession::open_with_state(&state_dir, &target, size, hash, if_exists)
        .map_err(xfer_error)?;
    let offset = session.offset();
    reply(output, &PutReady { offset })?;
    let grant_for_check = grant.clone();
    let check = StreamAuthCheck::new(
        host,
        grant,
        pairing_id,
        "fs.write",
        Effect::Write,
        target.clone(),
        Access::Write,
        Some(CreateKind::File),
    );
    let check_for_feed = check.clone();
    let outcome = feed(
        input,
        Some(size - offset),
        output,
        Some(check.clone()),
        move |fed| {
            let guarded = RecheckingReader::new(fed, check_for_feed.clone());
            let mut policy_err: Option<ErrorBody> = None;
            let res = session.finish(hash, guarded, mode, |candidate| {
                if let Err(err) = grant_for_check
                    .check_path(candidate, Access::Write)
                    .and_then(|checked| {
                        legion_tools::fs::check_not_protected_tree(&grant_for_check, &checked)?;
                        grant_for_check.check_create(&checked, CreateKind::File, true)?;
                        Ok(())
                    })
                {
                    policy_err = Some(err);
                    return Err(legion_xfer::Error::PermissionDenied {
                        path: candidate.to_path_buf(),
                        source: io::Error::new(
                            io::ErrorKind::PermissionDenied,
                            "policy refused renamed target",
                        ),
                    });
                }
                Ok(())
            });
            if let Some(denied) = check_for_feed.take_denied() {
                return Err(denied);
            }
            match (res, policy_err) {
                (Ok(done), _) => Ok(done),
                (Err(_), Some(err)) => Err(err),
                (Err(err), None) => Err(xfer_error(err)),
            }
        },
    )??;
    // The file is whole and in place; a time that cannot be set leaves it as it is, and a later
    // listing just sees it as changed.
    if let Some(ref final_path) = outcome.path
        && let Some(time) = mtime.and_then(system_time)
    {
        let _ = fs::set_times(final_path, FileTimes::new().set_modified(time));
    }
    let (placed_id, placed_mtime) = outcome
        .path
        .as_deref()
        .map(|final_path| {
            let fid = file_identity(final_path).ok();
            let mt = fs::symlink_metadata(final_path)
                .ok()
                .and_then(|m| m.modified().ok())
                .map(Timestamp::from_system_time);
            (fid, mt)
        })
        .unwrap_or((None, None));
    let renamed_path = outcome
        .path
        .filter(|final_path| final_path != &target)
        .map(|p| p.to_string_lossy().into_owned());
    reply(
        output,
        &PutDone {
            ok: true,
            size: outcome.report.size,
            hash: outcome.report.hash,
            path: renamed_path,
            skipped: outcome.skipped,
            file_id: placed_id,
            mtime: placed_mtime,
        },
    )
}

fn pack<W: Write>(
    host: &Host,
    grant: &Grant,
    pairing_id: Option<&str>,
    abort_on_revoke: Option<CancellationToken>,
    root: &str,
    names: &[String],
    output: &mut BufWriter<W>,
) -> Result<(), Failure> {
    let root = resolve_root(grant, root, Access::Read)?;
    if !fs::metadata(&root).is_ok_and(|meta| meta.is_dir()) {
        return Err(Failure::Reply(ErrorBody::new(
            ErrorCode::Conflict,
            format!(
                "{} is not a directory, so nothing can be packed from it.",
                root.display()
            ),
        )));
    }
    let mut ready = PackReady {
        ready: true,
        skipped: Vec::new(),
    };
    let mut keep = Vec::with_capacity(names.len());
    for name in names {
        let found = named(grant, &root, name, Access::Read).and_then(|path| {
            fs::symlink_metadata(&path)
                .map(|_| ())
                .map_err(|error| Named::Refused(error.to_string()))
        });
        match found {
            Ok(()) => keep.push(name.as_str()),
            Err(Named::Missing) => ready.skipped.push(SkippedEntry {
                path: name.clone(),
                reason: "it is no longer there since it was listed".to_owned(),
            }),
            Err(Named::Refused(reason)) => ready.skipped.push(SkippedEntry {
                path: name.clone(),
                reason,
            }),
        }
    }
    reply(output, &ready)?;
    let check = StreamAuthCheck::new(
        host,
        grant,
        pairing_id,
        "fs.read",
        Effect::Read,
        root.clone(),
        Access::Read,
        None,
    );
    let _watchdog = RevokeWatchdog::start(check.clone(), abort_on_revoke);
    let mut guarded = RecheckingWriter::new(output.get_mut(), check.clone());
    if let Err(error) = legion_xfer::pack(&root, &keep, &mut guarded) {
        if let Some(denied) = check.take_denied() {
            return Err(Failure::Raw(denied));
        }
        return Err(Failure::Raw(xfer_error(error)));
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn unpack<R: Read + Send + 'static, W: Write>(
    host: &Host,
    grant: &Grant,
    pairing_id: Option<&str>,
    root: &str,
    entries: &[Planned],
    if_exists: IfExists,
    input: &mut Option<BufReader<R>>,
    output: &mut BufWriter<W>,
) -> Result<(), Failure> {
    let root = resolve_root(grant, root, Access::Write)?;
    let state_dir = host.paths.state_dir().to_path_buf();
    recover_pending_renames_checked(
        &state_dir,
        Some(RenameRecoveryScope::WithinDir(&root)),
        |p, intent| can_write_rename_intent(grant, p, intent),
    );
    if fs::symlink_metadata(&root).is_err() {
        grant.check_create(&root, CreateKind::Dir, true)?;
    }
    let mut plan = HashMap::with_capacity(entries.len());
    for entry in entries {
        let name = legion_xfer::validate_relative_path(&entry.path).map_err(xfer_error)?;
        let incomplete = match entry.kind {
            EntryKind::File => entry.size.is_none(),
            EntryKind::Symlink => entry.target.is_none(),
            EntryKind::Dir => false,
        };
        let expected = Expected {
            kind: entry.kind,
            size: entry.size,
            hash: entry.hash,
            target: entry.target.clone(),
        };
        if incomplete || plan.insert(name, expected).is_some() {
            return Err(Failure::Reply(ErrorBody::new(
                ErrorCode::InvalidArgs,
                format!(
                    "The unpack's plan lists {} twice, or without its size or link target.",
                    entry.path
                ),
            )));
        }
    }
    if fs::metadata(&root).is_ok_and(|meta| !meta.is_dir()) {
        return Err(Failure::Reply(
            ErrorBody::new(
                ErrorCode::Conflict,
                format!(
                    "{} is not a directory, so a tree cannot be written into it.",
                    root.display()
                ),
            )
            .with_hint("Name a directory, or a path that does not exist yet."),
        ));
    }
    reply(output, &UnpackReady { ready: true })?;
    let check = StreamAuthCheck::new(
        host,
        grant,
        pairing_id,
        "fs.write",
        Effect::Write,
        root.clone(),
        Access::Write,
        None,
    );
    let check_for_feed = check.clone();
    let grant_for_unpack = grant.clone();
    let root_for_unpack = root.clone();
    let (report, mut refused, violation) =
        feed(input, None, output, Some(check.clone()), move |fed| {
            let rechecked = RecheckingReader::new(fed, check_for_feed.clone());
            let watch = Rc::new(RefCell::new(Watch::new(plan)));
            let mut refused: HashMap<String, String> = HashMap::new();
            let guarded = Guarded::new(rechecked, Rc::clone(&watch));
            let watch_for_hash = Rc::clone(&watch);
            let report = legion_xfer::unpack_with_options_hashes_and_state(
                &state_dir,
                &root_for_unpack,
                guarded,
                if_exists,
                |rel, kind| {
                    let mut watch = watch.borrow_mut();
                    // The entry the unpack is about to write must be the one whose header was checked.
                    let checked_entry = watch.current.take();
                    if checked_entry
                        .is_none_or(|(name, checked_kind)| name != rel || checked_kind != kind)
                    {
                        let _ = watch
                            .refuse(rel, "is not the entry whose header was checked".to_owned());
                        return false;
                    }
                    drop(watch);
                    let create_kind = match kind {
                        EntryKind::File => CreateKind::File,
                        EntryKind::Dir => CreateKind::Dir,
                        EntryKind::Symlink => CreateKind::Symlink,
                    };
                    match checked_create(&grant_for_unpack, &root_for_unpack, rel, create_kind) {
                        Ok(_) => true,
                        Err(error) => {
                            refused.insert(rel.to_owned(), error.message);
                            false
                        }
                    }
                },
                |candidate, kind| {
                    let create_kind = match kind {
                        EntryKind::File => CreateKind::File,
                        EntryKind::Dir => CreateKind::Dir,
                        EntryKind::Symlink => CreateKind::Symlink,
                    };
                    grant_for_unpack
                        .check_path(candidate, Access::Write)
                        .and_then(|checked| {
                            legion_tools::fs::check_not_protected_tree(
                                &grant_for_unpack,
                                &checked,
                            )?;
                            grant_for_unpack.check_create(&checked, create_kind, true)?;
                            Ok(())
                        })
                        .map_err(|_| legion_xfer::Error::PermissionDenied {
                            path: candidate.to_path_buf(),
                            source: io::Error::new(
                                io::ErrorKind::PermissionDenied,
                                "policy refused renamed target",
                            ),
                        })
                },
                |rel| {
                    watch_for_hash
                        .borrow()
                        .expected(rel)
                        .and_then(|expected| expected.hash)
                },
            );
            let watch = watch.borrow();
            let mut violation = watch.refused.clone();
            if violation.is_none()
                && let Ok(report) = &report
            {
                violation = unaccounted(&watch, report);
            }
            (report, refused, violation)
        })?;
    if let Some(denied) = check.take_denied() {
        return Err(Failure::Reply(denied));
    }
    if let Some((name, reason)) = violation {
        return Err(Failure::Reply(
            ErrorBody::new(
                ErrorCode::Failed,
                format!("The archive's entry {name} {reason}, so the unpack stopped there."),
            )
            .with_details(json!({"violation": {"path": name, "reason": reason}})),
        ));
    }
    let report = report.map_err(xfer_error)?;
    let mut skipped: Vec<SkippedEntry> = report
        .skipped
        .into_iter()
        .map(|entry| match refused.remove(&entry.path) {
            Some(reason) => SkippedEntry {
                path: entry.path,
                reason,
            },
            None => entry,
        })
        .collect();
    skipped.sort();
    let files_count = report.files.len() as u64;
    let dirs_count = report.dirs.len() as u64;
    let symlinks_count = report.symlinks.len() as u64;
    let files_skipped = report.skipped_existing.len() as u64;
    reply(
        output,
        &UnpackDone {
            ok: true,
            files: files_count,
            dirs: dirs_count,
            symlinks: symlinks_count,
            bytes: report.bytes,
            files_skipped,
            skipped,
            renamed: report.renamed,
            verified_files: report.files,
            verified_symlinks: report.symlinks,
            placed_meta: report.placed_meta,
            case_collisions: report.case_collisions,
        },
    )
}

/// The first planned entry an archive that ended well never accounted for: neither written nor
/// left out for a reason the unpack gave, and whether what was written adds up to the plan.
fn unaccounted(watch: &Watch, report: &legion_xfer::UnpackReport) -> Option<(String, String)> {
    let accounted: HashSet<&str> = report
        .files
        .iter()
        .chain(&report.dirs)
        .chain(&report.symlinks)
        .chain(&report.skipped_existing)
        .map(String::as_str)
        .chain(report.skipped.iter().map(|entry| entry.path.as_str()))
        .collect();
    let mut missing: Vec<&String> = watch
        .planned()
        .filter(|name| !accounted.contains(name.as_str()))
        .collect();
    missing.sort();
    if let Some(name) = missing.first() {
        return Some(((*name).clone(), "was planned and never came".to_owned()));
    }
    let planned_bytes: u64 = report
        .files
        .iter()
        .filter_map(|name| watch.expected(name).and_then(|expected| expected.size))
        .sum();
    (planned_bytes != report.bytes).then(|| {
        (
            "the archive".to_owned(),
            format!(
                "carried {} bytes of files planned at {planned_bytes}",
                report.bytes
            ),
        )
    })
}

#[allow(clippy::too_many_arguments)]
fn delete<W: Write>(
    grant: &Grant,
    root: &str,
    names: &[String],
    dry_run: bool,
    remove_root: bool,
    expect_hashes: &HashMap<String, Hash>,
    expect_root_hash: Option<Hash>,
    forbid_file_id: Option<(u64, u64)>,
    output: &mut W,
) -> Result<(), Failure> {
    let checked_root = if remove_root {
        let checked = grant.check_path(root, Access::Write)?;
        legion_tools::fs::check_not_protected_tree(grant, &checked)?;
        if fs::symlink_metadata(&checked).is_ok_and(|meta| meta.file_type().is_dir()) {
            grant.check_tree(&checked)?;
        }
        checked
    } else {
        resolve_root(grant, root, Access::Write)?
    };
    if dry_run {
        return reply(output, &DeleteDone::default());
    }
    let mut done = DeleteDone::default();
    let mut valid: Vec<(usize, String)> = Vec::with_capacity(names.len());
    for name in names {
        match legion_xfer::validate_relative_path(name) {
            Ok(normal) => valid.push((normal.matches('/').count(), normal)),
            Err(error) => done.skipped.push(SkippedEntry {
                path: name.clone(),
                reason: xfer_error(error).message,
            }),
        }
    }
    // Deepest first, so a directory's children are gone before it is.
    valid.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| b.1.cmp(&a.1)));
    valid.dedup();
    busy(output, || {
        for (_, name) in valid {
            let expected = expect_hashes.get(&name).copied();
            match remove_one(grant, &checked_root, &name, expected) {
                Ok(true) => done.removed += 1,
                Ok(false) => {}
                Err(reason) => done.skipped.push(SkippedEntry { path: name, reason }),
            }
        }
        if remove_root {
            match remove_root_path(grant, &checked_root, expect_root_hash, forbid_file_id) {
                Ok(true) => done.removed += 1,
                Ok(false) => {}
                Err(reason) => done.skipped.push(SkippedEntry {
                    path: checked_root.to_string_lossy().into_owned(),
                    reason,
                }),
            }
        }
    })?;
    done.skipped.sort();
    reply(output, &done)
}

/// Removes one name under `root`, never following a symbolic link; says whether there was
/// anything to remove, or why it was left.
fn remove_one(
    grant: &Grant,
    root: &Path,
    name: &str,
    expected_hash: Option<Hash>,
) -> Result<bool, String> {
    let path = match named(grant, root, name, Access::Write) {
        Ok(path) => path,
        Err(Named::Missing) => return Ok(false),
        Err(Named::Refused(reason)) => return Err(reason),
    };
    if let Err(error) = legion_tools::fs::check_not_protected_tree(grant, &path) {
        return Err(error.message);
    }
    if path
        .file_name()
        .and_then(OsStr::to_str)
        .is_some_and(legion_xfer::is_legion_internal_name)
    {
        return Err("it is an internal Legion transfer file".to_owned());
    }
    let meta = match fs::symlink_metadata(&path) {
        Ok(meta) => meta,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error.to_string()),
    };
    let kind = meta.file_type();
    if kind.is_file()
        && let Some(expected) = expected_hash
    {
        let current = legion_xfer::hash::hash_file(&path).map_err(|error| error.to_string())?;
        if current != expected {
            return Err("it changed after it was copied, so the source was kept".to_owned());
        }
    }
    let removed = if kind.is_dir() {
        grant.check_tree(&path).map_err(|error| error.message)?;
        fs::remove_dir(&path).map_err(|error| {
            if fs::read_dir(&path).is_ok_and(|mut entries| entries.next().is_some()) {
                "it is a directory that still holds entries the transfer left alone".to_owned()
            } else {
                error.to_string()
            }
        })
    } else if kind.is_symlink() {
        // A link to a directory is a directory entry on Windows and is removed as one there.
        fs::remove_file(&path)
            .or_else(|error| fs::remove_dir(&path).map_err(|_| error))
            .map_err(|error| error.to_string())
    } else {
        fs::remove_file(&path).map_err(|error| error.to_string())
    };
    removed.map(|()| true)
}

/// Removes the source root itself (a single file, symlink, or now-empty directory) after checking
/// its hash when `expected_hash` is given and refusing if its filesystem identity matches
/// `forbid_file_id`.
fn remove_root_path(
    grant: &Grant,
    path: &Path,
    expected_hash: Option<Hash>,
    forbid_file_id: Option<(u64, u64)>,
) -> Result<bool, String> {
    let meta = match fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error.to_string()),
    };
    if let Some((forbid_dev, forbid_ino)) = forbid_file_id
        && let Ok(current_id) = symlink_file_identity(path)
        && current_id.dev == forbid_dev
        && current_id.ino == forbid_ino
    {
        return Err(
            "source and destination refer to the same file, so the source was kept".to_owned(),
        );
    }
    let kind = meta.file_type();
    if kind.is_file() {
        if let Some(expected) = expected_hash {
            let current = legion_xfer::hash::hash_file(path).map_err(|error| error.to_string())?;
            if current != expected {
                return Err("it changed after it was copied, so the source was kept".to_owned());
            }
        }
        fs::remove_file(path).map_err(|error| error.to_string())?;
        Ok(true)
    } else if kind.is_dir() {
        grant.check_tree(path).map_err(|error| error.message)?;
        match fs::remove_dir(path) {
            Ok(()) => Ok(true),
            Err(error) => {
                if fs::read_dir(path).is_ok_and(|mut entries| entries.next().is_some()) {
                    Ok(false)
                } else {
                    Err(error.to_string())
                }
            }
        }
    } else if kind.is_symlink() {
        fs::remove_file(path)
            .or_else(|error| fs::remove_dir(path).map_err(|_| error))
            .map_err(|error| error.to_string())?;
        Ok(true)
    } else {
        Err("only files, directories, and symbolic links can be removed".to_owned())
    }
}

fn validate_probe_marker(marker: &str) -> Result<(), Failure> {
    let ok = marker.starts_with(".legion-move-probe-")
        && marker.ends_with(".tmp")
        && marker.len() <= 128
        && !marker.contains(['/', '\\', '\0'])
        && !marker.contains("..");
    if !ok {
        return Err(Failure::Reply(ErrorBody::new(
            ErrorCode::InvalidArgs,
            format!("Invalid move probe marker {marker:?}."),
        )));
    }
    Ok(())
}

fn create_private_marker(path: &Path) -> io::Result<fs::File> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)
    }
    #[cfg(not(unix))]
    {
        fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
    }
}

#[allow(clippy::too_many_arguments)]
fn probe_write<R: Read + Send + 'static, W: Write>(
    host: &Host,
    grant: &Grant,
    action: ProbeWriteAction,
    dir: &str,
    marker: Option<&str>,
    nearest_ancestor: bool,
    hold: bool,
    input: &mut Option<BufReader<R>>,
    output: &mut W,
) -> Result<(), Failure> {
    let state_dir = host.paths.state_dir();
    legion_xfer::sweep_abandoned_temps(state_dir, legion_xfer::PROBE_SWEEP_AGE);
    match action {
        ProbeWriteAction::Create => {
            let checked = grant.check_path(dir, Access::Write)?;
            let probe_dir = if fs::metadata(&checked).is_ok_and(|m| m.is_dir()) {
                checked
            } else if nearest_ancestor {
                let mut found = None;
                for anc in checked.ancestors().skip(1) {
                    if anc.as_os_str().is_empty() {
                        continue;
                    }
                    if fs::metadata(anc).is_ok_and(|m| m.is_dir()) {
                        if let Ok(allowed) = grant.check_path(anc, Access::Write) {
                            found = Some(allowed);
                        }
                        break;
                    }
                }
                match found {
                    Some(d) => d,
                    None => {
                        return reply(output, &ProbeReply::default());
                    }
                }
            } else {
                return reply(output, &ProbeReply::default());
            };

            let pid = std::process::id();
            let now_nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            for _ in 0..32 {
                let seq = STREAM_TEMP_SEQ.fetch_add(1, Ordering::Relaxed);
                let name = format!(".legion-move-probe-{pid}-{now_nanos:x}-{seq}.tmp");
                let marker_path = probe_dir.join(&name);
                grant.check_path(&marker_path, Access::Write)?;
                grant.check_create(&marker_path, CreateKind::File, true)?;
                match create_private_marker(&marker_path) {
                    Ok(file) => {
                        let locality =
                            ProbeFsLocality::from(legion_os::fs_kind::detect_fs_locality(&probe_dir));
                        let network_fs = match &locality {
                            ProbeFsLocality::Network(name) => Some(name.clone()),
                            _ => None,
                        };
                        if hold {
                            let marker_id = symlink_file_identity(&marker_path).ok();
                            let owned =
                                legion_xfer::register_owned_temp(state_dir, &marker_path, &file);
                            drop(file);
                            let res = reply(
                                output,
                                &ProbeReply {
                                    dir: Some(probe_dir.to_string_lossy().into_owned()),
                                    marker: Some(name),
                                    exists: true,
                                    network_fs,
                                    fs_locality: Some(locality),
                                    ..ProbeReply::default()
                                },
                            );
                            let mut confirm_requested = false;
                            if res.is_ok()
                                && let Some(reader) = input.as_mut()
                            {
                                // Hold the marker for as long as the probe's pipe stays open.
                                // Never self-expire on a wall-clock timeout: the caller sends
                                // `"confirm\n"` after its `probe_read` finishes, or closes stdin
                                // on abort.
                                let mut line = String::new();
                                if let Ok(n) = reader.read_line(&mut line)
                                    && n > 0
                                    && line.trim() == "confirm"
                                {
                                    confirm_requested = true;
                                }
                            }
                            let still_held = match (
                                marker_id,
                                fs::symlink_metadata(&marker_path),
                                symlink_file_identity(&marker_path),
                            ) {
                                (Some(orig), Ok(meta), Ok(now)) => {
                                    meta.file_type().is_file()
                                        && meta.len() == 0
                                        && orig.same_file(&now)
                                }
                                _ => false,
                            };
                            if let Ok(meta) = fs::symlink_metadata(&marker_path)
                                && meta.file_type().is_file()
                                && meta.len() == 0
                            {
                                let _ = fs::remove_file(&marker_path);
                            }
                            drop(owned);
                            if res.is_ok() && confirm_requested {
                                return reply(output, &ProbeConfirm { held: still_held });
                            }
                            return res;
                        }
                        legion_xfer::record_probe_marker(state_dir, &marker_path, &file);
                        drop(file);
                        return reply(
                            output,
                            &ProbeReply {
                                dir: Some(probe_dir.to_string_lossy().into_owned()),
                                marker: Some(name),
                                exists: true,
                                network_fs,
                                fs_locality: Some(locality),
                                ..ProbeReply::default()
                            },
                        );
                    }
                    Err(err) if err.kind() == io::ErrorKind::AlreadyExists => continue,
                    Err(err) => {
                        return Err(Failure::Reply(io_error("create", &marker_path, &err)));
                    }
                }
            }
            Err(Failure::Reply(ErrorBody::new(
                ErrorCode::Failed,
                format!(
                    "Could not create a unique move probe marker in {}.",
                    probe_dir.display()
                ),
            )))
        }
        ProbeWriteAction::Remove => {
            let marker = marker.ok_or_else(|| {
                Failure::Reply(ErrorBody::new(
                    ErrorCode::InvalidArgs,
                    "probe_write remove requires `marker`.",
                ))
            })?;
            validate_probe_marker(marker)?;
            let checked_dir = grant.check_path(dir, Access::Write)?;
            let marker_path = checked_dir.join(marker);
            grant.check_path(&marker_path, Access::Write)?;
            if let Ok(meta) = fs::symlink_metadata(&marker_path)
                && meta.file_type().is_file()
                && meta.len() == 0
            {
                let _ = fs::remove_file(&marker_path);
            }
            legion_xfer::forget_probe_marker(state_dir, &marker_path);
            reply(
                output,
                &ProbeReply {
                    dir: None,
                    marker: None,
                    exists: false,
                    ..ProbeReply::default()
                },
            )
        }
        ProbeWriteAction::Behavior => {
            let checked_dir = grant.check_path(dir, Access::Write)?;
            if !fs::metadata(&checked_dir).is_ok_and(|m| m.is_dir()) {
                return reply(output, &ProbeReply::default());
            }
            let behavior = probe_dir_name_behavior(&checked_dir);
            reply(
                output,
                &ProbeReply {
                    dir: Some(checked_dir.to_string_lossy().into_owned()),
                    case_insensitive: Some(behavior.case_insensitive),
                    unicode_normalizing: Some(behavior.unicode_normalizing),
                    ..ProbeReply::default()
                },
            )
        }
    }
}

fn probe_read<W: Write>(
    grant: &Grant,
    dir: &str,
    marker: &str,
    subdirs: &[String],
    output: &mut W,
) -> Result<(), Failure> {
    validate_probe_marker(marker)?;
    let checked_dir = grant.check_path(dir, Access::Read)?;
    let root_marker = grant.check_path(checked_dir.join(marker), Access::Read)?;
    let mut exists = fs::symlink_metadata(&root_marker).is_ok();
    if !exists {
        for sub in subdirs {
            if let Ok(sub_path) = named(grant, &checked_dir, sub, Access::Read)
                && let Ok(sub_marker) = grant.check_path(sub_path.join(marker), Access::Read)
                && fs::symlink_metadata(&sub_marker).is_ok()
            {
                exists = true;
                break;
            }
        }
    }
    let locality = ProbeFsLocality::from(legion_os::fs_kind::detect_fs_locality(&checked_dir));
    let network_fs = match &locality {
        ProbeFsLocality::Network(name) => Some(name.clone()),
        _ => None,
    };
    reply(
        output,
        &ProbeReply {
            exists,
            network_fs,
            fs_locality: Some(locality),
            ..ProbeReply::default()
        },
    )
}

/// Extracts the caller's exact final entry name before `canonicalize` can rewrite its case or
/// Unicode normalization on case-insensitive or normalizing filesystems. Falls back to the checked
/// path's final component when `raw` does not name an explicit entry (e.g. ends with `.`, `..`, or
/// `~`).
fn requested_entry_name(raw: &str, checked: &Path, windows_separators: bool) -> Option<OsString> {
    let trimmed = if windows_separators {
        raw.trim_end_matches(['/', '\\'])
    } else {
        raw.trim_end_matches('/')
    };
    let tail = if windows_separators {
        trimmed.rsplit(['/', '\\']).next().unwrap_or("")
    } else {
        trimmed.rsplit('/').next().unwrap_or("")
    };
    let explicit = !tail.is_empty()
        && tail != "."
        && tail != ".."
        && tail != "~"
        && !(windows_separators && tail.ends_with(':'));
    if explicit {
        Some(OsString::from(tail))
    } else {
        checked.file_name().map(OsStr::to_os_string)
    }
}

fn perform_recoverable_two_step_rename(
    state_dir: &Path,
    grant: &Grant,
    from_parent: &Path,
    from_name: &OsStr,
    to_name: &OsStr,
    create_kind: CreateKind,
) -> Result<(), Failure> {
    let from_path = from_parent.join(from_name);
    let to_path = from_parent.join(to_name);
    let pid = std::process::id();
    let mut temp_name = None;
    for _ in 0..32 {
        let seq = STREAM_TEMP_SEQ.fetch_add(1, Ordering::Relaxed);
        let candidate_name = visible_rename_temp_name(from_name, pid, seq);
        let candidate_path = from_parent.join(&candidate_name);
        if fs::symlink_metadata(&candidate_path).is_err() {
            temp_name = Some(candidate_name);
            break;
        }
    }
    let temp_name = temp_name.ok_or_else(|| {
        Failure::Reply(ErrorBody::new(
            ErrorCode::Failed,
            "Could not allocate a temporary name for rename.",
        ))
    })?;
    let temp_path = from_parent.join(&temp_name);
    let temp_checked = grant.check_path(&temp_path, Access::Write)?;
    grant.check_create(&temp_checked, create_kind, false)?;

    let renames_dir = pending_renames_dir(state_dir);
    fs::create_dir_all(&renames_dir)
        .map_err(|err| Failure::Reply(io_error("create", &renames_dir, &err)))?;
    let key = rename_intent_key(from_parent, &temp_name);
    let lock_path = renames_dir.join(format!("{key}.lock"));
    let intent_path = renames_dir.join(format!("{key}.json"));
    let intent_lock = legion_xfer::try_lock_state_file(&lock_path)
        .map_err(xfer_error)?
        .ok_or_else(|| {
            Failure::Reply(ErrorBody::new(
                ErrorCode::Conflict,
                "Another rename is already in progress for this entry.",
            ))
        })?;
    let intent = PendingRenameIntent {
        parent: from_parent.to_string_lossy().into_owned(),
        from_name: from_name.to_string_lossy().into_owned(),
        temp_name: temp_name.clone(),
        to_name: to_name.to_string_lossy().into_owned(),
    };
    let intent_bytes = serde_json::to_vec(&intent).map_err(|err| {
        Failure::Reply(ErrorBody::new(
            ErrorCode::Internal,
            format!("Could not serialize rename recovery record: {err}."),
        ))
    })?;
    write_atomic(&intent_path, &intent_bytes, WriteOptions::with_mode(0o600)).map_err(|err| {
        Failure::Reply(ErrorBody::new(
            ErrorCode::Failed,
            format!("Could not write rename recovery record: {err}."),
        ))
    })?;

    if let Err(err) = move_no_replace_safe(state_dir, &from_path, &temp_path) {
        let _ = fs::remove_file(&intent_path);
        legion_xfer::unlink_held_lock(&lock_path, intent_lock);
        return Err(Failure::Reply(io_error("rename", &temp_path, &err)));
    }
    if let Err(err) = move_no_replace_safe(state_dir, &temp_path, &to_path) {
        // Never use a replacing rename for rollback: if a new entry appeared at `from_path`, keep
        // `temp_path` and the pending rename intent intact and report where the original entry is.
        if move_no_replace_safe(state_dir, &temp_path, &from_path).is_ok() {
            let _ = fs::remove_file(&intent_path);
            legion_xfer::unlink_held_lock(&lock_path, intent_lock);
            return Err(Failure::Reply(io_error("rename", &to_path, &err)));
        }
        return Err(Failure::Reply(ErrorBody::new(
            ErrorCode::Conflict,
            format!(
                "Could not finish renaming {} to {} ({err}), and {} could not be restored without overwriting an existing entry; the original entry was kept at {}.",
                from_path.display(),
                to_path.display(),
                from_path.display(),
                temp_path.display(),
            ),
        )));
    }
    let _ = fs::remove_file(&intent_path);
    legion_xfer::unlink_held_lock(&lock_path, intent_lock);
    Ok(())
}

fn rename_same_file<W: Write>(
    host: &Host,
    grant: &Grant,
    from: &str,
    to: &str,
    output: &mut W,
) -> Result<(), Failure> {
    // Authorised as `fs.move` and subjected to the exact same path, `paths`, and protected-tree
    // checks as `fs.move`.
    let (from_checked, to_checked) = legion_tools::fs::check_move_paths(grant, from, to)?;
    let state_dir = host.paths.state_dir();
    if let Some(parent) = from_checked.parent() {
        recover_pending_renames_checked(
            state_dir,
            Some(RenameRecoveryScope::ExactDir(parent)),
            |p, intent| can_write_rename_intent(grant, p, intent),
        );
    }

    let (Some(from_parent), Some(to_parent)) = (from_checked.parent(), to_checked.parent()) else {
        return Err(Failure::Reply(ErrorBody::new(
            ErrorCode::InvalidArgs,
            "Cannot rename a filesystem root.",
        )));
    };
    let (Some(from_name), Some(to_name)) = (
        requested_entry_name(from, &from_checked, cfg!(windows)),
        requested_entry_name(to, &to_checked, cfg!(windows)),
    ) else {
        return Err(Failure::Reply(ErrorBody::new(
            ErrorCode::InvalidArgs,
            "Cannot rename a filesystem root.",
        )));
    };
    let from_path = from_parent.join(&from_name);
    let to_path = to_parent.join(&to_name);

    let from_meta = fs::symlink_metadata(&from_path)
        .map_err(|err| Failure::Reply(io_error("inspect", &from_path, &err)))?;
    let create_kind = if from_meta.file_type().is_symlink() {
        CreateKind::Symlink
    } else if from_meta.is_dir() {
        CreateKind::Dir
    } else {
        CreateKind::File
    };
    grant.check_create(&to_checked, create_kind, false)?;
    let from_id = symlink_file_identity(&from_path)
        .map_err(|err| Failure::Reply(io_error("inspect", &from_path, &err)))?;
    let (same_file, to_is_symlink) = match fs::symlink_metadata(&to_path) {
        Ok(to_meta) => {
            let to_id = symlink_file_identity(&to_path)
                .map_err(|err| Failure::Reply(io_error("inspect", &to_path, &err)))?;
            (from_id.same_file(&to_id), to_meta.file_type().is_symlink())
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => (false, false),
        Err(err) => return Err(Failure::Reply(io_error("inspect", &to_path, &err))),
    };
    let from_parent_id = file_identity(from_parent)
        .map_err(|err| Failure::Reply(io_error("inspect", from_parent, &err)))?;
    let to_parent_id = file_identity(to_parent)
        .map_err(|err| Failure::Reply(io_error("inspect", to_parent, &err)))?;
    if !from_parent_id.same_file(&to_parent_id) {
        return Err(Failure::Reply(ErrorBody::new(
            ErrorCode::InvalidArgs,
            "Source and destination are not in the same directory.",
        )));
    }

    let either_symlink = from_meta.file_type().is_symlink() || to_is_symlink;
    let fs_behavior = probe_dir_name_behavior(from_parent);
    let decision = classify_same_entry(
        Some(from_parent_id),
        Some(to_parent_id),
        &from_name,
        &to_name,
        same_file,
        either_symlink,
        fs_behavior,
    );
    // Stream `rename` is strictly limited to same-entry case or Unicode-normalization renames.
    // Every other rename (including arbitrary moves of distinct files) is refused here so callers
    // must use `fs.move` or the full transfer path.
    if decision != SameEntryDecision::CaseOrNormalizationRename {
        let message = match decision {
            SameEntryDecision::SameEntryNoOp => {
                format!("Cannot rename {} onto itself.", from_path.display())
            }
            SameEntryDecision::RefuseSameFileAlias => format!(
                "Cannot rename {} onto {} because they are different links to the same entry.",
                from_path.display(),
                to_path.display()
            ),
            SameEntryDecision::DistinctEntries => format!(
                "Stream rename only renames an entry's letter case or Unicode spelling in place; {} and {} are not the same entry.",
                from_path.display(),
                to_path.display()
            ),
            SameEntryDecision::CaseOrNormalizationRename => unreachable!(),
        };
        return Err(Failure::Reply(
            ErrorBody::new(ErrorCode::InvalidArgs, message)
                .with_hint("Use fs.move or fs.transfer to move distinct paths."),
        ));
    }

    // Try a direct rename first (succeeds in one kernel operation on modern APFS, NTFS, and most
    // folding filesystems). Fall back to the recoverable visible two-step route ONLY when the
    // direct rename leaves the old spelling in place or reports an error leaving `from` intact.
    let direct_ok = fs::rename(&from_path, &to_path).is_ok()
        && directory_contains_exact_name(from_parent, &to_name).unwrap_or(false);
    if !direct_ok {
        perform_recoverable_two_step_rename(
            state_dir,
            grant,
            from_parent,
            &from_name,
            &to_name,
            create_kind,
        )?;
    }

    #[cfg(unix)]
    if let Ok(dir_handle) = fs::File::open(from_parent) {
        let _ = dir_handle.sync_all();
    }

    // Verify by listing the directory and comparing entry name bytes (not `exists()`) that the
    // directory now holds the exact spelling the caller requested.
    if let Ok(found) = directory_contains_exact_name(from_parent, &to_name)
        && !found
    {
        return Err(Failure::Reply(ErrorBody::new(
            ErrorCode::Failed,
            format!(
                "The filesystem did not store the renamed entry with the exact requested name {}.",
                to_path.display()
            ),
        )));
    }
    reply(output, &RenameDone { ok: true })
}

/// Default lease for stream `lock` calls before automatic expiry unless renewed (`renew\n`).
const MOVE_LOCK_DEFAULT_LEASE: Duration = Duration::from_secs(10);

/// Minimum caller-requested lease duration (for tests) and hard cap on total lock hold time.
const MOVE_LOCK_MIN_LEASE: Duration = Duration::from_millis(50);
const MOVE_LOCK_MAX_HOLD: Duration = Duration::from_secs(60);

/// Maximum number of concurrent move locks one principal may hold across all streams on a node.
const MAX_LOCKS_PER_PRINCIPAL: usize = 256;

#[derive(Clone, Debug, Serialize, Deserialize)]
struct MoveLockMeta {
    principal: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    job_id: Option<String>,
    expires_at_ms: u64,
}

fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

/// Sibling metadata file (`<key>.meta`) paired with a move-lock `<key>.lock` file. Stored
/// separately from the locked file because Windows enforces mandatory byte-range locks that
/// prevent other processes from reading a locked `.lock` file.
fn move_lock_meta_path(lock_path: &Path) -> PathBuf {
    lock_path.with_extension("meta")
}

fn write_lock_meta(lock_path: &Path, meta: &MoveLockMeta) {
    if let Ok(bytes) = serde_json::to_vec(meta) {
        let _ = write_atomic(
            move_lock_meta_path(lock_path),
            &bytes,
            WriteOptions::with_mode(0o600),
        );
    }
}

fn read_lock_meta(lock_path: &Path) -> Option<MoveLockMeta> {
    let bytes = fs::read(move_lock_meta_path(lock_path)).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// Drops acquired move locks cleanly by removing each sibling `.meta` and `.lock` file so normal
/// release, lease expiry, or partial-acquisition failure leaves zero stale lock files behind.
struct HeldLockFiles {
    files: Vec<(PathBuf, fs::File)>,
}

impl HeldLockFiles {
    fn new(cap: usize) -> Self {
        Self {
            files: Vec::with_capacity(cap),
        }
    }

    fn push(&mut self, path: PathBuf, file: fs::File) {
        self.files.push((path, file));
    }

    fn update_expiry(&mut self, principal: &str, job_id: Option<&str>, expires_at_ms: u64) {
        let meta = MoveLockMeta {
            principal: principal.to_owned(),
            job_id: job_id.map(str::to_owned),
            expires_at_ms,
        };
        for (path, _) in &self.files {
            write_lock_meta(path, &meta);
        }
    }
}

impl Drop for HeldLockFiles {
    fn drop(&mut self) {
        for (path, file) in self.files.drain(..).rev() {
            let _ = fs::remove_file(move_lock_meta_path(&path));
            legion_xfer::unlink_held_lock(&path, file);
        }
    }
}

/// Sweeps any unlocked `.lock` files (and orphaned `.meta` files) in `<state_dir>/xfer/move-lock/`
/// immediately and counts how many currently live locks are held by `principal`.
fn sweep_and_count_principal_locks(state_dir: &Path, principal: &str) -> usize {
    let dir = state_dir.join("xfer").join("move-lock");
    let Ok(entries) = fs::read_dir(&dir) else {
        return 0;
    };
    let now_ms = now_unix_ms();
    let mut held_by_principal = 0usize;
    let mut meta_candidates = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        match path.extension().and_then(OsStr::to_str) {
            Some("meta") => {
                meta_candidates.push(path);
                continue;
            }
            Some("lock") => {}
            _ => continue,
        }
        let Ok(file) = fs::OpenOptions::new().read(true).write(true).open(&path) else {
            continue;
        };
        match file.try_lock() {
            Ok(()) => {
                // Unlocked leftover (for example after SIGKILL): remove cleanly.
                let _ = fs::remove_file(move_lock_meta_path(&path));
                legion_xfer::unlink_held_lock(&path, file);
            }
            Err(_) => {
                // Actively held by another process: read unlocked sibling `.meta` and count toward
                // `principal`'s quota if valid.
                if let Some(meta) = read_lock_meta(&path)
                    && meta.principal == principal
                    && meta.expires_at_ms > now_ms
                {
                    held_by_principal += 1;
                }
            }
        }
    }
    for meta_path in meta_candidates {
        if !meta_path.with_extension("lock").exists() {
            let _ = fs::remove_file(&meta_path);
        }
    }
    held_by_principal
}

/// Acquires exclusive per-path move locks in `<state_dir>/xfer/move-lock/` in deterministic sorted
/// path order, enforces a per-principal lock limit, returns live metadata ONLY for paths the
/// caller may read (`fs.read`), and holds the locks for a short renewable lease that expires
/// automatically if not renewed or released.
#[allow(clippy::too_many_arguments)]
fn lock_paths<R: Read + Send + 'static, W: Write>(
    host: &Host,
    grant: &Grant,
    pairing_id: Option<&str>,
    paths: &[String],
    lease_ms: Option<u64>,
    job_id: Option<&str>,
    input: &mut Option<BufReader<R>>,
    output: &mut W,
) -> Result<(), Failure> {
    if paths.is_empty() || paths.len() > MAX_LOCKS_PER_PRINCIPAL {
        return Err(Failure::Reply(ErrorBody::new(
            ErrorCode::InvalidArgs,
            format!(
                "lock requires between 1 and {MAX_LOCKS_PER_PRINCIPAL} paths, and {} were given.",
                paths.len()
            ),
        )));
    }
    let lease = lease_ms
        .map(|ms| Duration::from_millis(ms).clamp(MOVE_LOCK_MIN_LEASE, MOVE_LOCK_DEFAULT_LEASE))
        .unwrap_or(MOVE_LOCK_DEFAULT_LEASE);
    let mut checked_pairs: Vec<(String, PathBuf)> = Vec::with_capacity(paths.len());
    for raw in paths {
        let checked = grant.check_path(raw, Access::Write)?;
        checked_pairs.push((raw.clone(), checked));
    }
    // Sort unique lock targets in deterministic byte order before taking any lock so concurrent
    // moves on this node can never deadlock.
    let mut lock_targets: Vec<PathBuf> = checked_pairs.iter().map(|(_, p)| p.clone()).collect();
    lock_targets.sort_by(|a, b| {
        a.as_os_str()
            .as_encoded_bytes()
            .cmp(b.as_os_str().as_encoded_bytes())
    });
    lock_targets.dedup();

    let state_dir = host.paths.state_dir();
    let held_count = sweep_and_count_principal_locks(state_dir, grant.principal());
    if held_count + lock_targets.len() > MAX_LOCKS_PER_PRINCIPAL {
        return Err(Failure::Reply(
            ErrorBody::new(
                ErrorCode::Busy,
                format!(
                    "{} already holds {held_count} move locks (limit {MAX_LOCKS_PER_PRINCIPAL}).",
                    grant.principal()
                ),
            )
            .with_hint(
                "Release existing locks or wait for their lease to expire before acquiring more.",
            ),
        ));
    }

    let initial_expiry_ms =
        now_unix_ms().saturating_add(u64::try_from(lease.as_millis()).unwrap_or(10_000));
    let meta = MoveLockMeta {
        principal: grant.principal().to_owned(),
        job_id: job_id.map(str::to_owned),
        expires_at_ms: initial_expiry_ms,
    };

    let mut locks = HeldLockFiles::new(lock_targets.len());
    for target in &lock_targets {
        let lock_path = legion_xfer::move_lock_state_path(state_dir, target);
        match legion_xfer::try_lock_state_file(&lock_path) {
            Ok(Some(file)) => {
                write_lock_meta(&lock_path, &meta);
                locks.push(lock_path, file);
            }
            Ok(None) => {
                let holder_info = read_lock_meta(&lock_path).and_then(|holder| {
                    let allowed = match holder.job_id.as_deref() {
                        Some(jid) => legion_tools::job::can_principal_see_job(
                            &host.paths,
                            grant,
                            pairing_id,
                            jid,
                        ),
                        None => holder.principal == grant.principal() || grant.roots().is_none(),
                    };
                    allowed.then_some(holder)
                });
                drop(locks);
                let mut err = match holder_info {
                    Some(holder) => {
                        let who = match holder.job_id.as_deref() {
                            Some(jid) => format!("job {jid} ({})", holder.principal),
                            None => holder.principal.clone(),
                        };
                        ErrorBody::new(
                            ErrorCode::Conflict,
                            format!(
                                "Another move or transfer ({who}) is currently finalizing {}.",
                                target.display()
                            ),
                        )
                        .with_details(json!({
                            "holder": holder.principal,
                            "job_id": holder.job_id,
                        }))
                    }
                    None => ErrorBody::new(
                        ErrorCode::Conflict,
                        format!(
                            "Another move or transfer is currently finalizing {}.",
                            target.display()
                        ),
                    ),
                };
                err = err.with_hint("Run the transfer again in a moment.");
                return Err(Failure::Reply(err));
            }
            Err(err) => {
                drop(locks);
                return Err(Failure::Reply(xfer_error(err)));
            }
        }
    }

    // Only return metadata that the caller could obtain from `stat` with its own `fs.read` rights.
    // A write-only role receives no existence, kind, size, mtime, or inode details.
    let read_grant = ConfigStore::new(host.paths.config_dir())
        .get::<Policy>()
        .ok()
        .and_then(|policy| {
            policy
                .authorize_name(
                    grant.principal(),
                    "fs.read",
                    Some(Effect::Read),
                    &host.paths,
                )
                .ok()
        });

    let mut entries = Vec::new();
    if let Some(ref rg) = read_grant {
        entries.reserve(checked_pairs.len());
        for (raw, checked) in &checked_pairs {
            if rg.check_path(raw, Access::Read).is_err() {
                continue;
            }
            let entry = match fs::symlink_metadata(checked) {
                Ok(meta) => {
                    let ft = meta.file_type();
                    let kind = if ft.is_file() {
                        "file"
                    } else if ft.is_dir() {
                        "dir"
                    } else if ft.is_symlink() {
                        "symlink"
                    } else {
                        "other"
                    };
                    let link_target = if ft.is_symlink() {
                        fs::read_link(checked)
                            .ok()
                            .map(|t| t.to_string_lossy().into_owned())
                    } else {
                        None
                    };
                    LockEntry {
                        path: raw.clone(),
                        exists: true,
                        kind: kind.to_owned(),
                        file_id: symlink_file_identity(checked).ok(),
                        size: if ft.is_file() { meta.len() } else { 0 },
                        mtime: meta.modified().ok().map(Timestamp::from_system_time),
                        link_target,
                    }
                }
                Err(err) if err.kind() == io::ErrorKind::NotFound => LockEntry {
                    path: raw.clone(),
                    exists: false,
                    kind: "missing".to_owned(),
                    ..LockEntry::default()
                },
                Err(err) => {
                    drop(locks);
                    return Err(Failure::Reply(io_error("inspect", checked, &err)));
                }
            };
            entries.push(entry);
        }
    }

    reply(
        output,
        &LockReady {
            locked: true,
            entries,
        },
    )?;

    // Hold locks for at most `lease` at a time (renewed by `"renew\n"` lines, up to
    // `MOVE_LOCK_MAX_HOLD` total), releasing immediately on EOF, `"release\n"`, or lease expiry.
    if let Some(mut reader) = input.take() {
        let (tx, rx) = mpsc::channel::<String>();
        std::thread::spawn(move || {
            let mut line_buf = Vec::new();
            loop {
                line_buf.clear();
                match reader.read_until(b'\n', &mut line_buf) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {
                        let text = String::from_utf8_lossy(&line_buf).trim().to_owned();
                        let keep_going = text == "renew";
                        if tx.send(text).is_err() || !keep_going {
                            break;
                        }
                    }
                }
            }
        });
        let started = Instant::now();
        let hard_deadline = started + MOVE_LOCK_MAX_HOLD;
        let mut lease_deadline = (started + lease).min(hard_deadline);
        loop {
            let now = Instant::now();
            if now >= lease_deadline {
                break;
            }
            match rx.recv_timeout(lease_deadline.saturating_duration_since(now)) {
                Ok(cmd) if cmd == "renew" => {
                    let next = (Instant::now() + lease).min(hard_deadline);
                    lease_deadline = next;
                    let rem_ms = u64::try_from(
                        lease_deadline
                            .saturating_duration_since(Instant::now())
                            .as_millis(),
                    )
                    .unwrap_or(0);
                    locks.update_expiry(
                        grant.principal(),
                        job_id,
                        now_unix_ms().saturating_add(rem_ms),
                    );
                }
                Ok(_) | Err(RecvTimeoutError::Timeout) | Err(RecvTimeoutError::Disconnected) => {
                    break;
                }
            }
        }
    }
    drop(locks);
    Ok(())
}

/// Runs `work` on a thread of its own and says `{"busy": true}` every [`HEARTBEAT`] until it is
/// done, so a reader waiting for the next line knows the operation is at work, not stuck.
fn busy<W: Write, T: Send>(output: &mut W, work: impl FnOnce() -> T + Send) -> Result<T, Failure> {
    std::thread::scope(|scope| {
        let (done, finished) = mpsc::channel();
        scope.spawn(move || {
            let _ = done.send(work());
        });
        wait_for(&finished, output)
    })
}

/// Like [`busy`], for work that reads the operation's input: a dedicated reader thread passes the
/// input on in chunks (`limit` bytes at most, or to its end) while the work runs on its own, and
/// this thread waits for `work` (writing `{"busy": true}` every [`HEARTBEAT`]). Decoupling the
/// input reader from the scoped worker guarantees that a stalled sender (0 B/s) never blocks
/// mid-stream revocation: once `work` detects revocation on its 2-second timer and exits, this
/// function returns immediately so `serve` can write the `denied` reply and close the stream.
fn feed<R: Read + Send + 'static, W: Write, T: Send>(
    input: &mut Option<BufReader<R>>,
    limit: Option<u64>,
    output: &mut W,
    check: Option<StreamAuthCheck>,
    work: impl FnOnce(Fed) -> T + Send,
) -> Result<T, Failure> {
    let Some(mut reader) = input.take() else {
        return Err(Failure::Reply(ErrorBody::new(
            ErrorCode::Internal,
            "The stream input was already consumed.",
        )));
    };
    let (chunks, fed) = mpsc::sync_channel::<Vec<u8>>(QUEUED_CHUNKS);
    let check_for_reader = check.clone();
    std::thread::spawn(move || {
        let mut left = limit.unwrap_or(u64::MAX);
        let mut trailing = 0u64;
        let mut taking = true;
        while left > 0 && trailing <= MOST_TRAILING {
            if check_for_reader
                .as_ref()
                .is_some_and(StreamAuthCheck::is_denied)
            {
                break;
            }
            let available = match reader.fill_buf() {
                Ok([]) => break,
                Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
                Err(_) => break,
                Ok(available) => available,
            };
            let take = usize::try_from(left)
                .unwrap_or(usize::MAX)
                .min(available.len());
            if taking && chunks.send(available[..take].to_vec()).is_err() {
                taking = false;
                if check_for_reader
                    .as_ref()
                    .is_some_and(StreamAuthCheck::is_denied)
                {
                    break;
                }
            }
            if !taking {
                trailing += take as u64;
            }
            reader.consume(take);
            left -= take as u64;
        }
        drop(chunks);
    });
    std::thread::scope(|scope| {
        let (done, finished) = mpsc::channel();
        scope.spawn(move || {
            let _ = done.send(work(Fed {
                chunks: fed,
                chunk: Vec::new(),
                at: 0,
                check,
            }));
        });
        wait_for(&finished, output)
    })
}

/// Waits for work to finish, saying it is busy every [`HEARTBEAT`].
fn wait_for<W: Write, T>(finished: &Receiver<T>, output: &mut W) -> Result<T, Failure> {
    loop {
        match finished.recv_timeout(HEARTBEAT) {
            Ok(result) => return Ok(result),
            Err(RecvTimeoutError::Timeout) => {
                line(output, &json!({"busy": true}))
                    .and_then(|()| flush_retry(output))
                    .map_err(|error| Failure::Raw(gone(&error)))?;
            }
            Err(RecvTimeoutError::Disconnected) => {
                return Err(Failure::Reply(ErrorBody::new(
                    ErrorCode::Internal,
                    "The stream operation stopped without an answer.",
                )));
            }
        }
    }
}

/// The input of an operation, as [`feed`] passes it on.
struct Fed {
    chunks: Receiver<Vec<u8>>,
    chunk: Vec<u8>,
    at: usize,
    check: Option<StreamAuthCheck>,
}

impl Read for Fed {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        while self.at == self.chunk.len() {
            match self.chunks.recv_timeout(RECHECK_POLL_INTERVAL) {
                Ok(chunk) => {
                    self.chunk = chunk;
                    self.at = 0;
                }
                Err(RecvTimeoutError::Timeout) => {
                    if let Some(ref check) = self.check {
                        let mut zero = 0u64;
                        check.maybe_recheck(&mut zero)?;
                    }
                }
                Err(RecvTimeoutError::Disconnected) => return Ok(0),
            }
        }
        let n = buf.len().min(self.chunk.len() - self.at);
        buf[..n].copy_from_slice(&self.chunk[self.at..self.at + n]);
        self.at += n;
        Ok(n)
    }
}

/// A manifest's time as the system's, or nothing for one it cannot hold.
fn system_time(time: Timestamp) -> Option<SystemTime> {
    let nanos = Duration::from_nanos(u64::from(time.nanos));
    if time.secs >= 0 {
        UNIX_EPOCH
            .checked_add(Duration::from_secs(time.secs.unsigned_abs()))?
            .checked_add(nanos)
    } else {
        UNIX_EPOCH
            .checked_sub(Duration::from_secs(time.secs.unsigned_abs()))?
            .checked_add(nanos)
    }
}

/// Why a name a peer sent cannot be used.
enum Named {
    /// A part of it does not exist.
    Missing,
    /// It may not be used, for this reason.
    Refused(String),
}

/// A relative name a peer sent for an entry under `root`, walked part by part without following
/// a symbolic link: every part before the last must be a real directory, so a link inside the tree
/// cannot lead a `pack` or `delete` somewhere else. The last part, when it is a link, is the link
/// itself. The path is then checked for `access` like any other.
fn named(grant: &Grant, root: &Path, name: &str, access: Access) -> Result<PathBuf, Named> {
    let name = legion_xfer::validate_relative_path(name)
        .map_err(|error| Named::Refused(xfer_error(error).message))?;
    let parts: Vec<&str> = name.split('/').collect();
    let mut path = root.to_path_buf();
    for (at, part) in parts.iter().enumerate() {
        path.push(part);
        if at + 1 == parts.len() {
            break;
        }
        let way = parts[..=at].join("/");
        match fs::symlink_metadata(&path) {
            Ok(meta) if meta.file_type().is_symlink() => {
                return Err(Named::Refused(format!(
                    "{way} on its way is a symbolic link, which is never followed"
                )));
            }
            Ok(meta) if meta.is_dir() => {}
            Ok(_) => {
                return Err(Named::Refused(format!(
                    "{way} on its way is not a directory"
                )));
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Err(Named::Missing),
            Err(error) => return Err(Named::Refused(error.to_string())),
        }
    }
    grant
        .check_path(&path, access)
        .map_err(|error| Named::Refused(error.message))
}

/// A relative name under `root`, validated as a transfer path and checked for creating or
/// replacing an entry of `kind` (including any missing parent directories).
fn checked_create(
    grant: &Grant,
    root: &Path,
    name: &str,
    kind: CreateKind,
) -> Result<PathBuf, ErrorBody> {
    let path = resolve_relative_path(root, name).map_err(xfer_error)?;
    let checked = grant.check_path(&path, Access::Write)?;
    legion_tools::fs::check_not_protected_tree(grant, &checked)?;
    grant.check_create(&checked, kind, true)?;
    Ok(checked)
}

fn io_error(action: &str, path: &Path, error: &io::Error) -> ErrorBody {
    let code = match error.kind() {
        io::ErrorKind::NotFound => ErrorCode::NotFound,
        io::ErrorKind::PermissionDenied => ErrorCode::Denied,
        _ => ErrorCode::Failed,
    };
    ErrorBody::new(
        code,
        format!("Could not {action} {}: {error}.", path.display()),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(dev: u64, ino: u64, nlink: u64) -> FileIdentity {
        FileIdentity { dev, ino, nlink }
    }

    const CASE_SENSITIVE_RAW: FsNameBehavior = FsNameBehavior {
        case_insensitive: false,
        unicode_normalizing: false,
    };
    const CASE_INSENSITIVE_RAW: FsNameBehavior = FsNameBehavior {
        case_insensitive: true,
        unicode_normalizing: false,
    };
    const CASE_SENSITIVE_NORM: FsNameBehavior = FsNameBehavior {
        case_insensitive: false,
        unicode_normalizing: true,
    };
    const CASE_INSENSITIVE_NORM: FsNameBehavior = FsNameBehavior {
        case_insensitive: true,
        unicode_normalizing: true,
    };

    #[test]
    fn classify_same_entry_covers_case_normalization_identities_and_aliases() {
        let dir_a = Some(id(1, 10, 2));
        let dir_b = Some(id(1, 20, 2));

        // 1. Identical byte-for-byte names in the same parent are always a no-op, under every FS
        // behavior and whether or not file identity was observed.
        for fs_mode in [
            CASE_SENSITIVE_RAW,
            CASE_INSENSITIVE_RAW,
            CASE_SENSITIVE_NORM,
            CASE_INSENSITIVE_NORM,
        ] {
            for same_file in [true, false] {
                for either_symlink in [false, true] {
                    assert_eq!(
                        classify_same_entry(
                            dir_a,
                            dir_a,
                            OsStr::new("Report.txt"),
                            OsStr::new("Report.txt"),
                            same_file,
                            either_symlink,
                            fs_mode,
                        ),
                        SameEntryDecision::SameEntryNoOp,
                        "identical name must be no-op under {fs_mode:?}"
                    );
                }
            }
        }

        // 2. ASCII case difference (`Report.txt` vs `report.txt`) resolving to the same file in
        // the same parent:
        // - case-insensitive (NTFS, default APFS): folding rename
        // - case-sensitive (Linux ext4, case-sensitive APFS): hardlink alias, refuse
        assert_eq!(
            classify_same_entry(
                dir_a,
                dir_a,
                OsStr::new("Report.txt"),
                OsStr::new("report.txt"),
                true,
                false,
                CASE_INSENSITIVE_RAW,
            ),
            SameEntryDecision::CaseOrNormalizationRename
        );
        assert_eq!(
            classify_same_entry(
                dir_a,
                dir_a,
                OsStr::new("Report.txt"),
                OsStr::new("report.txt"),
                true,
                false,
                CASE_INSENSITIVE_NORM,
            ),
            SameEntryDecision::CaseOrNormalizationRename
        );
        assert_eq!(
            classify_same_entry(
                dir_a,
                dir_a,
                OsStr::new("Report.txt"),
                OsStr::new("report.txt"),
                true,
                false,
                CASE_SENSITIVE_RAW,
            ),
            SameEntryDecision::RefuseSameFileAlias
        );
        assert_eq!(
            classify_same_entry(
                dir_a,
                dir_a,
                OsStr::new("Report.txt"),
                OsStr::new("report.txt"),
                true,
                false,
                CASE_SENSITIVE_NORM,
            ),
            SameEntryDecision::RefuseSameFileAlias
        );

        // 3. Non-ASCII case difference (`Ärger.txt` vs `ärger.txt`) resolving to the same file:
        let upper_umlaut = OsStr::new("\u{00c4}rger.txt");
        let lower_umlaut = OsStr::new("\u{00e4}rger.txt");
        assert_eq!(
            classify_same_entry(
                dir_a,
                dir_a,
                upper_umlaut,
                lower_umlaut,
                true,
                false,
                CASE_INSENSITIVE_RAW,
            ),
            SameEntryDecision::CaseOrNormalizationRename
        );
        assert_eq!(
            classify_same_entry(
                dir_a,
                dir_a,
                upper_umlaut,
                lower_umlaut,
                true,
                false,
                CASE_INSENSITIVE_NORM,
            ),
            SameEntryDecision::CaseOrNormalizationRename
        );
        assert_eq!(
            classify_same_entry(
                dir_a,
                dir_a,
                upper_umlaut,
                lower_umlaut,
                true,
                false,
                CASE_SENSITIVE_RAW,
            ),
            SameEntryDecision::RefuseSameFileAlias
        );
        assert_eq!(
            classify_same_entry(
                dir_a,
                dir_a,
                upper_umlaut,
                lower_umlaut,
                true,
                false,
                CASE_SENSITIVE_NORM,
            ),
            SameEntryDecision::RefuseSameFileAlias
        );

        // 4. Unicode NFC vs NFD (`caf\u{00e9}.txt` vs `cafe\u{0301}.txt`) resolving to the same
        // file:
        // - normalizing (APFS, HFS+, whether case-sensitive or case-insensitive): folding rename
        // - non-normalizing (Linux ext4, Windows NTFS): hardlink alias, refuse
        let nfc = OsStr::new("caf\u{00e9}.txt");
        let nfd = OsStr::new("cafe\u{0301}.txt");
        assert_eq!(
            classify_same_entry(dir_a, dir_a, nfd, nfc, true, false, CASE_SENSITIVE_NORM),
            SameEntryDecision::CaseOrNormalizationRename
        );
        assert_eq!(
            classify_same_entry(dir_a, dir_a, nfd, nfc, true, false, CASE_INSENSITIVE_NORM),
            SameEntryDecision::CaseOrNormalizationRename
        );
        assert_eq!(
            classify_same_entry(dir_a, dir_a, nfd, nfc, true, false, CASE_SENSITIVE_RAW),
            SameEntryDecision::RefuseSameFileAlias
        );
        assert_eq!(
            classify_same_entry(dir_a, dir_a, nfd, nfc, true, false, CASE_INSENSITIVE_RAW),
            SameEntryDecision::RefuseSameFileAlias
        );

        // 5. Combined non-ASCII case + NFC vs NFD (`Ä` vs `a\u{0308}`): folding rename only when
        // BOTH case-insensitive AND normalizing.
        let precomposed_upper = OsStr::new("\u{00c4}.txt");
        let decomposed_lower = OsStr::new("a\u{0308}.txt");
        assert_eq!(
            classify_same_entry(
                dir_a,
                dir_a,
                precomposed_upper,
                decomposed_lower,
                true,
                false,
                CASE_INSENSITIVE_NORM,
            ),
            SameEntryDecision::CaseOrNormalizationRename
        );
        for fs_mode in [
            CASE_SENSITIVE_RAW,
            CASE_INSENSITIVE_RAW,
            CASE_SENSITIVE_NORM,
        ] {
            assert_eq!(
                classify_same_entry(
                    dir_a,
                    dir_a,
                    precomposed_upper,
                    decomposed_lower,
                    true,
                    false,
                    fs_mode,
                ),
                SameEntryDecision::RefuseSameFileAlias,
                "combined case+norm mismatch must refuse under {fs_mode:?}"
            );
        }

        // 6. Different files that merely have case-different names on a case-sensitive filesystem
        // (e.g. `Report.txt` and `report.txt` are distinct inodes): DistinctEntries.
        assert_eq!(
            classify_same_entry(
                dir_a,
                dir_a,
                OsStr::new("Report.txt"),
                OsStr::new("report.txt"),
                false,
                false,
                CASE_SENSITIVE_RAW,
            ),
            SameEntryDecision::DistinctEntries
        );
        assert_eq!(
            classify_same_entry(
                dir_a,
                dir_a,
                upper_umlaut,
                lower_umlaut,
                false,
                false,
                CASE_SENSITIVE_RAW,
            ),
            SameEntryDecision::DistinctEntries
        );

        // 7. Hard links with unrelated names, symlinks, and different-parent aliases are always
        // refused even on case-insensitive normalizing filesystems.
        assert_eq!(
            classify_same_entry(
                dir_a,
                dir_a,
                OsStr::new("a.txt"),
                OsStr::new("b.txt"),
                true,
                false,
                CASE_INSENSITIVE_NORM,
            ),
            SameEntryDecision::RefuseSameFileAlias
        );
        assert_eq!(
            classify_same_entry(
                dir_a,
                dir_a,
                OsStr::new("Report.txt"),
                OsStr::new("report.txt"),
                true,
                true,
                CASE_INSENSITIVE_NORM,
            ),
            SameEntryDecision::RefuseSameFileAlias
        );
        assert_eq!(
            classify_same_entry(
                dir_a,
                dir_b,
                OsStr::new("Report.txt"),
                OsStr::new("report.txt"),
                true,
                false,
                CASE_INSENSITIVE_NORM,
            ),
            SameEntryDecision::RefuseSameFileAlias
        );
    }

    #[test]
    fn requested_entry_name_preserves_caller_spelling_across_unix_and_windows() {
        let checked = Path::new("/home/me/Report.txt");
        assert_eq!(
            requested_entry_name("~/report.txt", checked, false),
            Some(OsString::from("report.txt"))
        );
        assert_eq!(
            requested_entry_name("~/report.txt/", checked, false),
            Some(OsString::from("report.txt"))
        );
        assert_eq!(
            requested_entry_name(r"C:\Users\me\report.txt", checked, true),
            Some(OsString::from("report.txt"))
        );
        assert_eq!(
            requested_entry_name("~/report.txt", checked, true),
            Some(OsString::from("report.txt"))
        );
        assert_eq!(
            requested_entry_name("~/sub/.", checked, false),
            Some(OsString::from("Report.txt"))
        );
        assert_eq!(
            requested_entry_name("~/sub/..", checked, false),
            Some(OsString::from("Report.txt"))
        );
        assert_eq!(
            requested_entry_name("~", checked, false),
            Some(OsString::from("Report.txt"))
        );
    }

    #[test]
    fn decide_pending_rename_recovery_covers_every_state_on_sensitive_and_folding_models() {
        let from = OsStr::new("Notes.txt");
        let temp = OsStr::new("legion-renaming-42-8-Notes.txt");
        let to = OsStr::new("notes.txt");
        let list = |names: &[&str]| -> Vec<OsString> { names.iter().map(OsString::from).collect() };

        for fs_mode in [
            CASE_SENSITIVE_RAW,
            CASE_INSENSITIVE_RAW,
            CASE_SENSITIVE_NORM,
            CASE_INSENSITIVE_NORM,
        ] {
            // 1. Only original spelling present (crash before step 1 or already rolled back).
            assert_eq!(
                decide_pending_rename_recovery(&list(&["Notes.txt"]), from, temp, to, fs_mode),
                PendingRenameRecovery::DropStaleIntent,
                "only original under {fs_mode:?}"
            );
            // 2. Only temporary name present (crash between step 1 and step 2).
            assert_eq!(
                decide_pending_rename_recovery(
                    &list(&["legion-renaming-42-8-Notes.txt"]),
                    from,
                    temp,
                    to,
                    fs_mode,
                ),
                PendingRenameRecovery::CompleteToFinal,
                "only temp under {fs_mode:?}"
            );
            // 3. Only final spelling present (crash after step 2, before deleting intent).
            assert_eq!(
                decide_pending_rename_recovery(&list(&["notes.txt"]), from, temp, to, fs_mode),
                PendingRenameRecovery::DropStaleIntent,
                "only final under {fs_mode:?}"
            );
            // 4. Directory empty (user already removed or moved entry).
            assert_eq!(
                decide_pending_rename_recovery(&list(&[]), from, temp, to, fs_mode),
                PendingRenameRecovery::DropStaleIntent,
                "empty under {fs_mode:?}"
            );
            // 5. All three present -> keep everything untouched.
            assert_eq!(
                decide_pending_rename_recovery(
                    &list(&["Notes.txt", "legion-renaming-42-8-Notes.txt", "notes.txt"]),
                    from,
                    temp,
                    to,
                    fs_mode,
                ),
                PendingRenameRecovery::KeepBoth,
                "all three under {fs_mode:?}"
            );
        }

        // 6. Temporary and original both present:
        //    - Case-sensitive: `notes.txt` is a distinct free slot -> complete `temp -> to`.
        //    - Case-insensitive: `Notes.txt` occupies `notes.txt`'s folded slot -> keep both.
        assert_eq!(
            decide_pending_rename_recovery(
                &list(&["legion-renaming-42-8-Notes.txt", "Notes.txt"]),
                from,
                temp,
                to,
                CASE_SENSITIVE_RAW,
            ),
            PendingRenameRecovery::CompleteToFinal
        );
        assert_eq!(
            decide_pending_rename_recovery(
                &list(&["legion-renaming-42-8-Notes.txt", "Notes.txt"]),
                from,
                temp,
                to,
                CASE_INSENSITIVE_RAW,
            ),
            PendingRenameRecovery::KeepBoth
        );
        assert_eq!(
            decide_pending_rename_recovery(
                &list(&["legion-renaming-42-8-Notes.txt", "Notes.txt"]),
                from,
                temp,
                to,
                CASE_INSENSITIVE_NORM,
            ),
            PendingRenameRecovery::KeepBoth
        );

        // 7. Temporary and final both present:
        //    - Case-sensitive: `Notes.txt` is a distinct free slot -> reverse `temp -> from`.
        //    - Case-insensitive: `notes.txt` occupies `Notes.txt`'s folded slot -> keep both.
        assert_eq!(
            decide_pending_rename_recovery(
                &list(&["legion-renaming-42-8-Notes.txt", "notes.txt"]),
                from,
                temp,
                to,
                CASE_SENSITIVE_RAW,
            ),
            PendingRenameRecovery::ReverseToOriginal
        );
        assert_eq!(
            decide_pending_rename_recovery(
                &list(&["legion-renaming-42-8-Notes.txt", "notes.txt"]),
                from,
                temp,
                to,
                CASE_INSENSITIVE_RAW,
            ),
            PendingRenameRecovery::KeepBoth
        );
        assert_eq!(
            decide_pending_rename_recovery(
                &list(&["legion-renaming-42-8-Notes.txt", "notes.txt"]),
                from,
                temp,
                to,
                CASE_INSENSITIVE_NORM,
            ),
            PendingRenameRecovery::KeepBoth
        );

        // 8. Temporary and a third case-folded variant (`NOTES.TXT`) present:
        assert_eq!(
            decide_pending_rename_recovery(
                &list(&["legion-renaming-42-8-Notes.txt", "NOTES.TXT"]),
                from,
                temp,
                to,
                CASE_SENSITIVE_RAW,
            ),
            PendingRenameRecovery::CompleteToFinal
        );
        assert_eq!(
            decide_pending_rename_recovery(
                &list(&["legion-renaming-42-8-Notes.txt", "NOTES.TXT"]),
                from,
                temp,
                to,
                CASE_INSENSITIVE_RAW,
            ),
            PendingRenameRecovery::KeepBoth
        );

        // 9. Unicode NFC vs NFD normalization folding (`cafe\u{0301}.txt` -> `caf\u{00e9}.txt`):
        let nfd = OsStr::new("cafe\u{0301}.txt");
        let nfc = OsStr::new("caf\u{00e9}.txt");
        let temp_u = OsStr::new("legion-renaming-42-9-cafe\u{0301}.txt");
        assert_eq!(
            decide_pending_rename_recovery(
                &list(&["legion-renaming-42-9-cafe\u{0301}.txt", "caf\u{00e9}.txt"]),
                nfd,
                temp_u,
                nfc,
                CASE_SENSITIVE_NORM,
            ),
            PendingRenameRecovery::KeepBoth
        );
        assert_eq!(
            decide_pending_rename_recovery(
                &list(&["legion-renaming-42-9-cafe\u{0301}.txt", "caf\u{00e9}.txt"]),
                nfd,
                temp_u,
                nfc,
                CASE_SENSITIVE_RAW,
            ),
            PendingRenameRecovery::ReverseToOriginal
        );
    }

    #[test]
    fn visible_rename_temp_is_not_hidden_and_recovery_completes_or_reverses() {
        let dir = tempfile::tempdir().unwrap();
        let state_dir = dir.path().join("state");
        let work_dir = dir.path().join("work");
        fs::create_dir_all(&work_dir).unwrap();
        let renames_dir = pending_renames_dir(&state_dir);
        fs::create_dir_all(&renames_dir).unwrap();

        let has_exact_entry = |name: &str| -> bool {
            directory_contains_exact_name(&work_dir, OsStr::new(name)).unwrap()
        };
        let write_intent = |from_name: &str, temp_name: &str, to_name: &str| -> PathBuf {
            let key = rename_intent_key(&work_dir, temp_name);
            let intent_path = renames_dir.join(format!("{key}.json"));
            let intent = PendingRenameIntent {
                parent: work_dir.to_string_lossy().into_owned(),
                from_name: from_name.to_owned(),
                temp_name: temp_name.to_owned(),
                to_name: to_name.to_owned(),
            };
            write_atomic(
                &intent_path,
                &serde_json::to_vec(&intent).unwrap(),
                WriteOptions::with_mode(0o600),
            )
            .unwrap();
            intent_path
        };

        let temp_name = visible_rename_temp_name(OsStr::new("Report.txt"), 42, 7);
        assert!(
            !temp_name.starts_with('.'),
            "intermediate rename name must be visible (not dot-prefixed): {temp_name}"
        );
        assert!(
            !legion_xfer::is_legion_internal_name(&temp_name),
            "intermediate rename name must not be hidden from manifests/listings: {temp_name}"
        );

        // Case 1: Only temp present (crash between step 1 and step 2) -> completes `temp -> to`.
        fs::write(work_dir.join(&temp_name), b"important").unwrap();
        let intent_path = write_intent("Report.txt", &temp_name, "report.txt");
        recover_pending_renames(&state_dir, Some(RenameRecoveryScope::ExactDir(&work_dir)));
        assert!(!has_exact_entry(&temp_name));
        assert!(has_exact_entry("report.txt"));
        assert!(!has_exact_entry("Report.txt"));
        assert_eq!(fs::read(work_dir.join("report.txt")).unwrap(), b"important");
        assert!(!intent_path.exists());

        // Probe the host filesystem's actual folding behavior using the real directory.
        let host_fs = probe_dir_name_behavior(&work_dir);

        // Case 2: Only original spelling present (crash before step 1) -> drops stale intent.
        fs::write(work_dir.join("OnlyOrig.txt"), b"orig-only").unwrap();
        let temp_orig = visible_rename_temp_name(OsStr::new("OnlyOrig.txt"), 42, 10);
        let intent_orig = write_intent("OnlyOrig.txt", &temp_orig, "onlyorig.txt");
        recover_pending_renames(&state_dir, Some(RenameRecoveryScope::ExactDir(&work_dir)));
        assert!(has_exact_entry("OnlyOrig.txt"));
        assert!(!has_exact_entry(&temp_orig));
        assert!(!intent_orig.exists());

        // Case 3: Only final spelling present (crash after step 2) -> drops stale intent.
        fs::write(work_dir.join("onlyfinal.txt"), b"final-only").unwrap();
        let temp_final = visible_rename_temp_name(OsStr::new("OnlyFinal.txt"), 42, 11);
        let intent_final = write_intent("OnlyFinal.txt", &temp_final, "onlyfinal.txt");
        recover_pending_renames(&state_dir, Some(RenameRecoveryScope::ExactDir(&work_dir)));
        assert!(has_exact_entry("onlyfinal.txt"));
        assert!(!has_exact_entry(&temp_final));
        assert!(!intent_final.exists());

        // Case 4: Both `temp` and `to` (`notes.txt`) present.
        let temp_name2 = visible_rename_temp_name(OsStr::new("Notes.txt"), 42, 8);
        fs::write(work_dir.join(&temp_name2), b"original").unwrap();
        fs::write(work_dir.join("notes.txt"), b"occupant").unwrap();
        let intent_path2 = write_intent("Notes.txt", &temp_name2, "notes.txt");

        recover_pending_renames(&state_dir, Some(RenameRecoveryScope::ExactDir(&work_dir)));
        if host_fs.case_insensitive {
            // On NTFS / case-insensitive APFS, `notes.txt` occupies `Notes.txt`'s folded slot, so
            // recovery MUST preserve both visible `temp_name2` and `notes.txt` without overwriting!
            assert!(has_exact_entry(&temp_name2));
            assert!(has_exact_entry("notes.txt"));
            assert_eq!(fs::read(work_dir.join(&temp_name2)).unwrap(), b"original");
            assert_eq!(fs::read(work_dir.join("notes.txt")).unwrap(), b"occupant");
            assert!(intent_path2.exists());

            // Once the conflicting occupant is moved away, recovery completes cleanly to `notes.txt`.
            fs::remove_file(work_dir.join("notes.txt")).unwrap();
            recover_pending_renames(&state_dir, Some(RenameRecoveryScope::ExactDir(&work_dir)));
            assert!(!has_exact_entry(&temp_name2));
            assert!(has_exact_entry("notes.txt"));
            assert_eq!(fs::read(work_dir.join("notes.txt")).unwrap(), b"original");
            assert!(!intent_path2.exists());
        } else {
            // On a case-sensitive volume, `Notes.txt` is free, so recovery reverses `temp -> from`.
            assert!(!has_exact_entry(&temp_name2));
            assert!(has_exact_entry("Notes.txt"));
            assert!(has_exact_entry("notes.txt"));
            assert_eq!(fs::read(work_dir.join("Notes.txt")).unwrap(), b"original");
            assert_eq!(fs::read(work_dir.join("notes.txt")).unwrap(), b"occupant");
            assert!(!intent_path2.exists());
        }

        // Case 5: Both `temp` and `from` (`Draft.txt`) present.
        let temp_name3 = visible_rename_temp_name(OsStr::new("Draft.txt"), 42, 9);
        fs::write(work_dir.join(&temp_name3), b"in-flight").unwrap();
        fs::write(work_dir.join("Draft.txt"), b"recreated").unwrap();
        let intent_path3 = write_intent("Draft.txt", &temp_name3, "draft.txt");

        recover_pending_renames(&state_dir, Some(RenameRecoveryScope::ExactDir(&work_dir)));
        if host_fs.case_insensitive {
            assert!(has_exact_entry(&temp_name3));
            assert!(has_exact_entry("Draft.txt"));
            assert_eq!(fs::read(work_dir.join(&temp_name3)).unwrap(), b"in-flight");
            assert_eq!(fs::read(work_dir.join("Draft.txt")).unwrap(), b"recreated");
            assert!(intent_path3.exists());
        } else {
            assert!(!has_exact_entry(&temp_name3));
            assert!(has_exact_entry("Draft.txt"));
            assert!(has_exact_entry("draft.txt"));
            assert_eq!(fs::read(work_dir.join("Draft.txt")).unwrap(), b"recreated");
            assert_eq!(fs::read(work_dir.join("draft.txt")).unwrap(), b"in-flight");
            assert!(!intent_path3.exists());
        }
    }

    #[derive(Clone)]
    struct SyncRng(u64);

    impl SyncRng {
        fn new(seed: u64) -> Self {
            Self(seed ^ 0xa076_1d64_78bd_642f)
        }

        fn next(&mut self) -> u64 {
            let mut x = self.0;
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            self.0 = x;
            x
        }

        fn range(&mut self, upper: usize) -> usize {
            if upper <= 1 {
                0
            } else {
                (self.next() as usize) % upper
            }
        }
    }

    /// Deterministic adversarial `std::io::Read` that delivers 1-byte or random short reads,
    /// injects `ErrorKind::Interrupted`, and optionally triggers time-based re-auth ticks between
    /// operations by backdating `StreamAuthCheck::last_check`.
    struct AdversarialSyncRead {
        data: Vec<u8>,
        pos: usize,
        rng: SyncRng,
        single_byte: bool,
        inject_interrupted: bool,
        tick_check: Option<StreamAuthCheck>,
    }

    impl Read for AdversarialSyncRead {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            if let Some(ref check) = self.tick_check
                && self.rng.range(13) == 0
                && let Ok(mut last) = check.last_check.lock()
            {
                *last = Instant::now() - RECHECK_INTERVAL_TIME - Duration::from_millis(1);
            }
            if self.inject_interrupted && self.rng.range(9) == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::Interrupted,
                    "adversarial sync read eintr",
                ));
            }
            let remaining = self.data.len() - self.pos;
            if remaining == 0 || buf.is_empty() {
                return Ok(0);
            }
            let max_take = remaining.min(buf.len());
            let take = if self.single_byte {
                1
            } else {
                1 + self.rng.range(max_take.min(2048))
            };
            let take = take.min(max_take);
            buf[..take].copy_from_slice(&self.data[self.pos..self.pos + take]);
            self.pos += take;
            Ok(take)
        }
    }

    /// Deterministic adversarial `std::io::Write` that performs 1-byte or random short writes,
    /// injects `ErrorKind::Interrupted`, and optionally triggers time-based re-auth ticks between
    /// writes.
    struct AdversarialSyncWrite {
        out: Vec<u8>,
        rng: SyncRng,
        single_byte: bool,
        inject_interrupted: bool,
        tick_check: Option<StreamAuthCheck>,
    }

    impl Write for AdversarialSyncWrite {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            if let Some(ref check) = self.tick_check
                && self.rng.range(13) == 0
                && let Ok(mut last) = check.last_check.lock()
            {
                *last = Instant::now() - RECHECK_INTERVAL_TIME - Duration::from_millis(1);
            }
            if self.inject_interrupted && self.rng.range(9) == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::Interrupted,
                    "adversarial sync write eintr",
                ));
            }
            if buf.is_empty() {
                return Ok(0);
            }
            let take = if self.single_byte {
                1
            } else {
                1 + self.rng.range(buf.len().min(2048))
            };
            let take = take.min(buf.len());
            self.out.extend_from_slice(&buf[..take]);
            Ok(take)
        }

        fn flush(&mut self) -> io::Result<()> {
            if self.inject_interrupted && self.rng.range(11) == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::Interrupted,
                    "adversarial sync flush eintr",
                ));
            }
            Ok(())
        }
    }

    fn make_sync_payload(seed: u64, len: usize) -> Vec<u8> {
        let mut rng = SyncRng::new(seed);
        let mut out = vec![0u8; len];
        for chunk in out.chunks_mut(8) {
            let bytes = rng.next().to_le_bytes();
            let n = chunk.len();
            chunk.copy_from_slice(&bytes[..n]);
        }
        out
    }

    #[test]
    fn adversarial_io_preserves_bytes_across_rechecking_fed_and_all_stream_ops() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        fs::create_dir_all(&home).unwrap();
        let paths = legion_core::Paths::under(dir.path());
        let config = ConfigStore::new(paths.config_dir());
        let host = Host::new(paths.clone(), "box".to_owned());
        let policy = config.get::<Policy>().unwrap();
        let read_spec = ToolSpec {
            name: "fs.read".to_owned(),
            description: String::new(),
            effect: Effect::Read,
            input_schema: json!({"type": "object"}),
        };
        let write_spec = ToolSpec {
            name: "fs.write".to_owned(),
            description: String::new(),
            effect: Effect::Write,
            input_schema: json!({"type": "object"}),
        };
        let read_grant = policy.authorize("owner", &read_spec, &paths).unwrap();
        let write_grant = policy.authorize("owner", &write_spec, &paths).unwrap();

        // 1. Direct adversarial tests of `RecheckingReader`, `RecheckingWriter`, and `Fed`
        // across 32 deterministic seeds with short reads/writes, `Interrupted`, and forced re-auth
        // timer ticks between operations.
        for seed in 0u64..32 {
            let single_byte = seed % 8 == 0;
            let len = if single_byte {
                2_000 + (seed as usize) * 29
            } else {
                25_000 + (seed as usize) * 997
            };
            let payload = make_sync_payload(seed + 100, len);
            let sample_path = home.join("sample.bin");

            let rcheck = StreamAuthCheck::new(
                &host,
                &read_grant,
                None,
                "fs.read",
                Effect::Read,
                sample_path.clone(),
                Access::Read,
                None,
            );
            let mut r = RecheckingReader::new(
                AdversarialSyncRead {
                    data: payload.clone(),
                    pos: 0,
                    rng: SyncRng::new(seed * 3 + 1),
                    single_byte,
                    inject_interrupted: true,
                    tick_check: Some(rcheck.clone()),
                },
                rcheck,
            );
            let mut read_out = Vec::new();
            let mut tmp = [0u8; 4096];
            loop {
                match r.read(&mut tmp) {
                    Ok(0) => break,
                    Ok(n) => read_out.extend_from_slice(&tmp[..n]),
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                    Err(e) => panic!("RecheckingReader failed on seed {seed}: {e}"),
                }
            }
            assert_eq!(
                read_out, payload,
                "RecheckingReader mismatch on seed {seed}"
            );

            let wcheck = StreamAuthCheck::new(
                &host,
                &write_grant,
                None,
                "fs.write",
                Effect::Write,
                sample_path.clone(),
                Access::Write,
                Some(CreateKind::File),
            );
            let mut w = RecheckingWriter::new(
                AdversarialSyncWrite {
                    out: Vec::new(),
                    rng: SyncRng::new(seed * 5 + 2),
                    single_byte,
                    inject_interrupted: true,
                    tick_check: Some(wcheck.clone()),
                },
                wcheck,
            );
            w.write_all(&payload).unwrap();
            w.flush().unwrap();
            assert_eq!(
                w.inner.out, payload,
                "RecheckingWriter mismatch on seed {seed}"
            );

            // `feed` + `Fed` with both bounded (`Some(limit)`) and unbounded (`None`) input.
            let fcheck = StreamAuthCheck::new(
                &host,
                &write_grant,
                None,
                "fs.write",
                Effect::Write,
                sample_path,
                Access::Write,
                Some(CreateKind::File),
            );
            let mut input_opt = Some(BufReader::with_capacity(
                if single_byte { 16 } else { 1024 },
                AdversarialSyncRead {
                    data: payload.clone(),
                    pos: 0,
                    rng: SyncRng::new(seed * 7 + 3),
                    single_byte,
                    inject_interrupted: true,
                    tick_check: Some(fcheck.clone()),
                },
            ));
            let mut out_sink = Vec::new();
            let limit = if seed % 2 == 0 {
                Some(payload.len() as u64)
            } else {
                None
            };
            let fed_bytes = feed(
                &mut input_opt,
                limit,
                &mut out_sink,
                Some(fcheck),
                |mut fed| {
                    let mut got = Vec::new();
                    let mut buf = [0u8; 1024];
                    loop {
                        match fed.read(&mut buf) {
                            Ok(0) => break,
                            Ok(n) => got.extend_from_slice(&buf[..n]),
                            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                            Err(e) => panic!("Fed::read failed on seed {seed}: {e}"),
                        }
                    }
                    got
                },
            )
            .unwrap();
            assert_eq!(fed_bytes, payload, "Fed mismatch on seed {seed}");
        }

        // 2. End-to-end `serve` across `get`, `put`, `pack`, and `unpack` driven through
        // adversarial short reads/writes and `Interrupted` errors across 16 deterministic seeds.
        fn split_header_lines(raw: &[u8], count: usize) -> (Vec<Value>, &[u8]) {
            let mut rest = raw;
            let mut lines = Vec::with_capacity(count);
            for _ in 0..count {
                let nl = rest.iter().position(|&b| b == b'\n').unwrap();
                let val: Value = serde_json::from_slice(&rest[..nl]).unwrap();
                lines.push(val);
                rest = &rest[nl + 1..];
            }
            (lines, rest)
        }

        for seed in 0u64..16 {
            let single_byte = seed % 4 == 0;
            let file_len = if single_byte {
                1_500 + (seed as usize) * 53
            } else {
                40_000 + (seed as usize) * 1_319
            };
            let payload = make_sync_payload(seed + 500, file_len);
            let src_file = home.join(format!("src-{seed}.bin"));
            let dst_file = home.join(format!("dst-{seed}.bin"));
            fs::write(&src_file, &payload).unwrap();
            let expected_hash = legion_xfer::hash::hash_file(&src_file).unwrap();

            // `get` through adversarial writer -> verify raw bytes match file byte-for-byte.
            let get_hdr = format!(
                "{}\n",
                json!({
                    "protocol": STREAM_PROTOCOL,
                    "op": "get",
                    "path": src_file.to_string_lossy(),
                    "offset": 0
                })
            );
            let get_in = AdversarialSyncRead {
                data: get_hdr.into_bytes(),
                pos: 0,
                rng: SyncRng::new(seed * 11 + 1),
                single_byte,
                inject_interrupted: true,
                tick_check: None,
            };
            let mut get_out = AdversarialSyncWrite {
                out: Vec::new(),
                rng: SyncRng::new(seed * 11 + 2),
                single_byte,
                inject_interrupted: true,
                tick_check: None,
            };
            serve(&host, Some("owner"), get_in, &mut get_out).unwrap();
            let (get_lines, raw_file) = split_header_lines(&get_out.out, 2);
            assert_eq!(get_lines[1]["size"], json!(file_len));
            assert_eq!(raw_file, payload.as_slice(), "get mismatch on seed {seed}");

            // `put` receiving those exact bytes through adversarial reader & writer.
            let mut put_bytes = format!(
                "{}\n",
                json!({
                    "protocol": STREAM_PROTOCOL,
                    "op": "put",
                    "path": dst_file.to_string_lossy(),
                    "size": file_len,
                    "hash": expected_hash.to_string(),
                })
            )
            .into_bytes();
            put_bytes.extend_from_slice(raw_file);
            let put_in = AdversarialSyncRead {
                data: put_bytes,
                pos: 0,
                rng: SyncRng::new(seed * 13 + 3),
                single_byte,
                inject_interrupted: true,
                tick_check: None,
            };
            let mut put_out = AdversarialSyncWrite {
                out: Vec::new(),
                rng: SyncRng::new(seed * 13 + 4),
                single_byte,
                inject_interrupted: true,
                tick_check: None,
            };
            serve(&host, Some("owner"), put_in, &mut put_out).unwrap();
            let (put_lines, _) = split_header_lines(&put_out.out, 3);
            assert_eq!(put_lines[2]["ok"], Value::Bool(true), "put on seed {seed}");
            assert_eq!(
                fs::read(&dst_file).unwrap(),
                payload,
                "put bytes on seed {seed}"
            );

            // `pack` -> `unpack` on a multi-file directory tree through adversarial reader & writer.
            let src_tree = home.join(format!("tree-src-{seed}"));
            let dst_tree = home.join(format!("tree-dst-{seed}"));
            fs::create_dir_all(src_tree.join("sub")).unwrap();
            let a_bytes = make_sync_payload(seed + 1000, if single_byte { 777 } else { 9_001 });
            let b_bytes = make_sync_payload(seed + 2000, if single_byte { 513 } else { 14_333 });
            fs::write(src_tree.join("a.bin"), &a_bytes).unwrap();
            fs::write(src_tree.join("sub/b.bin"), &b_bytes).unwrap();
            let a_hash = legion_xfer::hash::hash_file(&src_tree.join("a.bin")).unwrap();
            let b_hash = legion_xfer::hash::hash_file(&src_tree.join("sub/b.bin")).unwrap();

            let pack_hdr = format!(
                "{}\n",
                json!({
                    "protocol": STREAM_PROTOCOL,
                    "op": "pack",
                    "root": src_tree.to_string_lossy(),
                    "names": ["sub", "a.bin", "sub/b.bin"],
                })
            );
            let pack_in = AdversarialSyncRead {
                data: pack_hdr.into_bytes(),
                pos: 0,
                rng: SyncRng::new(seed * 17 + 5),
                single_byte,
                inject_interrupted: true,
                tick_check: None,
            };
            let mut pack_out = AdversarialSyncWrite {
                out: Vec::new(),
                rng: SyncRng::new(seed * 17 + 6),
                single_byte,
                inject_interrupted: true,
                tick_check: None,
            };
            serve(&host, Some("owner"), pack_in, &mut pack_out).unwrap();
            let (pack_lines, tar_bytes) = split_header_lines(&pack_out.out, 2);
            assert_eq!(pack_lines[1]["ready"], Value::Bool(true));

            let mut unpack_bytes = format!(
                "{}\n",
                json!({
                    "protocol": STREAM_PROTOCOL,
                    "op": "unpack",
                    "root": dst_tree.to_string_lossy(),
                    "entries": [
                        {"path": "sub", "kind": "dir"},
                        {"path": "a.bin", "kind": "file", "size": a_bytes.len(), "hash": a_hash.to_string()},
                        {"path": "sub/b.bin", "kind": "file", "size": b_bytes.len(), "hash": b_hash.to_string()},
                    ],
                })
            )
            .into_bytes();
            unpack_bytes.extend_from_slice(tar_bytes);
            let unpack_in = AdversarialSyncRead {
                data: unpack_bytes,
                pos: 0,
                rng: SyncRng::new(seed * 19 + 7),
                single_byte,
                inject_interrupted: true,
                tick_check: None,
            };
            let mut unpack_out = AdversarialSyncWrite {
                out: Vec::new(),
                rng: SyncRng::new(seed * 19 + 8),
                single_byte,
                inject_interrupted: true,
                tick_check: None,
            };
            serve(&host, Some("owner"), unpack_in, &mut unpack_out).unwrap();
            let (unpack_lines, _) = split_header_lines(&unpack_out.out, 3);
            assert_eq!(
                unpack_lines[2]["ok"],
                Value::Bool(true),
                "unpack on seed {seed}"
            );
            assert_eq!(
                fs::read(dst_tree.join("a.bin")).unwrap(),
                a_bytes,
                "unpack a.bin mismatch on seed {seed}"
            );
            assert_eq!(
                fs::read(dst_tree.join("sub/b.bin")).unwrap(),
                b_bytes,
                "unpack sub/b.bin mismatch on seed {seed}"
            );
        }
    }

    #[test]
    fn stream_auth_recheck_tolerates_transient_inplace_save_and_rejects_real_revocation() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        fs::create_dir_all(&home).unwrap();
        let paths = legion_core::Paths::under(dir.path());
        fs::create_dir_all(paths.config_dir()).unwrap();
        let policy_path = paths.config_file("policy.toml");
        let valid_policy = "default = \"mover\"\n[principals.mover]\nrole = \"m\"\n[roles.m]\nallow = [\"effect:read\", \"effect:write\"]\n";
        fs::write(&policy_path, valid_policy).unwrap();

        let host = Host::new(paths.clone(), "box".to_owned());
        let config = ConfigStore::new(paths.config_dir());
        let policy = config.get::<Policy>().unwrap();
        let write_spec = ToolSpec {
            name: "fs.write".to_owned(),
            description: String::new(),
            effect: Effect::Write,
            input_schema: json!({"type": "object"}),
        };
        let grant = policy.authorize("mover", &write_spec, &paths).unwrap();
        let target = home.join("file.bin");
        let check = StreamAuthCheck::new(
            &host,
            &grant,
            None,
            "fs.write",
            Effect::Write,
            target,
            Access::Write,
            Some(CreateKind::File),
        );

        // 1. Transient half-written `policy.toml` restored after 150 ms must pass after retry.
        fs::write(&policy_path, &valid_policy[..12]).unwrap();
        let restore_path = policy_path.clone();
        let restorer = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(150));
            fs::write(&restore_path, valid_policy).unwrap();
        });
        assert!(
            check.verify().is_ok(),
            "transient in-place save must not trigger revocation"
        );
        restorer.join().unwrap();
        assert!(!check.is_denied());

        // 2. Genuinely broken `policy.toml` still denies within 2 seconds.
        fs::write(&policy_path, "this is = [broken toml\n").unwrap();
        let started = Instant::now();
        assert!(check.verify().is_err());
        assert!(check.is_denied());
        assert!(started.elapsed() < Duration::from_secs(2));
        let _ = check.take_denied();

        // 3. Genuine right narrowing (`effect:read` only) still denies within 2 seconds.
        fs::write(
            &policy_path,
            valid_policy.replace("effect:write", "effect:read"),
        )
        .unwrap();
        let started = Instant::now();
        assert!(check.verify().is_err());
        assert!(check.is_denied());
        assert!(started.elapsed() < Duration::from_secs(2));
    }
}
