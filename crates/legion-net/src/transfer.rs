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

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use legion_core::{BoxFuture, Ctx, Registry, Tool, ToolError, input_schema, parse_args};
use legion_proto::{Effect, ErrorBody, ErrorCode, EventKind, ToolSpec};
use legion_xfer::{Entry, Hash, Manifest, PlanConflict, PlanOptions, SkippedEntry};
use schemars::JsonSchema;
use serde::Deserialize;
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::task::JoinHandle;
use tokio_util::io::SyncIoBridge;

use crate::config::{FleetFile, validate_machine_name};
use crate::connector::{PipeHandle, Subcommand};
use crate::pool::Fleet;
use crate::stream::{
    self, DeleteDone, GetReady, Host, ManifestDone, PackReady, PutDone, PutReady, StatReply,
    UnpackDone, UnpackReady,
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

/// The shortest time between two `progress` events.
const PROGRESS_EVERY: Duration = Duration::from_millis(250);

/// How long an ended stream is given to exit and say why on stderr.
const EXIT_WAIT: Duration = Duration::from_secs(2);

/// How many paths a result lists in each of its lists; the counts are always whole.
const MOST_LISTED: usize = 100;

/// Adds `fs.transfer`, which holds the fleet's session pool to reach the other ends.
pub fn register(registry: &mut Registry, fleet: Fleet) {
    registry.add(TransferTool {
        spec: ToolSpec {
            name: "fs.transfer".to_owned(),
            description: "Copy a file or a directory tree between machines, or within one. Each of from and to is machine:/path or a path on this node; to names the destination itself, except that a file sent to an existing directory (or to a path ending in /) lands inside it under its own name. A file resumes where an interrupted transfer stopped and is renamed into place only once its BLAKE3 hash matches. A tree sends only what differs (size and modification time, or hash with checksum), so a second run sends nothing; delete removes what the source no longer has, exclude leaves paths out at both ends, and dry_run reports what would be sent and deleted without changing anything.".to_owned(),
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

/// Arguments of `fs.transfer`.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct TransferArgs {
    /// What to send: `machine:/path`, or a path on this node (absolute or starting with `~`).
    from: String,
    /// Where it goes, in the same form. It names the destination itself; a file sent to an
    /// existing directory lands inside it.
    to: String,
    /// Remove from the destination tree what the source no longer has.
    #[serde(default)]
    delete: bool,
    /// Compare files by BLAKE3 hash instead of size and modification time, which reads them all.
    #[serde(default)]
    checksum: bool,
    /// Globs of paths to leave out at both ends, relative to the tree; a trailing `/` matches
    /// directories only. What is left out at the destination is never deleted.
    #[serde(default)]
    exclude: Vec<String>,
    /// List and plan, and report what would be sent and deleted, without changing anything.
    #[serde(default)]
    dry_run: bool,
}

/// What `fs.transfer` returns.
#[derive(Debug, Default, Serialize)]
struct TransferReport {
    from: String,
    to: String,
    kind: &'static str,
    files_sent: u64,
    bytes_sent: u64,
    files_unchanged: u64,
    deleted: u64,
    conflicts: Vec<PlanConflict>,
    skipped: Vec<SkippedEntry>,
    #[serde(skip_serializing_if = "Option::is_none")]
    resumed_from: Option<u64>,
    duration_ms: u64,
    dry_run: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    would_send: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    would_delete: Option<Vec<String>>,
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

/// What has been sent so far, kept outside the transfer so a cancelled one can still say.
#[derive(Default)]
struct Sent {
    files: AtomicU64,
    bytes: AtomicU64,
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
    let work = job.run(&from, &to, &args);
    // Dropping the work closes every pipe it holds: a remote end sees its input end and its
    // process is killed, an end served here sees its pipe close, and neither renames anything
    // into place that was not complete.
    let mut report = tokio::select! {
        report = work => report?,
        () = ctx.cancelled() => {
            let files = sent.files.load(Ordering::Relaxed);
            let bytes = sent.bytes.load(Ordering::Relaxed);
            return Err(ErrorBody::new(
                ErrorCode::Cancelled,
                format!("The transfer from {} to {} was cancelled after {bytes} bytes and {files} files.", from.named(), to.named()),
            )
            .with_hint("Run it again to continue: a file resumes where it stopped and a tree sends only what is still missing.")
            .with_details(json!({"files_sent": files, "bytes_sent": bytes})));
        }
    };
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
            "dir" => self.tree(from, to, args).await,
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
        let mut report = TransferReport {
            from: from.named(),
            to: target.named(),
            kind: "file",
            ..TransferReport::default()
        };
        match there.kind.as_str() {
            "file" if there.size == size && there.hash == Some(hash) => {
                report.files_unchanged = 1;
                return Ok(report);
            }
            "file" | "missing" | "symlink" => {}
            _ => {
                return Err(ErrorBody::new(
                    ErrorCode::Conflict,
                    format!(
                        "{} on {} is a directory, and a file cannot replace it.",
                        target.path, target.machine
                    ),
                )
                .with_hint("Name a path inside it, or a different one.")
                .with_machine(target.machine.clone()));
            }
        }
        let partial = there.partial.filter(|len| *len <= size);
        if args.dry_run {
            report.files_sent = 1;
            report.bytes_sent = size - partial.unwrap_or(0);
            report.resumed_from = partial;
            report.would_send = Some(vec![target.path.clone()]);
            return Ok(report);
        }
        let (resumed, sent) = match self
            .send_file(from, &target, size, hash, source.mode, partial)
            .await
        {
            Err(error) if partial.is_some() && error.code == ErrorCode::Conflict => {
                // What an earlier attempt left was not a prefix of this file (it changed since):
                // the destination dropped it when the hash did not match, so start over once.
                self.send_file(from, &target, size, hash, source.mode, None)
                    .await?
            }
            other => other?,
        };
        report.files_sent = 1;
        report.bytes_sent = sent;
        report.resumed_from = (resumed > 0).then_some(resumed);
        Ok(report)
    }

    /// Sends one file, returning the offset it continued from and the bytes it sent.
    async fn send_file(
        &self,
        from: &End,
        to: &End,
        size: u64,
        hash: Hash,
        mode: Option<u32>,
        hint: Option<u64>,
    ) -> Result<(u64, u64), ErrorBody> {
        let mut offset = hint.unwrap_or(0);
        // The source first: a source that refuses leaves nothing behind at the destination.
        let (mut source, ready) = self.get(from, offset).await?;
        if ready.size != size {
            source.abort().await;
            return Err(changed(from));
        }
        let mut header =
            json!({"op": "put", "path": to.path, "size": size, "hash": hash, "offset": offset});
        if let Some(mode) = mode {
            header["mode"] = json!(mode);
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
        let mut meter = Meter::new(self.ctx, self.sent, size - offset, 1, false);
        let relayed = relay(&mut source, &mut dest, Some(size - offset), &mut meter).await;
        if let Err(broken) = relayed {
            return Err(broken.explain(source, dest).await);
        }
        let done = dest.first::<PutDone>().await;
        source.close().await;
        dest.close().await;
        let done = done?;
        if done.hash != hash {
            return Err(changed(from));
        }
        self.sent.files.fetch_add(1, Ordering::Relaxed);
        meter.emit(true);
        Ok((offset, size - offset))
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
        args: &TransferArgs,
    ) -> Result<TransferReport, ErrorBody> {
        let there: StatReply = self
            .request(to, json!({"op": "stat", "path": to.path}))
            .await?;
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
        let source = self.manifest(from, args).await?;
        let dest = if missing {
            Manifest::default()
        } else {
            self.manifest(to, args).await?
        };
        let plan = legion_xfer::plan::plan(
            &source,
            &dest,
            &PlanOptions {
                checksum: args.checksum,
                delete: args.delete,
            },
        );
        let mut report = TransferReport {
            from: from.named(),
            to: to.named(),
            kind: "tree",
            files_unchanged: plan.totals.files_unchanged,
            ..TransferReport::default()
        };
        let mut skipped: Vec<SkippedEntry> = Vec::new();
        skipped.extend(on(&from.machine, source.skipped));
        skipped.extend(on(&to.machine, dest.skipped));
        let conflicts = plan.conflicts.clone();
        if args.dry_run {
            report.files_sent = plan.totals.files_to_send;
            report.bytes_sent = plan.totals.bytes_to_send;
            report.deleted = plan.totals.entries_to_delete;
            report.would_send = Some(report.list(plan.send.clone()));
            report.would_delete = Some(report.list(plan.delete.clone()));
            report.conflicts = report.list(conflicts);
            report.skipped = report.list(skipped);
            return Ok(report);
        }
        for names in batches(&plan.delete) {
            let done: DeleteDone = self
                .request(to, json!({"op": "delete", "root": to.path, "names": names}))
                .await?;
            report.deleted += done.removed;
            skipped.extend(on(&to.machine, done.skipped));
        }
        let mut meter = Meter::new(
            self.ctx,
            self.sent,
            plan.totals.bytes_to_send,
            plan.totals.files_to_send,
            true,
        );
        // A destination that does not exist yet is made even when there is nothing to put in it,
        // so an empty tree arrives as an empty directory.
        let mut rounds = batches(&plan.send);
        if rounds.is_empty() && missing {
            rounds.push(&[]);
        }
        for names in rounds {
            let done = self
                .pack_unpack(from, to, names, &mut meter, &mut skipped)
                .await?;
            report.files_sent += done.files;
            report.bytes_sent += done.bytes;
            skipped.extend(on(&to.machine, done.skipped));
        }
        meter.emit(true);
        report.conflicts = report.list(conflicts);
        report.skipped = report.list(skipped);
        Ok(report)
    }

    async fn pack_unpack(
        &self,
        from: &End,
        to: &End,
        names: &[String],
        meter: &mut Meter<'_>,
        skipped: &mut Vec<SkippedEntry>,
    ) -> Result<UnpackDone, ErrorBody> {
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
        skipped.extend(on(&from.machine, ready.skipped));
        let mut dest = match self
            .open(to, json!({"op": "unpack", "root": to.path}))
            .await
        {
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
        if let Err(broken) = relay(&mut source, &mut dest, None, meter).await {
            return Err(broken.explain(source, dest).await);
        }
        // The archive ended; it is whole only if the source says it finished.
        if let Err(error) = source.finish().await {
            dest.abort().await;
            return Err(error);
        }
        dest.end_input().await;
        let done = dest.first::<UnpackDone>().await;
        dest.close().await;
        done
    }

    async fn manifest(&self, end: &End, args: &TransferArgs) -> Result<Manifest, ErrorBody> {
        let mut conn = self
            .open(
                end,
                json!({"op": "manifest", "root": end.path, "checksum": args.checksum, "exclude": args.exclude}),
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
        Conn::open(self.fleet, self.ctx, end, header).await
    }
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
    process: Process,
    /// Whether a reply has been read, which tells a server that never started from one that
    /// failed on the way.
    answered: bool,
    noise_lines: usize,
    noise_bytes: usize,
}

/// One line of a `manifest` reply.
#[derive(Deserialize)]
#[serde(untagged)]
enum Listed {
    Done(ManifestDone),
    Entry(Entry),
}

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
        end: &End,
        mut header: Value,
    ) -> Result<Conn, ErrorBody> {
        let (reader, writer, process): (BoxRead, BoxWrite, Process) = if end.remote {
            // The other node decides whether the caller may act as this principal, as it does
            // for a forwarded call.
            header["as"] = json!(ctx.principal());
            let pipe = fleet
                .open_pipe(ctx, &end.machine, Subcommand::Stream)
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
            process,
            answered: false,
            noise_lines: 0,
            noise_bytes: 0,
        };
        let mut line = header.to_string().into_bytes();
        line.push(b'\n');
        if let Err(error) = conn.write(&line).await {
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

    /// The next reply, which has the shape `T` or is the other end's error. Before the first
    /// one, lines that are neither (a login banner, a shell's own noise, even one that happens
    /// to be JSON) are skipped within bounds; after it, anything else is the other end gone wrong.
    async fn first<T: DeserializeOwned>(&mut self) -> Result<T, ErrorBody> {
        loop {
            let line = self.line().await?;
            if let Ok(value @ Value::Object(_)) = serde_json::from_slice::<Value>(&line) {
                if let Some(error) = value.get("error")
                    && let Ok(error) = serde_json::from_value::<ErrorBody>(error.clone())
                {
                    self.answered = true;
                    return Err(with_machine(error, &self.machine));
                }
                if let Ok(reply) = serde_json::from_value::<T>(value) {
                    self.answered = true;
                    return Ok(reply);
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
            self.noise_lines += 1;
            self.noise_bytes += line.len();
            if self.noise_lines > MOST_NOISE_LINES || self.noise_bytes > MOST_NOISE_BYTES {
                return Err(ErrorBody::new(
                    ErrorCode::Unreachable,
                    format!(
                        "{} printed {} lines that are not legion's before answering a stream.",
                        self.machine, self.noise_lines
                    ),
                )
                .with_hint("Something on the way (a login script, a banner) writes to stdout; make it quiet for sessions without a terminal.")
                .with_machine(self.machine.clone()));
            }
        }
    }

    /// One line of output, without reading more than a reply can be.
    async fn line(&mut self) -> Result<Vec<u8>, ErrorBody> {
        let mut line = Vec::new();
        let read = (&mut self.reader)
            .take(LONGEST_REPLY as u64 + 1)
            .read_until(b'\n', &mut line)
            .await;
        match read {
            Ok(0) => Err(self.ended("before it answered").await),
            Ok(_) if line.len() > LONGEST_REPLY => Err(ErrorBody::new(
                ErrorCode::Failed,
                format!(
                    "{} answered a stream with a line longer than {LONGEST_REPLY} bytes.",
                    self.machine
                ),
            )
            .with_machine(self.machine.clone())),
            Ok(_) => Ok(line),
            Err(error) => Err(self.ended(&format!("while it answered ({error})")).await),
        }
    }

    /// Reads a `manifest` reply: one entry per line, then the line that ends it.
    async fn manifest(&mut self) -> Result<Manifest, ErrorBody> {
        let mut manifest = Manifest::default();
        loop {
            match self.first::<Listed>().await? {
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
        match &mut self.process {
            Process::Local(task) => {
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
            Process::Remote(handle) => {
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
        let result = match &mut self.process {
            Process::Local(task) => match task.take() {
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
            Process::Remote(handle) => match handle.collect_exit_info(EXIT_WAIT).await {
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
        match self.process {
            Process::Remote(handle) => handle.close(EXIT_WAIT).await,
            Process::Local(Some(task)) => {
                drop(self.reader);
                let _ = tokio::time::timeout(EXIT_WAIT, task).await;
            }
            Process::Local(None) => {}
        }
    }

    /// Stops the server now. Its input ends without the rest of what it expected, so nothing it
    /// was writing is renamed into place.
    async fn abort(self) {
        drop(self.writer);
        drop(self.reader);
        match self.process {
            Process::Remote(handle) => handle.close(Duration::ZERO).await,
            Process::Local(Some(task)) => {
                let _ = tokio::time::timeout(EXIT_WAIT, task).await;
            }
            Process::Local(None) => {}
        }
    }
}

fn with_machine(error: ErrorBody, machine: &str) -> ErrorBody {
    if error.machine.is_some() {
        error
    } else {
        error.with_machine(machine)
    }
}

/// Where a relay broke.
enum Broken {
    /// The source's output failed or ended before it should have.
    Source(String),
    /// The destination stopped taking input.
    Destination(String),
}

impl Broken {
    /// The error to report, preferring what the end that broke said itself.
    async fn explain(self, mut source: Conn, mut dest: Conn) -> ErrorBody {
        match self {
            Broken::Source(what) => {
                let error = source.ended(&what).await;
                source.abort().await;
                dest.abort().await;
                error
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

/// Copies the source's output to the destination's input in fixed chunks: `exact` bytes, or to
/// the end of the source's output.
async fn relay(
    source: &mut Conn,
    dest: &mut Conn,
    exact: Option<u64>,
    meter: &mut Meter<'_>,
) -> Result<(), Broken> {
    let mut buf = vec![0u8; CHUNK];
    let mut copied = 0u64;
    loop {
        let want = match exact {
            Some(total) => usize::try_from(total - copied).unwrap_or(CHUNK).min(CHUNK),
            None => CHUNK,
        };
        if want == 0 {
            break;
        }
        let read = source
            .reader
            .read(&mut buf[..want])
            .await
            .map_err(|error| Broken::Source(format!("while it sent ({error})")))?;
        if read == 0 {
            if exact.is_some() {
                return Err(Broken::Source(format!(
                    "after {copied} bytes, before the rest"
                )));
            }
            break;
        }
        dest.write(&buf[..read])
            .await
            .map_err(|error| Broken::Destination(format!("while it received ({error})")))?;
        copied += read as u64;
        meter.add(&buf[..read]);
    }
    Ok(())
}

/// Counts what passes and tells the caller a few times a second.
struct Meter<'a> {
    ctx: &'a Ctx,
    sent: &'a Sent,
    bytes_total: u64,
    files_total: u64,
    tar: Option<TarCounter>,
    last: Instant,
}

impl<'a> Meter<'a> {
    fn new(
        ctx: &'a Ctx,
        sent: &'a Sent,
        bytes_total: u64,
        files_total: u64,
        tar: bool,
    ) -> Meter<'a> {
        Meter {
            ctx,
            sent,
            bytes_total,
            files_total,
            tar: tar.then(TarCounter::default),
            last: Instant::now(),
        }
    }

    fn add(&mut self, chunk: &[u8]) {
        let (bytes, files) = match &mut self.tar {
            Some(tar) => tar.feed(chunk),
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
        self.ctx.emit(
            EventKind::Progress,
            json!({
                "bytes": self.sent.bytes.load(Ordering::Relaxed),
                "bytes_total": self.bytes_total,
                "files": self.sent.files.load(Ordering::Relaxed),
                "files_total": self.files_total,
            }),
        );
    }
}

/// Follows a tar stream's headers as it passes, so progress counts the files' own bytes and each
/// file once all of it has gone, whatever the archive adds around them.
#[derive(Default)]
struct TarCounter {
    header: Vec<u8>,
    data_left: u64,
    pad_left: u64,
    regular: bool,
}

impl TarCounter {
    const BLOCK: usize = 512;

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
                }
                self.data_left -= take as u64;
                chunk = &chunk[take..];
                if self.data_left == 0 && self.regular {
                    files += 1;
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
            self.regular = matches!(header[156], b'0' | 0 | b'7');
            self.data_left = size;
            self.pad_left = (Self::BLOCK as u64 - size % Self::BLOCK as u64) % Self::BLOCK as u64;
            if self.regular && size == 0 {
                files += 1;
            }
        }
        (content, files)
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
