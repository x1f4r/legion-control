//! `fs.transfer`: a file or a directory tree from one machine to another, or within one.
//!
//! Each end is `machine:/path` or a path on this node. The bytes are relayed through this node,
//! which can reach both ends by definition: a remote end over a `legion stream` pipe
//! ([`Fleet::open_pipe`]), a local end through the same stream server run in-process, so both are
//! authorised, checked and audited by the node that owns the files, and a local end is held to
//! exactly what a remote one is.
//!
//! A file is hashed at the source, the destination says where an earlier attempt stopped, only the
//! rest is sent, and the destination renames it into place once the whole file's hash matches. A
//! tree is listed at both ends, [`legion_xfer::plan::plan`] decides what differs, and only that is
//! packed at the source and unpacked at the destination, so a second run sends nothing.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use legion_core::{BoxFuture, Ctx, Registry, Tool, ToolError, input_schema, parse_args};
use legion_proto::{Effect, ErrorBody, ErrorCode, EventKind, ToolSpec};
use legion_xfer::{
    CaseCollision, Entry, EntryKind, Hash, IfExists, Manifest, Plan, PlanConflict, PlanOptions,
    RenamedEntry, SkippedEntry,
};
use schemars::JsonSchema;
use serde::Deserialize;
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use tokio::io::{
    AsyncBufRead, AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader,
};
use tokio::task::{JoinHandle, JoinSet};
use tokio_util::io::SyncIoBridge;

use crate::config::{FleetFile, validate_machine_name};
use crate::connector::PipeHandle;
use crate::pool::Fleet;
use crate::stream::{
    self, DeleteDone, GetReady, Host, ManifestDone, PackReady, Planned, PutDone, PutReady,
    StatReply, UnpackDone, UnpackReady,
};

/// How many bytes the relay and the in-process pipes hold at once: memory stays bounded however
/// big the transfer, and a slow end slows the other.
const CHUNK: usize = 64 * 1024;

/// The longest reply line read from a stream. Replies are control, never bulk data.
const LONGEST_REPLY: usize = 16 * 1024 * 1024;

/// Lines that are not JSON skipped before a stream's first reply: a login banner, a shell's noise.
const MOST_NOISE_LINES: usize = 64;
/// Bytes of such noise skipped before giving up.
const MOST_NOISE_BYTES: usize = 64 * 1024;

/// The most names one `pack` or `delete` carries, and their total length, so a header stays far
/// below what a stream reads and a huge tree goes in several rounds.
const BATCH_NAMES: usize = 20_000;
const BATCH_BYTES: usize = 4 * 1024 * 1024;
/// Per-round caps for tree moves so verified source files are removed incrementally batch by batch.
const MOVE_BATCH_FILES: usize = 256;
const MOVE_BATCH_CONTENT_BYTES: u64 = 32 * 1024 * 1024;

/// The shortest time between two `progress` events.
const PROGRESS_EVERY: Duration = Duration::from_millis(250);

/// How long an operation's first reply may take from the moment its pipe is opened: long enough
/// for ssh to connect and a node to start and begin its answer, short enough that a node which will
/// never answer (an older legion waiting for input this one does not send, a stalled connection)
/// is given up on. A node at work on something long says it is busy before this runs out.
const FIRST_REPLY_TIMEOUT: Duration = Duration::from_secs(60);

/// How long a stream may go without moving a byte in either direction while bytes are relayed,
/// or without a line while a reply is awaited. A node that is busy says so every
/// [`stream::HEARTBEAT`], well inside this, so only a stall runs it out.
const IDLE_TIMEOUT: Duration = Duration::from_secs(120);

/// How long an end whose input was cut off is given to clean up (an unpack removes the temporary
/// file it was writing) and exit before it is killed.
const ABORT_GRACE: Duration = Duration::from_secs(2);

/// The longest `busy` lines may put off one reply. A node hashing or listing something very large
/// says it is busy every few seconds for as long as that takes; past this, a node that only ever
/// says so is taken to be stuck.
const MOST_BUSY: Duration = Duration::from_secs(6 * 60 * 60);

/// How long an ended stream is given to exit and say why on stderr.
const EXIT_WAIT: Duration = Duration::from_secs(2);

/// How long a source that has sent the end of its archive is given to report that it exited
/// cleanly; over a slow link the exit status can trail the last byte.
const FINISH_WAIT: Duration = Duration::from_secs(10);

/// The most entries a listing from another node may hold. Both listings of a tree are compared
/// in this node's memory; a tree bigger than this is transferred a subdirectory at a time.
const MOST_LISTED_ENTRIES: usize = 1_000_000;

/// The longest line of a listing from another node: an entry, whose path and link target a file
/// system keeps far shorter, or the line that ends it, whose list of what could not be read the
/// listing node keeps well within this.
const MOST_LISTING_LINE: usize = 1024 * 1024;

/// The most bytes a listing from another node may take in all: a million entries with ordinary
/// names fit several times over, a listing built to exhaust memory does not.
const MOST_LISTING_BYTES: usize = 512 * 1024 * 1024;

/// How many paths a result lists in each of its lists; the counts are always whole.
const MOST_LISTED: usize = 100;

/// Adds `fs.transfer`, which holds the fleet's session pool to reach the other ends.
pub fn register(registry: &mut Registry, fleet: Fleet) {
    registry.add(TransferTool {
        spec: ToolSpec {
            name: "fs.transfer".to_owned(),
            description: "Copy or move a file or a directory tree between machines, or within one. Each of from and to is machine:/path or a path on this node; to names the destination itself, except that a file sent to an existing directory (or to a path ending in /) lands inside it under its own name. A file resumes where an interrupted transfer stopped and is renamed into place only once its BLAKE3 hash matches. A tree sends only what differs (size and modification time, or hash with checksum), so a second run sends nothing; if_exists chooses fail, skip, replace (default) or rename (name (2).ext); move removes each source file only after its destination copy is verified by hash (refused up front unless the caller may fs.remove the source); delete removes what the source no longer has, exclude leaves paths out at both ends, and dry_run reports what would be sent, deleted and removed without changing anything.".to_owned(),
            effect: Effect::Write,
            input_schema: input_schema::<TransferArgs>(),
        },
        fleet,
    });
}

struct TransferTool {
    spec: ToolSpec,
    fleet: Fleet,
}

impl Tool for TransferTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn call<'a>(&'a self, ctx: &'a Ctx, args: Value) -> BoxFuture<'a, Result<Value, ToolError>> {
        Box::pin(async move {
            let args = parse_args::<TransferArgs>(&self.spec.name, args)?;
            let report = transfer(&self.fleet, ctx, args).await?;
            serde_json::to_value(&report).map_err(|error| {
                ErrorBody::new(
                    ErrorCode::Internal,
                    format!(
                        "fs.transfer returned a result that cannot be written as JSON: {error}."
                    ),
                )
            })
        })
    }
}

/// How `fs.transfer` behaves when a destination entry already exists.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum IfExistsArg {
    /// Overwrite the destination entry atomically (default).
    #[default]
    Replace,
    /// Refuse with `exists` before overwriting anything.
    Fail,
    /// Keep the existing destination entry untouched and skip sending.
    Skip,
    /// Write beside the existing entry as `name (2).ext`, counting up atomically at the destination.
    Rename,
}

impl From<IfExistsArg> for IfExists {
    fn from(value: IfExistsArg) -> Self {
        match value {
            IfExistsArg::Replace => IfExists::Replace,
            IfExistsArg::Fail => IfExists::Fail,
            IfExistsArg::Skip => IfExists::Skip,
            IfExistsArg::Rename => IfExists::Rename,
        }
    }
}

/// Arguments of `fs.transfer`.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct TransferArgs {
    /// What to send: `machine:/path`, or a path on this node (absolute or starting with `~`).
    pub(crate) from: String,
    /// Where it goes, in the same form. It names the destination itself; a file sent to an
    /// existing directory lands inside it.
    pub(crate) to: String,
    /// Remove from the destination tree what the source no longer has.
    #[serde(default)]
    pub(crate) delete: bool,
    /// Compare files by BLAKE3 hash instead of size and modification time, which reads them all.
    #[serde(default)]
    pub(crate) checksum: bool,
    /// Globs of paths to leave out at both ends, relative to the tree; a trailing `/` matches
    /// directories only. What is left out at the destination is never deleted.
    #[serde(default)]
    pub(crate) exclude: Vec<String>,
    /// List and plan, and report what would be sent, deleted and removed, without changing anything.
    #[serde(default)]
    pub(crate) dry_run: bool,
    /// What to do when a destination entry already exists: `replace` (default), `fail`, `skip`, or
    /// `rename` (`name (2).ext`, decided atomically at the destination).
    #[serde(default)]
    pub(crate) if_exists: IfExistsArg,
    /// Remove each source file after its destination copy is verified by BLAKE3 hash (requires
    /// `fs.remove` permission on the source; refused up front otherwise).
    #[serde(default, rename = "move")]
    pub(crate) move_source: bool,
}

fn is_zero(n: &u64) -> bool {
    *n == 0
}

/// What `fs.transfer` returns.
#[derive(Debug, Default, Serialize)]
pub(crate) struct TransferReport {
    from: String,
    to: String,
    kind: &'static str,
    files_sent: u64,
    bytes_sent: u64,
    files_unchanged: u64,
    #[serde(skip_serializing_if = "is_zero")]
    files_skipped: u64,
    deleted: u64,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    moved: bool,
    #[serde(skip_serializing_if = "is_zero")]
    source_removed: u64,
    conflicts: Vec<PlanConflict>,
    skipped: Vec<SkippedEntry>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    renamed: Vec<RenamedEntry>,
    #[serde(skip_serializing_if = "Option::is_none")]
    resumed_from: Option<u64>,
    duration_ms: u64,
    dry_run: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    would_send: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    would_delete: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    would_remove: Option<Vec<String>>,
    /// Whether a list above was cut at its first entries; the counts are whole.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    truncated: bool,
}

impl TransferReport {
    fn list<T>(&mut self, mut items: Vec<T>) -> Vec<T> {
        if items.len() > MOST_LISTED {
            items.truncate(MOST_LISTED);
            self.truncated = true;
        }
        items
    }
}

/// One end of a transfer, as the caller wrote it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Endpoint {
    /// A path on the node running the transfer.
    Local(String),
    /// A path on a machine named in `fleet.toml`.
    Remote {
        /// The machine's name.
        machine: String,
        /// The path there, expanded by that machine.
        path: String,
    },
}

/// Reads `machine:/path` or a local path.
///
/// A Windows drive path (`C:\…`, `C:/…`) is a local path. A colon after a slash or backslash is
/// part of a path. A one-letter name before the colon is refused, since it would be read as a drive
/// letter on one machine and a machine's name on another.
pub fn parse_endpoint(text: &str) -> Result<Endpoint, ErrorBody> {
    if text.is_empty() {
        return Err(
            ErrorBody::new(ErrorCode::InvalidArgs, "An end of the transfer is empty.")
                .with_hint("Write machine:/path, or a path on this node."),
        );
    }
    let bytes = text.as_bytes();
    let drive = bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && (bytes[2] == b'\\' || bytes[2] == b'/');
    if drive {
        return Ok(Endpoint::Local(text.to_owned()));
    }
    let Some((machine, path)) = text.split_once(':') else {
        return Ok(Endpoint::Local(text.to_owned()));
    };
    if machine.contains(['/', '\\']) {
        return Ok(Endpoint::Local(text.to_owned()));
    }
    if machine.is_empty() {
        return Err(ErrorBody::new(
            ErrorCode::InvalidArgs,
            format!("{text} names no machine before its colon."),
        )
        .with_hint("Write machine:/path, or a path on this node."));
    }
    if machine.chars().count() == 1 {
        return Err(ErrorBody::new(
            ErrorCode::InvalidArgs,
            format!("{text} is neither a drive path nor machine:/path."),
        )
        .with_hint(
            "A one-letter name before a colon reads as a Windows drive: write C:\\path or C:/path for a drive, and give the machine a longer name in fleet.toml.",
        ));
    }
    validate_machine_name(machine).map_err(|reason| {
        ErrorBody::new(
            ErrorCode::InvalidArgs,
            format!("{text} does not start with a machine's name: {reason}."),
        )
        .with_hint("Write machine:/path, or a path on this node.")
    })?;
    if path.is_empty() {
        return Err(ErrorBody::new(
            ErrorCode::InvalidArgs,
            format!("{text} names a machine but no path on it."),
        )
        .with_hint(format!("Write {machine}:/path, or {machine}:~/path.")));
    }
    Ok(Endpoint::Remote {
        machine: machine.to_owned(),
        path: path.to_owned(),
    })
}

/// An end, resolved against this node and `fleet.toml`.
#[derive(Clone, Debug)]
struct End {
    /// The machine it is on: as the caller named it, or this node's name.
    machine: String,
    path: String,
    remote: bool,
}

impl End {
    fn named(&self) -> String {
        format!("{}:{}", self.machine, self.path)
    }

    fn with_path(&self, path: String) -> End {
        End {
            machine: self.machine.clone(),
            path,
            remote: self.remote,
        }
    }
}

fn resolve(ctx: &Ctx, text: &str) -> Result<End, ErrorBody> {
    let here = ctx.node_name();
    match parse_endpoint(text)? {
        Endpoint::Local(path) => Ok(End {
            machine: here.to_owned(),
            path,
            remote: false,
        }),
        Endpoint::Remote { machine, path } if machine == here => Ok(End {
            machine,
            path,
            remote: false,
        }),
        Endpoint::Remote { machine, path } => {
            let fleet = ctx.config().get::<FleetFile>()?;
            if fleet.machine(&machine).is_none() {
                let known = fleet.names();
                let hint = if known.is_empty() {
                    format!("No machines are listed in fleet.toml (this node is {here}).")
                } else {
                    format!(
                        "Known machines in fleet.toml: {} (this node is {here}).",
                        known.join(", ")
                    )
                };
                return Err(ErrorBody::new(
                    ErrorCode::NotFound,
                    format!("There is no machine called {machine} in fleet.toml."),
                )
                .with_hint(hint)
                .with_machine(machine));
            }
            Ok(End {
                machine,
                path,
                remote: true,
            })
        }
    }
}

/// What has been sent so far, kept outside the transfer so a cancelled one can still say, and
/// the ends whose pipes were dropped before they were closed, so the transfer can still wait for
/// them.
#[derive(Default)]
struct Sent {
    files: AtomicU64,
    bytes: AtomicU64,
    deleted: AtomicU64,
    ends: Ends,
}

impl Sent {
    /// Gives every end left behind time to clean up and stop, together, and waits for them: a
    /// remote end has its input closed and is killed only if it is still there after
    /// [`ABORT_GRACE`]. Says whether all of them stopped; an end on this node runs on a thread,
    /// which cannot be killed.
    async fn settle(&self) -> bool {
        let mut stopping = JoinSet::new();
        for process in self.ends.take() {
            stopping.spawn(async move {
                match process {
                    Process::Remote(handle) => {
                        handle.close(ABORT_GRACE).await;
                        true
                    }
                    Process::Local(Some(task)) => {
                        tokio::time::timeout(EXIT_WAIT, task).await.is_ok()
                    }
                    Process::Local(None) => true,
                }
            });
        }
        let mut stopped = true;
        while let Some(joined) = stopping.join_next().await {
            stopped &= joined.unwrap_or(false);
        }
        stopped
    }
}

/// The processes of ends whose pipes were dropped before they were closed.
#[derive(Clone, Default)]
struct Ends(Arc<Mutex<Vec<Process>>>);

impl Ends {
    fn push(&self, process: Process) {
        self.0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(process);
    }

    fn take(&self) -> Vec<Process> {
        std::mem::take(
            &mut *self
                .0
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()),
        )
    }
}

/// The process behind one pipe. Dropped with its pipe before the pipe was closed, as when a
/// transfer fails or is cancelled half way, it is handed to the transfer's [`Ends`] rather than
/// killed on the spot, so it can still clean up.
struct Owned {
    process: Option<Process>,
    ends: Ends,
}

impl Drop for Owned {
    fn drop(&mut self) {
        if let Some(process) = self.process.take() {
            self.ends.push(process);
        }
    }
}

async fn transfer(
    fleet: &Fleet,
    ctx: &Ctx,
    args: TransferArgs,
) -> Result<TransferReport, ErrorBody> {
    let started = Instant::now();
    let from = resolve(ctx, &args.from)?;
    let to = resolve(ctx, &args.to)?;
    let sent = Sent::default();
    let job = Transfer {
        fleet,
        ctx,
        sent: &sent,
    };
    // A cancel drops the work, and with it every pipe it holds: each end sees its input end
    // without the rest of what it expected, so none renames anything into place that was not
    // complete, and each is then given time to clean up and stop before the transfer replies.
    let finished = {
        let work = job.run(&from, &to, &args);
        tokio::select! {
            report = work => Some(report),
            () = ctx.cancelled() => None,
        }
    };
    let stopped = sent.settle().await;
    let Some(report) = finished else {
        let files = sent.files.load(Ordering::Relaxed);
        let bytes = sent.bytes.load(Ordering::Relaxed);
        let deleted = sent.deleted.load(Ordering::Relaxed);
        let ends = if stopped {
            "Both ends have stopped."
        } else {
            "The end on this node is still finishing."
        };
        return Err(ErrorBody::new(
            ErrorCode::Cancelled,
            format!(
                "The transfer from {} to {} was cancelled after sending {bytes} bytes of {files} files and deleting {deleted} entries; those are what was sent, not what the destination confirmed it wrote. {ends}",
                from.named(),
                to.named()
            ),
        )
        .with_hint("Run it again to continue: a file resumes where it stopped and a tree sends only what is still missing.")
        .with_details(json!({"files_sent": files, "bytes_sent": bytes, "deleted": deleted, "stopped": stopped})));
    };
    let mut report = report?;
    report.dry_run = args.dry_run;
    report.duration_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    Ok(report)
}

struct Transfer<'a> {
    fleet: &'a Fleet,
    ctx: &'a Ctx,
    sent: &'a Sent,
}

impl Transfer<'_> {
    async fn run(
        &self,
        from: &End,
        to: &End,
        args: &TransferArgs,
    ) -> Result<TransferReport, ErrorBody> {
        let source: StatReply = self
            .request(
                from,
                json!({"op": "stat", "path": from.path, "checksum": true}),
            )
            .await?;
        match source.kind.as_str() {
            "file" => self.file(from, to, &source, args).await,
            "dir" => self.tree(from, to, &source, args).await,
            "missing" => Err(ErrorBody::new(
                ErrorCode::NotFound,
                format!("{} does not exist on {}.", from.path, from.machine),
            )
            .with_machine(from.machine.clone())),
            other => Err(ErrorBody::new(
                ErrorCode::InvalidArgs,
                format!(
                    "{} on {} is a {}, and only files and directories are transferred.",
                    from.path,
                    from.machine,
                    if other == "symlink" {
                        "symbolic link that leads nowhere"
                    } else {
                        "special file"
                    }
                ),
            )
            .with_machine(from.machine.clone())),
        }
    }

    /// Verifies that the caller is allowed `fs.remove` on `from` (including protected-tree checks)
    /// before any byte moves.
    async fn check_move_permission(&self, from: &End) -> Result<(), ErrorBody> {
        let _: DeleteDone = self
            .request(
                from,
                json!({
                    "op": "delete",
                    "root": from.path,
                    "names": [],
                    "dry_run": true,
                    "remove_root": true,
                }),
            )
            .await?;
        Ok(())
    }

    async fn file(
        &self,
        from: &End,
        to: &End,
        source: &StatReply,
        args: &TransferArgs,
    ) -> Result<TransferReport, ErrorBody> {
        let hash = source.hash.ok_or_else(|| {
            ErrorBody::new(
                ErrorCode::Failed,
                format!("{} on {} could not be hashed.", from.path, from.machine),
            )
            .with_machine(from.machine.clone())
        })?;
        let size = source.size;
        let if_exists = IfExists::from(args.if_exists);
        let mut target = to.clone();
        let mut there: StatReply = self
            .request(
                to,
                json!({"op": "stat", "path": to.path, "checksum": true, "if_size": size}),
            )
            .await?;
        if there.kind == "dir" || ends_in_separator(&to.path) {
            target = to.with_path(join(&to.path, file_name(&from.path)));
            there = self
                .request(
                    &target,
                    json!({"op": "stat", "path": target.path, "checksum": true, "if_size": size}),
                )
                .await?;
        }
        if args.move_source {
            check_not_same_or_subtree(from, &target, source, &there, false)?;
            self.check_move_permission(from).await?;
        }
        let mut report = TransferReport {
            from: from.named(),
            to: target.named(),
            kind: "file",
            moved: args.move_source,
            ..TransferReport::default()
        };
        match if_exists {
            IfExists::Fail if there.kind != "missing" => {
                return Err(ErrorBody::new(
                    ErrorCode::Exists,
                    format!("{} already exists on {}.", target.path, target.machine),
                )
                .with_hint(
                    "Pass if_exists=\"replace\", \"skip\", or \"rename\", or choose another destination.",
                )
                .with_machine(target.machine.clone()));
            }
            IfExists::Skip if there.kind != "missing" => {
                report.files_skipped = 1;
                return Ok(report);
            }
            IfExists::Replace => match there.kind.as_str() {
                "file" if there.size == size && there.hash == Some(hash) => {
                    report.files_unchanged = 1;
                    if args.move_source {
                        if args.dry_run {
                            report.source_removed = 1;
                            report.would_remove = Some(vec![from.path.clone()]);
                        } else {
                            let removed = self
                                .remove_source_root(from, Some(hash), &mut report.skipped)
                                .await?;
                            report.source_removed = removed;
                        }
                    }
                    return Ok(report);
                }
                "file" | "missing" | "symlink" => {}
                other => {
                    let what = if other == "dir" {
                        "a directory"
                    } else {
                        "a special file"
                    };
                    return Err(ErrorBody::new(
                        ErrorCode::Conflict,
                        format!(
                            "{} on {} is {what}, and a file cannot replace it.",
                            target.path, target.machine
                        ),
                    )
                    .with_hint("Name a different path.")
                    .with_machine(target.machine.clone()));
                }
            },
            IfExists::Fail | IfExists::Skip | IfExists::Rename => {}
        }
        let partial = there.partial.filter(|len| *len <= size);
        if args.dry_run {
            report.files_sent = 1;
            report.bytes_sent = size - partial.unwrap_or(0);
            report.resumed_from = partial;
            report.would_send = Some(vec![target.path.clone()]);
            if args.move_source {
                report.source_removed = 1;
                report.would_remove = Some(vec![from.path.clone()]);
            }
            return Ok(report);
        }
        let (resumed, sent, done) = match self
            .send_file(from, &target, source, partial, if_exists)
            .await
        {
            Err(error) if partial.is_some() && error.code == ErrorCode::Conflict => {
                // What an earlier attempt left was not a prefix of this file (it changed since):
                // the destination dropped it when the hash did not match, so start over once.
                self.send_file(from, &target, source, None, if_exists)
                    .await?
            }
            other => other?,
        };
        if done.skipped {
            report.files_skipped = 1;
            return Ok(report);
        }
        if let Some(ref actual_path) = done.path
            && if_exists == IfExists::Rename
        {
            report.to = format!("{}:{actual_path}", target.machine);
        }
        report.files_sent = 1;
        report.bytes_sent = sent;
        report.resumed_from = (resumed > 0).then_some(resumed);
        if args.move_source {
            let mut skipped = Vec::new();
            let removed = self
                .remove_source_root(from, Some(hash), &mut skipped)
                .await?;
            report.source_removed = removed;
            report.skipped = report.list(skipped);
        }
        Ok(report)
    }

    /// Sends one file, returning the offset it continued from, the bytes it sent, and the
    /// destination's `PutDone`.
    async fn send_file(
        &self,
        from: &End,
        to: &End,
        source_file: &StatReply,
        hint: Option<u64>,
        if_exists: IfExists,
    ) -> Result<(u64, u64, PutDone), ErrorBody> {
        let size = source_file.size;
        let Some(hash) = source_file.hash else {
            return Err(changed(from));
        };
        let mut offset = hint.unwrap_or(0);
        // The source first: a source that refuses leaves nothing behind at the destination.
        let (mut source, ready) = self.get(from, offset).await?;
        if ready.size != size {
            source.abort().await;
            return Err(changed(from));
        }
        let mut header =
            json!({"op": "put", "path": to.path, "size": size, "hash": hash, "offset": offset});
        if let Some(mode) = source_file.mode {
            header["mode"] = json!(mode);
        }
        if let Some(mtime) = source_file.mtime {
            header["mtime"] = json!(mtime);
        }
        if if_exists != IfExists::Replace {
            header["if_exists"] = json!(if_exists);
        }
        let mut dest = self.open(to, header).await?;
        let started: PutReady = match dest.first().await {
            Ok(ready) => ready,
            Err(error) => {
                source.abort().await;
                dest.abort().await;
                return Err(error);
            }
        };
        if started.offset != offset {
            // The destination knows better where to start; ask the source again from there.
            source.abort().await;
            offset = started.offset;
            let (again, ready) = match self.get(from, offset).await {
                Ok(opened) => opened,
                Err(error) => {
                    dest.abort().await;
                    return Err(error);
                }
            };
            if ready.size != size {
                again.abort().await;
                dest.abort().await;
                return Err(changed(from));
            }
            source = again;
        }
        let mut meter = Meter::new(
            self.ctx,
            self.sent,
            size,
            offset,
            1,
            Some(file_name(&from.path).to_owned()),
            false,
        );
        let relayed = relay(
            &mut source,
            &mut dest,
            Amount::Exactly(size - offset),
            &mut meter,
        )
        .await;
        if let Err(broken) = relayed {
            return Err(broken.explain(source, dest).await);
        }
        let done = dest.first::<PutDone>().await;
        source.close().await;
        dest.close().await;
        let done = done?;
        if !done.skipped && done.hash != hash {
            return Err(changed(from));
        }
        if !done.skipped {
            self.sent.files.fetch_add(1, Ordering::Relaxed);
        }
        meter.emit(true);
        Ok((offset, size - offset, done))
    }

    async fn get(&self, from: &End, offset: u64) -> Result<(Conn, GetReady), ErrorBody> {
        let mut source = self
            .open(
                from,
                json!({"op": "get", "path": from.path, "offset": offset}),
            )
            .await?;
        match source.first::<GetReady>().await {
            Ok(ready) => Ok((source, ready)),
            Err(error) => {
                source.abort().await;
                Err(error)
            }
        }
    }

    async fn tree(
        &self,
        from: &End,
        to: &End,
        source_stat: &StatReply,
        args: &TransferArgs,
    ) -> Result<TransferReport, ErrorBody> {
        let if_exists = IfExists::from(args.if_exists);
        let there: StatReply = self
            .request(to, json!({"op": "stat", "path": to.path}))
            .await?;
        if args.move_source {
            check_not_same_or_subtree(from, to, source_stat, &there, true)?;
            self.check_move_permission(from).await?;
        }
        let missing = match there.kind.as_str() {
            "missing" => true,
            "dir" => false,
            _ => {
                return Err(ErrorBody::new(
                    ErrorCode::Conflict,
                    format!(
                        "{} on {} is not a directory, and a tree cannot replace it.",
                        to.path, to.machine
                    ),
                )
                .with_hint("Name a directory, or a path that does not exist yet.")
                .with_machine(to.machine.clone()));
            }
        };
        // A move always verifies every file by BLAKE3 hash so no source file is removed without a
        // hash-verified copy at the destination.
        let effective_checksum = args.checksum || args.move_source;
        let source = self
            .manifest(from, effective_checksum, &args.exclude)
            .await?;
        let dest = if missing {
            Manifest::default()
        } else {
            self.manifest(to, effective_checksum, &args.exclude).await?
        };
        let mut plan = legion_xfer::plan::plan(
            &source,
            &dest,
            &PlanOptions {
                checksum: effective_checksum,
                delete: args.delete,
                if_exists,
            },
        );
        if if_exists == IfExists::Fail && !plan.existing.is_empty() {
            let first = &plan.existing[0];
            let count = plan.existing.len();
            let more = if count > 1 {
                format!(" (and {} more)", count - 1)
            } else {
                String::new()
            };
            return Err(ErrorBody::new(
                ErrorCode::Exists,
                format!(
                    "{first} already exists under {} on {}{more}.",
                    to.path, to.machine
                ),
            )
            .with_hint(
                "Pass if_exists=\"replace\", \"skip\", or \"rename\", or choose an empty destination.",
            )
            .with_details(json!({"existing": plan.existing.iter().take(MOST_LISTED).collect::<Vec<_>>(), "count": count}))
            .with_machine(to.machine.clone()));
        }
        let listed: HashMap<&str, &Entry> = source
            .entries
            .iter()
            .map(|entry| (entry.path.as_str(), entry))
            .collect();
        let dest_listed: HashMap<&str, &Entry> = dest
            .entries
            .iter()
            .map(|entry| (entry.path.as_str(), entry))
            .collect();
        // Where letter case is ignored, two names that differ only in case are one file, so
        // sending both would overwrite one with the other on every run. Neither is sent. Where
        // that cannot be told, the destination is taken to ignore case: holding back is safer.
        let caseless = there.case_insensitive.unwrap_or(true);
        let mut conflicts = plan.conflicts.clone();
        if caseless {
            conflicts.extend(hold_back_case_collisions(&mut plan, &listed, &to.machine));
        }
        let mut report = TransferReport {
            from: from.named(),
            to: to.named(),
            kind: "tree",
            files_unchanged: plan.totals.files_unchanged,
            files_skipped: plan.totals.files_skipped,
            moved: args.move_source,
            ..TransferReport::default()
        };
        let mut skipped: Vec<SkippedEntry> = Vec::new();
        skipped.extend(on(&from.machine, source.skipped));
        skipped.extend(on(&to.machine, dest.skipped));

        // Entries already verified identical at the destination (from an earlier interrupted move
        // or prior copy under `replace`) can safely be removed from the source once the rest lands.
        let already_verified: Vec<String> = if args.move_source && if_exists == IfExists::Replace {
            source
                .entries
                .iter()
                .filter(|src| {
                    dest_listed.get(src.path.as_str()).is_some_and(|dst| {
                        match (src.kind, dst.kind) {
                            (EntryKind::File, EntryKind::File) => {
                                src.hash.is_some() && src.hash == dst.hash
                            }
                            (EntryKind::Symlink, EntryKind::Symlink) => {
                                src.link_target == dst.link_target
                            }
                            _ => false,
                        }
                    })
                })
                .map(|src| src.path.clone())
                .collect()
        } else {
            Vec::new()
        };

        if args.dry_run {
            report.files_sent = plan.totals.files_to_send;
            report.bytes_sent = plan.totals.bytes_to_send;
            report.deleted = plan.totals.entries_to_delete;
            report.would_send = Some(report.list(plan.send.clone()));
            report.would_delete = Some(report.list(plan.delete.clone()));
            if args.move_source {
                let mut remove_list = planned_source_removals(
                    &source.entries,
                    &plan.send,
                    &already_verified,
                    &listed,
                    &conflicts,
                    &skipped,
                    args.exclude.is_empty() && report.files_skipped == 0,
                    &from.path,
                );
                report.source_removed = remove_list.len() as u64;
                report.would_remove = Some(report.list(std::mem::take(&mut remove_list)));
            }
            report.conflicts = report.list(conflicts);
            report.skipped = report.list(skipped);
            return Ok(report);
        }
        // Removals that make room for an entry about to be sent as another kind go first; what
        // the destination has and the source does not goes only once everything new has arrived,
        // so a transfer that fails half way never leaves a destination with less than it had.
        let sending: HashSet<&str> = plan.send.iter().map(String::as_str).collect();
        let (room, extras): (Vec<String>, Vec<String>) = plan
            .delete
            .iter()
            .cloned()
            .partition(|path| makes_room(path, &sending));
        report.deleted += self.delete(to, &room, &mut skipped).await?;
        let mut meter = Meter::new(
            self.ctx,
            self.sent,
            plan.totals.bytes_to_send,
            0,
            plan.totals.files_to_send,
            None,
            true,
        );
        // A destination that does not exist yet is made even when there is nothing to put in it,
        // so an empty tree arrives as an empty directory.
        let mut rounds = if args.move_source {
            move_batches(&plan.send, &listed)
        } else {
            batches(&plan.send)
        };
        if rounds.is_empty() && missing {
            rounds.push(&[]);
        }
        let mut renamed: Vec<RenamedEntry> = Vec::new();
        for names in rounds {
            let budget = archive_budget(names, &listed);
            let done = self
                .pack_unpack(
                    from,
                    to,
                    Round {
                        names,
                        budget,
                        listed: &listed,
                        checksum: effective_checksum,
                        if_exists,
                    },
                    &mut meter,
                    &mut skipped,
                )
                .await
                .map_err(|error| left_in_place(error, extras.len(), &to.machine))?;
            report.files_sent += done.files;
            report.bytes_sent += done.bytes;
            report.files_skipped += done.files_skipped;
            renamed.extend(done.renamed);
            skipped.extend(on(&to.machine, done.skipped));
            if caseless {
                conflicts.extend(case_conflicts(&done.case_collisions, &to.machine));
            }
            // Only remove source files/symlinks that this round just verified by hash and placed at
            // the destination; never remove anything skipped or omitted.
            if args.move_source {
                let mut verified =
                    Vec::with_capacity(done.verified_files.len() + done.verified_symlinks.len());
                verified.extend(done.verified_files);
                verified.extend(done.verified_symlinks);
                if !verified.is_empty() {
                    let hashes = expected_hashes_for(&verified, &listed);
                    report.source_removed += self
                        .delete_verified(from, &verified, &hashes, false, &mut skipped)
                        .await?;
                }
            }
        }
        meter.emit(true);
        conflicts.sort();
        conflicts.dedup();
        report.deleted += self.delete(to, &extras, &mut skipped).await?;
        if args.move_source {
            if !already_verified.is_empty() {
                let hashes = expected_hashes_for(&already_verified, &listed);
                report.source_removed += self
                    .delete_verified(from, &already_verified, &hashes, false, &mut skipped)
                    .await?;
            }
            // Remove source directories deepest-first only once every verified file has been
            // removed; any directory still holding excluded, skipped, or conflicted files stays.
            let dirs: Vec<String> = source
                .entries
                .iter()
                .filter(|entry| entry.kind == EntryKind::Dir)
                .map(|entry| entry.path.clone())
                .collect();
            let remove_root = args.exclude.is_empty()
                && conflicts.is_empty()
                && report.files_skipped == 0
                && skipped.is_empty();
            if !dirs.is_empty() || remove_root {
                let mut dir_skipped = Vec::new();
                report.source_removed += self
                    .delete_verified(from, &dirs, &HashMap::new(), remove_root, &mut dir_skipped)
                    .await?;
                // Leaving a directory that still holds excluded/skipped entries is expected; only
                // surface unexpected directory errors when nothing was intentionally left behind.
                if remove_root {
                    skipped.extend(dir_skipped);
                }
            }
        }
        report.conflicts = report.list(conflicts);
        report.skipped = report.list(skipped);
        report.renamed = report.list(renamed);
        Ok(report)
    }

    /// Removes `names` under the destination's root, in batches; says how many went.
    async fn delete(
        &self,
        to: &End,
        names: &[String],
        skipped: &mut Vec<SkippedEntry>,
    ) -> Result<u64, ErrorBody> {
        let mut removed = 0;
        for names in batches(names) {
            let done: DeleteDone = self
                .request(to, json!({"op": "delete", "root": to.path, "names": names}))
                .await?;
            removed += done.removed;
            self.sent.deleted.fetch_add(done.removed, Ordering::Relaxed);
            skipped.extend(on(&to.machine, done.skipped));
        }
        Ok(removed)
    }

    /// Removes a single source root file (verifying its BLAKE3 hash first) after a move succeeds.
    async fn remove_source_root(
        &self,
        from: &End,
        expect_hash: Option<Hash>,
        skipped: &mut Vec<SkippedEntry>,
    ) -> Result<u64, ErrorBody> {
        let mut header = json!({
            "op": "delete",
            "root": from.path,
            "names": [],
            "remove_root": true,
        });
        if let Some(hash) = expect_hash {
            header["expect_root_hash"] = json!(hash);
        }
        let done: DeleteDone = self.request(from, header).await?;
        skipped.extend(on(&from.machine, done.skipped));
        Ok(done.removed)
    }

    /// Removes verified source entries under `from`, verifying each regular file's hash still
    /// matches before unlinking, and optionally removes the source root directory once empty.
    async fn delete_verified(
        &self,
        from: &End,
        names: &[String],
        hashes: &HashMap<String, Hash>,
        remove_root: bool,
        skipped: &mut Vec<SkippedEntry>,
    ) -> Result<u64, ErrorBody> {
        let mut removed = 0;
        let chunks = batches(names);
        if chunks.is_empty() {
            if remove_root {
                let done: DeleteDone = self
                    .request(
                        from,
                        json!({
                            "op": "delete",
                            "root": from.path,
                            "names": [],
                            "remove_root": true,
                        }),
                    )
                    .await?;
                removed += done.removed;
                skipped.extend(on(&from.machine, done.skipped));
            }
            return Ok(removed);
        }
        let last_idx = chunks.len() - 1;
        for (idx, chunk) in chunks.into_iter().enumerate() {
            let chunk_hashes: HashMap<&str, Hash> = chunk
                .iter()
                .filter_map(|name| hashes.get(name).map(|h| (name.as_str(), *h)))
                .collect();
            let mut header = json!({
                "op": "delete",
                "root": from.path,
                "names": chunk,
                "remove_root": remove_root && idx == last_idx,
            });
            if !chunk_hashes.is_empty() {
                header["expect_hashes"] = json!(chunk_hashes);
            }
            let done: DeleteDone = self.request(from, header).await?;
            removed += done.removed;
            skipped.extend(on(&from.machine, done.skipped));
        }
        Ok(removed)
    }

    async fn pack_unpack(
        &self,
        from: &End,
        to: &End,
        round: Round<'_>,
        meter: &mut Meter<'_>,
        skipped: &mut Vec<SkippedEntry>,
    ) -> Result<UnpackDone, ErrorBody> {
        let Round {
            names,
            budget,
            listed,
            checksum,
            if_exists,
        } = round;
        let mut source = self
            .open(
                from,
                json!({"op": "pack", "root": from.path, "names": names}),
            )
            .await?;
        let ready: PackReady = match source.first().await {
            Ok(ready) => ready,
            Err(error) => {
                source.abort().await;
                return Err(error);
            }
        };
        // What the destination agrees to take: each name as the source listed it, less what the
        // source has just said it leaves out.
        let left_out: HashSet<&str> = ready.skipped.iter().map(|e| e.path.as_str()).collect();
        let entries: Vec<Planned> = names
            .iter()
            .filter(|name| !left_out.contains(name.as_str()))
            .filter_map(|name| listed.get(name.as_str()))
            .map(|entry| Planned {
                path: entry.path.clone(),
                kind: entry.kind,
                size: (entry.kind == EntryKind::File).then_some(entry.size),
                hash: if checksum { entry.hash } else { None },
                target: entry.link_target.clone(),
            })
            .collect();
        skipped.extend(on(&from.machine, ready.skipped));
        let mut unpack_header = json!({"op": "unpack", "root": to.path, "entries": entries});
        if if_exists != IfExists::Replace {
            unpack_header["if_exists"] = json!(if_exists);
        }
        let mut dest = match self.open(to, unpack_header).await {
            Ok(dest) => dest,
            Err(error) => {
                source.abort().await;
                return Err(error);
            }
        };
        if let Err(error) = dest.first::<UnpackReady>().await {
            source.abort().await;
            dest.abort().await;
            return Err(error);
        }
        if let Err(broken) = relay(&mut source, &mut dest, Amount::AtMost(budget), meter).await {
            return Err(blame(broken.explain(source, dest).await, from));
        }
        // The archive ended; it is whole only if the source says it finished.
        if let Err(error) = source.finish().await {
            dest.abort().await;
            return Err(error);
        }
        dest.end_input().await;
        let done = dest.first::<UnpackDone>().await;
        dest.close().await;
        done.map_err(|error| blame(error, from))
    }

    async fn manifest(
        &self,
        end: &End,
        checksum: bool,
        exclude: &[String],
    ) -> Result<Manifest, ErrorBody> {
        let mut conn = self
            .open(
                end,
                json!({"op": "manifest", "root": end.path, "checksum": checksum, "exclude": exclude}),
            )
            .await?;
        let listed = conn.manifest().await;
        match listed {
            Ok(manifest) => {
                conn.close().await;
                Ok(manifest)
            }
            Err(error) => {
                conn.abort().await;
                Err(error)
            }
        }
    }

    /// One operation that answers with one line.
    async fn request<T: DeserializeOwned>(&self, end: &End, header: Value) -> Result<T, ErrorBody> {
        let mut conn = self.open(end, header).await?;
        let answer = conn.first::<T>().await;
        conn.close().await;
        answer
    }

    async fn open(&self, end: &End, header: Value) -> Result<Conn, ErrorBody> {
        Conn::open(self.fleet, self.ctx, self.sent, end, header).await
    }
}

/// Takes out of the plan every entry whose name differs only in letter case from another's, at
/// either end, with whatever is under it, and says why for each.
fn hold_back_case_collisions(
    plan: &mut Plan,
    listed: &HashMap<&str, &Entry>,
    machine: &str,
) -> Vec<PlanConflict> {
    let conflicts = case_conflicts(&plan.case_collisions, machine);
    if conflicts.is_empty() {
        return conflicts;
    }
    let held: HashSet<&str> = conflicts.iter().map(|c| c.path.as_str()).collect();
    let is_held = |path: &str| {
        let mut at = path;
        loop {
            if held.contains(at) {
                return true;
            }
            match at.rsplit_once('/') {
                Some((parent, _)) => at = parent,
                None => return false,
            }
        }
    };
    plan.send.retain(|path| !is_held(path));
    plan.delete.retain(|path| !is_held(path));
    let files: Vec<&&Entry> = plan
        .send
        .iter()
        .filter_map(|path| listed.get(path.as_str()))
        .filter(|entry| entry.kind == EntryKind::File)
        .collect();
    plan.totals.files_to_send = files.len() as u64;
    plan.totals.bytes_to_send = files.iter().map(|entry| entry.size).sum();
    plan.totals.entries_to_delete = plan.delete.len() as u64;
    conflicts
}

/// One conflict for each name in each collision.
fn case_conflicts(collisions: &[CaseCollision], machine: &str) -> Vec<PlanConflict> {
    let mut conflicts = Vec::new();
    for collision in collisions {
        for path in &collision.paths {
            let others: Vec<&str> = collision
                .paths
                .iter()
                .filter(|other| *other != path)
                .map(String::as_str)
                .collect();
            conflicts.push(PlanConflict {
                path: path.clone(),
                reason: format!(
                    "its name differs only in letter case from {}, and the file system on {machine} cannot hold both names; rename one of them to send both",
                    others.join(", ")
                ),
            });
        }
    }
    conflicts
}

/// Whether removing `path` makes room for an entry about to be sent: the entry itself, of another
/// kind at the destination, or something inside a directory that a file or link replaces.
fn makes_room(path: &str, sending: &HashSet<&str>) -> bool {
    let mut at = path;
    loop {
        if sending.contains(at) {
            return true;
        }
        match at.rsplit_once('/') {
            Some((parent, _)) => at = parent,
            None => return false,
        }
    }
}

/// Says in a failed transfer's error that what the destination has and the source does not was
/// left where it was, since the new content did not all arrive.
fn left_in_place(error: ErrorBody, extras: usize, machine: &str) -> ErrorBody {
    if extras == 0 {
        return error;
    }
    let said = format!(
        "The {extras} entries on {machine} that the source no longer has were left in place, since not everything new arrived."
    );
    let hint = match &error.hint {
        Some(hint) => format!("{hint} {said}"),
        None => said,
    };
    let mut details = match error.details.clone() {
        Some(Value::Object(map)) => map,
        _ => serde_json::Map::new(),
    };
    details.insert("left_in_place".to_owned(), json!(extras));
    let mut error = error.with_hint(hint);
    error.details = Some(Value::Object(details));
    error
}

/// One archive's worth of a tree: its names, the most bytes the archive may take, what the source
/// listed for each, whether their hashes are checked, and the `if_exists` policy.
struct Round<'a> {
    names: &'a [String],
    budget: u64,
    listed: &'a HashMap<&'a str, &'a Entry>,
    checksum: bool,
    if_exists: IfExists,
}

/// Refuses moving a path onto itself or moving a directory into its own subtree on the same machine.
fn check_not_same_or_subtree(
    from: &End,
    to: &End,
    from_stat: &StatReply,
    to_stat: &StatReply,
    check_subtree: bool,
) -> Result<(), ErrorBody> {
    if from.machine != to.machine {
        return Ok(());
    }
    let from_path = from_stat.resolved_path.as_deref().unwrap_or(&from.path);
    let to_path = to_stat.resolved_path.as_deref().unwrap_or(&to.path);
    let caseless = to_stat
        .case_insensitive
        .or(from_stat.case_insensitive)
        .unwrap_or(false)
        || is_windows_drive_path(from_path)
        || is_windows_drive_path(to_path);
    let from_norm = normalize_compare_path(from_path, caseless);
    let to_norm = normalize_compare_path(to_path, caseless);
    if from_norm == to_norm {
        return Err(ErrorBody::new(
            ErrorCode::InvalidArgs,
            format!("Cannot move {} on {} onto itself.", from.path, from.machine),
        )
        .with_hint("Choose a different destination path.")
        .with_machine(from.machine.clone()));
    }
    if check_subtree {
        let is_subtree = if from_norm == "/" {
            to_norm.starts_with('/') && to_norm.len() > 1
        } else {
            to_norm
                .strip_prefix(&from_norm)
                .is_some_and(|rest| rest.starts_with('/'))
        };
        if is_subtree {
            return Err(ErrorBody::new(
                ErrorCode::InvalidArgs,
                format!(
                    "Cannot move {} on {} into its own subdirectory {}.",
                    from.path, from.machine, to.path
                ),
            )
            .with_hint("Choose a destination outside the source directory.")
            .with_machine(from.machine.clone()));
        }
    }
    Ok(())
}

fn is_windows_drive_path(path: &str) -> bool {
    let b = path.as_bytes();
    b.len() >= 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && (b[2] == b'\\' || b[2] == b'/')
}

fn normalize_compare_path(path: &str, caseless: bool) -> String {
    let mut s = path.replace('\\', "/");
    while s.len() > 1 && s.ends_with('/') {
        // Keep `C:/` as `C:/` rather than `C:`.
        if s.len() == 3 && s.as_bytes()[1] == b':' {
            break;
        }
        s.pop();
    }
    if caseless {
        s.make_ascii_lowercase();
    }
    s
}

/// Maps each regular file in `names` to its planned BLAKE3 hash from `listed`.
fn expected_hashes_for(names: &[String], listed: &HashMap<&str, &Entry>) -> HashMap<String, Hash> {
    names
        .iter()
        .filter_map(|name| {
            let entry = listed.get(name.as_str())?;
            (entry.kind == EntryKind::File)
                .then_some(entry.hash)
                .flatten()
                .map(|h| (name.clone(), h))
        })
        .collect()
}

/// Computes the source paths that `dry_run` with `move: true` would remove from the source tree.
#[allow(clippy::too_many_arguments)]
fn planned_source_removals(
    source_entries: &[Entry],
    sending: &[String],
    already_verified: &[String],
    listed: &HashMap<&str, &Entry>,
    conflicts: &[PlanConflict],
    skipped: &[SkippedEntry],
    whole_tree_eligible: bool,
    source_root: &str,
) -> Vec<String> {
    let mut out = Vec::new();
    for name in sending {
        if listed
            .get(name.as_str())
            .is_some_and(|e| e.kind != EntryKind::Dir)
        {
            out.push(name.clone());
        }
    }
    out.extend(already_verified.iter().cloned());
    if whole_tree_eligible && conflicts.is_empty() && skipped.is_empty() {
        let mut dirs: Vec<String> = source_entries
            .iter()
            .filter(|e| e.kind == EntryKind::Dir)
            .map(|e| e.path.clone())
            .collect();
        dirs.sort_by(|a, b| {
            b.matches('/')
                .count()
                .cmp(&a.matches('/').count())
                .then_with(|| b.cmp(a))
        });
        out.extend(dirs);
        out.push(source_root.to_owned());
    }
    out
}

/// Splits `names` into smaller move batches so verified source files are removed incrementally.
fn move_batches<'a>(names: &'a [String], listed: &HashMap<&str, &Entry>) -> Vec<&'a [String]> {
    let mut out = Vec::new();
    let mut start = 0;
    let mut name_bytes = 0usize;
    let mut content_bytes = 0u64;
    let mut file_count = 0usize;
    for (at, name) in names.iter().enumerate() {
        let entry_size = listed.get(name.as_str()).map_or(0, |e| e.size);
        let is_file = listed
            .get(name.as_str())
            .is_some_and(|e| e.kind == EntryKind::File);
        let would_exceed = at > start
            && (at - start >= BATCH_NAMES
                || name_bytes + name.len() > BATCH_BYTES
                || (is_file && file_count >= MOVE_BATCH_FILES)
                || (is_file
                    && content_bytes > 0
                    && content_bytes + entry_size > MOVE_BATCH_CONTENT_BYTES));
        if would_exceed {
            out.push(&names[start..at]);
            start = at;
            name_bytes = 0;
            content_bytes = 0;
            file_count = 0;
        }
        name_bytes += name.len() + 4;
        if is_file {
            content_bytes = content_bytes.saturating_add(entry_size);
            file_count += 1;
        }
    }
    if start < names.len() {
        out.push(&names[start..]);
    }
    out
}

/// A destination that refused an archive for not being what was planned says which entry and
/// why; the archive is the source's, so the error names the source.
fn blame(error: ErrorBody, from: &End) -> ErrorBody {
    let Some(violation) = error
        .details
        .as_ref()
        .and_then(|d| d.get("violation"))
        .cloned()
    else {
        return error;
    };
    ErrorBody::new(
        ErrorCode::Failed,
        format!(
            "{} sent an archive that is not what was planned: {}",
            from.machine, error.message
        ),
    )
    .with_hint(format!(
        "Nothing was renamed over what the destination had for that entry. If {} changed while it was sent, run the transfer again; if not, legion there is not behaving as it should.",
        from.machine
    ))
    .with_details(json!({"violation": violation, "refused_by": error.machine}))
    .with_machine(from.machine.clone())
}

fn changed(from: &End) -> ErrorBody {
    ErrorBody::new(
        ErrorCode::Conflict,
        format!(
            "{} on {} changed while it was being sent.",
            from.path, from.machine
        ),
    )
    .with_hint("Send it again once it stops changing.")
    .with_machine(from.machine.clone())
}

/// Names a machine in each reason, since a result gathers what both ends left out.
fn on(machine: &str, entries: Vec<SkippedEntry>) -> impl Iterator<Item = SkippedEntry> + '_ {
    entries.into_iter().map(move |entry| SkippedEntry {
        path: entry.path,
        reason: format!("{machine}: {}", entry.reason),
    })
}

fn batches(names: &[String]) -> Vec<&[String]> {
    let mut out = Vec::new();
    let mut start = 0;
    let mut bytes = 0;
    for (at, name) in names.iter().enumerate() {
        if at > start && (at - start >= BATCH_NAMES || bytes + name.len() > BATCH_BYTES) {
            out.push(&names[start..at]);
            start = at;
            bytes = 0;
        }
        bytes += name.len() + 4;
    }
    if start < names.len() {
        out.push(&names[start..]);
    }
    out
}

fn ends_in_separator(path: &str) -> bool {
    path.ends_with('/') || path.ends_with('\\')
}

/// The last part of a path as the caller wrote it, whichever separator the machine uses.
fn file_name(path: &str) -> &str {
    let trimmed = path.trim_end_matches(['/', '\\']);
    trimmed
        .rsplit(['/', '\\'])
        .next()
        .filter(|name| !name.is_empty())
        .unwrap_or(trimmed)
}

/// `dir` and `name` joined with the separator `dir` already uses.
fn join(dir: &str, name: &str) -> String {
    if ends_in_separator(dir) {
        format!("{dir}{name}")
    } else if dir.contains('\\') && !dir.contains('/') {
        format!("{dir}\\{name}")
    } else {
        format!("{dir}/{name}")
    }
}

type BoxRead = Box<dyn AsyncRead + Send + Unpin>;
type BoxWrite = Box<dyn AsyncWrite + Send + Unpin>;

/// One operation's pipe to the stream server at one end.
struct Conn {
    machine: String,
    reader: BufReader<BoxRead>,
    writer: Option<BoxWrite>,
    process: Owned,
    /// Whether a reply has been read, which tells a server that never started from one that
    /// failed on the way.
    answered: bool,
    noise_lines: usize,
    noise_bytes: usize,
    /// Whether the other end has said, on its first line, that it speaks this protocol.
    greeted: bool,
    /// The longest reply line taken once the other end has answered.
    longest: usize,
    /// How many bytes of lines have been read, which bounds a listing as a whole.
    received: usize,
    /// When the pipe was opened, which the first reply's time limit counts from.
    opened: tokio::time::Instant,
}

/// How much a listing from another node may hold: entries, bytes in all, and bytes in one line.
struct Bounds {
    entries: usize,
    bytes: usize,
    line: usize,
}

/// One line of a `manifest` reply.
#[derive(Deserialize)]
#[serde(untagged)]
enum Listed {
    Done(ManifestDone),
    Entry(Entry),
}

// A transfer holds two of these for its whole life, so the size of the larger variant, which
// differs by platform, costs nothing worth an allocation.
#[allow(clippy::large_enum_variant)]
enum Process {
    /// `legion stream` on another machine, through its connector.
    Remote(PipeHandle),
    /// The stream server run on this node for an end that is here.
    Local(Option<JoinHandle<Result<(), ErrorBody>>>),
}

impl Conn {
    async fn open(
        fleet: &Fleet,
        ctx: &Ctx,
        sent: &Sent,
        end: &End,
        mut header: Value,
    ) -> Result<Conn, ErrorBody> {
        header["protocol"] = json!(stream::STREAM_PROTOCOL);
        let (reader, writer, process): (BoxRead, BoxWrite, Process) = if end.remote {
            let op = header
                .get("op")
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    ErrorBody::new(ErrorCode::Internal, "Stream header is missing its op name.")
                        .with_machine(ctx.node_name())
                })?
                .to_owned();
            let (underlying_tool, underlying_effect) = stream::op_tool(&op).ok_or_else(|| {
                ErrorBody::new(
                    ErrorCode::Internal,
                    format!("Stream header has unknown op {op:?}."),
                )
                .with_machine(ctx.node_name())
            })?;
            let recorded = header.clone();
            // The other node decides whether the caller may act as this principal, as it does
            // for a forwarded call.
            header["as"] = json!(ctx.principal());
            let pipe = fleet
                .open_stream_pipe(
                    ctx,
                    &end.machine,
                    &op,
                    underlying_tool,
                    underlying_effect,
                    &recorded,
                )
                .await?;
            let (stdin, stdout, handle) = pipe.into_parts();
            (Box::new(stdout), Box::new(stdin), Process::Remote(handle))
        } else {
            let (client_out, server_in) = tokio::io::duplex(CHUNK);
            let (server_out, client_in) = tokio::io::duplex(CHUNK);
            let host = Host::new(ctx.paths().clone(), ctx.node_name());
            let principal = ctx.principal().to_owned();
            let input = SyncIoBridge::new(server_in);
            let output = SyncIoBridge::new(server_out);
            let task = tokio::task::spawn_blocking(move || {
                stream::serve(&host, Some(&principal), input, output)
            });
            (
                Box::new(client_in),
                Box::new(client_out),
                Process::Local(Some(task)),
            )
        };
        let mut conn = Conn {
            machine: end.machine.clone(),
            reader: BufReader::with_capacity(CHUNK, reader),
            writer: Some(writer),
            process: Owned {
                process: Some(process),
                ends: sent.ends.clone(),
            },
            answered: false,
            noise_lines: 0,
            noise_bytes: 0,
            greeted: false,
            longest: LONGEST_REPLY,
            received: 0,
            opened: tokio::time::Instant::now(),
        };
        let mut line = header.to_string().into_bytes();
        line.push(b'\n');
        let written = match tokio::time::timeout(FIRST_REPLY_TIMEOUT, conn.write(&line)).await {
            Ok(written) => written,
            Err(_) => {
                let error = conn.stalled(FIRST_REPLY_TIMEOUT);
                conn.abort().await;
                return Err(error);
            }
        };
        if let Err(error) = written {
            let error = conn
                .ended(&format!("before it took its header ({error})"))
                .await;
            conn.abort().await;
            return Err(error);
        }
        Ok(conn)
    }

    async fn write(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        let Some(writer) = self.writer.as_mut() else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "the input was already closed",
            ));
        };
        writer.write_all(bytes).await?;
        writer.flush().await
    }

    /// The next reply, which has the shape `T` or is the other end's error.
    ///
    /// The other end's first line says which protocol it speaks, and nothing it says is taken
    /// before that: a node too old to say it would ignore what this one asks of it (the entries an
    /// unpack must hold to, the hashes it must check), so it is refused before anything is sent
    /// that relies on them. `{"busy": true}` lines, which say the other end is at work, are passed
    /// over wherever they come, for at most [`MOST_BUSY`] for one reply. Before the first line,
    /// lines of text that are neither (a login banner, a shell's own noise, even one that happens
    /// to be JSON) are skipped within bounds, while bytes that are not text at all mean the other
    /// end speaks another protocol; after it, anything else is the other end gone wrong.
    async fn first<T: DeserializeOwned>(&mut self) -> Result<T, ErrorBody> {
        let waiting = tokio::time::Instant::now();
        loop {
            let line = self.line().await?;
            if let Ok(Value::Object(fields)) = serde_json::from_slice::<Value>(&line) {
                if !self.greeted {
                    if let Some(spoken) = fields.get("protocol") {
                        if spoken.as_u64() != Some(u64::from(stream::STREAM_PROTOCOL)) {
                            return Err(self.older(&format!(
                                "speaks stream protocol {spoken}, and this node speaks stream protocol {}",
                                stream::STREAM_PROTOCOL
                            )));
                        }
                        self.greeted = true;
                        self.answered = true;
                        continue;
                    }
                    if looks_like_a_reply(&fields) {
                        let said = fields
                            .get("error")
                            .and_then(|error| error.get("message"))
                            .and_then(Value::as_str)
                            .map(|message| format!(" (it said: {message})"))
                            .unwrap_or_default();
                        return Err(self.older(&format!(
                            "answered without saying which stream protocol it speaks{said}, as a legion older than this one does"
                        )));
                    }
                } else if fields.len() == 1 && fields.get("busy") == Some(&Value::Bool(true)) {
                    if waiting.elapsed() > MOST_BUSY {
                        return Err(ErrorBody::new(
                            ErrorCode::Timeout,
                            format!(
                                "{} said it was busy for more than {} hours without answering.",
                                self.machine,
                                MOST_BUSY.as_secs() / 3600
                            ),
                        )
                        .with_hint("Transfer a smaller part at a time, or check what is keeping that machine busy.")
                        .with_machine(self.machine.clone()));
                    }
                    continue;
                }
                if self.greeted {
                    if let Some(error) = fields.get("error")
                        && let Ok(error) = serde_json::from_value::<ErrorBody>(error.clone())
                    {
                        return Err(with_machine(error, &self.machine));
                    }
                    if let Ok(reply) = serde_json::from_value::<T>(Value::Object(fields)) {
                        return Ok(reply);
                    }
                }
            }
            if self.answered {
                return Err(ErrorBody::new(
                    ErrorCode::Failed,
                    format!(
                        "{} answered a stream with a line that is not what was asked for.",
                        self.machine
                    ),
                )
                .with_hint("Update legion there, so both ends speak the same stream protocol.")
                .with_machine(self.machine.clone()));
            }
            if std::str::from_utf8(&line).is_err() {
                return Err(self.mismatch("with bytes that are not text"));
            }
            self.noise_lines += 1;
            self.noise_bytes += line.len();
            if self.noise_lines > MOST_NOISE_LINES {
                return Err(self.mismatch(&format!(
                    "with {} lines that are not legion's",
                    self.noise_lines
                )));
            }
        }
    }

    /// One line of output, read within the time and length a reply may take. Before the first
    /// reply, a byte that cannot be in a line of text ends the read at once: it is data from a
    /// node that speaks another protocol, which may never send a newline.
    async fn line(&mut self) -> Result<Vec<u8>, ErrorBody> {
        // A reply is a JSON object however long it is; only other text, a banner, is held to
        // what noise may take before the first reply.
        let (wait, noise) = if self.answered {
            (IDLE_TIMEOUT, self.longest)
        } else {
            (
                FIRST_REPLY_TIMEOUT.saturating_sub(self.opened.elapsed()),
                MOST_NOISE_BYTES.saturating_sub(self.noise_bytes),
            )
        };
        let mut line = Vec::new();
        let binary_is_foreign = !self.answered;
        let read = tokio::time::timeout(
            wait,
            read_line(
                &mut self.reader,
                &mut line,
                Longest {
                    reply: self.longest,
                    noise,
                },
                binary_is_foreign,
            ),
        )
        .await;
        match read {
            Err(_) => Err(self.stalled(if self.answered {
                IDLE_TIMEOUT
            } else {
                FIRST_REPLY_TIMEOUT
            })),
            Ok(Ok(Read::Line)) => {
                self.received += line.len();
                Ok(line)
            }
            Ok(Ok(Read::End)) => Err(self.ended("before it answered").await),
            Ok(Ok(Read::Binary)) => Err(self.mismatch("with bytes that are not text")),
            Ok(Ok(Read::TooLong { reply: false })) if !self.answered => {
                Err(self.mismatch("with more than a banner before its answer"))
            }
            Ok(Ok(Read::TooLong { .. })) => Err(ErrorBody::new(
                ErrorCode::Failed,
                format!(
                    "{} answered a stream with a line longer than {} bytes.",
                    self.machine, self.longest
                ),
            )
            .with_machine(self.machine.clone())),
            Ok(Err(error)) => Err(self.ended(&format!("while it answered ({error})")).await),
        }
    }

    /// The other end speaks another version of the protocol, or one too old to say which.
    fn older(&self, what: &str) -> ErrorBody {
        ErrorBody::new(
            ErrorCode::Unsupported,
            format!("{} {what}.", self.machine),
        )
        .with_hint(format!(
            "Update legion on {} (or on this machine, if it is the older one), so both speak the same stream protocol.",
            self.machine
        ))
        .with_machine(self.machine.clone())
    }

    /// The other end answered with something other than this protocol's first line.
    fn mismatch(&self, how: &str) -> ErrorBody {
        ErrorBody::new(
            ErrorCode::Unsupported,
            format!(
                "{} answered a stream {how}, where a line of JSON was expected, so it does not speak this node's stream protocol.",
                self.machine
            ),
        )
        .with_hint(format!(
            "Update legion on {} (or on this machine, if it is the older one). If a login script or banner prints to stdout there, make it quiet for sessions without a terminal.",
            self.machine
        ))
        .with_machine(self.machine.clone())
    }

    /// The other end went silent for `waited`.
    fn stalled(&self, waited: Duration) -> ErrorBody {
        let seconds = waited.as_secs();
        if self.answered {
            ErrorBody::new(
                ErrorCode::Timeout,
                format!(
                    "{} went {seconds} seconds without a word in the middle of a stream.",
                    self.machine
                ),
            )
            .with_hint("The connection or the machine stalled; run the transfer again to continue where it stopped.")
            .with_machine(self.machine.clone())
        } else {
            ErrorBody::new(
                ErrorCode::Timeout,
                format!(
                    "{} did not answer a stream within {seconds} seconds.",
                    self.machine
                ),
            )
            .with_hint(format!(
                "Check that {} is reachable. If it runs an older legion, update it, so both speak the same stream protocol.",
                self.machine
            ))
            .with_machine(self.machine.clone())
        }
    }

    /// Reads a `manifest` reply: one entry per line, then the line that ends it, which must come
    /// and must count what came before it.
    async fn manifest(&mut self) -> Result<Manifest, ErrorBody> {
        self.manifest_within(Bounds {
            entries: MOST_LISTED_ENTRIES,
            bytes: MOST_LISTING_BYTES,
            line: MOST_LISTING_LINE,
        })
        .await
    }

    async fn manifest_within(&mut self, bounds: Bounds) -> Result<Manifest, ErrorBody> {
        let most = bounds.entries;
        self.longest = bounds.line;
        let mut manifest = Manifest::default();
        loop {
            let listed = self.first::<Listed>().await?;
            if self.received > bounds.bytes {
                return Err(ErrorBody::new(
                    ErrorCode::Failed,
                    format!(
                        "The listing from {} is larger than {} bytes, more than one transfer compares.",
                        self.machine, bounds.bytes
                    ),
                )
                .with_hint("Transfer its subdirectories one at a time.")
                .with_machine(self.machine.clone()));
            }
            match listed {
                Listed::Entry(_) if manifest.entries.len() == most => {
                    return Err(ErrorBody::new(
                        ErrorCode::Failed,
                        format!(
                            "The tree on {} holds more than {most} entries, more than one transfer compares.",
                            self.machine
                        ),
                    )
                    .with_hint("Transfer its subdirectories one at a time.")
                    .with_machine(self.machine.clone()));
                }
                Listed::Entry(entry) => manifest.entries.push(entry),
                Listed::Done(done) => {
                    if done.entries != manifest.entries.len() as u64 {
                        return Err(ErrorBody::new(
                            ErrorCode::Failed,
                            format!(
                                "{} listed {} entries and said it listed {}.",
                                self.machine,
                                manifest.entries.len(),
                                done.entries
                            ),
                        )
                        .with_machine(self.machine.clone()));
                    }
                    manifest.skipped = done.skipped;
                    return Ok(manifest);
                }
            }
        }
    }

    /// Why the stream ended early: the server's own error when it ran here, its exit status and
    /// stderr when it ran on another machine.
    async fn ended(&mut self, when: &str) -> ErrorBody {
        let machine = self.machine.clone();
        match self.process.process.as_mut() {
            Some(Process::Local(task)) => {
                if let Some(task) = task.take()
                    && let Ok(Ok(Err(error))) = tokio::time::timeout(EXIT_WAIT, task).await
                {
                    return with_machine(error, &machine);
                }
                ErrorBody::new(
                    ErrorCode::Failed,
                    format!("The stream on {machine} ended {when}."),
                )
                .with_machine(machine)
            }
            None => ErrorBody::new(
                ErrorCode::Failed,
                format!("The stream on {machine} ended {when}."),
            )
            .with_machine(machine),
            Some(Process::Remote(handle)) => {
                let (code, stderr) = handle.collect_exit_info(EXIT_WAIT).await;
                let status = code
                    .map(|code| format!(" (exit status {code})"))
                    .unwrap_or_default();
                let said = if stderr.is_empty() {
                    String::new()
                } else {
                    format!(": {stderr}")
                };
                let code = if self.answered {
                    ErrorCode::Failed
                } else {
                    ErrorCode::Unreachable
                };
                ErrorBody::new(
                    code,
                    format!("The stream to {machine} ended {when}{status}{said}."),
                )
                .with_hint(format!(
                    "Check that {machine} is reachable and that `legion stream` can be started on it."
                ))
                .with_machine(machine)
            }
        }
    }

    /// Waits for a server that produced raw bytes to say it finished them all.
    async fn finish(mut self) -> Result<(), ErrorBody> {
        self.writer.take();
        let machine = self.machine.clone();
        let result = match self.process.process.as_mut() {
            None => Ok(()),
            Some(Process::Local(task)) => match task.take() {
                Some(task) => match tokio::time::timeout(EXIT_WAIT, task).await {
                    Ok(Ok(Ok(()))) => Ok(()),
                    Ok(Ok(Err(error))) => Err(with_machine(error, &machine)),
                    _ => Err(ErrorBody::new(
                        ErrorCode::Failed,
                        format!("The stream on {machine} did not finish."),
                    )
                    .with_machine(machine)),
                },
                None => Ok(()),
            },
            Some(Process::Remote(handle)) => match handle.collect_exit_info(FINISH_WAIT).await {
                (Some(0), _) => Ok(()),
                _ => Err(self.ended("before it finished").await),
            },
        };
        self.close().await;
        result
    }

    /// Ends the server's input, as the end of an archive.
    async fn end_input(&mut self) {
        if let Some(mut writer) = self.writer.take() {
            let _ = writer.shutdown().await;
        }
    }

    /// Lets the server finish on its own: its input ends and it is given a moment to exit.
    async fn close(mut self) {
        self.end_input().await;
        match self.process.process.take() {
            Some(Process::Remote(handle)) => handle.close(EXIT_WAIT).await,
            Some(Process::Local(Some(task))) => {
                drop(self.reader);
                let _ = tokio::time::timeout(EXIT_WAIT, task).await;
            }
            Some(Process::Local(None)) | None => {}
        }
    }

    /// Stops the server. Its input ends without the rest of what it expected, so nothing it was
    /// writing is renamed into place, and it is given [`ABORT_GRACE`] to remove what it was
    /// writing and exit before it is killed.
    async fn abort(mut self) {
        let process = self.process.process.take();
        drop(self.writer);
        drop(self.reader);
        match process {
            Some(Process::Remote(handle)) => handle.close(ABORT_GRACE).await,
            Some(Process::Local(Some(task))) => {
                let _ = tokio::time::timeout(EXIT_WAIT, task).await;
            }
            Some(Process::Local(None)) | None => {}
        }
    }
}

/// Whether a JSON line before the protocol line is a reply from a node too old to send that line,
/// rather than a banner that happens to be JSON: it has a field one of this protocol's replies has.
fn looks_like_a_reply(fields: &serde_json::Map<String, Value>) -> bool {
    const REPLY_FIELDS: [&str; 10] = [
        "error", "kind", "path", "size", "offset", "ready", "removed", "done", "ok", "busy",
    ];
    REPLY_FIELDS.iter().any(|field| fields.contains_key(*field))
}

/// How reading a line ended.
enum Read {
    /// A whole line, or the last bytes before the end.
    Line,
    /// The end, before any byte.
    End,
    /// More than the longest line allowed; `reply` when the line began as a JSON object.
    TooLong { reply: bool },
    /// A NUL byte, which no line of text holds.
    Binary,
}

/// How long a line may grow: one that begins as a JSON object is a reply, anything else noise.
struct Longest {
    reply: usize,
    noise: usize,
}

/// Reads one line into `line`, newline included, without taking more than `longest` allows, and
/// stopping at the first NUL byte when `stop_at_binary` says to, without waiting for a newline.
async fn read_line<R: AsyncBufRead + Unpin>(
    reader: &mut R,
    line: &mut Vec<u8>,
    longest: Longest,
    stop_at_binary: bool,
) -> std::io::Result<Read> {
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            return Ok(if line.is_empty() {
                Read::End
            } else {
                Read::Line
            });
        }
        let (chunk, found) = match available.iter().position(|byte| *byte == b'\n') {
            Some(end) => (&available[..=end], true),
            None => (available, false),
        };
        if stop_at_binary && chunk.contains(&0) {
            return Ok(Read::Binary);
        }
        let reply = line
            .iter()
            .chain(chunk)
            .find(|byte| !byte.is_ascii_whitespace())
            == Some(&b'{');
        let most = if reply { longest.reply } else { longest.noise };
        if line.len() + chunk.len() > most {
            return Ok(Read::TooLong { reply });
        }
        line.extend_from_slice(chunk);
        let used = chunk.len();
        reader.consume(used);
        if found {
            return Ok(Read::Line);
        }
    }
}

fn with_machine(error: ErrorBody, machine: &str) -> ErrorBody {
    error.with_machine(machine)
}

/// Where a relay broke.
enum Broken {
    /// This end moved no bytes for [`IDLE_TIMEOUT`]: the source sent none, or the destination
    /// took none.
    Stalled { source: bool },
    /// The source's output failed or ended before it should have.
    Source(String),
    /// The source sent more than it may: the limit it went past.
    Excess(u64),
    /// The destination stopped taking input.
    Destination(String),
}

impl Broken {
    /// The error to report, preferring what the end that broke said itself.
    async fn explain(self, mut source: Conn, mut dest: Conn) -> ErrorBody {
        match self {
            Broken::Stalled { source: at_source } => {
                let machine = if at_source {
                    source.machine.clone()
                } else {
                    dest.machine.clone()
                };
                source.abort().await;
                dest.abort().await;
                ErrorBody::new(
                    ErrorCode::Timeout,
                    format!(
                        "{machine} moved no bytes for {} seconds in the middle of a transfer.",
                        IDLE_TIMEOUT.as_secs()
                    ),
                )
                .with_hint("The connection or the machine stalled; run the transfer again to continue where it stopped.")
                .with_machine(machine)
            }
            Broken::Source(what) => {
                let error = source.ended(&what).await;
                source.abort().await;
                dest.abort().await;
                error
            }
            Broken::Excess(limit) => {
                let machine = source.machine.clone();
                source.abort().await;
                dest.abort().await;
                ErrorBody::new(
                    ErrorCode::Failed,
                    format!(
                        "{machine} sent more than the {limit} bytes that were planned, so the transfer was stopped there."
                    ),
                )
                .with_hint("The source may have grown while it was sent; run the transfer again. If it keeps happening, legion there is not behaving as it should.")
                .with_machine(machine)
            }
            Broken::Destination(what) => {
                // A destination that refused mid-way says why on its output before it exits.
                let error = match tokio::time::timeout(EXIT_WAIT, dest.first::<Value>()).await {
                    Ok(Err(error)) => error,
                    _ => dest.ended(&what).await,
                };
                source.abort().await;
                dest.abort().await;
                error
            }
        }
    }
}

/// How much a relay may move.
#[derive(Clone, Copy, Debug)]
enum Amount {
    /// A file's bytes from the offset on, exactly as many as its source announced.
    Exactly(u64),
    /// An archive, which ends when its source ends it and is cut off once it passes this many
    /// bytes: what the planned entries can take, headers included.
    AtMost(u64),
}

/// Copies the source's output to the destination's input in fixed chunks, never more than
/// `amount` allows: a source that sends more is stopped before the excess reaches the destination.
async fn relay(
    source: &mut Conn,
    dest: &mut Conn,
    amount: Amount,
    meter: &mut Meter<'_>,
) -> Result<(), Broken> {
    let mut buf = vec![0u8; CHUNK];
    let mut copied = 0u64;
    let limit = match amount {
        Amount::Exactly(total) | Amount::AtMost(total) => total,
    };
    loop {
        // An archive is read one byte past its limit, to tell an archive that ends there from one
        // that goes on.
        let room = match amount {
            Amount::Exactly(_) => limit - copied,
            Amount::AtMost(_) => limit - copied + 1,
        };
        let want = usize::try_from(room).unwrap_or(CHUNK).min(CHUNK);
        if want == 0 {
            break;
        }
        let read = tokio::time::timeout(IDLE_TIMEOUT, source.reader.read(&mut buf[..want]))
            .await
            .map_err(|_| Broken::Stalled { source: true })?
            .map_err(|error| Broken::Source(format!("while it sent ({error})")))?;
        if read == 0 {
            if let Amount::Exactly(_) = amount {
                return Err(Broken::Source(format!(
                    "after {copied} bytes, before the rest"
                )));
            }
            break;
        }
        if copied + read as u64 > limit {
            return Err(Broken::Excess(limit));
        }
        tokio::time::timeout(IDLE_TIMEOUT, dest.write(&buf[..read]))
            .await
            .map_err(|_| Broken::Stalled { source: false })?
            .map_err(|error| Broken::Destination(format!("while it received ({error})")))?;
        copied += read as u64;
        meter.add(&buf[..read]);
    }
    if let Amount::Exactly(_) = amount {
        // All that was announced has passed; a source that has more is not behaving.
        let mut more = [0u8; 1];
        if let Ok(Ok(read)) = tokio::time::timeout(EXIT_WAIT, source.reader.read(&mut more)).await
            && read > 0
        {
            return Err(Broken::Excess(limit));
        }
    }
    Ok(())
}

/// The most bytes an archive of `names` can take: each entry's content padded to whole tar
/// blocks, and room for its headers, a long name or link target and an extended header for a
/// time before 1970, then the blocks that end an archive. A source that sends more is not sending
/// what was planned.
fn archive_budget(names: &[String], listed: &HashMap<&str, &Entry>) -> u64 {
    const BLOCK: u64 = 512;
    let padded = |len: u64| len.div_ceil(BLOCK) * BLOCK;
    let entries: u64 = names
        .iter()
        .map(|name| {
            let (size, target) = listed.get(name.as_str()).map_or((0, 0), |entry| {
                (
                    entry.size,
                    entry.link_target.as_ref().map_or(0, String::len),
                )
            });
            4 * BLOCK + padded(size) + 2 * padded(name.len() as u64 + 1) + padded(target as u64 + 1)
        })
        .sum();
    entries + 20 * BLOCK
}

/// Counts what passes and tells the caller a few times a second.
struct Meter<'a> {
    /// The call progress goes to; none where nobody listens.
    ctx: Option<&'a Ctx>,
    sent: &'a Sent,
    bytes_total: u64,
    /// Bytes already on disk before this run started (for resumed single files).
    bytes_base: u64,
    files_total: u64,
    current: Option<String>,
    tar: Option<TarCounter>,
    started: Instant,
    last: Instant,
    last_bytes_done: u64,
    last_files_done: u64,
}

impl<'a> Meter<'a> {
    fn new(
        ctx: &'a Ctx,
        sent: &'a Sent,
        bytes_total: u64,
        bytes_base: u64,
        files_total: u64,
        current: Option<String>,
        tar: bool,
    ) -> Meter<'a> {
        let now = Instant::now();
        Meter {
            ctx: Some(ctx),
            sent,
            bytes_total,
            bytes_base,
            files_total,
            current,
            tar: tar.then(TarCounter::default),
            started: now,
            last: now,
            last_bytes_done: bytes_base.min(bytes_total),
            last_files_done: 0,
        }
    }

    fn add(&mut self, chunk: &[u8]) {
        let (bytes, files) = match &mut self.tar {
            Some(tar) => {
                let res = tar.feed(chunk);
                if let Some(name) = tar.current_name() {
                    self.current = Some(name.to_owned());
                }
                res
            }
            None => (chunk.len() as u64, 0),
        };
        self.sent.bytes.fetch_add(bytes, Ordering::Relaxed);
        self.sent.files.fetch_add(files, Ordering::Relaxed);
        self.emit(false);
    }

    fn emit(&mut self, last: bool) {
        if !last && self.last.elapsed() < PROGRESS_EVERY {
            return;
        }
        self.last = Instant::now();
        let Some(ctx) = self.ctx else {
            return;
        };
        let transferred = self.sent.bytes.load(Ordering::Relaxed);
        let raw_bytes = self.bytes_base.saturating_add(transferred);
        let clamped_bytes = if self.bytes_total > 0 {
            raw_bytes.min(self.bytes_total)
        } else {
            raw_bytes
        };
        let bytes_done = clamped_bytes.max(self.last_bytes_done);
        self.last_bytes_done = bytes_done;

        let raw_files = self.sent.files.load(Ordering::Relaxed);
        let clamped_files = if self.files_total > 0 {
            raw_files.min(self.files_total)
        } else {
            raw_files
        };
        let files_done = clamped_files.max(self.last_files_done);
        self.last_files_done = files_done;

        let elapsed = self.started.elapsed().as_secs_f64();
        let rate = if elapsed > 0.001 && transferred > 0 {
            (transferred as f64 / elapsed).round() as u64
        } else {
            0
        };
        let mut payload = json!({
            "bytes_done": bytes_done,
            "bytes_total": self.bytes_total,
            "files_done": files_done,
            "files_total": self.files_total,
            "rate": rate,
        });
        if let Some(ref current) = self.current {
            payload["current"] = json!(current);
        }
        ctx.emit(EventKind::Progress, payload);
    }
}

/// Follows a tar stream's headers as it passes, so progress counts the files' own bytes, each
/// file once all of it has gone, and the current entry name, whatever the archive adds around them.
#[derive(Default)]
struct TarCounter {
    header: Vec<u8>,
    data_left: u64,
    pad_left: u64,
    regular: bool,
    pending_long_name: Option<Vec<u8>>,
    reading_long_name: bool,
    long_name_buf: Vec<u8>,
    current: Option<String>,
}

impl TarCounter {
    const BLOCK: usize = 512;

    fn current_name(&self) -> Option<&str> {
        self.current.as_deref()
    }

    /// Takes the next bytes of the archive; says how many were file content and how many files
    /// they completed.
    fn feed(&mut self, mut chunk: &[u8]) -> (u64, u64) {
        let mut content = 0;
        let mut files = 0;
        while !chunk.is_empty() {
            if self.data_left > 0 {
                let take = usize::try_from(self.data_left)
                    .unwrap_or(usize::MAX)
                    .min(chunk.len());
                if self.regular {
                    content += take as u64;
                } else if self.reading_long_name && self.long_name_buf.len() < 4096 {
                    let room = 4096 - self.long_name_buf.len();
                    self.long_name_buf
                        .extend_from_slice(&chunk[..take.min(room)]);
                }
                self.data_left -= take as u64;
                chunk = &chunk[take..];
                if self.data_left == 0 {
                    if self.regular {
                        files += 1;
                    } else if self.reading_long_name {
                        self.reading_long_name = false;
                        self.pending_long_name = Some(std::mem::take(&mut self.long_name_buf));
                    }
                }
                continue;
            }
            if self.pad_left > 0 {
                let take = usize::try_from(self.pad_left)
                    .unwrap_or(usize::MAX)
                    .min(chunk.len());
                self.pad_left -= take as u64;
                chunk = &chunk[take..];
                continue;
            }
            let take = (Self::BLOCK - self.header.len()).min(chunk.len());
            self.header.extend_from_slice(&chunk[..take]);
            chunk = &chunk[take..];
            if self.header.len() < Self::BLOCK {
                continue;
            }
            let header = std::mem::take(&mut self.header);
            if header.iter().all(|byte| *byte == 0) {
                continue;
            }
            let size = Self::size(&header[124..136]);
            let typeflag = header[156];
            self.regular = matches!(typeflag, b'0' | 0 | b'7');
            self.reading_long_name = typeflag == b'L';
            if self.reading_long_name {
                self.long_name_buf.clear();
            } else if matches!(typeflag, b'0' | 0 | b'7' | b'5' | b'2') {
                let raw_name = match self.pending_long_name.take() {
                    Some(long) => long,
                    None => Self::header_name(&header),
                };
                let end = raw_name
                    .iter()
                    .position(|&b| b == 0)
                    .unwrap_or(raw_name.len());
                if let Ok(name) = std::str::from_utf8(&raw_name[..end])
                    && !name.is_empty()
                {
                    self.current = Some(name.to_owned());
                }
            }
            self.data_left = size;
            self.pad_left = (Self::BLOCK as u64 - size % Self::BLOCK as u64) % Self::BLOCK as u64;
            if self.regular && size == 0 {
                files += 1;
            }
        }
        (content, files)
    }

    fn header_name(header: &[u8]) -> Vec<u8> {
        fn trim(slice: &[u8]) -> &[u8] {
            let end = slice.iter().position(|&b| b == 0).unwrap_or(slice.len());
            &slice[..end]
        }
        let name = trim(&header[0..100]);
        let prefix = trim(&header[345..500]);
        if !prefix.is_empty() {
            let mut full = Vec::with_capacity(prefix.len() + 1 + name.len());
            full.extend_from_slice(prefix);
            full.push(b'/');
            full.extend_from_slice(name);
            full
        } else {
            name.to_vec()
        }
    }

    /// A header's size field: octal digits, or big-endian binary when its first bit is set.
    fn size(field: &[u8]) -> u64 {
        if field[0] & 0x80 != 0 {
            field[1..]
                .iter()
                .fold(u64::from(field[0] & 0x7f), |acc, byte| {
                    acc.wrapping_shl(8) | u64::from(*byte)
                })
        } else {
            field
                .iter()
                .skip_while(|byte| **byte == b' ')
                .take_while(|byte| (b'0'..=b'7').contains(*byte))
                .fold(0u64, |acc, byte| {
                    acc.wrapping_mul(8) + u64::from(byte - b'0')
                })
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    fn local(path: &str) -> Endpoint {
        Endpoint::Local(path.to_owned())
    }

    fn remote(machine: &str, path: &str) -> Endpoint {
        Endpoint::Remote {
            machine: machine.to_owned(),
            path: path.to_owned(),
        }
    }

    #[test]
    fn endpoints_tell_machines_from_paths_and_drives() {
        assert_eq!(parse_endpoint("/home/me/a").unwrap(), local("/home/me/a"));
        assert_eq!(parse_endpoint("~/notes.txt").unwrap(), local("~/notes.txt"));
        assert_eq!(
            parse_endpoint(r"C:\Users\me\a").unwrap(),
            local(r"C:\Users\me\a")
        );
        assert_eq!(
            parse_endpoint("C:/Users/me/a").unwrap(),
            local("C:/Users/me/a")
        );
        assert_eq!(
            parse_endpoint(r"\\server\share\a").unwrap(),
            local(r"\\server\share\a")
        );
        assert_eq!(
            parse_endpoint("/home/me/a:b").unwrap(),
            local("/home/me/a:b")
        );
        assert_eq!(
            parse_endpoint("relative/a:b").unwrap(),
            local("relative/a:b")
        );
        assert_eq!(
            parse_endpoint("pi:/home/me/a").unwrap(),
            remote("pi", "/home/me/a")
        );
        assert_eq!(parse_endpoint("tower:~/a").unwrap(), remote("tower", "~/a"));
        assert_eq!(
            parse_endpoint(r"laptop:C:\Users\me\a").unwrap(),
            remote("laptop", r"C:\Users\me\a")
        );
        assert_eq!(parse_endpoint("pi:a:b").unwrap(), remote("pi", "a:b"));
    }

    #[test]
    fn endpoints_refuse_what_cannot_be_told_apart() {
        for (text, code) in [
            ("", ErrorCode::InvalidArgs),
            ("c:path", ErrorCode::InvalidArgs),
            ("C:", ErrorCode::InvalidArgs),
            (":/x", ErrorCode::InvalidArgs),
            ("pi:", ErrorCode::InvalidArgs),
            ("Tower:/x", ErrorCode::InvalidArgs),
            ("all:/x", ErrorCode::InvalidArgs),
            ("my box:/x", ErrorCode::InvalidArgs),
        ] {
            let error = parse_endpoint(text).unwrap_err();
            assert_eq!(error.code, code, "{text:?}: {error:?}");
        }
        let hint = parse_endpoint("x:data").unwrap_err().hint.unwrap();
        assert!(hint.contains("drive"), "{hint}");
    }

    #[test]
    fn names_join_with_the_separator_the_directory_uses() {
        assert_eq!(file_name("/home/me/report.pdf"), "report.pdf");
        assert_eq!(file_name(r"C:\Users\me\report.pdf"), "report.pdf");
        assert_eq!(file_name("~/inbox/"), "inbox");
        assert_eq!(join("/home/me/inbox", "a.txt"), "/home/me/inbox/a.txt");
        assert_eq!(join("/home/me/inbox/", "a.txt"), "/home/me/inbox/a.txt");
        assert_eq!(join(r"C:\Users\me", "a.txt"), r"C:\Users\me\a.txt");
        assert_eq!(join("~", "a.txt"), "~/a.txt");
    }

    /// A listing's usual bounds, with at most `entries` entries.
    fn bounded(entries: usize) -> Bounds {
        Bounds {
            entries,
            bytes: MOST_LISTING_BYTES,
            line: MOST_LISTING_LINE,
        }
    }

    /// The line a node opens every stream with.
    const HELLO: &[u8] = b"{\"protocol\":1}\n";

    #[tokio::test(start_paused = true)]
    async fn a_node_that_does_not_say_it_speaks_this_protocol_is_refused() {
        for (said, says) in [
            // Older: it answers straight away, ignoring what it does not know.
            ("{\"ready\":true}\n", "without saying"),
            (
                "{\"error\":{\"code\":\"denied\",\"message\":\"No.\"}}\n",
                "it said: No.",
            ),
            // Newer.
            (
                "{\"protocol\":2}\n{\"ready\":true}\n",
                "speaks stream protocol 2",
            ),
        ] {
            let (mut conn, mut out, _in) = conn("old");
            out.write_all(said.as_bytes()).await.unwrap();
            let error = conn.first::<UnpackReady>().await.unwrap_err();
            assert_eq!(error.code, ErrorCode::Unsupported, "{said}: {error:?}");
            assert!(error.message.contains(says), "{said}: {error:?}");
            assert!(error.hint.unwrap().contains("Update legion on old"));
        }
        // A banner before the protocol line is still passed over.
        let (mut conn, mut out, _in) = conn("pi");
        out.write_all(b"Welcome!\n{\"motd\":\"hi\"}\n{\"protocol\":1}\n{\"ready\":true}\n")
            .await
            .unwrap();
        assert!(conn.first::<UnpackReady>().await.unwrap().ready);
    }

    #[tokio::test(start_paused = true)]
    async fn busy_lines_put_a_reply_off_only_so_long() {
        let (mut conn, mut out, _in) = conn("pi");
        tokio::spawn(async move {
            out.write_all(HELLO).await.unwrap();
            loop {
                tokio::time::sleep(stream::HEARTBEAT).await;
                if out.write_all(b"{\"busy\":true}\n").await.is_err() {
                    break;
                }
            }
        });
        let started = tokio::time::Instant::now();
        let error = conn.first::<StatReply>().await.unwrap_err();
        assert_eq!(error.code, ErrorCode::Timeout, "{error:?}");
        assert!(error.message.contains("busy"), "{error:?}");
        assert!(started.elapsed() > MOST_BUSY);
        assert!(started.elapsed() < MOST_BUSY + stream::HEARTBEAT * 2);
    }

    /// A pipe end that nobody writes to until a test says so, standing for the other node.
    fn conn(machine: &str) -> (Conn, tokio::io::DuplexStream, tokio::io::DuplexStream) {
        let (reader, peer_out) = tokio::io::duplex(CHUNK);
        let (writer, peer_in) = tokio::io::duplex(CHUNK);
        let conn = Conn {
            machine: machine.to_owned(),
            reader: BufReader::with_capacity(CHUNK, Box::new(reader)),
            writer: Some(Box::new(writer)),
            process: Owned {
                process: Some(Process::Local(None)),
                ends: Ends::default(),
            },
            answered: false,
            noise_lines: 0,
            noise_bytes: 0,
            greeted: false,
            longest: LONGEST_REPLY,
            received: 0,
            opened: tokio::time::Instant::now(),
        };
        (conn, peer_out, peer_in)
    }

    #[tokio::test(start_paused = true)]
    async fn an_end_that_never_answers_times_out_naming_it() {
        let (mut conn, _out, _in) = conn("pi");
        let started = tokio::time::Instant::now();
        let error = conn.first::<StatReply>().await.unwrap_err();
        assert_eq!(error.code, ErrorCode::Timeout, "{error:?}");
        assert_eq!(error.machine.as_deref(), Some("pi"));
        assert!(error.hint.unwrap().contains("older legion"));
        assert_eq!(started.elapsed(), FIRST_REPLY_TIMEOUT);
    }

    #[tokio::test(start_paused = true)]
    async fn an_end_that_says_it_is_busy_is_waited_for() {
        let (mut conn, mut out, _in) = conn("pi");
        let talker = tokio::spawn(async move {
            out.write_all(HELLO).await.unwrap();
            for _ in 0..30 {
                tokio::time::sleep(stream::HEARTBEAT).await;
                out.write_all(b"{\"busy\":true}\n").await.unwrap();
            }
            out.write_all(b"{\"kind\":\"file\",\"size\":3}\n")
                .await
                .unwrap();
            out
        });
        let reply = conn.first::<StatReply>().await.unwrap();
        assert_eq!((reply.kind.as_str(), reply.size), ("file", 3));
        // After the first reply, a silence longer than the idle limit is a stall.
        let _out = talker.await.unwrap();
        let error = conn.first::<StatReply>().await.unwrap_err();
        assert_eq!(error.code, ErrorCode::Timeout, "{error:?}");
        assert!(error.message.contains("middle of a stream"), "{error:?}");
    }

    #[tokio::test(start_paused = true)]
    async fn bytes_that_are_not_text_before_a_reply_are_another_protocol() {
        let (mut conn, mut out, _in) = conn("old");
        out.write_all(b"Welcome!\nreadme.txt\0\0\0\0\0\x000000644\0")
            .await
            .unwrap();
        let error = conn.first::<PackReady>().await.unwrap_err();
        assert_eq!(error.code, ErrorCode::Unsupported, "{error:?}");
        assert!(error.hint.unwrap().contains("Update legion on old"));
    }

    #[tokio::test(start_paused = true)]
    async fn a_relay_that_stops_moving_times_out_naming_the_end_that_stalled() {
        let sent = Sent::default();
        let (mut source, mut source_out, _source_in) = conn("tower");
        let (mut dest, _dest_out, mut dest_in) = conn("pi");
        source_out.write_all(&[7u8; 100]).await.unwrap();
        let drain = tokio::spawn(async move {
            let mut taken = Vec::new();
            let _ = dest_in.read_to_end(&mut taken).await;
            taken.len()
        });
        let mut meter = Meter {
            ctx: None,
            sent: &sent,
            bytes_total: 1000,
            bytes_base: 0,
            files_total: 1,
            current: None,
            tar: None,
            started: Instant::now(),
            last: Instant::now(),
            last_bytes_done: 0,
            last_files_done: 0,
        };
        let started = tokio::time::Instant::now();
        let broken = relay(&mut source, &mut dest, Amount::Exactly(1000), &mut meter)
            .await
            .err()
            .unwrap();
        assert_eq!(started.elapsed(), IDLE_TIMEOUT);
        let error = broken.explain(source, dest).await;
        assert_eq!(error.code, ErrorCode::Timeout, "{error:?}");
        assert_eq!(error.machine.as_deref(), Some("tower"));
        assert_eq!(drain.await.unwrap(), 100);
        drop(source_out);
    }

    #[tokio::test(start_paused = true)]
    async fn a_listing_is_bounded_and_must_end_with_its_count() {
        let entry = |path: &str| {
            format!(
                "{{\"path\":\"{path}\",\"kind\":\"file\",\"size\":1,\"mtime\":{{\"secs\":1,\"nanos\":0}}}}\n"
            )
        };
        let listing = |count: usize, done: Option<usize>| {
            let mut text = String::from_utf8_lossy(HELLO).into_owned();
            text.extend((0..count).map(|at| entry(&format!("f{at}"))));
            if let Some(said) = done {
                text.push_str(&format!("{{\"done\":true,\"entries\":{said}}}\n"));
            }
            text
        };
        for (text, most, code, says) in [
            (
                listing(4, Some(4)),
                3,
                ErrorCode::Failed,
                "more than 3 entries",
            ),
            (
                listing(2, Some(3)),
                10,
                ErrorCode::Failed,
                "said it listed 3",
            ),
            (listing(2, None), 10, ErrorCode::Failed, "ended"),
        ] {
            let (mut conn, mut out, _in) = conn("pi");
            out.write_all(text.as_bytes()).await.unwrap();
            drop(out);
            let error = conn.manifest_within(bounded(most)).await.unwrap_err();
            assert_eq!(error.code, code, "{error:?}");
            assert!(error.message.contains(says), "{error:?}");
            assert_eq!(error.machine.as_deref(), Some("pi"));
        }
        let (mut conn, mut out, _in) = conn("pi");
        out.write_all(listing(3, Some(3)).as_bytes()).await.unwrap();
        assert_eq!(
            conn.manifest_within(bounded(3))
                .await
                .unwrap()
                .entries
                .len(),
            3
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_listing_line_is_bounded_in_bytes() {
        // A link whose target alone is far longer than any path a file system takes.
        let target = "t".repeat(2 * 1024 * 1024);
        let line = format!(
            "{{\"path\":\"l\",\"kind\":\"symlink\",\"size\":0,\"mtime\":{{\"secs\":1,\"nanos\":0}},\"link_target\":\"{target}\"}}\n"
        );
        let (mut listing, mut out, _in) = conn("pi");
        let writer = tokio::spawn(async move {
            out.write_all(HELLO).await.unwrap();
            let _ = out.write_all(line.as_bytes()).await;
            let _ = out.write_all(b"{\"done\":true,\"entries\":1}\n").await;
            out
        });
        let error = listing.manifest_within(bounded(10)).await.unwrap_err();
        drop(writer);
        assert_eq!(error.code, ErrorCode::Failed, "{error:?}");
        assert_eq!(error.machine.as_deref(), Some("pi"));
        assert!(error.message.contains("longer than"), "{error:?}");

        // Many ordinary lines may not add up to more than a listing may take in all.
        let (mut listing, mut out, _in) = conn("pi");
        let writer = tokio::spawn(async move {
            out.write_all(HELLO).await.unwrap();
            for at in 0..50 {
                let line = format!(
                    "{{\"path\":\"f{at}\",\"kind\":\"file\",\"size\":1,\"mtime\":{{\"secs\":1,\"nanos\":0}}}}\n"
                );
                if out.write_all(line.as_bytes()).await.is_err() {
                    break;
                }
            }
            out
        });
        let error = listing
            .manifest_within(Bounds {
                entries: 1000,
                bytes: 1000,
                line: MOST_LISTING_LINE,
            })
            .await
            .unwrap_err();
        drop(writer);
        assert_eq!(error.code, ErrorCode::Failed, "{error:?}");
        assert!(error.hint.unwrap().contains("subdirectories"));
    }

    #[tokio::test(start_paused = true)]
    async fn a_long_first_reply_is_read_whole_while_noise_stays_bounded() {
        // A listing whose only line names 800 entries it could not read: far longer than any
        // banner, and a reply all the same.
        let skipped: Vec<Value> = (0..800)
            .map(|at| json!({"path": format!("locked/directory-{at:04}"), "reason": "Permission denied (os error 13), which is the whole of it"}))
            .collect();
        let done = json!({"done": true, "entries": 0, "skipped": skipped}).to_string();
        assert!(done.len() > MOST_NOISE_BYTES);
        let (mut listing, mut out, _in) = conn("pi");
        // More than the pipe holds, so it is written while the reply is read.
        let writer = tokio::spawn(async move {
            out.write_all(HELLO).await.unwrap();
            out.write_all(format!("{done}\n").as_bytes()).await.unwrap();
            out
        });
        let listed = listing.manifest_within(bounded(10)).await.unwrap();
        assert_eq!(listed.skipped.len(), 800);
        drop(writer);

        // Text that is not JSON is still held to what a banner may take.
        let (mut noisy, mut out, _in) = conn("pi");
        let banner = "x".repeat(MOST_NOISE_BYTES + 10);
        let writer = tokio::spawn(async move {
            let _ = out.write_all(format!("{banner}\n").as_bytes()).await;
            out
        });
        let error = noisy.first::<StatReply>().await.unwrap_err();
        drop(writer);
        assert_eq!(error.code, ErrorCode::Unsupported, "{error:?}");
    }

    #[test]
    fn only_what_is_replaced_by_another_kind_makes_room() {
        let sending: HashSet<&str> = ["x", "new.txt"].into_iter().collect();
        assert!(makes_room("x", &sending));
        assert!(makes_room("x/inside/deep.txt", &sending));
        assert!(!makes_room("extra.txt", &sending));
        assert!(!makes_room("xy/z", &sending));
    }

    #[test]
    fn batches_split_by_count_and_size() {
        let names: Vec<String> = (0..(BATCH_NAMES * 2 + 5))
            .map(|i| format!("f{i}"))
            .collect();
        let split = batches(&names);
        assert_eq!(split.len(), 3);
        assert_eq!(split.iter().map(|b| b.len()).sum::<usize>(), names.len());
        let long: Vec<String> = (0..3).map(|_| "x".repeat(BATCH_BYTES - 10)).collect();
        assert_eq!(batches(&long).len(), 3);
        assert!(batches(&[]).is_empty());
    }

    #[test]
    fn tar_counter_counts_file_content_and_files_across_any_chunking() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join("sub/a.txt"), vec![7u8; 1000]).unwrap();
        std::fs::write(dir.path().join("empty"), b"").unwrap();
        let long = "n".repeat(150);
        std::fs::write(dir.path().join(&long), vec![1u8; 70_000]).unwrap();
        let mut archive = Vec::new();
        legion_xfer::pack(
            dir.path(),
            &["sub", "sub/a.txt", "empty", long.as_str()],
            &mut archive,
        )
        .unwrap();
        for chunk in [1, 7, 511, 512, 513, 65_536, archive.len()] {
            let mut counter = TarCounter::default();
            let (mut content, mut files) = (0, 0);
            for piece in archive.chunks(chunk) {
                let (c, f) = counter.feed(piece);
                content += c;
                files += f;
            }
            assert_eq!((content, files), (71_000, 3), "chunk {chunk}");
        }
    }
}
