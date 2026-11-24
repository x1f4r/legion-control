//! `legion stream`: the pipe bulk file data moves over.
//!
//! JSON carries control and never bulk bytes, so a transfer opens a second pipe per operation. Its
//! first line on stdin is one JSON object, the header, saying what to do; raw bytes then move in the
//! direction the operation needs. The remote command line is always the fixed `legion stream`, and
//! every path and value rides inside the header, so nothing a caller supplies is ever quoted for a
//! remote shell.
//!
//! Every reply starts with one JSON line, so a client can skip a login banner before it and tell a
//! refusal (`{"error": …}`) from data:
//!
//! | `op` | Header | Reply |
//! | --- | --- | --- |
//! | `manifest` | `{root, checksum?, exclude?}` | one line per entry, then `{done, entries, skipped}` |
//! | `stat` | `{path, checksum?, if_size?}` | `{kind, size, mode?, hash?, partial?}` |
//! | `get` | `{path, offset?}` | `{size}`, then the bytes from `offset` to the end |
//! | `put` | `{path, size, hash, offset?, mode?}` | `{offset}`; reads the rest; `{ok, size, hash}` |
//! | `pack` | `{root, names}` | `{ready, skipped}`, then a tar stream of those names |
//! | `unpack` | `{root}` | `{ready}`; reads a tar stream to its end; `{ok, files, …}` |
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

use std::fs;
use std::io::{self, BufRead, BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::time::Instant;

use legion_core::audit::{self, AuditLog, Entry as AuditEntry, Outcome};
use legion_core::config::ConfigStore;
use legion_core::policy::Policy;
use legion_core::{Access, Door, Grant, Paths};
use legion_proto::{Effect, ErrorBody, ErrorCode, ToolSpec};
use legion_tools::xfer::{error as xfer_error, manifest, resolve_root};
use legion_xfer::{Entry, Hash, ManifestOptions, SkippedEntry, resolve_relative_path};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// The longest header read. A header carries the names a `pack` or `delete` acts on, which a
/// transfer sends in batches well below this; anything longer is a client gone wrong.
pub const LONGEST_HEADER: usize = 64 * 1024 * 1024;

/// How much output is gathered before it is written: a file's bytes go out in large writes, and
/// a listing's lines are not written one by one.
const OUTPUT_BUFFER: usize = 64 * 1024;

/// How much of the input an `unpack` reads past the end of its archive before it stops waiting
/// for the sender to finish: the padding a tar writer adds is a few blocks.
const MOST_TRAILING: u64 = 1024 * 1024;

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
    /// A sender's `offset` is not read: what is on disk decides where it starts.
    Put {
        path: String,
        size: u64,
        hash: String,
        #[serde(default)]
        mode: Option<u32>,
    },
    Pack {
        root: String,
        names: Vec<String>,
    },
    Unpack {
        root: String,
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
        match self {
            Op::Manifest { .. } | Op::Stat { .. } | Op::Get { .. } | Op::Pack { .. } => {
                ("fs.read", Effect::Read)
            }
            Op::Put { .. } | Op::Unpack { .. } => ("fs.write", Effect::Write),
            Op::Delete { .. } => ("fs.remove", Effect::Destroy),
        }
    }
}

/// What `stat` says about a path.
#[derive(Debug, Serialize, Deserialize)]
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
    /// The length of a partial file a `put` left beside it, which the next one continues from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub partial: Option<u64>,
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
        } => put(grant, &path, size, &hash, mode, input, output),
        Op::Pack { root, names } => pack(grant, &root, &names, output),
        Op::Unpack { root } => unpack(grant, &root, input, output),
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
    let listing = manifest(
        grant,
        &host.paths,
        &root,
        &ManifestOptions { checksum, exclude },
    )?;
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
    let path = resolve_root(grant, path, Access::Read)?;
    let partial = || {
        legion_xfer::resume_offset(&path)
            .ok()
            .filter(|len| *len > 0)
    };
    let found = match fs::symlink_metadata(&path) {
        Ok(meta) => meta,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return reply(
                output,
                &StatReply {
                    kind: "missing".to_owned(),
                    size: 0,
                    mode: None,
                    hash: None,
                    partial: partial(),
                },
            );
        }
        Err(error) => return Err(Failure::Reply(io_error("look at", &path, &error))),
    };
    let kind = found.file_type();
    let answer = if kind.is_dir() {
        StatReply {
            kind: "dir".to_owned(),
            size: 0,
            mode: None,
            hash: None,
            partial: None,
        }
    } else if kind.is_file() {
        let hash = checksum && if_size.is_none_or(|size| size == found.len());
        let listed = legion_xfer::manifest::build(
            &path,
            &ManifestOptions {
                checksum: hash,
                exclude: Vec::new(),
            },
        )
        .map_err(xfer_error)?;
        let entry = listed.entries.into_iter().next();
        StatReply {
            kind: "file".to_owned(),
            size: entry.as_ref().map_or(found.len(), |entry| entry.size),
            mode: entry.as_ref().and_then(|entry| entry.mode),
            hash: entry.and_then(|entry| entry.hash),
            partial: partial(),
        }
    } else {
        StatReply {
            kind: if kind.is_symlink() {
                "symlink"
            } else {
                "other"
            }
            .to_owned(),
            size: 0,
            mode: None,
            hash: None,
            partial: None,
        }
    };
    reply(output, &answer)
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

fn put<R: BufRead, W: Write>(
    grant: &Grant,
    path: &str,
    size: u64,
    hash: &str,
    mode: Option<u32>,
    input: &mut R,
    output: &mut W,
) -> Result<(), Failure> {
    let target = grant.check_path(path, Access::Write)?;
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
    let done = legion_xfer::put(&target, size, hash, offset, input, mode).map_err(xfer_error)?;
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
        match checked(grant, &root, name, Access::Read) {
            Ok(path) => match fs::symlink_metadata(&path) {
                Ok(_) => keep.push(name.as_str()),
                Err(error) => ready.skipped.push(SkippedEntry {
                    path: name.clone(),
                    reason: format!("it cannot be read since it was listed: {error}"),
                }),
            },
            Err(error) => ready.skipped.push(SkippedEntry {
                path: name.clone(),
                reason: error.message,
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
    input: &mut R,
    output: &mut W,
) -> Result<(), Failure> {
    let root = resolve_root(grant, root, Access::Write)?;
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
    let mut refused: Vec<SkippedEntry> = Vec::new();
    let report = legion_xfer::unpack_filtered(&root, &mut *input, |rel, _| {
        match checked(grant, &root, rel, Access::Write) {
            Ok(_) => true,
            Err(error) => {
                refused.push(SkippedEntry {
                    path: rel.to_owned(),
                    reason: error.message,
                });
                false
            }
        }
    })
    .map_err(xfer_error)?;
    // What follows the archive's end is the writer's padding; reading it lets the sender finish
    // its last write instead of finding the pipe closed.
    let _ = io::copy(&mut input.take(MOST_TRAILING), &mut io::sink());
    let mut skipped: Vec<SkippedEntry> = report
        .skipped
        .into_iter()
        .map(|entry| {
            match refused
                .iter()
                .position(|refusal| refusal.path == entry.path)
            {
                Some(at) => refused.swap_remove(at),
                None => entry,
            }
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
        },
    )
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
    for (_, name) in valid {
        match remove_one(grant, &root, &name) {
            Ok(true) => done.removed += 1,
            Ok(false) => {}
            Err(reason) => done.skipped.push(SkippedEntry { path: name, reason }),
        }
    }
    done.skipped.sort();
    reply(output, &done)
}

/// Removes one name under `root`, never following a symbolic link; says whether there was
/// anything to remove, or why it was left.
fn remove_one(grant: &Grant, root: &Path, name: &str) -> Result<bool, String> {
    let path = checked(grant, root, name, Access::Write).map_err(|error| error.message)?;
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
