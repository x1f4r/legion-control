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
use std::collections::{HashMap, HashSet};
use std::fs::{self, FileTimes};
use std::io::{self, BufRead, BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::guard::{Expected, Guarded, Watch};
use legion_core::audit::{self, AuditLog, Entry as AuditEntry, Outcome};
use legion_core::config::ConfigStore;
use legion_core::policy::Policy;
use legion_core::{Access, Door, Grant, Paths};
use legion_proto::{Effect, ErrorBody, ErrorCode, ToolSpec};
use legion_tools::xfer::{error as xfer_error, manifest, resolve_root};
use legion_xfer::{
    CaseCollision, Entry, EntryKind, Hash, ManifestOptions, SkippedEntry, Timestamp,
    resolve_relative_path,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// The version of this exchange: a header names it, and a node refuses one it does not speak.
pub const STREAM_PROTOCOL: u32 = 1;

/// How often an operation at work on something long says so, where a reply line is awaited.
pub const HEARTBEAT: Duration = Duration::from_secs(10);

/// The operations this node knows, as `unsupported` names them.
const OPS: [&str; 7] = ["manifest", "stat", "get", "put", "pack", "unpack", "delete"];

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
    },
    Delete {
        root: String,
        names: Vec<String>,
    },
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
        "manifest" | "stat" | "get" | "pack" => Some(("fs.read", Effect::Read)),
        "put" | "unpack" => Some(("fs.write", Effect::Write)),
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
}

/// The line that ends a `manifest`.
#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct ManifestDone {
    pub done: bool,
    pub entries: u64,
    #[serde(default)]
    pub skipped: Vec<SkippedEntry>,
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
    pub skipped: Vec<SkippedEntry>,
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

/// How an operation failed: in time to say so on stdout, or after its raw bytes had started.
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
pub fn serve<R: Read, W: Write>(
    host: &Host,
    pinned: Option<&str>,
    input: R,
    output: W,
) -> Result<(), ErrorBody> {
    let started = Instant::now();
    let mut input = BufReader::new(input);
    let mut output = BufWriter::with_capacity(OUTPUT_BUFFER, output);
    // Said before anything else, refusals included, so a reader can tell this node's replies
    // from those of one too old to hold to what it asks.
    let _ =
        line(&mut output, &json!({ "protocol": STREAM_PROTOCOL })).and_then(|()| output.flush());
    let (header, recorded) = match read_header(&mut input) {
        Ok(read) => read,
        Err(error) => {
            let error = error.with_machine(host.name.clone());
            let _ = reply_error(&mut output, &error);
            return Err(error);
        }
    };
    let tool = format!("stream.{}", header.op.name());
    let config = ConfigStore::new(host.paths.config_dir());
    let (principal, result) = match authorise(host, &config, pinned, &header) {
        Ok(grant) => {
            let principal = grant.principal().to_owned();
            (
                Some(principal),
                run(host, &grant, header.op, &mut input, &mut output),
            )
        }
        Err((principal, error)) => (principal, Err(Failure::Reply(error))),
    };
    let result = match result {
        Ok(()) => output.flush().map_err(|error| {
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
    record(host, principal, &tool, recorded, started, &result);
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
    header: &Header,
) -> Result<Grant, (Option<String>, ErrorBody)> {
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

fn record(
    host: &Host,
    principal: Option<String>,
    tool: &str,
    args: Value,
    started: Instant,
    result: &Result<(), ErrorBody>,
) {
    let entry = AuditEntry {
        time: audit::now(),
        principal,
        door: Door::Stream,
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

fn run<R: BufRead, W: Write>(
    host: &Host,
    grant: &Grant,
    op: Op,
    input: &mut R,
    output: &mut W,
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
        } => stat(grant, &path, checksum, if_size, output),
        Op::Get { path, offset } => get(grant, &path, offset, output),
        Op::Put {
            path,
            size,
            hash,
            mode,
            mtime,
        } => put(grant, &path, size, &hash, mode, mtime, input, output),
        Op::Pack { root, names } => pack(grant, &root, &names, output),
        Op::Unpack { root, entries } => unpack(grant, &root, &entries, input, output),
        Op::Delete { root, names } => delete(grant, &root, &names, output),
    }
}

fn line<W: Write, T: Serialize>(output: &mut W, value: &T) -> io::Result<()> {
    serde_json::to_writer(&mut *output, value).map_err(io::Error::other)?;
    output.write_all(b"\n")
}

fn reply<W: Write, T: Serialize>(output: &mut W, value: &T) -> Result<(), Failure> {
    line(output, value)
        .and_then(|()| output.flush())
        .map_err(|error| Failure::Raw(gone(&error)))
}

fn reply_error<W: Write>(output: &mut W, error: &ErrorBody) -> io::Result<()> {
    line(output, &json!({ "error": error }))?;
    output.flush()
}

fn gone(error: &io::Error) -> ErrorBody {
    ErrorBody::new(
        ErrorCode::Failed,
        format!("The other end of the stream went away: {error}."),
    )
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
    let options = ManifestOptions { checksum, exclude };
    let listing = busy(output, || manifest(grant, &host.paths, &root, &options))??;
    write_listing(output, listing.entries, listing.skipped)
        .map_err(|error| Failure::Raw(gone(&error)))
}

fn write_listing<W: Write>(
    output: &mut W,
    entries: Vec<Entry>,
    skipped: Vec<SkippedEntry>,
) -> io::Result<()> {
    for entry in &entries {
        line(output, entry)?;
    }
    line(
        output,
        &ManifestDone {
            done: true,
            entries: entries.len() as u64,
            skipped,
        },
    )?;
    output.flush()
}

fn stat<W: Write>(
    grant: &Grant,
    path: &str,
    checksum: bool,
    if_size: Option<u64>,
    output: &mut W,
) -> Result<(), Failure> {
    // The partial file a `put` to this path would continue sits beside the path as given, a link
    // kept as a link, and is only looked at when it may be read itself.
    let given = grant.check_path(path, Access::Read)?;
    let partial = || {
        legion_xfer::part_path(&given)
            .ok()
            .filter(|part| grant.check_path(part, Access::Read).is_ok())
            .and_then(|_| legion_xfer::resume_offset(&given).ok())
            .filter(|len| *len > 0)
    };
    let path = resolve_root(grant, path, Access::Read)?;
    let found = match fs::symlink_metadata(&path) {
        Ok(meta) => meta,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return reply(
                output,
                &StatReply {
                    kind: "missing".to_owned(),
                    partial: partial(),
                    case_insensitive: case_insensitive(&path),
                    ..StatReply::default()
                },
            );
        }
        Err(error) => return Err(Failure::Reply(io_error("look at", &path, &error))),
    };
    let kind = found.file_type();
    let answer = if kind.is_dir() {
        StatReply {
            kind: "dir".to_owned(),
            case_insensitive: case_insensitive(&path),
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
            case_insensitive: None,
        }
    } else {
        StatReply {
            kind: if kind.is_symlink() {
                "symlink"
            } else {
                "other"
            }
            .to_owned(),
            ..StatReply::default()
        }
    };
    reply(output, &answer)
}

/// Whether the file system holding `path`, or the nearest directory above it that exists,
/// ignores letter case, told without writing anything: a name there, one of its entries or of the
/// directories above it, is looked up again with the case of its letters flipped, and the two are
/// the same file only where case is ignored. `None` when no name nearby has a letter to flip.
fn case_insensitive(path: &Path) -> Option<bool> {
    let dir = path
        .ancestors()
        .find(|dir| fs::metadata(dir).is_ok_and(|meta| meta.is_dir()))?;
    let inside = fs::read_dir(dir)
        .into_iter()
        .flatten()
        .take(64)
        .filter_map(Result::ok)
        .map(|entry| entry.path());
    let above = dir.ancestors().map(Path::to_path_buf);
    for candidate in inside.chain(above) {
        // A link and what it leads to are the same file whatever their names.
        if fs::symlink_metadata(&candidate).is_ok_and(|meta| meta.file_type().is_symlink()) {
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
            .map(|c| {
                if c.is_ascii_lowercase() {
                    c.to_ascii_uppercase()
                } else {
                    c.to_ascii_lowercase()
                }
            })
            .collect();
        if flipped == name {
            continue;
        }
        let twin = parent.join(flipped);
        return Some(
            fs::symlink_metadata(&twin).is_ok()
                && same_file::is_same_file(&candidate, &twin).unwrap_or(false),
        );
    }
    None
}

fn get<W: Write>(grant: &Grant, path: &str, offset: u64, output: &mut W) -> Result<(), Failure> {
    let path = grant.check_path(path, Access::Read)?;
    let source = legion_xfer::open_get(&path, offset).map_err(xfer_error)?;
    reply(
        output,
        &GetReady {
            size: source.size(),
        },
    )?;
    source
        .stream_to(&mut *output)
        .map_err(|error| Failure::Raw(xfer_error(error)))?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn put<R: BufRead, W: Write>(
    grant: &Grant,
    path: &str,
    size: u64,
    hash: &str,
    mode: Option<u32>,
    mtime: Option<Timestamp>,
    input: &mut R,
    output: &mut W,
) -> Result<(), Failure> {
    let target = grant.check_path(path, Access::Write)?;
    // The partial file beside the target is created, continued, truncated and removed, so it is
    // held to what the target is: beside a role's root, it is outside it.
    let part = legion_xfer::part_path(&target).map_err(xfer_error)?;
    grant.check_path(&part, Access::Write)?;
    let hash: Hash = hash.parse().map_err(xfer_error)?;
    if fs::symlink_metadata(&target).is_ok_and(|meta| meta.is_dir()) {
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
    if let Some(parent) = target.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent)
            .map_err(|error| Failure::Reply(io_error("create", parent, &error)))?;
    }
    // The sender's offset is only a hint: what is on disk decides, so a sender that lost track
    // of an earlier attempt still continues it rather than starting over.
    let mut offset = legion_xfer::resume_offset(&target).map_err(xfer_error)?;
    if offset > size {
        offset = 0;
    }
    reply(output, &PutReady { offset })?;
    let done = feed(input, Some(size - offset), output, |fed| {
        legion_xfer::put(&target, size, hash, offset, fed, mode)
    })?
    .map_err(xfer_error)?;
    // The file is whole and in place; a time that cannot be set leaves it as it is, and a later
    // listing just sees it as changed.
    if let Some(time) = mtime.and_then(system_time) {
        let _ = fs::set_times(&target, FileTimes::new().set_modified(time));
    }
    reply(
        output,
        &PutDone {
            ok: true,
            size: done.size,
            hash: done.hash,
        },
    )
}

fn pack<W: Write>(
    grant: &Grant,
    root: &str,
    names: &[String],
    output: &mut W,
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
    legion_xfer::pack(&root, &keep, &mut *output)
        .map_err(|error| Failure::Raw(xfer_error(error)))?;
    Ok(())
}

fn unpack<R: BufRead, W: Write>(
    grant: &Grant,
    root: &str,
    entries: &[Planned],
    input: &mut R,
    output: &mut W,
) -> Result<(), Failure> {
    let root = resolve_root(grant, root, Access::Write)?;
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
    let (report, mut refused, violation) = feed(input, None, output, |fed| {
        let watch = Rc::new(RefCell::new(Watch::new(plan)));
        let mut refused: HashMap<String, String> = HashMap::new();
        let guarded = Guarded::new(fed, Rc::clone(&watch));
        let report = legion_xfer::unpack_filtered(&root, guarded, |rel, kind| {
            let mut watch = watch.borrow_mut();
            // The entry the unpack is about to write must be the one whose header was checked.
            let checked_entry = watch.current.take();
            if checked_entry.is_none_or(|(name, checked_kind)| name != rel || checked_kind != kind)
            {
                let _ = watch.refuse(rel, "is not the entry whose header was checked".to_owned());
                return false;
            }
            drop(watch);
            match checked(grant, &root, rel, Access::Write) {
                Ok(_) => true,
                Err(error) => {
                    refused.insert(rel.to_owned(), error.message);
                    false
                }
            }
        });
        let watch = watch.borrow();
        let mut violation = watch.refused.clone();
        if violation.is_none()
            && let Ok(report) = &report
        {
            violation = unaccounted(&watch, report);
        }
        (report, refused, violation)
    })?;
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
    reply(
        output,
        &UnpackDone {
            ok: true,
            files: report.files.len() as u64,
            dirs: report.dirs.len() as u64,
            symlinks: report.symlinks.len() as u64,
            bytes: report.bytes,
            skipped,
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

fn delete<W: Write>(
    grant: &Grant,
    root: &str,
    names: &[String],
    output: &mut W,
) -> Result<(), Failure> {
    let root = resolve_root(grant, root, Access::Write)?;
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
            match remove_one(grant, &root, &name) {
                Ok(true) => done.removed += 1,
                Ok(false) => {}
                Err(reason) => done.skipped.push(SkippedEntry { path: name, reason }),
            }
        }
    })?;
    done.skipped.sort();
    reply(output, &done)
}

/// Removes one name under `root`, never following a symbolic link; says whether there was
/// anything to remove, or why it was left.
fn remove_one(grant: &Grant, root: &Path, name: &str) -> Result<bool, String> {
    let path = match named(grant, root, name, Access::Write) {
        Ok(path) => path,
        Err(Named::Missing) => return Ok(false),
        Err(Named::Refused(reason)) => return Err(reason),
    };
    let meta = match fs::symlink_metadata(&path) {
        Ok(meta) => meta,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error.to_string()),
    };
    let kind = meta.file_type();
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

/// Like [`busy`], for work that reads the operation's input: this thread passes the input on in
/// chunks (`limit` bytes at most, or to its end) while the work runs on its own, then says it is
/// busy until the work is done. The input itself stays on this thread, which a locked standard
/// input needs. Once the work stops reading, at most [`MOST_TRAILING`] more bytes are read, so a
/// sender's last write finds a reader and a sender that goes on finds the pipe closed.
fn feed<R: BufRead, W: Write, T: Send>(
    input: &mut R,
    limit: Option<u64>,
    output: &mut W,
    work: impl FnOnce(Fed) -> T + Send,
) -> Result<T, Failure> {
    std::thread::scope(|scope| {
        let (chunks, fed) = mpsc::sync_channel::<Vec<u8>>(QUEUED_CHUNKS);
        let (done, finished) = mpsc::channel();
        scope.spawn(move || {
            let _ = done.send(work(Fed {
                chunks: fed,
                chunk: Vec::new(),
                at: 0,
            }));
        });
        let mut left = limit.unwrap_or(u64::MAX);
        let mut trailing = 0u64;
        let mut taking = true;
        while left > 0 && trailing <= MOST_TRAILING {
            let available = match input.fill_buf() {
                Ok([]) | Err(_) => break,
                Ok(available) => available,
            };
            let take = usize::try_from(left)
                .unwrap_or(usize::MAX)
                .min(available.len());
            if taking && chunks.send(available[..take].to_vec()).is_err() {
                taking = false;
            }
            if !taking {
                trailing += take as u64;
            }
            input.consume(take);
            left -= take as u64;
        }
        drop(chunks);
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
                    .and_then(|()| output.flush())
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
}

impl Read for Fed {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        while self.at == self.chunk.len() {
            match self.chunks.recv() {
                Ok(chunk) => {
                    self.chunk = chunk;
                    self.at = 0;
                }
                Err(_) => return Ok(0),
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

/// A relative name under `root`, validated as a transfer path and checked for `access`.
fn checked(grant: &Grant, root: &Path, name: &str, access: Access) -> Result<PathBuf, ErrorBody> {
    let path = resolve_relative_path(root, name).map_err(xfer_error)?;
    grant.check_path(&path, access)
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
