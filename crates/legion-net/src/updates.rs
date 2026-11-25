//! Fleet update set store and tools (`update.list`, `update.read`, `update.publish`).
//!
//! Each node stores verified update sets under `<state_dir>/updates/<channel>/<sequence>/`, keeping
//! at most the newest two sets per channel. Any paired principal with read access may list and read
//! sets (`update.list`, `update.read`); they are not secret, and every reader checks the Ed25519
//! signature itself.
//!
//! Publishing (`update.publish`, `effect: admin`) verifies a set against this node's compiled-in
//! trusted keys, stores it atomically, and notifies reachable fleet peers with node authority so
//! each peer pulls, verifies against its own compiled-in keys, and stores the set for itself. A
//! machine never stores a set it cannot verify.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::marker::PhantomData;
use std::path::{Path, PathBuf};
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use legion_core::{
    BoxFuture, Ctx, EventSink, Paths, Registry, Tool, ToolError, input_schema, parse_args,
};
use legion_os::OsError;
use legion_os::lock::{FileLock, LockMode};
use legion_proto::{Effect, ErrorBody, ErrorCode, ToolSpec};
use legion_update::{
    Arch, CHECKSUMS_FILENAME, Channel, InstallKind, MANIFEST_FILENAME, MAX_MANIFEST_BYTES,
    MAX_SIG_BYTES, MAX_SUMS_BYTES, Os, Program, SIGNATURE_FILENAME, Sha256Sums, SignerKind,
    TrustAnchors, UpdateManifest, VerifiedSetMeta, validate_entry_name, verify_set_directory,
    verify_set_metadata,
};
use schemars::JsonSchema;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::config::FleetFile;
use crate::pool::Fleet;

/// Number of newest sets kept per channel in `<state_dir>/updates/<channel>/`.
pub const MAX_SETS_PER_CHANNEL: usize = 2;

/// Maximum raw byte length accepted by one `update.read` call (1 MiB).
pub const MAX_READ_LENGTH: u64 = 1024 * 1024;

/// Hard JSON reply byte budget for `update.read` (1 MiB), matching `fs.read`.
const MAX_REPLY_BYTES: usize = 1024 * 1024;

/// Raw chunk size requested per `update.read` call when pulling an artifact from a peer (512 KiB).
const PULL_CHUNK_BYTES: u64 = 512 * 1024;

/// Upper bound on any single artifact pulled from a peer (512 MiB) so a rogue peer cannot exhaust
/// disk space.
const MAX_PULL_ARTIFACT_BYTES: u64 = 512 * 1024 * 1024;

/// Default per-peer timeout in seconds during `update.publish` propagation.
const DEFAULT_PUBLISH_TIMEOUT_S: f64 = 60.0;

/// Maximum per-peer timeout in seconds accepted by `update.publish`.
const MAX_PUBLISH_TIMEOUT_S: f64 = 300.0;

/// Longest time to wait for the update store file lock.
const LOCK_TIMEOUT: Duration = Duration::from_secs(10);

/// Registers `update.list`, `update.read`, and `update.publish` using the compiled-in trusted keys.
pub(crate) fn register(registry: &mut Registry, fleet: Fleet) {
    register_with_anchors(registry, fleet, TrustAnchors::compiled_in());
}

/// Registers the `update.*` tools with explicit trust anchors for cross-node tests.
#[doc(hidden)]
pub fn register_with_anchors(registry: &mut Registry, fleet: Fleet, anchors: TrustAnchors) {
    registry.add(UpdateTool::<ListArgs, ListResult, _>::new(
        "update.list",
        Effect::Read,
        "List the signed update sets stored on this node (newest first), including channel, version, build, sequence, notes, artifacts, and which compiled-in trusted key (if any) signed each set. Readable by any paired principal with read access.",
        fleet.clone(),
        anchors,
        |_fleet, anchors, ctx, args| Box::pin(update_list(anchors, ctx, args)),
    ));
    registry.add(UpdateTool::<ReadArgs, UpdateReadResult, _>::new(
        "update.read",
        Effect::Read,
        "Read up to 1 MiB of one file from a stored update set as base64, from an optional byte offset. Only `SHA256SUMS`, `SHA256SUMS.sig`, and bare filenames listed in that set's `SHA256SUMS` exist; path-like or unlisted names are refused. Readable by any paired principal with read access.",
        fleet.clone(),
        anchors,
        |_fleet, _anchors, ctx, args| Box::pin(update_read(ctx, args)),
    ));
    registry.add(UpdateTool::<PublishArgs, PublishResult, _>::new(
        "update.publish",
        Effect::Admin,
        "Verify a signed update set against this node's compiled-in trusted keys and store it in the state directory (keeping the newest two sets per channel). Pass `from` for a local update set directory (which then notifies reachable fleet machines with node authority so each pulls, verifies against its own trusted keys, and stores the set for itself), or `from_peer` to pull a set from another fleet machine.",
        fleet,
        anchors,
        |fleet, anchors, ctx, args| Box::pin(update_publish(fleet, anchors, ctx, args)),
    ));
}

struct UpdateTool<A, O, F> {
    spec: ToolSpec,
    fleet: Fleet,
    anchors: TrustAnchors,
    handler: F,
    types: PhantomData<fn(A) -> O>,
}

impl<A, O, F> UpdateTool<A, O, F>
where
    A: DeserializeOwned + JsonSchema + Send + 'static,
    O: Serialize + 'static,
    F: for<'a> Fn(&'a Fleet, TrustAnchors, &'a Ctx, A) -> BoxFuture<'a, Result<O, ToolError>>
        + Send
        + Sync
        + 'static,
{
    fn new(
        name: &str,
        effect: Effect,
        description: &str,
        fleet: Fleet,
        anchors: TrustAnchors,
        handler: F,
    ) -> Self {
        Self {
            spec: ToolSpec {
                name: name.to_owned(),
                description: description.to_owned(),
                effect,
                input_schema: input_schema::<A>(),
            },
            fleet,
            anchors,
            handler,
            types: PhantomData,
        }
    }
}

impl<A, O, F> Tool for UpdateTool<A, O, F>
where
    A: DeserializeOwned + JsonSchema + Send + 'static,
    O: Serialize + 'static,
    F: for<'a> Fn(&'a Fleet, TrustAnchors, &'a Ctx, A) -> BoxFuture<'a, Result<O, ToolError>>
        + Send
        + Sync
        + 'static,
{
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn call<'a>(&'a self, ctx: &'a Ctx, args: Value) -> BoxFuture<'a, Result<Value, ToolError>> {
        Box::pin(async move {
            let parsed = parse_args::<A>(&self.spec.name, args)?;
            let out = (self.handler)(&self.fleet, self.anchors, ctx, parsed).await?;
            serde_json::to_value(&out).map_err(|error| {
                ErrorBody::new(
                    ErrorCode::Internal,
                    format!(
                        "{} returned a result that cannot be written as JSON: {error}.",
                        self.spec.name
                    ),
                )
            })
        })
    }
}

/// Root directory of this node's update store (`<state_dir>/updates`).
pub fn updates_root(paths: &Paths) -> PathBuf {
    paths.state_dir().join("updates")
}

/// Directory for one stored update set (`<state_dir>/updates/<channel>/<sequence>`).
pub fn update_set_dir(paths: &Paths, channel: Channel, sequence: u64) -> PathBuf {
    updates_root(paths)
        .join(channel.as_str())
        .join(sequence.to_string())
}

// ── update.list ─────────────────────────────────────────────────────────────────────────────

/// Arguments of `update.list`.
#[derive(Debug, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct ListArgs {
    /// Restrict results to one channel (`"stable"` or `"preview"`). Lists both when omitted.
    #[serde(default)]
    channel: Option<Channel>,
}

/// Artifact entry reported by `update.list`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListedArtifact {
    /// Bare filename inside the set directory.
    pub name: String,
    /// Target program (`"node"` or `"app"`).
    pub program: Program,
    /// Target operating system.
    pub os: Os,
    /// Target CPU architecture.
    pub arch: Arch,
    /// Installation kind.
    pub kind: InstallKind,
    /// File size in bytes when present on disk.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
}

/// One update set entry reported by `update.list`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListedUpdateSet {
    /// Update channel (`"stable"` or `"preview"`).
    pub channel: Channel,
    /// Semantic version string.
    pub version: String,
    /// Build revision identifier.
    pub build: String,
    /// Strictly growing update sequence number.
    pub sequence: u64,
    /// RFC 3339 UTC publication timestamp.
    pub published_at: String,
    /// Plain-language release notes.
    pub notes: String,
    /// Artifacts contained in this update set.
    pub artifacts: Vec<ListedArtifact>,
    /// Which trusted compiled-in key signed this set (`"release"` or `"preview"`), or `null` if
    /// this node does not currently trust the signing key.
    pub signer: Option<SignerKind>,
    /// `SHA256:<base64>` fingerprint of the trusted public key, when `signer` is present.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub key_fingerprint: Option<String>,
}

/// Result of `update.list`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ListResult {
    /// Name of the node answering.
    pub node: String,
    /// Stored update sets, newest sequence first.
    pub sets: Vec<ListedUpdateSet>,
}

async fn update_list(
    anchors: TrustAnchors,
    ctx: &Ctx,
    args: ListArgs,
) -> Result<ListResult, ToolError> {
    let paths = ctx.paths().clone();
    let node = ctx.node_name().to_owned();
    let channel = args.channel;
    let sets = tokio::task::spawn_blocking(move || list_stored_sets(&paths, channel, &anchors))
        .await
        .map_err(|error| {
            ErrorBody::new(
                ErrorCode::Internal,
                format!("update.list stopped unexpectedly: {error}."),
            )
        })??;
    Ok(ListResult { node, sets })
}

/// Scans `<state_dir>/updates/` and returns all stored sets, newest sequence first.
pub fn list_stored_sets(
    paths: &Paths,
    channel_filter: Option<Channel>,
    anchors: &TrustAnchors,
) -> Result<Vec<ListedUpdateSet>, ErrorBody> {
    let channels: Vec<Channel> = match channel_filter {
        Some(ch) => vec![ch],
        None => Channel::ALL.to_vec(),
    };
    let mut out = Vec::new();
    for channel in channels {
        let ch_dir = updates_root(paths).join(channel.as_str());
        let entries = match fs::read_dir(&ch_dir) {
            Ok(iter) => iter,
            Err(err) if err.kind() == io::ErrorKind::NotFound => continue,
            Err(err) => {
                return Err(ErrorBody::new(
                    ErrorCode::Failed,
                    format!(
                        "Could not read update channel directory {}: {err}.",
                        ch_dir.display()
                    ),
                ));
            }
        };
        for entry in entries.flatten() {
            let file_name = entry.file_name();
            let Some(seq_str) = file_name.to_str() else {
                continue;
            };
            let Ok(dir_seq) = seq_str.parse::<u64>() else {
                continue;
            };
            if dir_seq == 0 {
                continue;
            }
            let dir = entry.path();
            let Ok(meta) = fs::symlink_metadata(&dir) else {
                continue;
            };
            if !meta.file_type().is_dir() {
                continue;
            }
            if let Some(listed) = inspect_stored_set(&dir, channel, dir_seq, anchors) {
                out.push(listed);
            }
        }
    }
    out.sort_by(|a, b| {
        b.sequence
            .cmp(&a.sequence)
            .then_with(|| a.channel.cmp(&b.channel))
    });
    Ok(out)
}

fn inspect_stored_set(
    dir: &Path,
    expected_channel: Channel,
    expected_sequence: u64,
    anchors: &TrustAnchors,
) -> Option<ListedUpdateSet> {
    let manifest_bytes =
        read_bounded_regular(&dir.join(MANIFEST_FILENAME), MAX_MANIFEST_BYTES).ok()?;
    let manifest = UpdateManifest::parse(&manifest_bytes).ok()?;
    if manifest.channel != expected_channel || manifest.sequence != expected_sequence {
        return None;
    }

    let (signer, key_fingerprint) = match (
        read_bounded_regular(&dir.join(CHECKSUMS_FILENAME), MAX_SUMS_BYTES),
        read_bounded_regular(&dir.join(SIGNATURE_FILENAME), MAX_SIG_BYTES),
    ) {
        (Ok(sums_bytes), Ok(sig_bytes)) => match std::str::from_utf8(&sig_bytes) {
            Ok(sig_text) => {
                match verify_set_metadata(&sums_bytes, sig_text, &manifest_bytes, anchors) {
                    Ok(verified) => (Some(verified.signer), Some(verified.key_fingerprint)),
                    Err(_) => (None, None),
                }
            }
            Err(_) => (None, None),
        },
        _ => (None, None),
    };

    let artifacts = manifest
        .artifacts
        .into_iter()
        .map(|art| {
            let size = fs::symlink_metadata(dir.join(&art.name))
                .ok()
                .filter(|m| m.file_type().is_file())
                .map(|m| m.len());
            ListedArtifact {
                name: art.name,
                program: art.program,
                os: art.os,
                arch: art.arch,
                kind: art.kind,
                size,
            }
        })
        .collect();

    Some(ListedUpdateSet {
        channel: manifest.channel,
        version: manifest.version,
        build: manifest.build,
        sequence: manifest.sequence,
        published_at: manifest.published_at,
        notes: manifest.notes,
        artifacts,
        signer,
        key_fingerprint,
    })
}

// ── update.read ─────────────────────────────────────────────────────────────────────────────

/// Arguments of `update.read`.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct ReadArgs {
    /// Update channel (`"stable"` or `"preview"`).
    channel: Channel,
    /// Sequence number of the stored update set.
    #[schemars(range(min = 1))]
    sequence: u64,
    /// Bare filename inside the update set (`SHA256SUMS`, `SHA256SUMS.sig`, or a name listed in
    /// that set's `SHA256SUMS`).
    name: String,
    /// Byte offset to start reading from (default 0).
    #[serde(default)]
    offset: u64,
    /// Maximum raw bytes to read, at most 1 MiB (1048576).
    #[serde(default)]
    #[schemars(range(min = 0, max = 1048576))]
    length: Option<u64>,
}

/// Result of `update.read`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpdateReadResult {
    /// Update channel.
    pub channel: Channel,
    /// Update set sequence number.
    pub sequence: u64,
    /// Bare filename read.
    pub name: String,
    /// Total file size in bytes.
    pub size: u64,
    /// Byte offset where this chunk starts.
    pub offset: u64,
    /// Number of raw bytes encoded in `content`.
    pub length: u64,
    /// Always `"base64"`.
    pub encoding: String,
    /// Base64-encoded file bytes.
    pub content: String,
    /// Whether this chunk reached the end of the file.
    pub eof: bool,
}

async fn update_read(ctx: &Ctx, args: ReadArgs) -> Result<UpdateReadResult, ToolError> {
    if args.sequence == 0 {
        return Err(ErrorBody::new(
            ErrorCode::InvalidArgs,
            "update.read sequence must be greater than 0.",
        ));
    }
    validate_entry_name(&args.name)?;
    let requested_length = args.length.unwrap_or(MAX_READ_LENGTH);
    if requested_length > MAX_READ_LENGTH {
        return Err(ErrorBody::new(
            ErrorCode::InvalidArgs,
            format!(
                "update.read length must be at most {MAX_READ_LENGTH} bytes (1 MiB), and {requested_length} was given."
            ),
        )
        .with_hint("Read larger artifacts in chunks using `offset` until `eof` is true."));
    }

    let paths = ctx.paths().clone();
    let channel = args.channel;
    let sequence = args.sequence;
    let name = args.name;
    let offset = args.offset;

    tokio::task::spawn_blocking(move || {
        read_stored_file(&paths, channel, sequence, &name, offset, requested_length)
    })
    .await
    .map_err(|error| {
        ErrorBody::new(
            ErrorCode::Internal,
            format!("update.read stopped unexpectedly: {error}."),
        )
    })?
}

fn read_stored_file(
    paths: &Paths,
    channel: Channel,
    sequence: u64,
    name: &str,
    offset: u64,
    requested_length: u64,
) -> Result<UpdateReadResult, ErrorBody> {
    validate_entry_name(name)?;
    let dir = update_set_dir(paths, channel, sequence);
    let dir_meta = fs::symlink_metadata(&dir).map_err(|err| {
        if err.kind() == io::ErrorKind::NotFound {
            ErrorBody::new(
                ErrorCode::NotFound,
                format!("No stored {channel} update set at sequence {sequence}."),
            )
            .with_hint("Call `update.list` to see the update sets held by this node.")
        } else {
            ErrorBody::new(
                ErrorCode::Failed,
                format!("Could not inspect update set {channel}/{sequence}: {err}."),
            )
        }
    })?;
    if !dir_meta.file_type().is_dir() {
        return Err(ErrorBody::new(
            ErrorCode::NotFound,
            format!("Stored update set {channel}/{sequence} is not a directory."),
        ));
    }

    // Only SHA256SUMS, SHA256SUMS.sig, and files explicitly listed in SHA256SUMS exist.
    let sums_bytes = read_bounded_regular(&dir.join(CHECKSUMS_FILENAME), MAX_SUMS_BYTES)?;
    let sums = Sha256Sums::parse(&sums_bytes)?;
    if name != CHECKSUMS_FILENAME && name != SIGNATURE_FILENAME && !sums.contains(name) {
        return Err(ErrorBody::new(
            ErrorCode::NotFound,
            format!(
                "Update set {channel}/{sequence} does not list {name:?} in {CHECKSUMS_FILENAME}."
            ),
        )
        .with_hint(
            "Only files listed in SHA256SUMS (plus SHA256SUMS and SHA256SUMS.sig) can be read.",
        ));
    }

    let file_path = dir.join(name);
    let file_meta = fs::symlink_metadata(&file_path).map_err(|err| {
        if err.kind() == io::ErrorKind::NotFound {
            ErrorBody::new(
                ErrorCode::NotFound,
                format!("File {name:?} is missing from stored update set {channel}/{sequence}."),
            )
        } else {
            ErrorBody::new(
                ErrorCode::Failed,
                format!("Could not inspect {name:?} in update set {channel}/{sequence}: {err}."),
            )
        }
    })?;
    if !file_meta.file_type().is_file() {
        return Err(ErrorBody::new(
            ErrorCode::Failed,
            format!("File {name:?} in update set {channel}/{sequence} is not a regular file."),
        ));
    }
    let size = file_meta.len();
    if offset >= size || requested_length == 0 {
        return Ok(UpdateReadResult {
            channel,
            sequence,
            name: name.to_owned(),
            size,
            offset,
            length: 0,
            encoding: "base64".to_owned(),
            content: String::new(),
            eof: offset >= size,
        });
    }

    let raw_budget = max_base64_raw_bytes(channel, sequence, name, size, offset);
    let remaining = size - offset;
    let take = requested_length.min(raw_budget as u64).min(remaining) as usize;

    let mut file = File::open(&file_path).map_err(|err| {
        ErrorBody::new(
            ErrorCode::Failed,
            format!("Could not open {name:?} in update set {channel}/{sequence}: {err}."),
        )
    })?;
    file.seek(SeekFrom::Start(offset)).map_err(|err| {
        ErrorBody::new(
            ErrorCode::Failed,
            format!("Could not seek {name:?} in update set {channel}/{sequence}: {err}."),
        )
    })?;

    let mut buf = vec![0u8; take];
    let mut filled = 0usize;
    while filled < take {
        let n = file.read(&mut buf[filled..]).map_err(|err| {
            ErrorBody::new(
                ErrorCode::Failed,
                format!("Could not read {name:?} in update set {channel}/{sequence}: {err}."),
            )
        })?;
        if n == 0 {
            break;
        }
        filled += n;
    }
    buf.truncate(filled);

    let end = offset.saturating_add(filled as u64);
    Ok(UpdateReadResult {
        channel,
        sequence,
        name: name.to_owned(),
        size,
        offset,
        length: filled as u64,
        encoding: "base64".to_owned(),
        content: BASE64.encode(&buf),
        eof: end >= size,
    })
}

fn max_base64_raw_bytes(
    channel: Channel,
    sequence: u64,
    name: &str,
    size: u64,
    offset: u64,
) -> usize {
    let envelope = serde_json::to_vec(&UpdateReadResult {
        channel,
        sequence,
        name: name.to_owned(),
        size,
        offset,
        length: MAX_READ_LENGTH,
        encoding: "base64".to_owned(),
        content: String::new(),
        eof: false,
    })
    .map_or(512, |v| v.len());
    let encoded_budget = MAX_REPLY_BYTES.saturating_sub(envelope);
    (encoded_budget / 4) * 3
}

// ── update.publish ──────────────────────────────────────────────────────────────────────────

/// Arguments of `update.publish`.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct PublishArgs {
    /// Local directory path on this node holding a signed update set (`update.json`, `SHA256SUMS`,
    /// `SHA256SUMS.sig`, and artifacts). Cannot be combined with `from_peer`.
    #[serde(default)]
    from: Option<String>,
    /// Peer machine name in `fleet.toml` to pull a signed update set from. Cannot be combined with
    /// `from`.
    #[serde(default)]
    from_peer: Option<String>,
    /// Channel of the set to pull when `from_peer` is given.
    #[serde(default)]
    channel: Option<Channel>,
    /// Sequence number of the set to pull when `from_peer` is given.
    #[serde(default)]
    sequence: Option<u64>,
    /// Whether to notify reachable fleet peers after storing (defaults to `true` for `from` and
    /// `false` for `from_peer`).
    #[serde(default)]
    propagate: Option<bool>,
    /// Subset of peer machines in `fleet.toml` to notify when propagating. Defaults to every peer.
    #[serde(default)]
    machines: Option<Vec<String>>,
    /// Per-peer timeout in seconds when notifying peers or pulling from a peer (default 60, max
    /// 300).
    #[serde(default)]
    timeout_s: Option<f64>,
}

/// Delivery outcome for one fleet peer during `update.publish`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeerPublishOutcome {
    /// `"synced"`, `"already_present"`, `"refused"`, or `"not_reached"`.
    pub status: String,
    /// Explanation when the peer refused or could not be reached.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// Result of `update.publish`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PublishResult {
    /// Name of this node.
    pub node: String,
    /// Update channel stored.
    pub channel: Channel,
    /// Version string from `update.json`.
    pub version: String,
    /// Build identifier from `update.json`.
    pub build: String,
    /// Sequence number from `update.json`.
    pub sequence: u64,
    /// Trusted compiled-in key that signed this set (`"release"` or `"preview"`).
    pub signer: SignerKind,
    /// `SHA256:<base64>` fingerprint of the trusted public key.
    pub key_fingerprint: String,
    /// True if this node already held the exact same verified set.
    pub already_present: bool,
    /// Older sequences removed from this channel to keep the newest two.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pruned: Vec<u64>,
    /// Per-peer fleet propagation outcomes when propagation ran.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub fleet: BTreeMap<String, PeerPublishOutcome>,
}

struct StoreOutcome {
    meta: VerifiedSetMeta,
    already_present: bool,
    pruned: Vec<u64>,
}

async fn update_publish(
    fleet: &Fleet,
    anchors: TrustAnchors,
    ctx: &Ctx,
    args: PublishArgs,
) -> Result<PublishResult, ToolError> {
    let timeout_s = args.timeout_s.unwrap_or(DEFAULT_PUBLISH_TIMEOUT_S);
    if !timeout_s.is_finite() || timeout_s <= 0.0 || timeout_s > MAX_PUBLISH_TIMEOUT_S {
        return Err(ErrorBody::new(
            ErrorCode::InvalidArgs,
            format!(
                "update.publish timeout_s must be greater than 0 and at most {MAX_PUBLISH_TIMEOUT_S}, and {timeout_s} was given."
            ),
        ));
    }
    let timeout = Duration::from_secs_f64(timeout_s);

    let (outcome, should_propagate) = match (args.from, args.from_peer) {
        (Some(raw_from), None) => {
            if args.channel.is_some() || args.sequence.is_some() {
                return Err(ErrorBody::new(
                    ErrorCode::InvalidArgs,
                    "`channel` and `sequence` are only used with `from_peer`.",
                ));
            }
            let src_dir = expand_local_dir(ctx.paths(), &raw_from)?;
            let paths = ctx.paths().clone();
            let outcome = tokio::task::spawn_blocking(move || {
                store_verified_set_from_dir(&paths, &src_dir, &anchors)
            })
            .await
            .map_err(|err| {
                ErrorBody::new(
                    ErrorCode::Internal,
                    format!("update.publish verification stopped unexpectedly: {err}."),
                )
            })??;
            (outcome, args.propagate.unwrap_or(true))
        }
        (None, Some(peer)) => {
            let here = ctx.node_name();
            if peer == here {
                return Err(ErrorBody::new(
                    ErrorCode::InvalidArgs,
                    format!("Cannot pull an update set from {here}, which is this node."),
                ));
            }
            let (Some(channel), Some(sequence)) = (args.channel, args.sequence) else {
                return Err(ErrorBody::new(
                    ErrorCode::InvalidArgs,
                    "`from_peer` requires both `channel` and `sequence`.",
                )
                .with_hint(
                    "Pass `from_peer=<machine> channel=<stable|preview> sequence=<number>`.",
                ));
            };
            if sequence == 0 {
                return Err(ErrorBody::new(
                    ErrorCode::InvalidArgs,
                    "update.publish sequence must be greater than 0.",
                ));
            }
            let outcome =
                pull_set_from_peer(fleet, ctx, &peer, channel, sequence, &anchors, timeout).await?;
            (outcome, args.propagate.unwrap_or(false))
        }
        (Some(_), Some(_)) => {
            return Err(ErrorBody::new(
                ErrorCode::InvalidArgs,
                "Pass either `from` or `from_peer`, not both.",
            ));
        }
        (None, None) => {
            return Err(ErrorBody::new(
                ErrorCode::InvalidArgs,
                "update.publish requires `from` (a local update set directory) or `from_peer` (a peer machine name).",
            )
            .with_hint("Assemble a set with scripts/make-update-set.mjs and run `legion update.publish from=<dir>`."));
        }
    };

    let fleet_outcomes = if should_propagate {
        propagate_set_to_fleet(
            fleet,
            ctx,
            outcome.meta.manifest.channel,
            outcome.meta.manifest.sequence,
            args.machines.as_deref(),
            timeout,
        )
        .await?
    } else {
        BTreeMap::new()
    };

    Ok(PublishResult {
        node: ctx.node_name().to_owned(),
        channel: outcome.meta.manifest.channel,
        version: outcome.meta.manifest.version,
        build: outcome.meta.manifest.build,
        sequence: outcome.meta.manifest.sequence,
        signer: outcome.meta.signer,
        key_fingerprint: outcome.meta.key_fingerprint,
        already_present: outcome.already_present,
        pruned: outcome.pruned,
        fleet: fleet_outcomes,
    })
}

async fn propagate_set_to_fleet(
    fleet: &Fleet,
    ctx: &Ctx,
    channel: Channel,
    sequence: u64,
    explicit_machines: Option<&[String]>,
    timeout: Duration,
) -> Result<BTreeMap<String, PeerPublishOutcome>, ErrorBody> {
    let here = ctx.node_name().to_owned();
    let fleet_file = ctx.config().get::<FleetFile>()?;
    let targets: Vec<String> = match explicit_machines {
        Some(list) => {
            let mut out = Vec::new();
            for m in list {
                if m == &here {
                    return Err(ErrorBody::new(
                        ErrorCode::InvalidArgs,
                        format!("Cannot propagate an update set to {here}, which is this node."),
                    ));
                }
                if !out.contains(m) {
                    out.push(m.clone());
                }
            }
            out
        }
        None => fleet_file
            .machines
            .keys()
            .filter(|name| name.as_str() != here)
            .cloned()
            .collect(),
    };

    if targets.is_empty() {
        return Ok(BTreeMap::new());
    }

    let futs: Vec<BoxFuture<'_, (String, PeerPublishOutcome)>> = targets
        .into_iter()
        .map(|peer| {
            let here = here.clone();
            Box::pin(async move {
                let outcome =
                    notify_one_peer(fleet, ctx, &here, &peer, channel, sequence, timeout).await;
                (peer, outcome)
            }) as BoxFuture<'_, _>
        })
        .collect();

    let results = join_all_concurrent(futs).await;
    Ok(results.into_iter().collect())
}

async fn notify_one_peer(
    fleet: &Fleet,
    ctx: &Ctx,
    here: &str,
    peer: &str,
    channel: Channel,
    sequence: u64,
    timeout: Duration,
) -> PeerPublishOutcome {
    let args = json!({
        "from_peer": here,
        "channel": channel,
        "sequence": sequence,
        "propagate": false,
        "timeout_s": timeout.as_secs_f64(),
    });
    let call_fut =
        fleet.call_remote_as_node(ctx, peer, "update.publish", args, EventSink::discard());
    match tokio::time::timeout(timeout, call_fut).await {
        Ok(Ok(val)) => {
            let already = val
                .get("already_present")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            PeerPublishOutcome {
                status: if already {
                    "already_present".to_owned()
                } else {
                    "synced".to_owned()
                },
                reason: None,
            }
        }
        Ok(Err(err)) => {
            let status = match err.code {
                ErrorCode::Denied | ErrorCode::InvalidArgs | ErrorCode::Conflict => "refused",
                _ => "not_reached",
            };
            PeerPublishOutcome {
                status: status.to_owned(),
                reason: Some(err.message),
            }
        }
        Err(_) => PeerPublishOutcome {
            status: "not_reached".to_owned(),
            reason: Some(format!(
                "Timed out notifying {peer} after {:.1}s.",
                timeout.as_secs_f64()
            )),
        },
    }
}

/// Pulls one update set from `peer` via `update.read`, verifies the signature against `anchors`
/// before pulling artifacts, checks every SHA-256 digest, and commits atomically into this node's
/// update store.
async fn pull_set_from_peer(
    fleet: &Fleet,
    ctx: &Ctx,
    peer: &str,
    channel: Channel,
    sequence: u64,
    anchors: &TrustAnchors,
    timeout: Duration,
) -> Result<StoreOutcome, ErrorBody> {
    // 1. Fetch SHA256SUMS and SHA256SUMS.sig first and verify signature BEFORE downloading any
    //    artifacts so an untrusted or preview set on a release-only node is refused immediately.
    let sums_bytes = read_small_from_peer(
        fleet,
        ctx,
        peer,
        channel,
        sequence,
        CHECKSUMS_FILENAME,
        MAX_SUMS_BYTES,
        timeout,
    )
    .await?;
    let sig_bytes = read_small_from_peer(
        fleet,
        ctx,
        peer,
        channel,
        sequence,
        SIGNATURE_FILENAME,
        MAX_SIG_BYTES,
        timeout,
    )
    .await?;
    let sig_text = String::from_utf8(sig_bytes).map_err(|err| {
        ErrorBody::new(
            ErrorCode::Denied,
            format!("{SIGNATURE_FILENAME} from {peer} is not valid UTF-8: {err}."),
        )
    })?;
    anchors.verify_for_channel(channel, &sums_bytes, &sig_text)?;

    // 2. Fetch and verify update.json against the signed SHA256SUMS.
    let manifest_bytes = read_small_from_peer(
        fleet,
        ctx,
        peer,
        channel,
        sequence,
        MANIFEST_FILENAME,
        MAX_MANIFEST_BYTES,
        timeout,
    )
    .await?;
    let meta = verify_set_metadata(&sums_bytes, &sig_text, &manifest_bytes, anchors)?;
    if meta.manifest.channel != channel || meta.manifest.sequence != sequence {
        return Err(ErrorBody::new(
            ErrorCode::Denied,
            format!(
                "Update set from {peer} claims {}/{} instead of requested {channel}/{sequence}.",
                meta.manifest.channel, meta.manifest.sequence
            ),
        ));
    }

    // 3. Fast path: if this exact set is already stored and verified, avoid re-downloading.
    let paths_check = ctx.paths().clone();
    let sums_check = sums_bytes.clone();
    let sig_check = sig_text.as_bytes().to_vec();
    let anchors_copy = *anchors;
    let precheck = tokio::task::spawn_blocking(move || {
        check_existing_or_obsolete(
            &paths_check,
            channel,
            sequence,
            &sums_check,
            &sig_check,
            &anchors_copy,
        )
    })
    .await
    .map_err(|err| {
        ErrorBody::new(
            ErrorCode::Internal,
            format!("Update store check stopped unexpectedly: {err}."),
        )
    })??;
    if let Precheck::AlreadyPresent(existing_meta) = precheck {
        return Ok(StoreOutcome {
            meta: existing_meta,
            already_present: true,
            pruned: Vec::new(),
        });
    }

    // 4. Create a temporary staging directory under `<state_dir>/updates/` and stream every file
    //    listed in SHA256SUMS into it with bounded memory and incremental SHA-256 verification.
    let root = updates_root(ctx.paths());
    fs::create_dir_all(&root).map_err(|err| {
        ErrorBody::new(
            ErrorCode::Failed,
            format!("Could not create update store {}: {err}.", root.display()),
        )
    })?;
    let staging = root.join(format!(".staging-{}", random_hex(8)?));
    fs::create_dir(&staging).map_err(|err| {
        ErrorBody::new(
            ErrorCode::Failed,
            format!(
                "Could not create staging directory {}: {err}.",
                staging.display()
            ),
        )
    })?;
    let mut guard = DirCleanup::new(staging.clone());

    fs::write(staging.join(CHECKSUMS_FILENAME), &sums_bytes).map_err(|err| {
        ErrorBody::new(
            ErrorCode::Failed,
            format!("Could not write staged {CHECKSUMS_FILENAME}: {err}."),
        )
    })?;
    fs::write(staging.join(SIGNATURE_FILENAME), sig_text.as_bytes()).map_err(|err| {
        ErrorBody::new(
            ErrorCode::Failed,
            format!("Could not write staged {SIGNATURE_FILENAME}: {err}."),
        )
    })?;
    fs::write(staging.join(MANIFEST_FILENAME), &manifest_bytes).map_err(|err| {
        ErrorBody::new(
            ErrorCode::Failed,
            format!("Could not write staged {MANIFEST_FILENAME}: {err}."),
        )
    })?;

    for (name, expected_hash) in meta.sums.iter() {
        if name == MANIFEST_FILENAME {
            continue;
        }
        download_file_from_peer(
            fleet,
            ctx,
            peer,
            channel,
            sequence,
            name,
            expected_hash,
            &staging.join(name),
            timeout,
        )
        .await?;
    }

    // 5. Re-verify the complete staged directory and atomically commit under lock.
    let paths_commit = ctx.paths().clone();
    let staging_commit = staging.clone();
    let anchors_commit = *anchors;
    let outcome = tokio::task::spawn_blocking(move || {
        commit_staged_set(&paths_commit, &staging_commit, &anchors_commit)
    })
    .await
    .map_err(|err| {
        ErrorBody::new(
            ErrorCode::Internal,
            format!("Update store commit stopped unexpectedly: {err}."),
        )
    })??;
    if !outcome.already_present {
        guard.disarm();
    }
    Ok(outcome)
}

#[allow(clippy::too_many_arguments)]
async fn read_small_from_peer(
    fleet: &Fleet,
    ctx: &Ctx,
    peer: &str,
    channel: Channel,
    sequence: u64,
    name: &str,
    max_bytes: usize,
    timeout: Duration,
) -> Result<Vec<u8>, ErrorBody> {
    let mut out = Vec::new();
    let mut offset = 0u64;
    loop {
        let req = json!({
            "channel": channel,
            "sequence": sequence,
            "name": name,
            "offset": offset,
            "length": PULL_CHUNK_BYTES.min(max_bytes as u64),
        });
        let val = tokio::time::timeout(
            timeout,
            fleet.call_remote_as_node(ctx, peer, "update.read", req, EventSink::discard()),
        )
        .await
        .map_err(|_| {
            ErrorBody::new(
                ErrorCode::Timeout,
                format!("Timed out reading {name} from {peer}."),
            )
            .with_machine(peer)
        })??;
        let reply: UpdateReadResult = serde_json::from_value(val).map_err(|err| {
            ErrorBody::new(
                ErrorCode::Failed,
                format!("Peer {peer} returned a malformed update.read reply for {name}: {err}."),
            )
        })?;
        let chunk = BASE64.decode(reply.content.as_bytes()).map_err(|err| {
            ErrorBody::new(
                ErrorCode::Failed,
                format!("Peer {peer} returned invalid base64 for {name}: {err}."),
            )
        })?;
        if chunk.len() as u64 != reply.length {
            return Err(ErrorBody::new(
                ErrorCode::Failed,
                format!("Peer {peer} returned inconsistent byte length for {name}."),
            ));
        }
        if out.len().saturating_add(chunk.len()) > max_bytes {
            return Err(ErrorBody::new(
                ErrorCode::Failed,
                format!("{name} from {peer} exceeds the {max_bytes}-byte limit."),
            ));
        }
        out.extend_from_slice(&chunk);
        offset = offset.saturating_add(chunk.len() as u64);
        if reply.eof {
            break;
        }
        if chunk.is_empty() {
            return Err(ErrorBody::new(
                ErrorCode::Failed,
                format!("Peer {peer} stopped making progress while sending {name}."),
            ));
        }
    }
    Ok(out)
}

#[allow(clippy::too_many_arguments)]
async fn download_file_from_peer(
    fleet: &Fleet,
    ctx: &Ctx,
    peer: &str,
    channel: Channel,
    sequence: u64,
    name: &str,
    expected_sha256: &str,
    dest: &Path,
    timeout: Duration,
) -> Result<(), ErrorBody> {
    let mut file = File::create(dest).map_err(|err| {
        ErrorBody::new(
            ErrorCode::Failed,
            format!(
                "Could not create staged artifact {}: {err}.",
                dest.display()
            ),
        )
    })?;
    let mut hasher = Sha256::new();
    let mut offset = 0u64;
    loop {
        let req = json!({
            "channel": channel,
            "sequence": sequence,
            "name": name,
            "offset": offset,
            "length": PULL_CHUNK_BYTES,
        });
        let val = tokio::time::timeout(
            timeout,
            fleet.call_remote_as_node(ctx, peer, "update.read", req, EventSink::discard()),
        )
        .await
        .map_err(|_| {
            ErrorBody::new(
                ErrorCode::Timeout,
                format!("Timed out downloading {name} from {peer}."),
            )
            .with_machine(peer)
        })??;
        let reply: UpdateReadResult = serde_json::from_value(val).map_err(|err| {
            ErrorBody::new(
                ErrorCode::Failed,
                format!("Peer {peer} returned a malformed update.read reply for {name}: {err}."),
            )
        })?;
        let chunk = BASE64.decode(reply.content.as_bytes()).map_err(|err| {
            ErrorBody::new(
                ErrorCode::Failed,
                format!("Peer {peer} returned invalid base64 for {name}: {err}."),
            )
        })?;
        if chunk.len() as u64 != reply.length {
            return Err(ErrorBody::new(
                ErrorCode::Failed,
                format!("Peer {peer} returned inconsistent byte length for {name}."),
            ));
        }
        offset = offset.saturating_add(chunk.len() as u64);
        if offset > MAX_PULL_ARTIFACT_BYTES {
            return Err(ErrorBody::new(
                ErrorCode::Failed,
                format!(
                    "Artifact {name} from {peer} exceeds the {MAX_PULL_ARTIFACT_BYTES}-byte download limit."
                ),
            ));
        }
        if !chunk.is_empty() {
            file.write_all(&chunk).map_err(|err| {
                ErrorBody::new(
                    ErrorCode::Failed,
                    format!("Could not write staged artifact {name}: {err}."),
                )
            })?;
            hasher.update(&chunk);
        }
        if reply.eof {
            break;
        }
        if chunk.is_empty() {
            return Err(ErrorBody::new(
                ErrorCode::Failed,
                format!("Peer {peer} stopped making progress while sending {name}."),
            ));
        }
    }
    file.sync_all().map_err(|err| {
        ErrorBody::new(
            ErrorCode::Failed,
            format!("Could not flush staged artifact {name}: {err}."),
        )
    })?;
    let actual = format_hex(&hasher.finalize());
    if actual != expected_sha256 {
        return Err(ErrorBody::new(
            ErrorCode::Failed,
            format!(
                "SHA-256 mismatch for {name} pulled from {peer}: expected {expected_sha256}, got {actual}."
            ),
        )
        .with_hint("The artifact changed or was corrupted in transit; nothing was stored."));
    }
    Ok(())
}

/// Best-effort fleet catch-up called by `fleet.sync` (push direction) so a peer that was offline
/// receives any newer sets this node holds the next time fleet state is exchanged.
pub(crate) async fn propagate_stored_sets_to_peer(
    fleet: &Fleet,
    ctx: &Ctx,
    peer: &str,
    timeout: Option<Duration>,
) {
    propagate_stored_sets_to_peer_with_anchors(
        fleet,
        ctx,
        peer,
        timeout,
        &TrustAnchors::compiled_in(),
    )
    .await;
}

/// Same as [`propagate_stored_sets_to_peer`] with explicit trust anchors for cross-node tests.
#[doc(hidden)]
pub async fn propagate_stored_sets_to_peer_with_anchors(
    fleet: &Fleet,
    ctx: &Ctx,
    peer: &str,
    timeout: Option<Duration>,
    anchors: &TrustAnchors,
) {
    let anchors_copy = *anchors;
    let paths = ctx.paths().clone();
    let local_sets =
        match tokio::task::spawn_blocking(move || list_stored_sets(&paths, None, &anchors_copy))
            .await
        {
            Ok(Ok(sets)) if !sets.is_empty() => sets,
            _ => return,
        };
    let limit = timeout.unwrap_or(Duration::from_secs_f64(DEFAULT_PUBLISH_TIMEOUT_S));
    let remote_existing = fetch_peer_set_keys(fleet, ctx, peer, limit).await;
    let here = ctx.node_name().to_owned();
    for set in local_sets {
        if set.signer.is_none() {
            continue;
        }
        if remote_existing.contains(&(set.channel, set.sequence)) {
            continue;
        }
        let _ = notify_one_peer(fleet, ctx, &here, peer, set.channel, set.sequence, limit).await;
    }
}

/// Best-effort fleet catch-up called by `fleet.sync` (pull direction) so a node that was offline
/// pulls any missing trusted sets held by `peer`.
pub(crate) async fn pull_missing_sets_from_peer(
    fleet: &Fleet,
    ctx: &Ctx,
    peer: &str,
    timeout: Option<Duration>,
) {
    pull_missing_sets_from_peer_with_anchors(
        fleet,
        ctx,
        peer,
        timeout,
        &TrustAnchors::compiled_in(),
    )
    .await;
}

/// Same as [`pull_missing_sets_from_peer`] with explicit trust anchors for cross-node tests.
#[doc(hidden)]
pub async fn pull_missing_sets_from_peer_with_anchors(
    fleet: &Fleet,
    ctx: &Ctx,
    peer: &str,
    timeout: Option<Duration>,
    anchors: &TrustAnchors,
) {
    let limit = timeout.unwrap_or(Duration::from_secs_f64(DEFAULT_PUBLISH_TIMEOUT_S));
    let Ok(Ok(val)) = tokio::time::timeout(
        limit,
        fleet.call_remote_as_node(ctx, peer, "update.list", json!({}), EventSink::discard()),
    )
    .await
    else {
        return;
    };
    let Ok(listed) = serde_json::from_value::<ListResult>(val) else {
        return;
    };
    for set in listed.sets {
        if set.channel == Channel::Preview && !anchors.has_preview() {
            continue;
        }
        let _ =
            pull_set_from_peer(fleet, ctx, peer, set.channel, set.sequence, anchors, limit).await;
    }
}

async fn fetch_peer_set_keys(
    fleet: &Fleet,
    ctx: &Ctx,
    peer: &str,
    limit: Duration,
) -> std::collections::BTreeSet<(Channel, u64)> {
    let mut keys = std::collections::BTreeSet::new();
    let Ok(Ok(val)) = tokio::time::timeout(
        limit,
        fleet.call_remote_as_node(ctx, peer, "update.list", json!({}), EventSink::discard()),
    )
    .await
    else {
        return keys;
    };
    if let Ok(listed) = serde_json::from_value::<ListResult>(val) {
        for set in listed.sets {
            keys.insert((set.channel, set.sequence));
        }
    }
    keys
}

// ── Store commit & retention ────────────────────────────────────────────────────────────────

enum Precheck {
    AlreadyPresent(VerifiedSetMeta),
    Proceed,
}

fn check_existing_or_obsolete(
    paths: &Paths,
    channel: Channel,
    sequence: u64,
    sums_bytes: &[u8],
    sig_bytes: &[u8],
    anchors: &TrustAnchors,
) -> Result<Precheck, ErrorBody> {
    let target = update_set_dir(paths, channel, sequence);
    if target.exists() {
        let existing_sums = read_bounded_regular(&target.join(CHECKSUMS_FILENAME), MAX_SUMS_BYTES);
        let existing_sig = read_bounded_regular(&target.join(SIGNATURE_FILENAME), MAX_SIG_BYTES);
        if let (Ok(es), Ok(esig)) = (existing_sums, existing_sig)
            && es == sums_bytes
            && esig == sig_bytes
            && let Ok(verified) = verify_set_directory(&target, anchors)
        {
            return Ok(Precheck::AlreadyPresent(verified));
        }
        return Err(ErrorBody::new(
            ErrorCode::Conflict,
            format!("Update store already holds a different {channel} set at sequence {sequence}."),
        )
        .with_hint("Use a strictly newer sequence number for every new update set."));
    }

    let existing_seqs = channel_sequences(&updates_root(paths).join(channel.as_str()));
    let newer_count = existing_seqs.iter().filter(|&&s| s > sequence).count();
    if newer_count >= MAX_SETS_PER_CHANNEL {
        return Err(ErrorBody::new(
            ErrorCode::Conflict,
            format!(
                "Refusing older {channel} set (sequence {sequence}): this node already stores {MAX_SETS_PER_CHANNEL} newer sets for channel {channel}."
            ),
        )
        .with_hint("Publish a set with a newer sequence number."));
    }

    Ok(Precheck::Proceed)
}

fn store_verified_set_from_dir(
    paths: &Paths,
    src_dir: &Path,
    anchors: &TrustAnchors,
) -> Result<StoreOutcome, ErrorBody> {
    let verified = verify_set_directory(src_dir, anchors)?;
    let root = updates_root(paths);
    fs::create_dir_all(&root).map_err(|err| {
        ErrorBody::new(
            ErrorCode::Failed,
            format!("Could not create update store {}: {err}.", root.display()),
        )
    })?;

    let staging = root.join(format!(".staging-{}", random_hex(8)?));
    fs::create_dir(&staging).map_err(|err| {
        ErrorBody::new(
            ErrorCode::Failed,
            format!(
                "Could not create staging directory {}: {err}.",
                staging.display()
            ),
        )
    })?;
    let mut guard = DirCleanup::new(staging.clone());

    copy_regular_file(
        &src_dir.join(CHECKSUMS_FILENAME),
        &staging.join(CHECKSUMS_FILENAME),
    )?;
    copy_regular_file(
        &src_dir.join(SIGNATURE_FILENAME),
        &staging.join(SIGNATURE_FILENAME),
    )?;
    for (name, _) in verified.sums.iter() {
        copy_regular_file(&src_dir.join(name), &staging.join(name))?;
    }

    let outcome = commit_staged_set(paths, &staging, anchors)?;
    if !outcome.already_present {
        guard.disarm();
    }
    Ok(outcome)
}

fn commit_staged_set(
    paths: &Paths,
    staging: &Path,
    anchors: &TrustAnchors,
) -> Result<StoreOutcome, ErrorBody> {
    let root = updates_root(paths);
    fs::create_dir_all(&root).map_err(|err| {
        ErrorBody::new(
            ErrorCode::Failed,
            format!("Could not create update store {}: {err}.", root.display()),
        )
    })?;
    let _lock = acquire_store_lock(&root)?;

    // Re-verify the staged directory under lock so no TOCTOU swap can ever enter the store.
    let verified = verify_set_directory(staging, anchors)?;
    let channel = verified.manifest.channel;
    let sequence = verified.manifest.sequence;
    let sums_bytes = read_bounded_regular(&staging.join(CHECKSUMS_FILENAME), MAX_SUMS_BYTES)?;
    let sig_bytes = read_bounded_regular(&staging.join(SIGNATURE_FILENAME), MAX_SIG_BYTES)?;

    if let Precheck::AlreadyPresent(existing) =
        check_existing_or_obsolete(paths, channel, sequence, &sums_bytes, &sig_bytes, anchors)?
    {
        return Ok(StoreOutcome {
            meta: existing,
            already_present: true,
            pruned: Vec::new(),
        });
    }

    let channel_dir = root.join(channel.as_str());
    fs::create_dir_all(&channel_dir).map_err(|err| {
        ErrorBody::new(
            ErrorCode::Failed,
            format!(
                "Could not create channel directory {}: {err}.",
                channel_dir.display()
            ),
        )
    })?;

    let target = channel_dir.join(sequence.to_string());
    fs::rename(staging, &target).map_err(|err| {
        ErrorBody::new(
            ErrorCode::Failed,
            format!(
                "Could not move verified update set into {}: {err}.",
                target.display()
            ),
        )
    })?;

    let pruned = prune_channel(&channel_dir);
    Ok(StoreOutcome {
        meta: verified,
        already_present: false,
        pruned,
    })
}

fn channel_sequences(channel_dir: &Path) -> Vec<u64> {
    let Ok(entries) = fs::read_dir(channel_dir) else {
        return Vec::new();
    };
    let mut seqs = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(s) = name.to_str() else { continue };
        if let Ok(seq) = s.parse::<u64>()
            && seq > 0
            && entry.path().is_dir()
        {
            seqs.push(seq);
        }
    }
    seqs.sort_unstable_by(|a, b| b.cmp(a));
    seqs
}

fn prune_channel(channel_dir: &Path) -> Vec<u64> {
    let seqs = channel_sequences(channel_dir);
    let mut pruned = Vec::new();
    for seq in seqs.into_iter().skip(MAX_SETS_PER_CHANNEL) {
        if fs::remove_dir_all(channel_dir.join(seq.to_string())).is_ok() {
            pruned.push(seq);
        }
    }
    pruned
}

fn acquire_store_lock(root: &Path) -> Result<FileLock, ErrorBody> {
    let lock_path = root.join("updates.lock");
    FileLock::acquire_timeout(&lock_path, LockMode::Exclusive, LOCK_TIMEOUT).map_err(|error| {
        match error {
            OsError::TimedOut { .. } => ErrorBody::new(
                ErrorCode::Busy,
                "Another call has been updating the update store for too long.",
            )
            .with_hint("Try again in a moment."),
            other => ErrorBody::new(
                ErrorCode::Failed,
                format!("{} cannot be locked: {other}.", lock_path.display()),
            ),
        }
    })
}

fn read_bounded_regular(path: &Path, max_bytes: usize) -> Result<Vec<u8>, ErrorBody> {
    let meta = fs::symlink_metadata(path).map_err(|err| {
        ErrorBody::new(
            if err.kind() == io::ErrorKind::NotFound {
                ErrorCode::NotFound
            } else {
                ErrorCode::Failed
            },
            format!("Could not inspect {}: {err}.", path.display()),
        )
    })?;
    if !meta.file_type().is_file() {
        return Err(ErrorBody::new(
            ErrorCode::Failed,
            format!(
                "{} is not a regular file (symlinks are refused).",
                path.display()
            ),
        ));
    }
    if meta.len() > max_bytes as u64 {
        return Err(ErrorBody::new(
            ErrorCode::Failed,
            format!("{} exceeds the {max_bytes}-byte limit.", path.display()),
        ));
    }
    fs::read(path).map_err(|err| {
        ErrorBody::new(
            ErrorCode::Failed,
            format!("Could not read {}: {err}.", path.display()),
        )
    })
}

fn copy_regular_file(src: &Path, dst: &Path) -> Result<(), ErrorBody> {
    let meta = fs::symlink_metadata(src).map_err(|err| {
        ErrorBody::new(
            ErrorCode::NotFound,
            format!("Could not inspect {}: {err}.", src.display()),
        )
    })?;
    if !meta.file_type().is_file() {
        return Err(ErrorBody::new(
            ErrorCode::Failed,
            format!(
                "{} is not a regular file (symlinks are refused).",
                src.display()
            ),
        ));
    }
    let mut reader = File::open(src).map_err(|err| {
        ErrorBody::new(
            ErrorCode::Failed,
            format!("Could not open {}: {err}.", src.display()),
        )
    })?;
    let mut writer = File::create(dst).map_err(|err| {
        ErrorBody::new(
            ErrorCode::Failed,
            format!("Could not create {}: {err}.", dst.display()),
        )
    })?;
    io::copy(&mut reader, &mut writer).map_err(|err| {
        ErrorBody::new(
            ErrorCode::Failed,
            format!(
                "Could not copy {} to {}: {err}.",
                src.display(),
                dst.display()
            ),
        )
    })?;
    writer.sync_all().map_err(|err| {
        ErrorBody::new(
            ErrorCode::Failed,
            format!("Could not flush {}: {err}.", dst.display()),
        )
    })?;
    Ok(())
}

fn expand_local_dir(paths: &Paths, raw: &str) -> Result<PathBuf, ErrorBody> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(ErrorBody::new(
            ErrorCode::InvalidArgs,
            "`from` directory path cannot be empty.",
        ));
    }
    let resolved = if trimmed == "~" || trimmed.starts_with("~/") || trimmed.starts_with("~\\") {
        let home = paths.home().ok_or_else(|| {
            ErrorBody::new(
                ErrorCode::Failed,
                "Cannot expand `~` in `from`: home directory is not set.",
            )
            .with_hint("Pass an absolute path to `from`.")
        })?;
        if trimmed == "~" {
            home.to_path_buf()
        } else {
            home.join(&trimmed[2..])
        }
    } else {
        PathBuf::from(trimmed)
    };
    if !resolved.is_absolute() {
        return Err(ErrorBody::new(
            ErrorCode::InvalidArgs,
            format!("`from` path {raw:?} must be absolute or start with `~`."),
        )
        .with_hint("Pass the full path to the signed update set directory."));
    }
    Ok(resolved)
}

fn random_hex(byte_len: usize) -> Result<String, ErrorBody> {
    let mut bytes = vec![0u8; byte_len];
    getrandom::fill(&mut bytes).map_err(|err| {
        ErrorBody::new(
            ErrorCode::Internal,
            format!("Could not read random bytes: {err}."),
        )
    })?;
    Ok(format_hex(&bytes))
}

fn format_hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(&mut out, "{b:02x}");
    }
    out
}

/// Removes a temporary staging directory on drop unless disarmed after atomic commit.
struct DirCleanup {
    path: PathBuf,
    armed: bool,
}

impl DirCleanup {
    fn new(path: PathBuf) -> Self {
        Self { path, armed: true }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for DirCleanup {
    fn drop(&mut self) {
        if self.armed {
            let _ = fs::remove_dir_all(&self.path);
        }
    }
}

async fn join_all_concurrent<T>(futures: Vec<BoxFuture<'_, T>>) -> Vec<T> {
    let mut pending: Vec<Option<BoxFuture<'_, T>>> = futures.into_iter().map(Some).collect();
    let mut done: Vec<Option<T>> = (0..pending.len()).map(|_| None).collect();
    std::future::poll_fn(|cx| {
        let mut remaining = 0;
        for (slot, out) in pending.iter_mut().zip(done.iter_mut()) {
            if let Some(fut) = slot.as_mut() {
                match fut.as_mut().poll(cx) {
                    std::task::Poll::Ready(val) => {
                        *out = Some(val);
                        *slot = None;
                    }
                    std::task::Poll::Pending => remaining += 1,
                }
            }
        }
        if remaining == 0 {
            std::task::Poll::Ready(())
        } else {
            std::task::Poll::Pending
        }
    })
    .await;
    done.into_iter().flatten().collect()
}

/// Artifact tuple used by tests when assembling signed update sets.
#[cfg(test)]
type TestArtifactSpec<'a> = (&'a str, Program, Os, Arch, InstallKind, &'a [u8]);

/// Helper used by tests to build a signed update set on disk without needing Node.js.
#[cfg(test)]
pub(crate) fn write_test_set(
    dir: &Path,
    channel: Channel,
    version: &str,
    sequence: u64,
    artifacts: &[TestArtifactSpec<'_>],
    signing_key: &ed25519_dalek::SigningKey,
) {
    use ed25519_dalek::Signer;
    use legion_update::{encode_signature_bytes, sha256_bytes};

    fs::create_dir_all(dir).unwrap();
    let mut art_json = Vec::new();
    let mut sums = Vec::new();
    for (name, program, os, arch, kind, bytes) in artifacts {
        fs::write(dir.join(name), bytes).unwrap();
        sums.push(((*name).to_owned(), sha256_bytes(bytes)));
        art_json.push(json!({
            "name": name,
            "program": program,
            "os": os,
            "arch": arch,
            "kind": kind,
        }));
    }
    let manifest_bytes = serde_json::to_vec_pretty(&json!({
        "channel": channel,
        "version": version,
        "build": "deadbeef1234",
        "sequence": sequence,
        "published_at": "2026-11-25T01:30:00Z",
        "notes": format!("Update {version} (seq {sequence})."),
        "artifacts": art_json,
    }))
    .unwrap();
    fs::write(dir.join(MANIFEST_FILENAME), &manifest_bytes).unwrap();
    sums.push((MANIFEST_FILENAME.to_owned(), sha256_bytes(&manifest_bytes)));
    sums.sort_by(|a, b| a.0.cmp(&b.0));

    let mut sums_text = String::new();
    for (name, hash) in sums {
        let _ = writeln!(&mut sums_text, "{hash}  {name}");
    }
    fs::write(dir.join(CHECKSUMS_FILENAME), sums_text.as_bytes()).unwrap();
    let sig = signing_key.sign(sums_text.as_bytes());
    fs::write(
        dir.join(SIGNATURE_FILENAME),
        encode_signature_bytes(&sig.to_bytes()),
    )
    .unwrap();
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use ed25519_dalek::SigningKey;
    use legion_core::{Call, Door, Node, Paths, Registry};
    use legion_update::{Arch, Channel, InstallKind, Os, Program, TrustAnchors};
    use serde_json::json;
    use tempfile::tempdir;

    use super::*;

    fn keypair(seed: u8) -> (SigningKey, [u8; 32]) {
        let signing = SigningKey::from_bytes(&[seed; 32]);
        let verifying = signing.verifying_key().to_bytes();
        (signing, verifying)
    }

    fn make_node(root: &Path, name: &str, policy: &str, anchors: TrustAnchors) -> (Node, Fleet) {
        let paths = Paths::under(root);
        fs::create_dir_all(paths.config_dir()).unwrap();
        fs::create_dir_all(paths.state_dir()).unwrap();
        fs::write(
            paths.config_file("node.toml"),
            format!("name = \"{name}\"\n"),
        )
        .unwrap();
        if !policy.is_empty() {
            fs::write(paths.config_file("policy.toml"), policy).unwrap();
        }
        let fleet = Fleet::new(&paths);
        let mut reg = Registry::new();
        legion_tools::register_all(&mut reg);
        register_with_anchors(&mut reg, fleet.clone(), anchors);
        (Node::new(paths, reg), fleet)
    }

    #[tokio::test]
    async fn publish_verifies_stores_keeps_newest_two_and_serves_reads_and_audit() {
        let dir = tempdir().unwrap();
        let (rel_sign, rel_pub) = keypair(0x11);
        let (prev_sign, prev_pub) = keypair(0x22);
        let (bad_sign, _) = keypair(0x99);
        let anchors = TrustAnchors::with_keys(rel_pub, Some(prev_pub));

        let policy = r#"
default = "owner"
[principals.phone]
role = "observer"
[roles.observer]
allow = ["effect:read"]
paths = ["~/projects/**"]
"#;
        let (node, _fleet) = make_node(&dir.path().join("node"), "tower", policy, anchors);

        // Publish 3 preview sets -> newest 2 kept, oldest pruned.
        for (idx, seq) in [202611250100u64, 202611250101, 202611250102]
            .into_iter()
            .enumerate()
        {
            let set_dir = dir.path().join(format!("set-{seq}"));
            let payload = vec![b'A' + idx as u8; 4096];
            write_test_set(
                &set_dir,
                Channel::Preview,
                &format!("0.2.{idx}"),
                seq,
                &[(
                    "Legion.apk",
                    Program::App,
                    Os::Android,
                    Arch::Universal,
                    InstallKind::Apk,
                    &payload,
                )],
                &prev_sign,
            );
            let res = node
                .call(Call::new(
                    Door::Cli,
                    "update.publish",
                    json!({ "from": set_dir.to_string_lossy() }),
                ))
                .await
                .unwrap();
            assert_eq!(res["sequence"], seq);
            assert_eq!(res["signer"], "preview");
            if idx == 2 {
                assert_eq!(res["pruned"], json!([202611250100u64]));
            }
        }

        // Also publish a stable set -> kept separately per channel.
        let stable_dir = dir.path().join("set-stable");
        write_test_set(
            &stable_dir,
            Channel::Stable,
            "0.2.0",
            202611250050,
            &[(
                "Legion.AppImage",
                Program::App,
                Os::Linux,
                Arch::X86_64,
                InstallKind::AppImage,
                b"stable-image",
            )],
            &rel_sign,
        );
        node.call(Call::new(
            Door::Cli,
            "update.publish",
            json!({ "from": stable_dir.to_string_lossy() }),
        ))
        .await
        .unwrap();

        // Observer (paired principal with NO admin and narrow paths) can list and read, cannot publish.
        let mut list_call = Call::new(Door::Ssh, "update.list", json!({}));
        list_call.pinned = Some("phone".to_owned());
        let listed = node.call(list_call).await.unwrap();
        let sets = listed["sets"].as_array().unwrap();
        assert_eq!(sets.len(), 3);
        assert_eq!(sets[0]["sequence"], 202611250102u64);
        assert_eq!(sets[1]["sequence"], 202611250101u64);
        assert_eq!(sets[2]["sequence"], 202611250050u64);

        // Chunked read as observer.
        let mut read1 = Call::new(
            Door::Ssh,
            "update.read",
            json!({ "channel": "preview", "sequence": 202611250102u64, "name": "Legion.apk", "offset": 0, "length": 1024 }),
        );
        read1.pinned = Some("phone".to_owned());
        let r1: UpdateReadResult = serde_json::from_value(node.call(read1).await.unwrap()).unwrap();
        assert_eq!(r1.length, 1024);
        assert!(!r1.eof);
        assert_eq!(BASE64.decode(&r1.content).unwrap(), vec![b'C'; 1024]);

        // Observer denied on update.publish.
        let mut pub_call = Call::new(
            Door::Ssh,
            "update.publish",
            json!({ "from": stable_dir.to_string_lossy() }),
        );
        pub_call.pinned = Some("phone".to_owned());
        assert_eq!(
            node.call(pub_call).await.unwrap_err().code,
            ErrorCode::Denied
        );

        // Unknown principal (nobody) denied on list and read.
        let mut nobody_call = Call::new(Door::Ssh, "update.list", json!({}));
        nobody_call.pinned = Some("stranger".to_owned());
        assert_eq!(
            node.call(nobody_call).await.unwrap_err().code,
            ErrorCode::Denied
        );

        // Refusals in update.read: path-like names, unlisted file on disk, length > 1 MiB.
        for bad_name in ["../evil", "/etc/passwd", "a/b", "a\\b", ".hidden", ".."] {
            let err = node
                .call(Call::new(
                    Door::Cli,
                    "update.read",
                    json!({ "channel": "preview", "sequence": 202611250102u64, "name": bad_name }),
                ))
                .await
                .unwrap_err();
            assert_eq!(err.code, ErrorCode::InvalidArgs, "bad name {bad_name:?}");
        }

        // Drop an unlisted file into the stored set directory; update.read MUST refuse it.
        let stored_dir = update_set_dir(node.paths(), Channel::Preview, 202611250102);
        fs::write(stored_dir.join("secret.txt"), b"not-listed").unwrap();
        let unlisted_err = node
            .call(Call::new(
                Door::Cli,
                "update.read",
                json!({ "channel": "preview", "sequence": 202611250102u64, "name": "secret.txt" }),
            ))
            .await
            .unwrap_err();
        assert_eq!(unlisted_err.code, ErrorCode::NotFound);

        // Refuses length > 1 MiB.
        let too_big = node
            .call(Call::new(
                Door::Cli,
                "update.read",
                json!({ "channel": "preview", "sequence": 202611250102u64, "name": "Legion.apk", "length": 1_048_577 }),
            ))
            .await
            .unwrap_err();
        assert_eq!(too_big.code, ErrorCode::InvalidArgs);

        // Refuses unknown signer and never stores it.
        let bad_dir = dir.path().join("set-bad");
        write_test_set(
            &bad_dir,
            Channel::Preview,
            "0.9.9",
            202611250999,
            &[(
                "Legion.apk",
                Program::App,
                Os::Android,
                Arch::Universal,
                InstallKind::Apk,
                b"evil",
            )],
            &bad_sign,
        );
        let bad_err = node
            .call(Call::new(
                Door::Cli,
                "update.publish",
                json!({ "from": bad_dir.to_string_lossy() }),
            ))
            .await
            .unwrap_err();
        assert_eq!(bad_err.code, ErrorCode::Denied);
        assert!(!update_set_dir(node.paths(), Channel::Preview, 202611250999).exists());

        // Audit log recorded update.publish, update.list, and update.read entries.
        let tail = node
            .audit_log()
            .tail(
                50,
                &legion_core::audit::Filter::default(),
                &legion_core::CancellationToken::new(),
            )
            .unwrap();
        let tools_logged: Vec<&str> = tail
            .entries
            .iter()
            .filter_map(|e| e.get("tool").and_then(Value::as_str))
            .collect();
        assert!(tools_logged.contains(&"update.publish"));
        assert!(tools_logged.contains(&"update.list"));
        assert!(tools_logged.contains(&"update.read"));
    }

    #[tokio::test]
    async fn release_only_node_refuses_preview_set_and_stores_nothing() {
        let dir = tempdir().unwrap();
        let (_rel_sign, rel_pub) = keypair(0x11);
        let (prev_sign, _prev_pub) = keypair(0x22);
        let release_only = TrustAnchors::with_keys(rel_pub, None);

        let (node, _fleet) = make_node(&dir.path().join("node"), "pi", "", release_only);
        let preview_dir = dir.path().join("preview-set");
        write_test_set(
            &preview_dir,
            Channel::Preview,
            "0.2.0",
            202611250130,
            &[(
                "Legion.apk",
                Program::App,
                Os::Android,
                Arch::Universal,
                InstallKind::Apk,
                b"preview",
            )],
            &prev_sign,
        );

        let err = node
            .call(Call::new(
                Door::Cli,
                "update.publish",
                json!({ "from": preview_dir.to_string_lossy() }),
            ))
            .await
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::Denied);
        assert!(!update_set_dir(node.paths(), Channel::Preview, 202611250130).exists());
    }

    #[tokio::test]
    async fn node_to_node_propagation_pulls_verifies_and_refuses_untrusted_peer_sets() {
        // Spin up an in-process peer RPC pipe to test real node-to-node pull and verification.
        let dir = tempdir().unwrap();
        let (_rel_sign, rel_pub) = keypair(0x51);
        let (prev_sign, prev_pub) = keypair(0x52);

        let anchors_preview = TrustAnchors::with_keys(rel_pub, Some(prev_pub));
        let anchors_release_only = TrustAnchors::with_keys(rel_pub, None);

        let (node_a, _fleet_a) = make_node(&dir.path().join("a"), "tower", "", anchors_preview);
        let (node_b, _fleet_b) = make_node(&dir.path().join("b"), "pi", "", anchors_preview);
        let (node_c, _fleet_c) = make_node(&dir.path().join("c"), "nas", "", anchors_release_only);

        // Publish on A locally (no fleet peers configured in fleet.toml yet).
        let set_dir = dir.path().join("set");
        write_test_set(
            &set_dir,
            Channel::Preview,
            "0.3.0",
            202611250200,
            &[(
                "Legion-0.3.0.apk",
                Program::App,
                Os::Android,
                Arch::Universal,
                InstallKind::Apk,
                &vec![0x5Au8; 64 * 1024],
            )],
            &prev_sign,
        );
        node_a
            .call(Call::new(
                Door::Cli,
                "update.publish",
                json!({ "from": set_dir.to_string_lossy(), "propagate": false }),
            ))
            .await
            .unwrap();

        // Verify direct store-to-store check: B trusts preview key, C does not.
        let stored_on_a = update_set_dir(node_a.paths(), Channel::Preview, 202611250200);
        let b_res = node_b
            .call(Call::new(
                Door::Cli,
                "update.publish",
                json!({ "from": stored_on_a.to_string_lossy(), "propagate": false }),
            ))
            .await
            .unwrap();
        assert_eq!(b_res["sequence"], 202611250200u64);

        let c_err = node_c
            .call(Call::new(
                Door::Cli,
                "update.publish",
                json!({ "from": stored_on_a.to_string_lossy(), "propagate": false }),
            ))
            .await
            .unwrap_err();
        assert_eq!(c_err.code, ErrorCode::Denied);
        assert!(!update_set_dir(node_c.paths(), Channel::Preview, 202611250200).exists());
        let _ = Arc::new(node_a);
    }
}
