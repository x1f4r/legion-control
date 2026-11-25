//! Where a call runs. Every door hands its calls to a [`Router`], so the fleet milestone replaces
//! [`Local`] with a router that reaches other machines for all of them at once.

use std::collections::BTreeMap;
use std::io::{self, Seek, SeekFrom};
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use legion_proto::{Effect, ErrorBody, ErrorCode, FanOut, MachineOutcome, Target};
use serde_json::{Value, json};
use tokio::io::{AsyncRead, ReadBuf};
use tokio_util::sync::CancellationToken;

use crate::ctx::Door;
use crate::node::{Call, Node};
use crate::policy::{Access, Policy};
use crate::tool::BoxFuture;

/// The result of a routed call, preserving whether it targeted one machine or fanned out to
/// several so doors can render fan-outs and choose exit/error status without guessing from JSON.
#[derive(Clone, Debug, PartialEq)]
pub enum RouteOutcome {
    /// A call targeting one machine (or this node).
    One(Result<Value, ErrorBody>),
    /// A fan-out across several machines or `all`, keyed by machine name.
    Many(BTreeMap<String, Result<Value, ErrorBody>>),
}

impl RouteOutcome {
    /// Converts this outcome into the wire shape the design specifies: the tool's own object or
    /// error for one machine, and `{"results": {"<machine>": {"ok": …} | {"error": …}}}` for a
    /// fan-out.
    pub fn into_wire(self) -> Result<Value, ErrorBody> {
        match self {
            RouteOutcome::One(result) => result,
            RouteOutcome::Many(results) => {
                let wire = FanOut {
                    results: results
                        .into_iter()
                        .map(|(machine, outcome)| {
                            let outcome = match outcome {
                                Ok(value) => MachineOutcome::Ok(value),
                                Err(error) => MachineOutcome::Error(error),
                            };
                            (machine, outcome)
                        })
                        .collect(),
                };
                serde_json::to_value(wire).map_err(|error| {
                    ErrorBody::new(
                        ErrorCode::Internal,
                        format!("Fan-out result could not be written as JSON: {error}."),
                    )
                })
            }
        }
    }

    /// Whether every machine targeted by the call failed.
    ///
    /// For one machine, this is `true` on `Err`. For a fan-out, this is `true` when at least one
    /// machine was called and every one of them returned `Err` (partial success is `false`, and an
    /// empty fan-out with no machines is `false`).
    pub fn all_failed(&self) -> bool {
        match self {
            RouteOutcome::One(result) => result.is_err(),
            RouteOutcome::Many(results) => {
                !results.is_empty() && results.values().all(Result::is_err)
            }
        }
    }
}

/// Each machine a call would run on, with the effect its tool has there, or `None` where that
/// cannot be established.
pub type MachineEffects = Vec<(String, Option<Effect>)>;

/// One request to open a byte range of a file through a [`Router`].
#[derive(Clone, Debug)]
pub struct ContentRequest {
    /// Target machine name, or `None` for this node.
    pub machine: Option<String>,
    /// File path on the target machine (absolute or starting with `~`).
    pub path: String,
    /// Starting byte offset.
    pub offset: u64,
    /// Maximum number of bytes to read, or `None` to read through the end of the file.
    pub length: Option<u64>,
    /// Principal pinned by the door (for example the HTTP bearer token's principal).
    pub principal: String,
    /// Door through which the request arrived, recorded in the audit log.
    pub door: Door,
    /// Whether `open_content` should write an immediate `content.read` audit line.
    ///
    /// Doors that coalesce audit entries per ticket (such as `legion serve`) set this to `false`
    /// and record their own coalesced `content.ticket` / `content.read` entries.
    pub audit: bool,
    /// Cancelled when the caller disconnects or the server shuts down.
    pub cancel: CancellationToken,
}

/// An opened file byte range with metadata and a streaming async reader.
pub struct OpenedContent {
    /// Total file size in bytes when opened.
    pub size: u64,
    /// Starting byte offset of `reader`.
    pub offset: u64,
    /// Exact number of bytes `reader` will yield.
    pub length: u64,
    /// File modification time: whole seconds since the Unix epoch.
    pub mtime_secs: i64,
    /// File modification time: sub-second nanoseconds (`0..1_000_000_000`).
    pub mtime_nanos: u32,
    /// Opaque file identity (a hash of `(dev, ino)` on Unix, `None` elsewhere) folded into strong
    /// `ETag` values so replacing a file with identical size and timestamp invalidates cached
    /// ranges.
    pub file_id: Option<u64>,
    /// Async byte reader yielding `length` bytes with backpressure.
    pub reader: Box<dyn AsyncRead + Send + Unpin>,
}

/// Failure opening a file byte range.
#[derive(Debug)]
pub enum ContentError {
    /// Requested `offset` exceeds the file's current `size`.
    OutOfRange {
        /// Current file size in bytes.
        size: u64,
    },
    /// Structured Legion error (denied, not_found, conflict, unreachable, ...).
    Error(ErrorBody),
}

/// Runs a call on the machine or machines its `on` names.
pub trait Router: Send + Sync + 'static {
    /// Runs `call` where `on` says: this node when `on` is absent.
    fn call<'a>(
        &'a self,
        node: &'a Node,
        call: Call,
        on: Option<Target>,
    ) -> BoxFuture<'a, RouteOutcome>;

    /// Opens a byte range of a file on this node or another machine, checking `fs.read`
    /// permission and recording an audit entry.
    fn open_content<'a>(
        &'a self,
        node: &'a Node,
        req: ContentRequest,
    ) -> BoxFuture<'a, Result<OpenedContent, ContentError>> {
        Box::pin(async move {
            let here = node.name();
            match req.machine.as_deref() {
                None => open_local_content(node, &req).await,
                Some(machine) if machine == here => open_local_content(node, &req).await,
                Some(machine) => Err(ContentError::Error(
                    ErrorBody::new(
                        ErrorCode::Unreachable,
                        format!(
                            "{machine} cannot be reached from {here}: this router serves only local files."
                        ),
                    )
                    .with_machine(here),
                )),
            }
        })
    }

    /// The effect `tool` has on each machine a call sent where `on` says would run on, learned
    /// the way this router learns it to authorise the call: from this node's spec when the call
    /// runs here or the tool is built in, and otherwise from what the target reports. An effect
    /// is `None` where it cannot be established, such as for a tool that exists only on a
    /// machine this router cannot reach or asks through a relay. Nothing runs.
    ///
    /// Fails with the error the call itself would get when the tool is not on this node and the
    /// call runs here, or when a target cannot be resolved or reached.
    fn effects<'a>(
        &'a self,
        node: &'a Node,
        tool: &'a str,
        on: Option<&'a Target>,
        _cancel: CancellationToken,
    ) -> BoxFuture<'a, Result<MachineEffects, ErrorBody>> {
        Box::pin(async move {
            let here = node.name();
            match on {
                Some(target) if target.single() != Some(here.as_str()) => Ok(vec![(
                    target.to_string(),
                    node.registry().get(tool).map(|tool| tool.spec().effect),
                )]),
                _ => Ok(vec![(here, Some(node.spec(tool)?.effect))]),
            }
        })
    }

    /// Closes any open remote sessions cleanly (EOF, short wait, then kill) before the process
    /// exits.
    fn shutdown(&self) -> BoxFuture<'_, ()> {
        Box::pin(async {})
    }
}

/// Authorises `fs.read` for `req.principal`, confines `req.path` via [`Grant::check_path`], opens
/// the requested byte range on this node, and records an audit entry.
pub async fn open_local_content(
    node: &Node,
    req: &ContentRequest,
) -> Result<OpenedContent, ContentError> {
    let started = Instant::now();
    let here = node.name();
    let args = json!({
        "path": req.path,
        "offset": req.offset,
        "length": req.length,
    });
    let outcome = open_local_content_inner(node, &here, req).await;
    if req.audit {
        let audit_result = match &outcome {
            Ok(_) => Ok(Value::Null),
            Err(ContentError::OutOfRange { size }) => Err(ErrorBody::new(
                ErrorCode::Conflict,
                format!(
                    "{} cannot be read at byte {}: there are {size} bytes.",
                    req.path, req.offset
                ),
            )),
            Err(ContentError::Error(err)) => Err(err.clone()),
        };
        node.record_audit(
            here,
            Some(req.principal.clone()),
            req.door,
            "content.read".to_owned(),
            &args,
            started,
            &audit_result,
        )
        .await;
    }
    outcome
}

async fn open_local_content_inner(
    node: &Node,
    here: &str,
    req: &ContentRequest,
) -> Result<OpenedContent, ContentError> {
    if req.cancel.is_cancelled() {
        return Err(ContentError::Error(
            ErrorBody::new(ErrorCode::Cancelled, "The content read was cancelled.")
                .with_machine(here),
        ));
    }
    let policy = node
        .config()
        .get::<Policy>()
        .map_err(|err| ContentError::Error(err.with_machine(here)))?;
    let grant = policy
        .authorize_name(&req.principal, "fs.read", Some(Effect::Read), node.paths())
        .map_err(|err| ContentError::Error(err.with_machine(here)))?;
    let resolved = grant
        .check_path(&req.path, Access::Read)
        .map_err(|err| ContentError::Error(err.with_machine(here)))?;

    let offset = req.offset;
    let length = req.length;
    let here_owned = here.to_owned();
    let opened = tokio::task::spawn_blocking(move || {
        let mut file = std::fs::File::open(&resolved).map_err(|err| {
            let code = match err.kind() {
                io::ErrorKind::NotFound => ErrorCode::NotFound,
                io::ErrorKind::PermissionDenied => ErrorCode::Denied,
                _ => ErrorCode::Failed,
            };
            ContentError::Error(
                ErrorBody::new(code, format!("Could not open the requested file: {err}."))
                    .with_machine(&here_owned),
            )
        })?;
        let meta = file.metadata().map_err(|err| {
            ContentError::Error(
                ErrorBody::new(
                    ErrorCode::Failed,
                    format!("Could not inspect the requested file: {err}."),
                )
                .with_machine(&here_owned),
            )
        })?;
        if !meta.is_file() {
            return Err(ContentError::Error(
                ErrorBody::new(
                    ErrorCode::Conflict,
                    "The requested path is not a regular file.",
                )
                .with_hint("Name a regular file rather than a directory or special file.")
                .with_machine(&here_owned),
            ));
        }
        let size = meta.len();
        if offset > size {
            return Err(ContentError::OutOfRange { size });
        }
        let available = size - offset;
        let actual_length = length.map_or(available, |n| n.min(available));
        if offset > 0 && actual_length > 0 {
            file.seek(SeekFrom::Start(offset)).map_err(|err| {
                ContentError::Error(
                    ErrorBody::new(
                        ErrorCode::Failed,
                        format!("Could not seek the requested file: {err}."),
                    )
                    .with_machine(&here_owned),
                )
            })?;
        }
        let (mtime_secs, mtime_nanos) = meta
            .modified()
            .ok()
            .map_or((0, 0), system_time_to_unix_pair);
        let file_id = metadata_file_id(&meta);
        Ok((file, size, actual_length, mtime_secs, mtime_nanos, file_id))
    })
    .await
    .map_err(|err| {
        ContentError::Error(
            ErrorBody::new(
                ErrorCode::Internal,
                format!("Opening file stopped unexpectedly: {err}."),
            )
            .with_machine(here),
        )
    })??;

    let (file, size, actual_length, mtime_secs, mtime_nanos, file_id) = opened;
    let reader: Box<dyn AsyncRead + Send + Unpin> = Box::new(LocalFileReader {
        file: tokio::fs::File::from_std(file),
        remaining: actual_length,
        cancel: req.cancel.clone(),
    });
    Ok(OpenedContent {
        size,
        offset,
        length: actual_length,
        mtime_secs,
        mtime_nanos,
        file_id,
        reader,
    })
}

#[cfg(unix)]
fn metadata_file_id(meta: &std::fs::Metadata) -> Option<u64> {
    use std::os::unix::fs::MetadataExt;
    let mut input = [0u8; 16];
    input[..8].copy_from_slice(&meta.dev().to_le_bytes());
    input[8..].copy_from_slice(&meta.ino().to_le_bytes());
    let digest = blake3::hash(&input);
    let mut out = [0u8; 8];
    out.copy_from_slice(&digest.as_bytes()[..8]);
    Some(u64::from_le_bytes(out))
}

#[cfg(not(unix))]
fn metadata_file_id(_meta: &std::fs::Metadata) -> Option<u64> {
    None
}

fn system_time_to_unix_pair(time: SystemTime) -> (i64, u32) {
    match time.duration_since(UNIX_EPOCH) {
        Ok(d) => (d.as_secs().min(i64::MAX as u64) as i64, d.subsec_nanos()),
        Err(err) => {
            let d = err.duration();
            let whole = d.as_secs().min(i64::MAX as u64) as i64;
            let nanos = d.subsec_nanos();
            if nanos == 0 {
                (-whole, 0)
            } else {
                (-whole - 1, 1_000_000_000 - nanos)
            }
        }
    }
}

struct LocalFileReader {
    file: tokio::fs::File,
    remaining: u64,
    cancel: CancellationToken,
}

impl AsyncRead for LocalFileReader {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        if self.cancel.is_cancelled() {
            return Poll::Ready(Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "content read was cancelled",
            )));
        }
        if self.remaining == 0 || buf.remaining() == 0 {
            return Poll::Ready(Ok(()));
        }
        let max_read = usize::try_from(self.remaining)
            .unwrap_or(usize::MAX)
            .min(buf.remaining());
        let slice = buf.initialize_unfilled_to(max_read);
        let mut sub = ReadBuf::new(slice);
        match Pin::new(&mut self.file).poll_read(cx, &mut sub) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Err(err)) => Poll::Ready(Err(err)),
            Poll::Ready(Ok(())) => {
                let n = sub.filled().len();
                buf.advance(n);
                if n == 0 {
                    let missing = self.remaining;
                    self.remaining = 0;
                    return Poll::Ready(Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        format!("file shrank while reading ({missing} bytes remaining)"),
                    )));
                }
                self.remaining -= n as u64;
                Poll::Ready(Ok(()))
            }
        }
    }
}

/// Runs calls on this node, and refuses any other machine as unreachable, since this build cannot
/// reach one.
pub struct Local;

impl Router for Local {
    fn call<'a>(
        &'a self,
        node: &'a Node,
        call: Call,
        on: Option<Target>,
    ) -> BoxFuture<'a, RouteOutcome> {
        Box::pin(async move {
            let here = node.name();
            RouteOutcome::One(match on {
                None => node.call(call).await,
                Some(target) if target.single() == Some(here.as_str()) => node.call(call).await,
                Some(target) => Err(ErrorBody::new(
                    ErrorCode::Unreachable,
                    format!("{target} cannot be reached from {here}: this build of legion runs calls only on the node it runs on."),
                )
                .with_hint(format!(
                    "Routing calls to other machines arrives in a later milestone. Until then, leave out on (--on on the command line), or name this node ({here})."
                ))
                .with_machine(here)),
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn route_outcome_wire_shape_and_all_failed() {
        let ok = RouteOutcome::One(Ok(json!({"v": 1})));
        assert!(!ok.all_failed());
        assert_eq!(ok.into_wire(), Ok(json!({"v": 1})));

        let err = RouteOutcome::One(Err(ErrorBody::new(ErrorCode::Denied, "No.")));
        assert!(err.all_failed());
        assert_eq!(err.into_wire().unwrap_err().code, ErrorCode::Denied);

        let empty = RouteOutcome::Many(BTreeMap::new());
        assert!(!empty.all_failed());
        assert_eq!(empty.into_wire(), Ok(json!({"results": {}})));

        let mut partial = BTreeMap::new();
        partial.insert("pi".to_owned(), Ok(json!({"ok": 1})));
        partial.insert(
            "tower".to_owned(),
            Err(ErrorBody::new(ErrorCode::Unreachable, "Down.")),
        );
        let partial = RouteOutcome::Many(partial);
        assert!(!partial.all_failed());
        assert_eq!(
            partial.into_wire(),
            Ok(json!({
                "results": {
                    "pi": {"ok": {"ok": 1}},
                    "tower": {"error": {"code": "unreachable", "message": "Down."}}
                }
            }))
        );

        let mut failed = BTreeMap::new();
        failed.insert(
            "pi".to_owned(),
            Err(ErrorBody::new(ErrorCode::Unreachable, "Down.")),
        );
        failed.insert(
            "tower".to_owned(),
            Err(ErrorBody::new(ErrorCode::NotFound, "Unknown.")),
        );
        let failed = RouteOutcome::Many(failed);
        assert!(failed.all_failed());
    }
}
