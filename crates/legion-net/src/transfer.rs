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

use std::collections::{HashMap, HashSet, VecDeque};
use std::ffi::{OsStr, OsString};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use legion_core::policy::Policy;
use legion_core::{
    Access, BoxFuture, Call, CancellationToken, Ctx, Door, EventSink, Node, Paths, Registry, Tool,
    ToolError, input_schema, parse_args,
};
use legion_os::atomic::{FileIdentity, WriteOptions, write_atomic};
use legion_os::detach::{DetachOptions, spawn_detached};
use legion_os::shell::CommandSpec;
use legion_proto::{Effect, ErrorBody, ErrorCode, EventKind, ToolSpec};
use legion_tools::job::{self as transfer_job, JobProgress, TransferJobSpec};
use legion_xfer::{
    CaseCollision, Entry, EntryKind, Hash, IfExists, Manifest, Plan, PlanConflict, PlanOptions,
    RenamedEntry, SkippedEntry, Timestamp,
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
    self, CAP_IF_EXISTS, CAP_LOCK, CAP_MOVE, CAP_PROBE, CAP_RENAME, DeleteDone, FsNameBehavior,
    GetReady, Host, LockEntry, LockReady, ManifestDone, PackReady, Planned, ProbeReply, PutDone,
    PutReady, RenameDone, SameEntryDecision, StatReply, UnpackDone, UnpackReady,
    classify_same_entry,
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
            description: "Copy or move a file or a directory tree between machines, or within one. Each of from and to is machine:/path or a path on this node; to names the destination itself, except that a file sent to an existing directory (or to a path ending in /) lands inside it under its own name. A file resumes where an interrupted transfer stopped and is renamed into place only once its BLAKE3 hash matches. A tree sends only what differs (size and modification time, or hash with checksum), so a second run sends nothing; if_exists chooses fail, skip, replace (default) or rename (name (2).ext); move removes each source file only after its destination copy is verified by hash (refused up front unless the caller may fs.remove the source); background runs the transfer as a detached job on this node under the caller's principal and grant, returning {job: {id, machine}} immediately (track with job.list, job.status, job.wait, and job.cancel); delete removes what the source no longer has, exclude leaves paths out at both ends, and dry_run reports what would be sent, deleted and removed without changing anything.".to_owned(),
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
            if args.background {
                return start_background(ctx, args).await;
            }
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

impl IfExistsArg {
    fn as_str(self) -> &'static str {
        match self {
            IfExistsArg::Replace => "replace",
            IfExistsArg::Fail => "fail",
            IfExistsArg::Skip => "skip",
            IfExistsArg::Rename => "rename",
        }
    }
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
    /// Run the transfer as a detached job on this node and return `{job: {id, machine}}` at once.
    /// The job runs under the caller's principal and grant, survives the caller disconnecting, and
    /// finishes with the same result object a foreground transfer returns.
    #[serde(default)]
    pub(crate) background: bool,
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

/// Hidden CLI subcommand invoked by `fs.transfer` (`background: true`) to supervise a detached
/// transfer job.
const TRANSFER_SUPERVISE_SUBCOMMAND: &str = "transfer-supervise";
const STATE_DIR_FLAG: &str = "--state-dir";
const CONFIG_DIR_FLAG: &str = "--config-dir";
const HOME_DIR_FLAG: &str = "--home";
const TRANSFER_REQUEST_FILE: &str = "transfer.json";
const TRANSFER_CANCEL_POLL: Duration = Duration::from_millis(100);

#[derive(Clone, Debug, Serialize, Deserialize)]
struct TransferJobRequest {
    args: TransferArgs,
    principal: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    device: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pairing_id: Option<String>,
    door: Door,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    credential_expires: Option<String>,
    to_machine: String,
}

fn initial_transfer_title(from: &End, to: &End, move_source: bool) -> String {
    let verb = if move_source { "Move" } else { "Copy" };
    let item = file_name(&from.path);
    let label = if item.is_empty() { &from.path } else { item };
    format!("{verb} {label} to {}", to.machine)
}

fn initial_transfer_name(from: &End, move_source: bool) -> String {
    let prefix = if move_source { "move" } else { "copy" };
    let item = file_name(&from.path);
    if item.is_empty() {
        prefix.to_owned()
    } else {
        let raw = format!("{prefix}-{item}");
        match raw.char_indices().nth(60) {
            Some((end, _)) => format!("{}…", &raw[..end]),
            None => raw,
        }
    }
}

async fn start_background(ctx: &Ctx, mut args: TransferArgs) -> Result<Value, ErrorBody> {
    let from = resolve(ctx, &args.from)?;
    let to = resolve(ctx, &args.to)?;

    // Enforce local policy checks up front so obvious denials fail before spawning a job; the
    // detached supervisor also runs through `Node::call` under `pinned: Some(principal)` so every
    // local and remote check runs under the caller's exact grant.
    if !from.remote {
        ctx.grant().check_path(&from.path, Access::Read)?;
    }
    if !to.remote {
        ctx.grant().check_path(&to.path, Access::Write)?;
    }
    if args.move_source {
        let policy = ctx.config().get::<Policy>()?;
        policy
            .authorize_name(
                ctx.principal(),
                "fs.remove",
                Some(Effect::Destroy),
                ctx.paths(),
            )
            .map_err(|err| err.with_machine(ctx.node_name()))?;
        if !from.remote {
            ctx.grant().check_path(&from.path, Access::Write)?;
        }
    }

    let title = initial_transfer_title(&from, &to, args.move_source);
    let name = initial_transfer_name(&from, args.move_source);
    let state_dir = ctx.paths().state_dir().to_path_buf();
    let config_dir = ctx.paths().config_dir().to_path_buf();
    let home_dir = ctx.paths().home().map(Path::to_path_buf);

    let mut local_paths = Vec::new();
    if !from.remote {
        local_paths.push(from.path.clone());
    }
    if !to.remote {
        local_paths.push(to.path.clone());
    }
    let created = transfer_job::create_transfer_job(
        &state_dir,
        TransferJobSpec {
            name,
            title: title.clone(),
            principal: ctx.principal().to_owned(),
            device: ctx.device().map(str::to_owned),
            pairing_id: ctx.pairing_id().map(str::to_owned),
            transfer_paths: local_paths,
            transfer_has_remote: from.remote || to.remote,
            transfer_local_from: (!from.remote).then(|| from.path.clone()),
            transfer_local_to: (!to.remote).then(|| to.path.clone()),
            transfer_move: args.move_source,
            exclusive: false,
        },
    )
    .await?;

    args.background = false;
    let request = TransferJobRequest {
        args,
        principal: ctx.principal().to_owned(),
        device: ctx.device().map(str::to_owned),
        pairing_id: ctx.pairing_id().map(str::to_owned),
        door: ctx.door(),
        credential_expires: ctx
            .credential_expires()
            .map(|ts| ts.strftime("%Y-%m-%dT%H:%M:%S%.3fZ").to_string()),
        to_machine: to.machine.clone(),
    };
    let request_path = created.dir.join(TRANSFER_REQUEST_FILE);
    let req_bytes = serde_json::to_vec_pretty(&request).map_err(|err| {
        ErrorBody::new(
            ErrorCode::Internal,
            format!("Cannot serialize background transfer request: {err}."),
        )
    })?;
    let initial_progress = JobProgress {
        bytes_done: 0,
        bytes_total: None,
        files_done: 0,
        files_total: None,
        current: None,
        rate: 0,
        attempt: None,
    };
    let job_dir_for_write = created.dir.clone();
    if let Err(err) = tokio::task::spawn_blocking(move || {
        write_atomic(&request_path, &req_bytes, WriteOptions::with_mode(0o600)).map_err(|e| {
            ErrorBody::new(
                ErrorCode::Failed,
                format!(
                    "Cannot write transfer request {}: {e}.",
                    request_path.display()
                ),
            )
        })?;
        transfer_job::write_transfer_progress(&job_dir_for_write, &initial_progress, None)
    })
    .await
    .map_err(|err| {
        ErrorBody::new(
            ErrorCode::Internal,
            format!("fs.transfer stopped unexpectedly while preparing job files: {err}."),
        )
    })? {
        transfer_job::record_transfer_start_failure(&state_dir, &created.id, &err.message).await;
        return Err(err);
    }

    let exe = match legion_core::current_exe() {
        Ok(exe) => exe,
        Err(err) => {
            transfer_job::record_transfer_start_failure(&state_dir, &created.id, &err.message)
                .await;
            return Err(err);
        }
    };

    let mut argv = vec![
        TRANSFER_SUPERVISE_SUBCOMMAND.to_owned(),
        created.id.clone(),
        STATE_DIR_FLAG.to_owned(),
        state_dir.to_string_lossy().into_owned(),
        CONFIG_DIR_FLAG.to_owned(),
        config_dir.to_string_lossy().into_owned(),
    ];
    if let Some(ref home) = home_dir {
        argv.push(HOME_DIR_FLAG.to_owned());
        argv.push(home.to_string_lossy().into_owned());
    }
    let supervisor_cmd = CommandSpec::argv(exe, argv);
    let handle = match spawn_detached(DetachOptions::new(supervisor_cmd).name(&created.id)).await {
        Ok(handle) => handle,
        Err(err) => {
            let message = format!("Could not launch the detached transfer supervisor: {err}.");
            transfer_job::record_transfer_start_failure(&state_dir, &created.id, &message).await;
            return Err(ErrorBody::new(ErrorCode::Failed, message)
                .with_hint("Check `job.status` for the recorded failure details."));
        }
    };

    transfer_job::attach_transfer_detach(&state_dir, &created.id, handle.clone()).await?;
    let _state =
        transfer_job::wait_transfer_supervisor_ready(&state_dir, &created.id, &handle).await?;

    Ok(json!({
        "job": {
            "id": created.id,
            "machine": ctx.node_name(),
        }
    }))
}

/// Runs the hidden `transfer-supervise` subcommand when `args` starts with `"transfer-supervise"`.
///
/// Returns `None` for any other command line so callers (such as `legion-cli`'s `main`) can
/// dispatch normally without adding the subcommand to user-facing help.
pub fn try_supervise_cli(args: impl IntoIterator<Item = OsString>) -> Option<ExitCode> {
    let mut iter = args.into_iter();
    let first = iter.next()?;
    if first != OsStr::new(TRANSFER_SUPERVISE_SUBCOMMAND) {
        return None;
    }
    let rest: Vec<OsString> = iter.collect();
    Some(run_transfer_supervise_cli(&rest))
}

struct SuperviseArgs {
    id: String,
    state_dir: Option<PathBuf>,
    config_dir: Option<PathBuf>,
    home_dir: Option<PathBuf>,
}

fn parse_supervise_args(rest: &[OsString]) -> Result<SuperviseArgs, String> {
    let mut id: Option<String> = None;
    let mut state_dir: Option<PathBuf> = None;
    let mut config_dir: Option<PathBuf> = None;
    let mut home_dir: Option<PathBuf> = None;
    let mut index = 0usize;

    while index < rest.len() {
        let arg = &rest[index];
        if arg == OsStr::new(STATE_DIR_FLAG) {
            let value = rest
                .get(index + 1)
                .ok_or_else(|| format!("{STATE_DIR_FLAG} needs a path."))?;
            if state_dir.replace(PathBuf::from(value)).is_some() {
                return Err(format!("{STATE_DIR_FLAG} was given twice."));
            }
            index += 2;
            continue;
        }
        if arg == OsStr::new(CONFIG_DIR_FLAG) {
            let value = rest
                .get(index + 1)
                .ok_or_else(|| format!("{CONFIG_DIR_FLAG} needs a path."))?;
            if config_dir.replace(PathBuf::from(value)).is_some() {
                return Err(format!("{CONFIG_DIR_FLAG} was given twice."));
            }
            index += 2;
            continue;
        }
        if arg == OsStr::new(HOME_DIR_FLAG) {
            let value = rest
                .get(index + 1)
                .ok_or_else(|| format!("{HOME_DIR_FLAG} needs a path."))?;
            if home_dir.replace(PathBuf::from(value)).is_some() {
                return Err(format!("{HOME_DIR_FLAG} was given twice."));
            }
            index += 2;
            continue;
        }
        let text = arg
            .to_str()
            .ok_or_else(|| "job id must be valid Unicode.".to_owned())?;
        if id.replace(text.to_owned()).is_some() {
            return Err(format!("unexpected argument {text:?}."));
        }
        index += 1;
    }

    let id = id.ok_or_else(|| "missing job id.".to_owned())?;
    Ok(SuperviseArgs {
        id,
        state_dir,
        config_dir,
        home_dir,
    })
}

fn run_transfer_supervise_cli(rest: &[OsString]) -> ExitCode {
    let parsed = match parse_supervise_args(rest) {
        Ok(parsed) => parsed,
        Err(message) => {
            let _ = writeln!(
                std::io::stderr(),
                "legion {TRANSFER_SUPERVISE_SUBCOMMAND}: {message}"
            );
            return ExitCode::from(2);
        }
    };
    if let Err(error) = transfer_job::validate_transfer_job_id(&parsed.id) {
        let _ = writeln!(
            std::io::stderr(),
            "legion {TRANSFER_SUPERVISE_SUBCOMMAND}: {}",
            error.message
        );
        return ExitCode::from(2);
    }
    let paths = match (parsed.config_dir, parsed.state_dir) {
        (Some(cfg), Some(st)) => match Paths::from_explicit_dirs(cfg, st) {
            Ok(p) => match parsed.home_dir {
                Some(home) => p.with_home(home),
                None => p,
            },
            Err(error) => {
                let _ = writeln!(
                    std::io::stderr(),
                    "legion {TRANSFER_SUPERVISE_SUBCOMMAND}: {}",
                    error.message
                );
                return ExitCode::from(1);
            }
        },
        _ => match Paths::from_env() {
            Ok(p) => match parsed.home_dir {
                Some(home) => p.with_home(home),
                None => p,
            },
            Err(error) => {
                let _ = writeln!(
                    std::io::stderr(),
                    "legion {TRANSFER_SUPERVISE_SUBCOMMAND}: {}",
                    error.message
                );
                return ExitCode::from(1);
            }
        },
    };

    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            let _ = writeln!(
                std::io::stderr(),
                "legion {TRANSFER_SUPERVISE_SUBCOMMAND}: failed to start async runtime: {error}"
            );
            return ExitCode::from(1);
        }
    };

    match runtime.block_on(supervise_transfer(paths, &parsed.id)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            let _ = writeln!(
                std::io::stderr(),
                "legion {TRANSFER_SUPERVISE_SUBCOMMAND}: {message}"
            );
            ExitCode::from(1)
        }
    }
}

async fn supervise_transfer(paths: Paths, id: &str) -> Result<(), String> {
    let dir = transfer_job::transfer_job_dir(paths.state_dir(), id).map_err(|err| err.message)?;
    let Some(lock) = transfer_job::acquire_transfer_supervisor_lock(&dir).await? else {
        return Ok(());
    };

    if transfer_job::transfer_exit_exists(&dir) {
        let _ = std::fs::remove_file(dir.join(TRANSFER_REQUEST_FILE));
        transfer_job::remove_transfer_exit_reserve(&dir);
        let _ = lock.unlock();
        return Ok(());
    }

    if transfer_job::transfer_cancel_requested(&dir) {
        let _ = std::fs::remove_file(dir.join(TRANSFER_REQUEST_FILE));
        let _ = transfer_job::finish_transfer_job_cancelled(
            &dir,
            "Cancelled before the transfer started.",
        );
        let _ = lock.unlock();
        return Ok(());
    }

    transfer_job::prepare_transfer_exit_reserve(&dir);

    let req_path = dir.join(TRANSFER_REQUEST_FILE);
    let req_bytes = match std::fs::read(&req_path) {
        Ok(bytes) => {
            let _ = std::fs::remove_file(&req_path);
            bytes
        }
        Err(err) => {
            let _ = transfer_job::finish_transfer_job_err(
                &dir,
                ErrorBody::new(
                    ErrorCode::Failed,
                    format!(
                        "Cannot read transfer request {}: {err}.",
                        req_path.display()
                    ),
                ),
            );
            let _ = lock.unlock();
            return Ok(());
        }
    };
    let req: TransferJobRequest = match serde_json::from_slice(&req_bytes) {
        Ok(req) => req,
        Err(err) => {
            let _ = transfer_job::finish_transfer_job_err(
                &dir,
                ErrorBody::new(
                    ErrorCode::Failed,
                    format!(
                        "Transfer request {} is not valid JSON: {err}.",
                        req_path.display()
                    ),
                ),
            );
            let _ = lock.unlock();
            return Ok(());
        }
    };

    transfer_job::append_transfer_log(
        &dir,
        &format!(
            "{} {} -> {} (principal: {})",
            if req.args.move_source { "move" } else { "copy" },
            req.args.from,
            req.args.to,
            req.principal
        ),
    );

    let fleet = Fleet::new(&paths);
    let mut registry = legion_tools::registry();
    crate::register(&mut registry, fleet.clone());
    let paths_for_watcher = paths.clone();
    let id_for_watcher = id.to_owned();
    let node = Node::new(paths, registry);

    let cancel_token = CancellationToken::new();
    let watcher_stop = CancellationToken::new();
    let revoked_flag = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let dir_for_watcher = dir.clone();
    let watcher = tokio::spawn({
        let cancel_token = cancel_token.clone();
        let watcher_stop = watcher_stop.clone();
        let revoked_flag = Arc::clone(&revoked_flag);
        async move {
            let mut ticks: u32 = 0;
            loop {
                if transfer_job::transfer_cancel_requested(&dir_for_watcher) {
                    cancel_token.cancel();
                    break;
                }
                if ticks.is_multiple_of(5)
                    && !transfer_job::is_transfer_job_authorized(
                        &paths_for_watcher,
                        &id_for_watcher,
                    )
                {
                    revoked_flag.store(true, Ordering::Relaxed);
                    cancel_token.cancel();
                    break;
                }
                ticks = ticks.wrapping_add(1);
                tokio::select! {
                    biased;
                    () = watcher_stop.cancelled() => break,
                    () = tokio::time::sleep(TRANSFER_CANCEL_POLL) => {}
                }
            }
        }
    });

    struct MonotonicProgress {
        bytes_done: u64,
        files_done: u64,
        attempt: u32,
        title: Option<String>,
    }

    let state = Arc::new(Mutex::new(MonotonicProgress {
        bytes_done: 0,
        files_done: 0,
        attempt: 1,
        title: None,
    }));
    let dir_for_events = dir.clone();
    let move_source = req.args.move_source;
    let to_machine = req.to_machine.clone();
    let events = EventSink::new(move |kind, data| {
        if kind != EventKind::Progress {
            return;
        }
        let Ok(mut progress) = serde_json::from_value::<JobProgress>(data) else {
            return;
        };
        let title_update = {
            let mut guard = state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let attempt = progress.attempt.unwrap_or(1);
            if attempt > guard.attempt {
                guard.attempt = attempt;
                guard.bytes_done = progress.bytes_done;
            } else {
                progress.bytes_done = progress.bytes_done.max(guard.bytes_done);
                guard.bytes_done = progress.bytes_done;
            }
            progress.files_done = progress.files_done.max(guard.files_done);
            guard.files_done = progress.files_done;
            if guard.title.is_none()
                && let Some(total) = progress.files_total
                && total > 1
            {
                let verb = if move_source { "Move" } else { "Copy" };
                guard.title = Some(format!("{verb} {total} items to {to_machine}"));
            }
            guard.title.clone()
        };
        let _ = transfer_job::write_transfer_progress(&dir_for_events, &progress, title_update);
    });

    let mut foreground_args = req.args;
    foreground_args.background = false;
    let args_value = serde_json::to_value(&foreground_args).unwrap_or(Value::Null);
    let credential_expires = req
        .credential_expires
        .as_deref()
        .and_then(|s| s.parse::<jiff::Timestamp>().ok());

    let call = Call {
        tool: "fs.transfer".to_owned(),
        args: args_value,
        as_principal: None,
        door: req.door,
        // Pin to the caller's principal so the detached job NEVER escalates to `owner` or
        // `policy.default` and always evaluates live `policy.toml` for that exact principal.
        pinned: Some(req.principal),
        device: req.device,
        pairing_id: req.pairing_id,
        cancel: cancel_token.clone(),
        events,
        hops: 0,
        credential_expires,
    };

    let outcome = node.call(call).await;
    fleet.close().await;
    watcher_stop.cancel();
    let _ = watcher.await;

    let write_res = match outcome {
        Ok(result) => {
            transfer_job::append_transfer_log(&dir, "transfer completed");
            transfer_job::finish_transfer_job_ok(&dir, result)
        }
        Err(error) if error.code == ErrorCode::Cancelled || cancel_token.is_cancelled() => {
            let detail = if revoked_flag.load(Ordering::Relaxed) {
                "Cancelled because permissions or device access were revoked.".to_owned()
            } else {
                error.message
            };
            transfer_job::append_transfer_log(&dir, &format!("transfer cancelled: {detail}"));
            transfer_job::finish_transfer_job_cancelled(&dir, &detail)
        }
        Err(error) => {
            transfer_job::append_transfer_log(&dir, &format!("transfer failed: {}", error.message));
            transfer_job::finish_transfer_job_err(&dir, error)
        }
    };
    let _ = lock.unlock();
    write_res.map_err(|err| err.message)
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

/// Result of probing whether two directories (one on `from`, one on `to`) are distinct.
#[derive(Clone, Debug, PartialEq, Eq)]
enum DirPairProof {
    /// Marker created in `to_dir` was confirmed absent in `from_dir`.
    ProvenDistinct,
    /// Marker created in `to_dir` appeared in `from_dir` (same directory, bind mount,
    /// symlinked directory, fleet alias, or shared network mount).
    SameDirectory,
    /// The probe could not conclude; callers must refuse removal by default and keep both copies.
    Inconclusive(String),
}

/// Active exclusive move locks held across one or two endpoints in deterministic `(machine, path)`
/// order, together with the live metadata observed on each path under those locks.
struct HeldMoveLocks {
    conns: Vec<Conn>,
    from_entries: HashMap<String, LockEntry>,
    to_entries: HashMap<String, LockEntry>,
}

impl HeldMoveLocks {
    async fn release(self) {
        for conn in self.conns.into_iter().rev() {
            conn.close().await;
        }
    }
}

/// One candidate file or symlink eligible to be removed from `from` after its destination copy is
/// re-verified under exclusive locks on both source and destination.
struct MoveRemovalCandidate {
    /// Relative path under `from` (empty for single-file root).
    rel: String,
    /// Full path on `from`.
    from_path: String,
    /// Actual full path placed on `to` (accounting for `if_exists=rename`).
    to_path: String,
    kind: EntryKind,
    expected_hash: Option<Hash>,
    expected_size: u64,
    expected_link_target: Option<String>,
    /// Post-commit `(file_id, size, mtime)` recorded when the file was placed; if missing or
    /// mismatched under the held locks, the destination is re-hashed before removing the source.
    recorded_dest: Option<(Option<FileIdentity>, u64, Option<Timestamp>)>,
    /// Whether `from` and `to` share a machine or directory (requires `src_id != dst_id`).
    same_machine_or_dir: bool,
}

/// Maximum number of candidate files locked together in one tree-move verification round (at most
/// 128 source+destination paths per `lock` call, well under the 256-path cap).
const MOVE_LOCK_BATCH: usize = 64;

impl Transfer<'_> {
    async fn run(
        &self,
        from: &End,
        to: &End,
        args: &TransferArgs,
    ) -> Result<TransferReport, ErrorBody> {
        let (source, from_caps): (StatReply, HashSet<String>) = self
            .request_with_caps(
                from,
                json!({"op": "stat", "path": from.path, "checksum": true}),
                &[],
            )
            .await?;
        match source.kind.as_str() {
            "file" => self.file(from, to, &source, &from_caps, args).await,
            "dir" => self.tree(from, to, &source, &from_caps, args).await,
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
            .request_required(
                from,
                json!({
                    "op": "delete",
                    "root": from.path,
                    "names": [],
                    "dry_run": true,
                    "remove_root": true,
                }),
                &[CAP_MOVE],
            )
            .await?;
        Ok(())
    }

    /// Ensures both endpoints advertise the stream capabilities required for `if_exists` and
    /// `move`, refusing older peers before any bytes move.
    fn check_peer_capabilities(
        from: &End,
        from_caps: &HashSet<String>,
        to: &End,
        to_caps: &HashSet<String>,
        if_exists: IfExists,
        args: &TransferArgs,
    ) -> Result<(), ErrorBody> {
        if if_exists != IfExists::Replace && !to_caps.contains(CAP_IF_EXISTS) {
            return Err(missing_peer_capability(
                &to.machine,
                &format!("if_exists={}", args.if_exists.as_str()),
            ));
        }
        if args.move_source {
            if !from_caps.contains(CAP_MOVE)
                || !from_caps.contains(CAP_PROBE)
                || !from_caps.contains(CAP_LOCK)
            {
                return Err(missing_peer_capability(&from.machine, "move"));
            }
            if !to_caps.contains(CAP_MOVE)
                || !to_caps.contains(CAP_PROBE)
                || !to_caps.contains(CAP_LOCK)
            {
                return Err(missing_peer_capability(&to.machine, "move"));
            }
        }
        Ok(())
    }

    /// Resolves the case-sensitivity and Unicode-normalization behavior of the parent directory
    /// holding `from` and `to` (never inspecting inside a directory being renamed). When read-only
    /// `stat` left either property unknown and `same_file` holds on one parent, asks the node to
    /// run `probe_dir_name_behavior` via `probe_write` (`action = "behavior"`) instead of assuming
    /// case-sensitivity.
    #[allow(clippy::too_many_arguments)]
    async fn resolve_parent_fs_behavior(
        &self,
        from: &End,
        source_stat: &StatReply,
        to_path: &str,
        there_stat: &StatReply,
        same_file: bool,
        dry_run: bool,
    ) -> FsNameBehavior {
        // Prefer the parent-directory fields from `stat`, falling back to the legacy fields only
        // for non-directories (where `stat` already inspected the parent) or older peers.
        let mut ci = there_stat
            .parent_case_insensitive
            .or(source_stat.parent_case_insensitive);
        let mut norm = there_stat
            .parent_unicode_normalizing
            .or(source_stat.parent_unicode_normalizing);
        if source_stat.kind != "dir" {
            ci = ci
                .or(there_stat.case_insensitive)
                .or(source_stat.case_insensitive);
            norm = norm
                .or(there_stat.unicode_normalizing)
                .or(source_stat.unicode_normalizing);
        }

        let same_parent = source_stat
            .parent_id
            .zip(there_stat.parent_id)
            .is_some_and(|(a, b)| a.same_file(&b));
        let either_symlink = source_stat.is_symlink || there_stat.is_symlink;
        let from_name = OsStr::new(file_name(&from.path));
        let to_name = OsStr::new(file_name(to_path));
        let needs_probe = (ci.is_none() || norm.is_none())
            && same_file
            && same_parent
            && !either_symlink
            && from_name.as_encoded_bytes() != to_name.as_encoded_bytes();

        if needs_probe {
            if !dry_run {
                let parent = parent_dir_of(&from.path);
                if let Ok(reply) = self
                    .request_required::<ProbeReply>(
                        from,
                        json!({
                            "op": "probe_write",
                            "action": "behavior",
                            "dir": parent,
                        }),
                        &[CAP_PROBE],
                    )
                    .await
                {
                    if let Some(probed_ci) = reply.case_insensitive {
                        ci = Some(probed_ci);
                    }
                    if let Some(probed_norm) = reply.unicode_normalizing {
                        norm = Some(probed_norm);
                    }
                }
            } else if source_stat.kind == "dir"
                || source_stat.file_id.is_some_and(|id| id.nlink == 1)
            {
                // Dry-run must not write probe files. Directories cannot be hardlinked and
                // `nlink == 1` files have no hardlink aliases, so a same-file same-parent entry
                // with a differing name can only be a folding alias.
                let fold_ci = stream::names_fold_equivalent(
                    from_name,
                    to_name,
                    FsNameBehavior {
                        case_insensitive: true,
                        unicode_normalizing: false,
                    },
                );
                let fold_norm = stream::names_fold_equivalent(
                    from_name,
                    to_name,
                    FsNameBehavior {
                        case_insensitive: false,
                        unicode_normalizing: true,
                    },
                );
                let fold_both = stream::names_fold_equivalent(
                    from_name,
                    to_name,
                    FsNameBehavior {
                        case_insensitive: true,
                        unicode_normalizing: true,
                    },
                );
                if ci.is_none() && (fold_ci || (fold_both && !fold_norm)) {
                    ci = Some(true);
                }
                if norm.is_none() && (fold_norm || (fold_both && !fold_ci)) {
                    norm = Some(true);
                }
            }
        }

        FsNameBehavior {
            case_insensitive: ci.unwrap_or(false),
            unicode_normalizing: norm.unwrap_or(false),
        }
    }

    /// Always probes `(from_dir, to_dir)` by creating a unique marker in `to_dir` and checking
    /// whether it appears in `from_dir`, caching conclusive results for the transfer.
    ///
    /// Never treats an error or missing directory as distinct: returns
    /// [`DirPairProof::Inconclusive`] so callers keep both the copy and the source.
    async fn prove_dir_pair(
        &self,
        from: &End,
        from_dir: &str,
        to: &End,
        to_dir: &str,
        cache: &mut HashMap<(String, String), DirPairProof>,
    ) -> DirPairProof {
        let key = (from_dir.to_owned(), to_dir.to_owned());
        if let Some(cached) = cache.get(&key) {
            return cached.clone();
        }
        let created: ProbeReply = match self
            .request_required(
                to,
                json!({
                    "op": "probe_write",
                    "action": "create",
                    "dir": to_dir,
                    "nearest_ancestor": false,
                }),
                &[CAP_PROBE],
            )
            .await
        {
            Ok(reply) => reply,
            Err(err) => {
                return DirPairProof::Inconclusive(format!(
                    "could not probe {} on {}: {}",
                    to_dir, to.machine, err.message
                ));
            }
        };
        let (Some(probe_dir), Some(marker)) = (created.dir, created.marker) else {
            return DirPairProof::Inconclusive(format!(
                "{} on {} could not be probed for directory distinctness",
                to_dir, to.machine
            ));
        };
        let check_res: Result<ProbeReply, ErrorBody> = self
            .request_required(
                from,
                json!({
                    "op": "probe_read",
                    "dir": from_dir,
                    "marker": marker,
                    "subdirs": [],
                }),
                &[CAP_PROBE],
            )
            .await;
        // Always remove the probe marker, even if the read check failed.
        let _ = self
            .request_required::<ProbeReply>(
                to,
                json!({
                    "op": "probe_write",
                    "action": "remove",
                    "dir": probe_dir,
                    "marker": marker,
                }),
                &[CAP_PROBE],
            )
            .await;
        match check_res {
            Ok(reply) => {
                let proof = if reply.exists {
                    DirPairProof::SameDirectory
                } else {
                    DirPairProof::ProvenDistinct
                };
                cache.insert(key, proof.clone());
                proof
            }
            Err(err) => DirPairProof::Inconclusive(format!(
                "could not check directory probe in {} on {}: {}",
                from_dir, from.machine, err.message
            )),
        }
    }

    /// Up-front tree-move probe checking whether `to_dir` (or its nearest existing ancestor when
    /// `to_dir` does not exist yet) aliases `from_dir` or any subdirectory of `from_dir`.
    #[allow(clippy::too_many_arguments)]
    async fn probe_tree_root_upfront(
        &self,
        from: &End,
        from_dir: &str,
        from_subdirs: &[String],
        to: &End,
        to_dir: &str,
        to_exists: bool,
        cache: &mut HashMap<(String, String), DirPairProof>,
    ) -> Result<bool, ErrorBody> {
        let created: ProbeReply = match self
            .request_required(
                to,
                json!({
                    "op": "probe_write",
                    "action": "create",
                    "dir": to_dir,
                    "nearest_ancestor": !to_exists,
                }),
                &[CAP_PROBE],
            )
            .await
        {
            Ok(reply) => reply,
            Err(_) => return Ok(false),
        };
        let (Some(probe_dir), Some(marker)) = (created.dir, created.marker) else {
            return Ok(false);
        };
        let check_res: Result<ProbeReply, ErrorBody> = self
            .request_required(
                from,
                json!({
                    "op": "probe_read",
                    "dir": from_dir,
                    "marker": marker,
                    "subdirs": from_subdirs,
                }),
                &[CAP_PROBE],
            )
            .await;
        let _ = self
            .request_required::<ProbeReply>(
                to,
                json!({
                    "op": "probe_write",
                    "action": "remove",
                    "dir": probe_dir,
                    "marker": marker,
                }),
                &[CAP_PROBE],
            )
            .await;
        match check_res {
            Ok(reply) => {
                if !reply.exists && to_exists {
                    cache.insert(
                        (from_dir.to_owned(), to_dir.to_owned()),
                        DirPairProof::ProvenDistinct,
                    );
                }
                Ok(reply.exists)
            }
            Err(_) => Ok(false),
        }
    }

    /// Acquires exclusive move locks on `from_paths` (on `from`) and `to_paths` (on `to`) in a
    /// fixed global order by `(machine, path)`. Returns a clean retryable `conflict` when busy.
    async fn acquire_move_locks(
        &self,
        from: &End,
        from_paths: &[String],
        to: &End,
        to_paths: &[String],
    ) -> Result<HeldMoveLocks, ErrorBody> {
        if from.machine == to.machine {
            let mut combined = Vec::with_capacity(from_paths.len() + to_paths.len());
            let mut seen = HashSet::new();
            for p in from_paths.iter().chain(to_paths.iter()) {
                if seen.insert(p.clone()) {
                    combined.push(p.clone());
                }
            }
            let mut conn = self
                .open_with_caps(from, json!({"op": "lock", "paths": combined}), &[CAP_LOCK])
                .await?;
            let ready: LockReady = match conn.first().await {
                Ok(ready) => ready,
                Err(err) => {
                    conn.abort().await;
                    return Err(err);
                }
            };
            let mut by_path: HashMap<String, LockEntry> = ready
                .entries
                .into_iter()
                .map(|e| (e.path.clone(), e))
                .collect();
            let mut from_entries = HashMap::with_capacity(from_paths.len());
            let mut to_entries = HashMap::with_capacity(to_paths.len());
            for p in from_paths {
                if let Some(entry) = by_path.get(p) {
                    from_entries.insert(p.clone(), entry.clone());
                }
            }
            for p in to_paths {
                if let Some(entry) = by_path.remove(p) {
                    to_entries.insert(p.clone(), entry);
                }
            }
            return Ok(HeldMoveLocks {
                conns: vec![conn],
                from_entries,
                to_entries,
            });
        }

        // Different machines: always lock the lexicographically smaller machine first so two
        // crossing moves (`a -> b` and `b -> a`) acquire locks in the exact same order.
        let from_first = from.machine < to.machine;
        let (first_end, first_paths, second_end, second_paths) = if from_first {
            (from, from_paths, to, to_paths)
        } else {
            (to, to_paths, from, from_paths)
        };
        let mut conn1 = self
            .open_with_caps(
                first_end,
                json!({"op": "lock", "paths": first_paths}),
                &[CAP_LOCK],
            )
            .await?;
        let ready1: LockReady = match conn1.first().await {
            Ok(ready) => ready,
            Err(err) => {
                conn1.abort().await;
                return Err(err);
            }
        };
        let mut conn2 = match self
            .open_with_caps(
                second_end,
                json!({"op": "lock", "paths": second_paths}),
                &[CAP_LOCK],
            )
            .await
        {
            Ok(conn) => conn,
            Err(err) => {
                conn1.close().await;
                return Err(err);
            }
        };
        let ready2: LockReady = match conn2.first().await {
            Ok(ready) => ready,
            Err(err) => {
                conn2.abort().await;
                conn1.close().await;
                return Err(err);
            }
        };
        let map1: HashMap<String, LockEntry> = ready1
            .entries
            .into_iter()
            .map(|e| (e.path.clone(), e))
            .collect();
        let map2: HashMap<String, LockEntry> = ready2
            .entries
            .into_iter()
            .map(|e| (e.path.clone(), e))
            .collect();
        let (from_entries, to_entries) = if from_first {
            (map1, map2)
        } else {
            (map2, map1)
        };
        Ok(HeldMoveLocks {
            conns: vec![conn1, conn2],
            from_entries,
            to_entries,
        })
    }

    /// Re-verifies one candidate's destination copy while holding exclusive locks on both source
    /// and destination. Returns `Ok(true)` if the source is safe to delete, `Ok(false)` with an
    /// explanatory reason in `Err(reason)` if the destination disappeared, changed, or aliases the
    /// source.
    async fn verify_candidate_under_locks(
        &self,
        to: &End,
        cand: &MoveRemovalCandidate,
        from_entries: &HashMap<String, LockEntry>,
        to_entries: &HashMap<String, LockEntry>,
    ) -> Result<bool, String> {
        let Some(src_entry) = from_entries.get(&cand.from_path) else {
            return Err("source metadata was unavailable under lock".to_owned());
        };
        if !src_entry.exists {
            return Ok(false);
        }
        let Some(dst_entry) = to_entries.get(&cand.to_path) else {
            return Err("destination metadata was unavailable under lock".to_owned());
        };
        if !dst_entry.exists {
            return Err(format!(
                "destination {} on {} disappeared before the source was removed, so the source was kept",
                cand.to_path, to.machine
            ));
        }
        if cand.same_machine_or_dir {
            match (src_entry.file_id, dst_entry.file_id) {
                (Some(src_id), Some(dst_id)) if !src_id.same_file(&dst_id) => {}
                _ => {
                    return Err(
                        "source and destination could not be proven to be distinct files under lock, so the source was kept"
                            .to_owned(),
                    );
                }
            }
        }
        match cand.kind {
            EntryKind::Symlink => {
                if dst_entry.kind != "symlink" || dst_entry.link_target != cand.expected_link_target
                {
                    return Err(format!(
                        "destination symlink {} on {} changed before the source was removed, so the source was kept",
                        cand.to_path, to.machine
                    ));
                }
                Ok(true)
            }
            EntryKind::File => {
                if dst_entry.kind != "file" || dst_entry.size != cand.expected_size {
                    return Err(format!(
                        "destination {} on {} changed before the source was removed, so the source was kept",
                        cand.to_path, to.machine
                    ));
                }
                let fast_match = cand
                    .recorded_dest
                    .is_some_and(|(rec_id, rec_size, rec_mtime)| {
                        rec_id.is_some()
                            && rec_mtime.is_some()
                            && dst_entry.file_id == rec_id
                            && dst_entry.size == rec_size
                            && dst_entry.mtime == rec_mtime
                    });
                if fast_match {
                    return Ok(true);
                }
                // Metadata was not recorded (e.g. `files_unchanged`) or changed since copy:
                // re-hash the destination file right now while holding both locks.
                let mut stat_req = json!({
                    "op": "stat",
                    "path": cand.to_path,
                    "checksum": true,
                    "if_size": cand.expected_size,
                });
                if let Some(h) = cand.expected_hash {
                    stat_req["if_hash"] = json!(h);
                }
                let dst_stat: StatReply = self.request(to, stat_req).await.map_err(|err| {
                    format!(
                        "could not re-verify destination {} on {}: {}",
                        cand.to_path, to.machine, err.message
                    )
                })?;
                if dst_entry.kind != "file"
                    || dst_stat.kind != "file"
                    || dst_stat.size != cand.expected_size
                    || cand.expected_hash.is_none()
                    || dst_stat.hash != cand.expected_hash
                {
                    return Err(format!(
                        "destination {} on {} no longer matches the source hash, so the source was kept",
                        cand.to_path, to.machine
                    ));
                }
                Ok(true)
            }
            EntryKind::Dir => Ok(false),
        }
    }

    /// Acquires exclusive locks on a single-file move's `(from, target)`, re-verifies the
    /// destination under those locks, removes `from` while holding them, and releases them.
    #[allow(clippy::too_many_arguments)]
    async fn lock_verify_and_remove_single_file(
        &self,
        from: &End,
        to: &End,
        actual_to_path: &str,
        expected_hash: Hash,
        expected_size: u64,
        recorded_dest: Option<(Option<FileIdentity>, u64, Option<Timestamp>)>,
        same_machine_or_dir: bool,
        skipped: &mut Vec<SkippedEntry>,
    ) -> Result<u64, ErrorBody> {
        let from_paths = vec![from.path.clone()];
        let to_paths = vec![actual_to_path.to_owned()];
        let locks = self
            .acquire_move_locks(from, &from_paths, to, &to_paths)
            .await?;
        let cand = MoveRemovalCandidate {
            rel: String::new(),
            from_path: from.path.clone(),
            to_path: actual_to_path.to_owned(),
            kind: EntryKind::File,
            expected_hash: Some(expected_hash),
            expected_size,
            expected_link_target: None,
            recorded_dest,
            same_machine_or_dir,
        };
        let verified = self
            .verify_candidate_under_locks(to, &cand, &locks.from_entries, &locks.to_entries)
            .await;
        let result = match verified {
            Ok(true) => {
                let forbid = same_machine_or_dir
                    .then(|| {
                        locks
                            .to_entries
                            .get(actual_to_path)
                            .and_then(|e| e.file_id)
                            .map(|id| (id.dev, id.ino))
                    })
                    .flatten();
                self.remove_source_root(from, Some(expected_hash), forbid, skipped)
                    .await
            }
            Ok(false) => Ok(0),
            Err(reason) => Err(ErrorBody::new(ErrorCode::Conflict, reason)
                .with_hint("Run the transfer again once no other move is modifying these paths.")
                .with_machine(from.machine.clone())),
        };
        locks.release().await;
        result
    }

    /// Acquires exclusive locks on batches of tree-move candidates in fixed `(machine, path)`
    /// order, re-verifies every destination file under those locks, deletes only the verified
    /// source files while holding the locks, and returns `(removed_count, any_kept)`.
    async fn lock_verify_and_delete_tree_candidates(
        &self,
        from: &End,
        to: &End,
        candidates: &[MoveRemovalCandidate],
        skipped: &mut Vec<SkippedEntry>,
    ) -> Result<(u64, bool), ErrorBody> {
        let mut total_removed = 0u64;
        let mut any_kept = false;
        for chunk in candidates.chunks(MOVE_LOCK_BATCH) {
            let from_paths: Vec<String> = chunk.iter().map(|c| c.from_path.clone()).collect();
            let to_paths: Vec<String> = chunk.iter().map(|c| c.to_path.clone()).collect();
            let locks = self
                .acquire_move_locks(from, &from_paths, to, &to_paths)
                .await?;
            let mut to_delete: Vec<String> = Vec::with_capacity(chunk.len());
            let mut hashes: HashMap<String, Hash> = HashMap::new();
            for cand in chunk {
                match self
                    .verify_candidate_under_locks(to, cand, &locks.from_entries, &locks.to_entries)
                    .await
                {
                    Ok(true) => {
                        to_delete.push(cand.rel.clone());
                        if let Some(h) = cand.expected_hash {
                            hashes.insert(cand.rel.clone(), h);
                        }
                    }
                    Ok(false) => {}
                    Err(reason) => {
                        any_kept = true;
                        skipped.push(SkippedEntry {
                            path: cand.rel.clone(),
                            reason,
                        });
                    }
                }
            }
            let delete_res = if to_delete.is_empty() {
                Ok(0)
            } else {
                self.delete_verified(from, &to_delete, &hashes, false, skipped)
                    .await
            };
            locks.release().await;
            let removed = delete_res?;
            if removed < to_delete.len() as u64 {
                any_kept = true;
            }
            total_removed += removed;
        }
        Ok((total_removed, any_kept))
    }

    async fn file(
        &self,
        from: &End,
        to: &End,
        source: &StatReply,
        from_caps: &HashSet<String>,
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
        let (mut there, to_caps): (StatReply, HashSet<String>) = self
            .request_with_caps(
                to,
                json!({"op": "stat", "path": to.path, "checksum": true, "if_size": size, "if_hash": hash}),
                &[],
            )
            .await?;
        if there.kind == "dir" || ends_in_separator(&to.path) {
            target = to.with_path(join(&to.path, file_name(&from.path)));
            there = self
                .request(
                    &target,
                    json!({"op": "stat", "path": target.path, "checksum": true, "if_size": size, "if_hash": hash}),
                )
                .await?;
        }
        Self::check_peer_capabilities(from, from_caps, &target, &to_caps, if_exists, args)?;
        let mut proof_cache: HashMap<(String, String), DirPairProof> = HashMap::new();
        let from_parent = parent_dir_of(&from.path);
        let to_parent = parent_dir_of(&target.path);
        let mut upfront_proof: Option<DirPairProof> = None;
        if args.move_source {
            self.check_move_permission(from).await?;
            // On one node, classify same-entry vs folding rename vs alias using parent
            // identities, exact caller entry names, file identity, and the host filesystem's
            // case/Unicode-normalization behavior.
            if from.machine == target.machine {
                let same_file = source
                    .file_id
                    .zip(there.file_id)
                    .is_some_and(|(a, b)| a.same_file(&b));
                let fs_behavior = self
                    .resolve_parent_fs_behavior(
                        from,
                        source,
                        &target.path,
                        &there,
                        same_file,
                        args.dry_run,
                    )
                    .await;
                match classify_same_entry(
                    source.parent_id,
                    there.parent_id,
                    OsStr::new(file_name(&from.path)),
                    OsStr::new(file_name(&target.path)),
                    same_file,
                    source.is_symlink || there.is_symlink,
                    fs_behavior,
                ) {
                    SameEntryDecision::CaseOrNormalizationRename => {
                        if !from_caps.contains(CAP_RENAME) {
                            return Err(missing_peer_capability(&from.machine, "rename"));
                        }
                        if !args.dry_run {
                            let _: RenameDone = self
                                .request_required(
                                    from,
                                    json!({
                                        "op": "rename",
                                        "from": from.path,
                                        "to": target.path,
                                    }),
                                    &[CAP_RENAME],
                                )
                                .await?;
                        }
                        return Ok(TransferReport {
                            from: from.named(),
                            to: target.named(),
                            kind: "file",
                            moved: true,
                            renamed: vec![RenamedEntry {
                                from: from.path.clone(),
                                to: target.path.clone(),
                            }],
                            ..TransferReport::default()
                        });
                    }
                    SameEntryDecision::SameEntryNoOp | SameEntryDecision::RefuseSameFileAlias => {
                        return Err(same_file_move_error(from, &target));
                    }
                    SameEntryDecision::DistinctEntries => {}
                }
            }
            check_not_same_or_subtree(from, &target, source, &there, false)?;

            // Always probe `(from_parent, to_parent)` regardless of machine names, devices, or
            // mounts so fleet aliases, bind mounts, and shared network folders are caught up front.
            if !args.dry_run {
                let proof = self
                    .prove_dir_pair(from, &from_parent, &target, &to_parent, &mut proof_cache)
                    .await;
                if proof == DirPairProof::SameDirectory && there.kind != "missing" {
                    let proven_distinct_same_dev = source
                        .file_id
                        .zip(there.file_id)
                        .is_some_and(|(a, b)| a.dev == b.dev && a.ino != b.ino);
                    if !proven_distinct_same_dev
                        && (if_exists != IfExists::Rename || from.machine == target.machine)
                    {
                        return Err(same_file_move_error(from, &target));
                    }
                }
                upfront_proof = Some(proof);
            }
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
                "file"
                    if there.size == size
                        && there.hash == Some(hash)
                        && !(args.move_source
                            && from.machine != target.machine
                            && source
                                .file_id
                                .zip(there.file_id)
                                .is_some_and(|(a, b)| a.same_file(&b))) =>
                {
                    report.files_unchanged = 1;
                    if args.move_source {
                        if args.dry_run {
                            report.source_removed = 1;
                            report.would_remove = Some(vec![from.path.clone()]);
                        } else {
                            let dir_proof = match upfront_proof {
                                Some(DirPairProof::ProvenDistinct) => DirPairProof::ProvenDistinct,
                                Some(DirPairProof::SameDirectory) => DirPairProof::SameDirectory,
                                _ => {
                                    self.prove_dir_pair(
                                        from,
                                        &from_parent,
                                        &target,
                                        &to_parent,
                                        &mut proof_cache,
                                    )
                                    .await
                                }
                            };
                            match dir_proof {
                                DirPairProof::Inconclusive(reason) => {
                                    report.skipped.push(SkippedEntry {
                                        path: from.path.clone(),
                                        reason: format!(
                                            "kept source because {} on {} and {} on {} could not be proven distinct ({reason})",
                                            from_parent, from.machine, to_parent, target.machine
                                        ),
                                    });
                                }
                                DirPairProof::SameDirectory if from.machine != target.machine => {
                                    return Err(same_file_move_error(from, &target));
                                }
                                DirPairProof::ProvenDistinct | DirPairProof::SameDirectory => {
                                    let same_machine_or_dir = from.machine == target.machine
                                        || dir_proof == DirPairProof::SameDirectory;
                                    let mut skipped = Vec::new();
                                    let removed = self
                                        .lock_verify_and_remove_single_file(
                                            from,
                                            &target,
                                            &target.path,
                                            hash,
                                            size,
                                            None,
                                            same_machine_or_dir,
                                            &mut skipped,
                                        )
                                        .await?;
                                    report.source_removed = removed;
                                    report.skipped = report.list(skipped);
                                }
                            }
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
            .send_file(from, &target, source, partial, if_exists, 1)
            .await
        {
            Err(error) if partial.is_some() && error.code == ErrorCode::Conflict => {
                // What an earlier attempt left was not a prefix of this file (it changed since):
                // the destination dropped it when the hash did not match, so start over once on
                // attempt 2 with honest per-attempt byte progress.
                self.sent.bytes.store(0, Ordering::Relaxed);
                self.send_file(from, &target, source, None, if_exists, 2)
                    .await?
            }
            other => other?,
        };
        if done.skipped {
            report.files_skipped = 1;
            return Ok(report);
        }
        let actual_target_path = if let Some(ref actual_path) = done.path
            && if_exists == IfExists::Rename
        {
            let chosen_name = file_name(actual_path);
            let reported_path = replace_path_filename(&target.path, chosen_name);
            report.to = format!("{}:{reported_path}", target.machine);
            reported_path
        } else {
            target.path.clone()
        };
        report.files_sent = 1;
        report.bytes_sent = sent;
        report.resumed_from = (resumed > 0).then_some(resumed);
        if args.move_source {
            let actual_to_parent = parent_dir_of(&actual_target_path);
            let dir_proof = self
                .prove_dir_pair(
                    from,
                    &from_parent,
                    &target,
                    &actual_to_parent,
                    &mut proof_cache,
                )
                .await;
            match dir_proof {
                DirPairProof::Inconclusive(reason) => {
                    report.skipped.push(SkippedEntry {
                        path: from.path.clone(),
                        reason: format!(
                            "kept source because {} on {} and {} on {} could not be proven distinct ({reason})",
                            from_parent, from.machine, actual_to_parent, target.machine
                        ),
                    });
                }
                DirPairProof::SameDirectory
                    if file_name(&from.path) == file_name(&actual_target_path) =>
                {
                    return Err(same_file_move_error(from, &target));
                }
                DirPairProof::ProvenDistinct | DirPairProof::SameDirectory => {
                    let same_machine_or_dir =
                        from.machine == target.machine || dir_proof == DirPairProof::SameDirectory;
                    let mut skipped = Vec::new();
                    let removed = self
                        .lock_verify_and_remove_single_file(
                            from,
                            &target,
                            &actual_target_path,
                            hash,
                            size,
                            Some((done.file_id, done.size, done.mtime)),
                            same_machine_or_dir,
                            &mut skipped,
                        )
                        .await?;
                    report.source_removed = removed;
                    report.skipped = report.list(skipped);
                }
            }
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
        attempt: u32,
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
        let mut req_caps: Vec<&str> = Vec::new();
        if if_exists != IfExists::Replace {
            header["if_exists"] = json!(if_exists);
            req_caps.push(CAP_IF_EXISTS);
        }
        let mut dest = self.open_with_caps(to, header, &req_caps).await?;
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
            attempt,
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
        from_caps: &HashSet<String>,
        args: &TransferArgs,
    ) -> Result<TransferReport, ErrorBody> {
        let if_exists = IfExists::from(args.if_exists);
        let (there, to_caps): (StatReply, HashSet<String>) = self
            .request_with_caps(to, json!({"op": "stat", "path": to.path}), &[])
            .await?;
        Self::check_peer_capabilities(from, from_caps, to, &to_caps, if_exists, args)?;
        if args.move_source {
            self.check_move_permission(from).await?;
            if from.machine == to.machine {
                let same_file = source_stat
                    .file_id
                    .zip(there.file_id)
                    .is_some_and(|(a, b)| a.same_file(&b));
                let fs_behavior = self
                    .resolve_parent_fs_behavior(
                        from,
                        source_stat,
                        &to.path,
                        &there,
                        same_file,
                        args.dry_run,
                    )
                    .await;
                match classify_same_entry(
                    source_stat.parent_id,
                    there.parent_id,
                    OsStr::new(file_name(&from.path)),
                    OsStr::new(file_name(&to.path)),
                    same_file,
                    source_stat.is_symlink || there.is_symlink,
                    fs_behavior,
                ) {
                    SameEntryDecision::CaseOrNormalizationRename => {
                        if !from_caps.contains(CAP_RENAME) {
                            return Err(missing_peer_capability(&from.machine, "rename"));
                        }
                        if !args.dry_run {
                            let _: RenameDone = self
                                .request_required(
                                    from,
                                    json!({
                                        "op": "rename",
                                        "from": from.path,
                                        "to": to.path,
                                    }),
                                    &[CAP_RENAME],
                                )
                                .await?;
                        }
                        return Ok(TransferReport {
                            from: from.named(),
                            to: to.named(),
                            kind: "tree",
                            moved: true,
                            renamed: vec![RenamedEntry {
                                from: from.path.clone(),
                                to: to.path.clone(),
                            }],
                            ..TransferReport::default()
                        });
                    }
                    SameEntryDecision::SameEntryNoOp | SameEntryDecision::RefuseSameFileAlias => {
                        return Err(same_file_move_error(from, to));
                    }
                    SameEntryDecision::DistinctEntries => {}
                }
                if let Some(src_id) = source_stat.file_id
                    && there.ancestor_ids.iter().any(|anc| anc.same_file(&src_id))
                {
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
            check_not_same_or_subtree(from, to, source_stat, &there, true)?;
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
        let mut proof_cache: HashMap<(String, String), DirPairProof> = HashMap::new();
        if args.move_source && !args.dry_run {
            let source_subdirs: Vec<String> = source
                .entries
                .iter()
                .filter(|e| e.kind == EntryKind::Dir)
                .map(|e| e.path.clone())
                .collect();
            let same_or_subtree = self
                .probe_tree_root_upfront(
                    from,
                    &from.path,
                    &source_subdirs,
                    to,
                    &to.path,
                    !missing,
                    &mut proof_cache,
                )
                .await?;
            if same_or_subtree {
                return Err(same_file_move_error(from, to));
            }
        }
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
            1,
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
        let mut unsafe_dirs: HashSet<String> = HashSet::new();
        let mut any_source_kept = false;
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
            let round_renamed: HashMap<String, String> = done
                .renamed
                .iter()
                .map(|r| (r.from.clone(), r.to.clone()))
                .collect();
            renamed.extend(done.renamed);
            skipped.extend(on(&to.machine, done.skipped));
            if caseless {
                conflicts.extend(case_conflicts(&done.case_collisions, &to.machine));
            }
            // Only remove source files/symlinks after:
            // 1. Proving their `(from_sub, to_sub)` directory pair distinct (cached per pair);
            // 2. Re-verifying the destination copy under exclusive locks on both source and dest.
            if args.move_source {
                let mut verified =
                    Vec::with_capacity(done.verified_files.len() + done.verified_symlinks.len());
                verified.extend(done.verified_files);
                verified.extend(done.verified_symlinks);
                if !verified.is_empty() {
                    let mut safe_candidates: Vec<MoveRemovalCandidate> = Vec::new();
                    for rel in verified {
                        let sub = rel.rsplit_once('/').map_or("", |(dir, _)| dir);
                        let from_sub = if sub.is_empty() {
                            from.path.clone()
                        } else {
                            join(&from.path, sub)
                        };
                        let to_sub = if sub.is_empty() {
                            to.path.clone()
                        } else {
                            join(&to.path, sub)
                        };
                        let proof = self
                            .prove_dir_pair(from, &from_sub, to, &to_sub, &mut proof_cache)
                            .await;
                        match proof {
                            DirPairProof::ProvenDistinct => {
                                let Some(entry) = listed.get(rel.as_str()) else {
                                    continue;
                                };
                                let dest_rel = round_renamed.get(&rel).unwrap_or(&rel);
                                let recorded_dest = done
                                    .placed_meta
                                    .get(&rel)
                                    .map(|m| (m.file_id, m.size, m.mtime));
                                safe_candidates.push(MoveRemovalCandidate {
                                    rel: rel.clone(),
                                    from_path: join(&from.path, &rel),
                                    to_path: join(&to.path, dest_rel),
                                    kind: entry.kind,
                                    expected_hash: entry.hash,
                                    expected_size: entry.size,
                                    expected_link_target: entry.link_target.clone(),
                                    recorded_dest,
                                    same_machine_or_dir: from.machine == to.machine,
                                });
                            }
                            DirPairProof::SameDirectory => {
                                any_source_kept = true;
                                if unsafe_dirs.insert(sub.to_owned()) {
                                    skipped.push(SkippedEntry {
                                        path: if sub.is_empty() {
                                            from.path.clone()
                                        } else {
                                            sub.to_owned()
                                        },
                                        reason: format!(
                                            "kept source because {} on {} and {} on {} refer to the same directory",
                                            from_sub, from.machine, to_sub, to.machine
                                        ),
                                    });
                                }
                            }
                            DirPairProof::Inconclusive(reason) => {
                                any_source_kept = true;
                                if unsafe_dirs.insert(sub.to_owned()) {
                                    skipped.push(SkippedEntry {
                                        path: if sub.is_empty() {
                                            from.path.clone()
                                        } else {
                                            sub.to_owned()
                                        },
                                        reason: format!(
                                            "kept source because {} on {} and {} on {} could not be proven distinct ({reason})",
                                            from_sub, from.machine, to_sub, to.machine
                                        ),
                                    });
                                }
                            }
                        }
                    }
                    if !safe_candidates.is_empty() {
                        let (removed, kept) = self
                            .lock_verify_and_delete_tree_candidates(
                                from,
                                to,
                                &safe_candidates,
                                &mut skipped,
                            )
                            .await?;
                        report.source_removed += removed;
                        any_source_kept |= kept;
                    }
                }
            }
        }
        meter.emit(true);
        conflicts.sort();
        conflicts.dedup();
        report.deleted += self.delete(to, &extras, &mut skipped).await?;
        if args.move_source {
            if !already_verified.is_empty() {
                let mut safe_candidates: Vec<MoveRemovalCandidate> = Vec::new();
                for rel in &already_verified {
                    let sub = rel.rsplit_once('/').map_or("", |(dir, _)| dir);
                    let from_sub = if sub.is_empty() {
                        from.path.clone()
                    } else {
                        join(&from.path, sub)
                    };
                    let to_sub = if sub.is_empty() {
                        to.path.clone()
                    } else {
                        join(&to.path, sub)
                    };
                    let proof = self
                        .prove_dir_pair(from, &from_sub, to, &to_sub, &mut proof_cache)
                        .await;
                    match proof {
                        DirPairProof::ProvenDistinct => {
                            let Some(entry) = listed.get(rel.as_str()) else {
                                continue;
                            };
                            safe_candidates.push(MoveRemovalCandidate {
                                rel: rel.clone(),
                                from_path: join(&from.path, rel),
                                to_path: join(&to.path, rel),
                                kind: entry.kind,
                                expected_hash: entry.hash,
                                expected_size: entry.size,
                                expected_link_target: entry.link_target.clone(),
                                recorded_dest: None,
                                same_machine_or_dir: from.machine == to.machine,
                            });
                        }
                        DirPairProof::SameDirectory => {
                            any_source_kept = true;
                            if unsafe_dirs.insert(sub.to_owned()) {
                                skipped.push(SkippedEntry {
                                    path: if sub.is_empty() {
                                        from.path.clone()
                                    } else {
                                        sub.to_owned()
                                    },
                                    reason: format!(
                                        "kept source because {} on {} and {} on {} refer to the same directory",
                                        from_sub, from.machine, to_sub, to.machine
                                    ),
                                });
                            }
                        }
                        DirPairProof::Inconclusive(reason) => {
                            any_source_kept = true;
                            if unsafe_dirs.insert(sub.to_owned()) {
                                skipped.push(SkippedEntry {
                                    path: if sub.is_empty() {
                                        from.path.clone()
                                    } else {
                                        sub.to_owned()
                                    },
                                    reason: format!(
                                        "kept source because {} on {} and {} on {} could not be proven distinct ({reason})",
                                        from_sub, from.machine, to_sub, to.machine
                                    ),
                                });
                            }
                        }
                    }
                }
                if !safe_candidates.is_empty() {
                    let (removed, kept) = self
                        .lock_verify_and_delete_tree_candidates(
                            from,
                            to,
                            &safe_candidates,
                            &mut skipped,
                        )
                        .await?;
                    report.source_removed += removed;
                    any_source_kept |= kept;
                }
            }
            // Remove source directories deepest-first only once every verified file has been
            // removed; any directory that could not be proven distinct (or still holds excluded,
            // skipped, or conflicted files) stays.
            let dir_is_unsafe = |dir: &str| {
                unsafe_dirs.contains("")
                    || unsafe_dirs.iter().any(|u| {
                        u == dir
                            || u.strip_prefix(dir)
                                .is_some_and(|rest| rest.starts_with('/'))
                    })
            };
            let dirs: Vec<String> = source
                .entries
                .iter()
                .filter(|entry| entry.kind == EntryKind::Dir && !dir_is_unsafe(&entry.path))
                .map(|entry| entry.path.clone())
                .collect();
            let remove_root = args.exclude.is_empty()
                && conflicts.is_empty()
                && report.files_skipped == 0
                && skipped.is_empty()
                && !any_source_kept
                && unsafe_dirs.is_empty();
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
        forbid_file_id: Option<(u64, u64)>,
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
        if let Some((dev, ino)) = forbid_file_id {
            header["forbid_file_id"] = json!([dev, ino]);
        }
        let done: DeleteDone = self.request_required(from, header, &[CAP_MOVE]).await?;
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
                    .request_required(
                        from,
                        json!({
                            "op": "delete",
                            "root": from.path,
                            "names": [],
                            "remove_root": true,
                        }),
                        &[CAP_MOVE],
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
            let done: DeleteDone = self.request_required(from, header, &[CAP_MOVE]).await?;
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
        let mut req_caps: Vec<&str> = Vec::new();
        if if_exists != IfExists::Replace {
            unpack_header["if_exists"] = json!(if_exists);
            req_caps.push(CAP_IF_EXISTS);
        }
        let mut dest = match self.open_with_caps(to, unpack_header, &req_caps).await {
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
        let (reply, _) = self.request_with_caps(end, header, &[]).await?;
        Ok(reply)
    }

    /// One operation that answers with one line and requires the peer to advertise `required_caps`.
    async fn request_required<T: DeserializeOwned>(
        &self,
        end: &End,
        header: Value,
        required_caps: &[&str],
    ) -> Result<T, ErrorBody> {
        let (reply, _) = self.request_with_caps(end, header, required_caps).await?;
        Ok(reply)
    }

    /// One operation that answers with one line, also returning the peer's advertised stream
    /// capabilities from its handshake.
    async fn request_with_caps<T: DeserializeOwned>(
        &self,
        end: &End,
        header: Value,
        required_caps: &[&str],
    ) -> Result<(T, HashSet<String>), ErrorBody> {
        let mut conn = self.open_with_caps(end, header, required_caps).await?;
        let answer = conn.first::<T>().await;
        let caps = conn.capabilities.clone();
        conn.close().await;
        answer.map(|reply| (reply, caps))
    }

    async fn open(&self, end: &End, header: Value) -> Result<Conn, ErrorBody> {
        self.open_with_caps(end, header, &[]).await
    }

    async fn open_with_caps(
        &self,
        end: &End,
        header: Value,
        required_caps: &[&str],
    ) -> Result<Conn, ErrorBody> {
        Conn::open(self.fleet, self.ctx, self.sent, end, header, required_caps).await
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

/// Builds the refusal returned when a peer runs an older Legion that does not advertise a
/// required stream capability.
fn missing_peer_capability(machine: &str, feature: &str) -> ErrorBody {
    ErrorBody::new(
        ErrorCode::Unsupported,
        format!("{machine} runs an older legion that does not support {feature}."),
    )
    .with_hint(format!(
        "Update legion on {machine} so it can honour {feature} safely."
    ))
    .with_machine(machine.to_owned())
}

/// Builds the refusal returned when a move's source and destination resolve to the same file or
/// directory (whether via alias, bind mount, network share, or hard link).
fn same_file_move_error(from: &End, to: &End) -> ErrorBody {
    ErrorBody::new(
        ErrorCode::InvalidArgs,
        format!(
            "Cannot move {} to {}: both paths refer to the same file or directory.",
            from.named(),
            to.named()
        ),
    )
    .with_hint("Choose a different destination path, or rename within one machine.")
    .with_machine(from.machine.clone())
}

/// Returns the parent directory of `path` in the path syntax the caller wrote.
fn parent_dir_of(path: &str) -> String {
    let trimmed = path.trim_end_matches(['/', '\\']);
    if let Some(pos) = trimmed.rfind(['/', '\\']) {
        let prefix = &trimmed[..pos];
        if prefix.is_empty() {
            return "/".to_owned();
        }
        // Preserve Windows drive root `C:\` or `C:/`.
        if prefix.len() == 2 && prefix.as_bytes()[1] == b':' {
            let sep = trimmed.as_bytes()[pos] as char;
            return format!("{prefix}{sep}");
        }
        return prefix.to_owned();
    }
    if trimmed == "~" {
        "~".to_owned()
    } else {
        ".".to_owned()
    }
}

/// Replaces only the final filename segment of `path` with `new_name`, preserving the caller's
/// prefix (`~/...`, `/...`, or Windows drive path). For `~` paths, always uses forward slashes so
/// reported paths are identical across Unix and Windows.
fn replace_path_filename(path: &str, new_name: &str) -> String {
    let trimmed = path.trim_end_matches(['/', '\\']);
    if trimmed.starts_with("~/") || trimmed.starts_with("~\\") {
        let normalized = trimmed.replace('\\', "/");
        return match normalized.rfind('/') {
            Some(pos) => format!("{}/{new_name}", &normalized[..pos]),
            None => format!("~/{new_name}"),
        };
    }
    match trimmed.rfind(['/', '\\']) {
        Some(pos) => {
            let sep = trimmed.as_bytes()[pos] as char;
            format!("{}{sep}{new_name}", &trimmed[..pos])
        }
        None => new_name.to_owned(),
    }
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
    /// Capabilities advertised by the other end in its greeting line.
    capabilities: HashSet<String>,
    /// Capabilities this operation requires the other end to advertise.
    required_caps: Vec<String>,
    /// The longest reply line taken once the other end has answered.
    longest: usize,
    /// How many bytes of lines have been read, which bounds a listing as a whole.
    received: usize,
    /// When the pipe was opened, which the first reply's time limit counts from.
    opened: tokio::time::Instant,
    /// Cancelled by a local in-process stream's auth watchdog if permissions/device are revoked
    /// while the stream is stalled.
    revoke_token: Option<CancellationToken>,
    /// Complete control lines read from `reader` during a relay (e.g. an early `{"ok": ...}` or
    /// mid-stream `{"error": ...}`) so [`Self::first`] receives them intact afterward.
    pending_lines: VecDeque<Vec<u8>>,
    /// Any incomplete control line already consumed from `reader` while watching for heartbeats or
    /// errors during a relay, continued seamlessly by [`Self::line`].
    partial_line: Vec<u8>,
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
        required_caps: &[&str],
    ) -> Result<Conn, ErrorBody> {
        header["protocol"] = json!(stream::STREAM_PROTOCOL);
        let (reader, writer, process, revoke_token): (
            BoxRead,
            BoxWrite,
            Process,
            Option<CancellationToken>,
        ) = if end.remote {
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
            (
                Box::new(stdout),
                Box::new(stdin),
                Process::Remote(handle),
                None,
            )
        } else {
            let (client_out, server_in) = tokio::io::duplex(CHUNK);
            let (server_out, client_in) = tokio::io::duplex(CHUNK);
            let host = Host::new(ctx.paths().clone(), ctx.node_name());
            let principal = ctx.principal().to_owned();
            let device = ctx.device().map(str::to_owned);
            let pairing_id = ctx.pairing_id().map(str::to_owned);
            let revoke_token = CancellationToken::new();
            let revoke_for_server = revoke_token.clone();
            let input = SyncIoBridge::new(server_in);
            let output = SyncIoBridge::new(server_out);
            let task = tokio::task::spawn_blocking(move || {
                stream::serve_with_cancel(
                    &host,
                    Some(&principal),
                    Door::Stream,
                    device.as_deref(),
                    pairing_id.as_deref(),
                    Some(revoke_for_server),
                    input,
                    output,
                )
            });
            (
                Box::new(client_in),
                Box::new(client_out),
                Process::Local(Some(task)),
                Some(revoke_token),
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
            capabilities: HashSet::new(),
            required_caps: required_caps.iter().map(|c| (*c).to_owned()).collect(),
            longest: LONGEST_REPLY,
            received: 0,
            opened: tokio::time::Instant::now(),
            revoke_token,
            pending_lines: VecDeque::new(),
            partial_line: Vec::new(),
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
                        if let Some(caps) = fields.get("capabilities").and_then(Value::as_array) {
                            for cap in caps.iter().filter_map(Value::as_str) {
                                self.capabilities.insert(cap.to_owned());
                            }
                        }
                        self.greeted = true;
                        self.answered = true;
                        if let Some(missing) = self
                            .required_caps
                            .iter()
                            .find(|cap| !self.capabilities.contains(*cap))
                        {
                            return Err(missing_peer_capability(&self.machine, missing));
                        }
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
        if let Some(line) = self.pending_lines.pop_front() {
            self.received += line.len();
            return Ok(line);
        }
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
        let mut line = std::mem::take(&mut self.partial_line);
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
#[derive(Debug)]
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

async fn wait_revoked(token: &Option<CancellationToken>) {
    match token {
        Some(t) => t.cancelled().await,
        None => std::future::pending().await,
    }
}

async fn write_once_opt(writer: &mut Option<BoxWrite>, bytes: &[u8]) -> std::io::Result<usize> {
    let Some(w) = writer.as_mut() else {
        return Err(std::io::Error::new(
            std::io::ErrorKind::BrokenPipe,
            "the input was already closed",
        ));
    };
    w.write(bytes).await
}

async fn flush_opt(writer: &mut Option<BoxWrite>) -> std::io::Result<()> {
    let Some(w) = writer.as_mut() else {
        return Err(std::io::Error::new(
            std::io::ErrorKind::BrokenPipe,
            "the input was already closed",
        ));
    };
    w.flush().await
}

/// Drains whatever control bytes are already buffered in `dest.reader` into `dest.partial_line`
/// and classifies each complete newline-terminated line:
/// - `{"busy": true}\n` heartbeats are discarded so the relay keeps watching for later errors.
/// - `{"error": ...}\n` or malformed/oversized lines are queued on `dest.pending_lines` and stop
///   the relay immediately with [`Broken::Destination`].
/// - Any other JSON reply (e.g. an early `{"ok": true, ...}\n`) is queued on `dest.pending_lines`
///   and sets `*dest_replied_ok = true` so [`Conn::first`] reads it intact once the relay ends.
fn drain_dest_control(dest: &mut Conn, dest_replied_ok: &mut bool) -> Result<(), Broken> {
    while !*dest_replied_ok {
        let avail = dest.reader.buffer();
        if avail.is_empty() {
            break;
        }
        let (chunk_len, found) = match avail.iter().position(|byte| *byte == b'\n') {
            Some(end) => (end + 1, true),
            None => (avail.len(), false),
        };
        if dest.partial_line.len() + chunk_len > dest.longest {
            return Err(Broken::Destination("while it received".to_owned()));
        }
        dest.partial_line.extend_from_slice(&avail[..chunk_len]);
        dest.reader.consume(chunk_len);
        if !found {
            break;
        }
        let line = std::mem::take(&mut dest.partial_line);
        match serde_json::from_slice::<Value>(&line) {
            Ok(Value::Object(ref fields))
                if fields.len() == 1 && fields.get("busy") == Some(&Value::Bool(true)) => {}
            Ok(Value::Object(ref fields)) if fields.contains_key("error") => {
                dest.pending_lines.push_back(line);
                return Err(Broken::Destination("while it received".to_owned()));
            }
            Ok(Value::Object(_)) => {
                dest.pending_lines.push_back(line);
                *dest_replied_ok = true;
            }
            _ => {
                dest.pending_lines.push_back(line);
                return Err(Broken::Destination("while it received".to_owned()));
            }
        }
    }
    Ok(())
}

/// Copies the source's output to the destination's input in fixed chunks, never more than
/// `amount` allows: a source that sends more is stopped before the excess reaches the destination.
async fn relay(
    source: &mut Conn,
    dest: &mut Conn,
    amount: Amount,
    meter: &mut Meter<'_>,
) -> Result<(), Broken> {
    relay_with_timeout(source, dest, amount, meter, IDLE_TIMEOUT).await
}

/// Cancellation-safe core of [`relay`] with a configurable per-hop stall timeout.
///
/// Every `tokio::select!` branch operates on single-step, cancellation-safe futures (`read`,
/// `write`, `flush`, `fill_buf`), while all partial state (`buf`, `written`, `dest.partial_line`,
/// `dest.pending_lines`) lives outside `select!`. A heartbeat or early reply arriving on
/// `dest.reader` mid-chunk can therefore never discard or duplicate bytes already written.
async fn relay_with_timeout(
    source: &mut Conn,
    dest: &mut Conn,
    amount: Amount,
    meter: &mut Meter<'_>,
    idle_timeout: Duration,
) -> Result<(), Broken> {
    let mut buf = vec![0u8; CHUNK];
    let mut copied = 0u64;
    let limit = match amount {
        Amount::Exactly(total) | Amount::AtMost(total) => total,
    };
    let source_revoke = source.revoke_token.clone();
    let dest_revoke = dest.revoke_token.clone();
    // Once `dest` sends a non-error, non-busy reply (e.g. `UnpackDone` right before `source` sends
    // EOF), stop polling `dest.reader` so we do not mistake a normal early reply for an error.
    let mut dest_replied_ok = false;
    // Drain anything already buffered in `dest.reader` before entering the transfer loop.
    drain_dest_control(dest, &mut dest_replied_ok)?;

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
        let read = {
            let idle = tokio::time::sleep(idle_timeout);
            tokio::pin!(idle);
            loop {
                tokio::select! {
                    res = source.reader.read(&mut buf[..want]) => match res {
                        Ok(n) => break n,
                        Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
                        Err(error) => {
                            return Err(Broken::Source(format!("while it sent ({error})")));
                        }
                    },
                    res = dest.reader.fill_buf(), if !dest_replied_ok => match res {
                        Ok([]) | Err(_) => {
                            return Err(Broken::Destination("while it received".to_owned()));
                        }
                        Ok(_) => drain_dest_control(dest, &mut dest_replied_ok)?,
                    },
                    () = wait_revoked(&source_revoke) => {
                        return Err(Broken::Source("because access was revoked".to_owned()));
                    }
                    () = wait_revoked(&dest_revoke) => {
                        return Err(Broken::Destination("because access was revoked".to_owned()));
                    }
                    () = &mut idle => {
                        return Err(Broken::Stalled { source: true });
                    }
                }
            }
        };
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

        // Write `buf[..read]` with an explicit offset owned outside `select!`: each `select!` only
        // polls a single cancellation-safe `write(&buf[written..read])`, never `write_all`.
        let mut written = 0usize;
        {
            let idle = tokio::time::sleep(idle_timeout);
            tokio::pin!(idle);
            while written < read {
                tokio::select! {
                    res = write_once_opt(&mut dest.writer, &buf[written..read]) => match res {
                        Ok(0) => {
                            return Err(Broken::Destination(
                                "while it received (write zero)".to_owned(),
                            ));
                        }
                        Ok(n) => {
                            written += n;
                            idle.as_mut()
                                .reset(tokio::time::Instant::now() + idle_timeout);
                        }
                        Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
                        Err(error) => {
                            return Err(Broken::Destination(format!(
                                "while it received ({error})"
                            )));
                        }
                    },
                    res = dest.reader.fill_buf(), if !dest_replied_ok => match res {
                        Ok([]) | Err(_) => {
                            return Err(Broken::Destination("while it received".to_owned()));
                        }
                        Ok(_) => drain_dest_control(dest, &mut dest_replied_ok)?,
                    },
                    () = wait_revoked(&source_revoke) => {
                        return Err(Broken::Source("because access was revoked".to_owned()));
                    }
                    () = wait_revoked(&dest_revoke) => {
                        return Err(Broken::Destination("because access was revoked".to_owned()));
                    }
                    () = &mut idle => {
                        return Err(Broken::Stalled { source: false });
                    }
                }
            }
        }
        {
            let idle = tokio::time::sleep(idle_timeout);
            tokio::pin!(idle);
            loop {
                tokio::select! {
                    res = flush_opt(&mut dest.writer) => match res {
                        Ok(()) => break,
                        Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
                        Err(error) => {
                            return Err(Broken::Destination(format!(
                                "while it received ({error})"
                            )));
                        }
                    },
                    res = dest.reader.fill_buf(), if !dest_replied_ok => match res {
                        Ok([]) | Err(_) => {
                            return Err(Broken::Destination("while it received".to_owned()));
                        }
                        Ok(_) => drain_dest_control(dest, &mut dest_replied_ok)?,
                    },
                    () = wait_revoked(&source_revoke) => {
                        return Err(Broken::Source("because access was revoked".to_owned()));
                    }
                    () = wait_revoked(&dest_revoke) => {
                        return Err(Broken::Destination("because access was revoked".to_owned()));
                    }
                    () = &mut idle => {
                        return Err(Broken::Stalled { source: false });
                    }
                }
            }
        }
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
    /// Bytes already on disk before this attempt started (for resumed single files).
    bytes_base: u64,
    /// Bytes transferred in this attempt (tracks reality even when a stale partial restarts from 0).
    attempt_bytes: u64,
    files_total: u64,
    current: Option<String>,
    tar: Option<TarCounter>,
    started: Instant,
    last: Instant,
    last_bytes_done: u64,
    last_files_done: u64,
    attempt: u32,
}

impl<'a> Meter<'a> {
    #[cfg(test)]
    fn quiet(sent: &'a Sent) -> Meter<'a> {
        let now = Instant::now();
        Meter {
            ctx: None,
            sent,
            bytes_total: 0,
            bytes_base: 0,
            attempt_bytes: 0,
            files_total: 0,
            current: None,
            tar: None,
            started: now,
            last: now,
            last_bytes_done: 0,
            last_files_done: 0,
            attempt: 1,
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn new(
        ctx: &'a Ctx,
        sent: &'a Sent,
        bytes_total: u64,
        bytes_base: u64,
        files_total: u64,
        current: Option<String>,
        tar: bool,
        attempt: u32,
    ) -> Meter<'a> {
        let now = Instant::now();
        Meter {
            ctx: Some(ctx),
            sent,
            bytes_total,
            bytes_base,
            attempt_bytes: 0,
            files_total,
            current,
            tar: tar.then(TarCounter::default),
            started: now,
            last: now,
            last_bytes_done: bytes_base.min(bytes_total),
            last_files_done: 0,
            attempt,
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
        self.attempt_bytes = self.attempt_bytes.saturating_add(bytes);
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
        let transferred = self.attempt_bytes;
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
        if self.attempt > 1 {
            payload["attempt"] = json!(self.attempt);
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
            capabilities: HashSet::new(),
            required_caps: Vec::new(),
            longest: LONGEST_REPLY,
            received: 0,
            opened: tokio::time::Instant::now(),
            revoke_token: None,
            pending_lines: VecDeque::new(),
            partial_line: Vec::new(),
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
            attempt: 1,
            attempt_bytes: 0,
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

    #[test]
    fn replace_path_filename_preserves_caller_form_across_unix_and_windows() {
        // Caller gave `~/…`: always reports `~/…` with forward slashes regardless of remote OS.
        assert_eq!(
            replace_path_filename("~/doc.txt", "doc (2).txt"),
            "~/doc (2).txt"
        );
        assert_eq!(
            replace_path_filename("~/sub/dir/doc.txt", "doc (2).txt"),
            "~/sub/dir/doc (2).txt"
        );
        assert_eq!(
            replace_path_filename(r"~\sub\doc.txt", "doc (2).txt"),
            "~/sub/doc (2).txt"
        );

        // Combining with `file_name` on a Windows absolute path returned by a Windows peer:
        let win_actual = r"C:\Users\runneradmin\AppData\Local\Temp\.tmpfBCY6H\home\doc (2).txt";
        assert_eq!(
            replace_path_filename("~/doc.txt", file_name(win_actual)),
            "~/doc (2).txt"
        );

        // Absolute Unix and Windows paths keep their original directory and separator style.
        assert_eq!(
            replace_path_filename("/srv/data/doc.txt", "doc (2).txt"),
            "/srv/data/doc (2).txt"
        );
        assert_eq!(
            replace_path_filename(r"C:\Work\doc.txt", "doc (2).txt"),
            r"C:\Work\doc (2).txt"
        );
        assert_eq!(
            replace_path_filename("C:/Work/doc.txt", "doc (2).txt"),
            "C:/Work/doc (2).txt"
        );

        // Parent dir extraction handles Unix, tilde, and Windows drive roots.
        assert_eq!(parent_dir_of("~/doc.txt"), "~");
        assert_eq!(parent_dir_of("~/a/b.txt"), "~/a");
        assert_eq!(parent_dir_of("/doc.txt"), "/");
        assert_eq!(parent_dir_of(r"C:\doc.txt"), r"C:\");
        assert_eq!(parent_dir_of(r"C:\Work\doc.txt"), r"C:\Work");
    }

    #[derive(Clone)]
    struct Rng64(u64);

    impl Rng64 {
        fn new(seed: u64) -> Self {
            Self(seed ^ 0x9e37_79b9_7f4a_7c15)
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

    /// Deterministic adversarial `AsyncRead` that yields 1-byte or random short chunks, injects
    /// `Poll::Pending` wakeups and `Interrupted` errors, and never relies on wall-clock timing.
    struct AdversarialAsyncRead {
        data: Vec<u8>,
        pos: usize,
        rng: Rng64,
        single_byte: bool,
        inject_interrupted: bool,
        pending_every: usize,
        polls: usize,
    }

    impl AsyncRead for AdversarialAsyncRead {
        fn poll_read(
            mut self: std::pin::Pin<&mut Self>,
            cx: &mut std::task::Context<'_>,
            buf: &mut tokio::io::ReadBuf<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            self.polls += 1;
            if self.pending_every > 0 && self.polls.is_multiple_of(self.pending_every) {
                cx.waker().wake_by_ref();
                return std::task::Poll::Pending;
            }
            if self.inject_interrupted && self.rng.range(9) == 0 {
                return std::task::Poll::Ready(Err(std::io::Error::new(
                    std::io::ErrorKind::Interrupted,
                    "adversarial eintr",
                )));
            }
            let remaining = self.data.len() - self.pos;
            if remaining == 0 || buf.remaining() == 0 {
                return std::task::Poll::Ready(Ok(()));
            }
            let max_take = remaining.min(buf.remaining());
            let take = if self.single_byte {
                1
            } else {
                1 + self.rng.range(max_take.min(4096))
            };
            let take = take.min(max_take);
            buf.put_slice(&self.data[self.pos..self.pos + take]);
            self.pos += take;
            std::task::Poll::Ready(Ok(()))
        }
    }

    /// Deterministic adversarial `AsyncWrite` that performs 1-byte or random short writes, injects
    /// `Poll::Pending` wakeups on write and flush, injects `Interrupted`, and records every byte
    /// written into a shared buffer for exact byte-for-byte verification.
    struct AdversarialAsyncWrite {
        out: Arc<Mutex<Vec<u8>>>,
        rng: Rng64,
        single_byte: bool,
        inject_interrupted: bool,
        pending_every: usize,
        polls: usize,
        on_write_step: Option<Box<dyn FnMut(usize) + Send>>,
    }

    impl AsyncWrite for AdversarialAsyncWrite {
        fn poll_write(
            mut self: std::pin::Pin<&mut Self>,
            cx: &mut std::task::Context<'_>,
            buf: &[u8],
        ) -> std::task::Poll<std::io::Result<usize>> {
            self.polls += 1;
            if self.pending_every > 0 && self.polls.is_multiple_of(self.pending_every) {
                cx.waker().wake_by_ref();
                return std::task::Poll::Pending;
            }
            if self.inject_interrupted && self.rng.range(9) == 0 {
                return std::task::Poll::Ready(Err(std::io::Error::new(
                    std::io::ErrorKind::Interrupted,
                    "adversarial eintr",
                )));
            }
            if buf.is_empty() {
                return std::task::Poll::Ready(Ok(0));
            }
            let take = if self.single_byte {
                1
            } else {
                1 + self.rng.range(buf.len().min(4096))
            };
            let take = take.min(buf.len());
            let total = {
                let mut guard = self.out.lock().unwrap();
                guard.extend_from_slice(&buf[..take]);
                guard.len()
            };
            if let Some(ref mut cb) = self.on_write_step {
                cb(total);
            }
            std::task::Poll::Ready(Ok(take))
        }

        fn poll_flush(
            mut self: std::pin::Pin<&mut Self>,
            cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            self.polls += 1;
            if self.pending_every > 0 && self.polls.is_multiple_of(self.pending_every) {
                cx.waker().wake_by_ref();
                return std::task::Poll::Pending;
            }
            if self.inject_interrupted && self.rng.range(11) == 0 {
                return std::task::Poll::Ready(Err(std::io::Error::new(
                    std::io::ErrorKind::Interrupted,
                    "adversarial eintr flush",
                )));
            }
            std::task::Poll::Ready(Ok(()))
        }

        fn poll_shutdown(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            std::task::Poll::Ready(Ok(()))
        }
    }

    fn custom_conn(machine: &str, reader: BoxRead, writer: BoxWrite) -> Conn {
        Conn {
            machine: machine.to_owned(),
            reader: BufReader::with_capacity(CHUNK, reader),
            writer: Some(writer),
            process: Owned {
                process: Some(Process::Local(None)),
                ends: Ends::default(),
            },
            answered: true,
            noise_lines: 0,
            noise_bytes: 0,
            greeted: true,
            capabilities: HashSet::new(),
            required_caps: Vec::new(),
            longest: LONGEST_REPLY,
            received: 0,
            opened: tokio::time::Instant::now(),
            revoke_token: None,
            pending_lines: VecDeque::new(),
            partial_line: Vec::new(),
        }
    }

    fn make_payload(seed: u64, len: usize) -> Vec<u8> {
        let mut rng = Rng64::new(seed);
        let mut out = vec![0u8; len];
        for chunk in out.chunks_mut(8) {
            let bytes = rng.next().to_le_bytes();
            let n = chunk.len();
            chunk.copy_from_slice(&bytes[..n]);
        }
        out
    }

    #[tokio::test]
    async fn relay_regression_busy_heartbeat_mid_partial_write_never_duplicates_bytes() {
        // Reproduces the exact macOS failure deterministically without wall-clock timing:
        // `dest.writer` accepts a partial prefix of a 64 KiB chunk, then yields `Poll::Pending`
        // while `dest.reader` delivers `{"busy": true}\n` — cancelling the active write arm of
        // `tokio::select!`. The relay must resume at `written`, never from offset 0.
        struct MidChunkStallWriter {
            out: Arc<Mutex<Vec<u8>>>,
            busy_tx: Option<tokio::sync::oneshot::Sender<()>>,
            resume_rx: Option<tokio::sync::oneshot::Receiver<()>>,
            stalled_once: bool,
        }

        impl AsyncWrite for MidChunkStallWriter {
            fn poll_write(
                mut self: std::pin::Pin<&mut Self>,
                cx: &mut std::task::Context<'_>,
                buf: &[u8],
            ) -> std::task::Poll<std::io::Result<usize>> {
                if !self.stalled_once {
                    let prefix = buf.len().min(17 * 1024);
                    self.out.lock().unwrap().extend_from_slice(&buf[..prefix]);
                    self.stalled_once = true;
                    if let Some(tx) = self.busy_tx.take() {
                        let _ = tx.send(());
                    }
                    return std::task::Poll::Ready(Ok(prefix));
                }
                if let Some(rx) = self.resume_rx.as_mut() {
                    match std::pin::Pin::new(rx).poll(cx) {
                        std::task::Poll::Pending => return std::task::Poll::Pending,
                        std::task::Poll::Ready(_) => {
                            self.resume_rx = None;
                        }
                    }
                }
                self.out.lock().unwrap().extend_from_slice(buf);
                std::task::Poll::Ready(Ok(buf.len()))
            }

            fn poll_flush(
                self: std::pin::Pin<&mut Self>,
                _cx: &mut std::task::Context<'_>,
            ) -> std::task::Poll<std::io::Result<()>> {
                std::task::Poll::Ready(Ok(()))
            }

            fn poll_shutdown(
                self: std::pin::Pin<&mut Self>,
                _cx: &mut std::task::Context<'_>,
            ) -> std::task::Poll<std::io::Result<()>> {
                std::task::Poll::Ready(Ok(()))
            }
        }

        let payload = make_payload(0xfeed_face, 256 * 1024 + 137);
        let received = Arc::new(Mutex::new(Vec::new()));
        let (busy_tx, busy_rx) = tokio::sync::oneshot::channel();
        let (resume_tx, resume_rx) = tokio::sync::oneshot::channel();
        let (dest_ctrl_in, mut dest_ctrl_out) = tokio::io::duplex(4096);
        let (_src_sink, src_writer) = tokio::io::duplex(64);

        let ctrl_task = tokio::spawn(async move {
            busy_rx.await.unwrap();
            // Deliver three `{"busy": true}\n` heartbeats (one fragmented byte by byte) while
            // `MidChunkStallWriter` is parked mid-chunk, then release the writer and send `ok`.
            dest_ctrl_out.write_all(b"{\"busy\":true}\n").await.unwrap();
            for &b in b"{\"busy\": true}\n" {
                dest_ctrl_out.write_all(&[b]).await.unwrap();
                tokio::task::yield_now().await;
            }
            let _ = resume_tx.send(());
            dest_ctrl_out
                .write_all(b"{\"ok\":true,\"size\":262281}\n")
                .await
                .unwrap();
            dest_ctrl_out.shutdown().await.unwrap();
        });

        let mut source = custom_conn(
            "src",
            Box::new(std::io::Cursor::new(payload.clone())),
            Box::new(src_writer),
        );
        let mut dest = custom_conn(
            "dst",
            Box::new(dest_ctrl_in),
            Box::new(MidChunkStallWriter {
                out: Arc::clone(&received),
                busy_tx: Some(busy_tx),
                resume_rx: Some(resume_rx),
                stalled_once: false,
            }),
        );
        let sent = Sent::default();
        let mut meter = Meter::quiet(&sent);
        relay(
            &mut source,
            &mut dest,
            Amount::Exactly(payload.len() as u64),
            &mut meter,
        )
        .await
        .unwrap();
        ctrl_task.await.unwrap();

        let got = received.lock().unwrap().clone();
        assert_eq!(got.len(), payload.len());
        assert_eq!(
            got, payload,
            "bytes must match payload byte for byte after mid-chunk busy heartbeats"
        );
        let reply: Value = dest.first().await.unwrap();
        assert_eq!(reply["ok"], Value::Bool(true));
        assert_eq!(reply["size"], json!(262281));
    }

    #[tokio::test]
    async fn relay_is_cancellation_safe_under_adversarial_io_and_heartbeats() {
        // 64 deterministic seeds exercising 1-byte and random short reads/writes, `Poll::Pending`
        // wakeups at every await point, `Interrupted` errors, and fragmented `{"busy": true}\n`
        // plus trailing `{"ok": true}\n` lines across both `Amount::Exactly` and `Amount::AtMost`.
        for seed in 0u64..64 {
            let single_byte = seed % 8 == 0;
            let len = if single_byte {
                3_000 + (seed as usize) * 37
            } else {
                70_000 + (seed as usize) * 1_913
            };
            let payload = make_payload(seed + 1, len);
            let received = Arc::new(Mutex::new(Vec::new()));

            let mut ctrl_bytes = Vec::new();
            let heartbeats = 2 + (seed as usize % 6);
            for _ in 0..heartbeats {
                ctrl_bytes.extend_from_slice(b"{\"busy\":true}\n");
            }
            let ok_line = format!("{{\"ok\":true,\"seed\":{seed}}}\n");
            ctrl_bytes.extend_from_slice(ok_line.as_bytes());

            let source_reader = AdversarialAsyncRead {
                data: payload.clone(),
                pos: 0,
                rng: Rng64::new(seed * 3 + 11),
                single_byte,
                inject_interrupted: true,
                pending_every: 2 + (seed as usize % 3),
                polls: 0,
            };
            let dest_ctrl_reader = AdversarialAsyncRead {
                data: ctrl_bytes,
                pos: 0,
                rng: Rng64::new(seed * 5 + 29),
                single_byte: true,
                inject_interrupted: false,
                pending_every: 2 + (seed as usize % 4),
                polls: 0,
            };
            let dest_writer = AdversarialAsyncWrite {
                out: Arc::clone(&received),
                rng: Rng64::new(seed * 7 + 43),
                single_byte,
                inject_interrupted: true,
                pending_every: 2 + (seed as usize % 3),
                polls: 0,
                on_write_step: None,
            };
            let (_src_sink, src_writer) = tokio::io::duplex(64);
            let mut source = custom_conn("src", Box::new(source_reader), Box::new(src_writer));
            let mut dest = custom_conn("dst", Box::new(dest_ctrl_reader), Box::new(dest_writer));

            let sent = Sent::default();
            let mut meter = Meter::quiet(&sent);
            let amount = if seed % 2 == 0 {
                Amount::Exactly(payload.len() as u64)
            } else {
                Amount::AtMost(payload.len() as u64 + 1024)
            };
            relay(&mut source, &mut dest, amount, &mut meter)
                .await
                .unwrap_or_else(|_| panic!("relay failed on seed {seed}"));

            let got = received.lock().unwrap().clone();
            assert_eq!(got.len(), payload.len(), "length mismatch on seed {seed}");
            assert_eq!(got, payload, "byte mismatch on seed {seed}");

            let reply: Value = dest.first().await.unwrap();
            assert_eq!(reply["ok"], Value::Bool(true), "reply on seed {seed}");
            assert_eq!(reply["seed"], json!(seed), "reply seed on seed {seed}");
        }
    }

    #[tokio::test(start_paused = true)]
    async fn relay_detects_destination_error_after_busy_heartbeats_and_detects_stalls() {
        // 1. Multiple `{"busy": true}\n` heartbeats followed by a mid-stream `{"error": ...}\n`
        // while `dest.writer` is backpressured must immediately fail with that exact `ErrorBody`.
        struct NeverReadyWriter;
        impl AsyncWrite for NeverReadyWriter {
            fn poll_write(
                self: std::pin::Pin<&mut Self>,
                _cx: &mut std::task::Context<'_>,
                _buf: &[u8],
            ) -> std::task::Poll<std::io::Result<usize>> {
                std::task::Poll::Pending
            }
            fn poll_flush(
                self: std::pin::Pin<&mut Self>,
                _cx: &mut std::task::Context<'_>,
            ) -> std::task::Poll<std::io::Result<()>> {
                std::task::Poll::Pending
            }
            fn poll_shutdown(
                self: std::pin::Pin<&mut Self>,
                _cx: &mut std::task::Context<'_>,
            ) -> std::task::Poll<std::io::Result<()>> {
                std::task::Poll::Ready(Ok(()))
            }
        }

        let payload = make_payload(77, 128 * 1024);
        let (dest_ctrl_in, mut dest_ctrl_out) = tokio::io::duplex(4096);
        let (_src_sink, src_writer) = tokio::io::duplex(64);
        let mut source = custom_conn(
            "src",
            Box::new(std::io::Cursor::new(payload.clone())),
            Box::new(src_writer),
        );
        let mut dest = custom_conn("dst", Box::new(dest_ctrl_in), Box::new(NeverReadyWriter));

        let sender = tokio::spawn(async move {
            dest_ctrl_out.write_all(b"{\"busy\":true}\n").await.unwrap();
            dest_ctrl_out
                .write_all(b"{\"busy\": true}\n")
                .await
                .unwrap();
            let err_line =
                b"{\"error\":{\"code\":\"denied\",\"message\":\"revoked after heartbeat\"}}\n";
            for &b in err_line {
                dest_ctrl_out.write_all(&[b]).await.unwrap();
                tokio::task::yield_now().await;
            }
        });

        let sent = Sent::default();
        let mut meter = Meter::quiet(&sent);
        let broken = relay_with_timeout(
            &mut source,
            &mut dest,
            Amount::Exactly(payload.len() as u64),
            &mut meter,
            Duration::from_secs(5),
        )
        .await
        .expect_err("mid-stream destination error after busy heartbeats must fail relay");
        sender.await.unwrap();
        let explained = broken.explain(source, dest).await;
        assert_eq!(explained.code, ErrorCode::Denied, "{explained:?}");
        assert!(
            explained.message.contains("revoked after heartbeat"),
            "{explained:?}"
        );

        // 2. A stalled destination still times out within `idle_timeout` even if `{"busy": true}`
        // heartbeats keep arriving every second.
        let (dest_ctrl_in2, mut dest_ctrl_out2) = tokio::io::duplex(4096);
        let (_src_sink2, src_writer2) = tokio::io::duplex(64);
        let mut source2 = custom_conn(
            "src",
            Box::new(std::io::Cursor::new(payload.clone())),
            Box::new(src_writer2),
        );
        let mut dest2 = custom_conn("dst", Box::new(dest_ctrl_in2), Box::new(NeverReadyWriter));
        let hb_task = tokio::spawn(async move {
            for _ in 0..10 {
                tokio::time::sleep(Duration::from_secs(1)).await;
                if dest_ctrl_out2
                    .write_all(b"{\"busy\":true}\n")
                    .await
                    .is_err()
                {
                    break;
                }
            }
        });
        let started = tokio::time::Instant::now();
        let mut meter2 = Meter::quiet(&sent);
        let stalled = relay_with_timeout(
            &mut source2,
            &mut dest2,
            Amount::Exactly(payload.len() as u64),
            &mut meter2,
            Duration::from_secs(4),
        )
        .await
        .expect_err("stalled writer must time out despite busy heartbeats");
        assert!(
            matches!(stalled, Broken::Stalled { source: false }),
            "expected destination stall"
        );
        assert_eq!(started.elapsed(), Duration::from_secs(4));
        hb_task.abort();
    }
}
