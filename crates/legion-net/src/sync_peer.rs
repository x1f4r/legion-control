//! Network implementation of [`legion_sync::Peer`] over Legion's existing fleet pipe
//! (`Fleet::open_pipe_for(..., Subcommand::Stream)` — SSH or custom command connector, zero new
//! listening ports).
//!
//! Uses a persistent `sync.session` stream pipe so an entire pull pass (handshake + index pages +
//! thousands of small files or multi-gigabyte files) runs over a single pipe without per-file
//! process spawn overhead. Ranged resume (`FetchOptions::offset`), idle timeouts, cooperative
//! cancellation, login-banner skipping, and bounded page sizes are all enforced.

use std::io::{self, Read};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use legion_core::CancellationToken;
use legion_core::config::ConfigStore;
use legion_proto::{ErrorBody, ErrorCode};
use legion_sync::{
    FetchOptions, HandshakeHello, MAX_PAGE_RECORDS, Peer, PeerHandshake, RecordsPage, SyncError,
};
use legion_xfer::Hash;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{ChildStdin, ChildStdout};
use tokio::runtime::Handle;

use crate::connector::{PipeHandle, Subcommand};
use crate::pool::Fleet;
use crate::stream::STREAM_PROTOCOL;
use crate::sync_stream::{SessionRequest, SyncFetchReady, SyncIndexReply, SyncSessionReady};

/// Maximum banner/noise lines skipped before `{"protocol": 1}` on a fresh stream pipe.
const MOST_NOISE_LINES: usize = 64;
/// Maximum banner/noise bytes skipped before `{"protocol": 1}`.
const MOST_NOISE_BYTES: usize = 64 * 1024;
/// Maximum reply line length accepted from a sync peer (32 MiB — fits 4,096 index records).
const MAX_REPLY_LINE_BYTES: usize = 32 * 1024 * 1024;
/// Default timeout for handshake and index RPCs.
const DEFAULT_CONTROL_TIMEOUT: Duration = Duration::from_secs(30);
/// Time slice between cooperative cancellation checks while reading from the pipe.
const CANCEL_POLL_SLICE: Duration = Duration::from_millis(100);
/// Buffer capacity on the stream stdout reader (256 KiB for high-throughput bulk streaming).
const PIPE_READ_BUFFER: usize = 256 * 1024;
/// Grace period for closing an aborted or finished stream subprocess.
const CLOSE_GRACE: Duration = Duration::from_secs(2);

/// Network [`Peer`] backed by a reusable `legion stream` (`sync.session`) pipe opened via
/// [`Fleet::open_pipe_for`].
pub struct FleetSyncPeer {
    fleet: Fleet,
    config: Arc<ConfigStore>,
    local_member: String,
    peer_member: String,
    folder_id: String,
    runtime: Handle,
    cancel: CancellationToken,
    session: Arc<Mutex<Option<SyncSessionConn>>>,
}

struct SyncSessionConn {
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    handle: PipeHandle,
}

impl SyncSessionConn {
    async fn open(
        fleet: &Fleet,
        config: &ConfigStore,
        local_member: &str,
        peer_member: &str,
        folder_id: &str,
        timeout: Duration,
        cancel: CancellationToken,
    ) -> legion_sync::Result<Self> {
        let pipe = tokio::time::timeout(
            timeout,
            fleet.open_pipe_for(
                config,
                local_member,
                peer_member,
                Subcommand::Stream,
                cancel,
            ),
        )
        .await
        .map_err(|_| SyncError::PeerUnavailable {
            member: peer_member.to_owned(),
            detail: format!("timed out opening stream to {peer_member}"),
        })?
        .map_err(|err| map_error_body(peer_member, err))?;

        let (stdin, stdout, handle) = pipe.into_parts();
        let mut conn = Self {
            stdin,
            stdout: BufReader::with_capacity(PIPE_READ_BUFFER, stdout),
            handle,
        };

        // 1. Read the server's `{"protocol": 1}` greeting, skipping any SSH login banner lines.
        if let Err(err) = conn.read_protocol_greeting(peer_member, timeout).await {
            conn.abort().await;
            return Err(err);
        }

        // 2. Send the `sync.session` stream header.
        let header = json!({
            "protocol": STREAM_PROTOCOL,
            "op": "sync.session",
            "folder": folder_id,
            "caller": local_member,
        });
        if let Err(err) = conn.send_json_line(peer_member, &header, timeout).await {
            conn.abort().await;
            return Err(err);
        }

        // 3. Read `{"ready": true}` (or `{"error": ...}`).
        let reply_line = match conn.read_json_line(peer_member, timeout, None).await {
            Ok(line) => line,
            Err(err) => {
                conn.abort().await;
                return Err(err);
            }
        };
        let ready: SyncSessionReady = match parse_reply_or_error(peer_member, &reply_line) {
            Ok(r) => r,
            Err(err) => {
                conn.abort().await;
                return Err(err);
            }
        };
        if !ready.ready {
            conn.abort().await;
            return Err(SyncError::PeerUnavailable {
                member: peer_member.to_owned(),
                detail: "peer declined to start sync session".to_owned(),
            });
        }
        Ok(conn)
    }

    async fn read_protocol_greeting(
        &mut self,
        peer_member: &str,
        timeout: Duration,
    ) -> legion_sync::Result<()> {
        let deadline = Instant::now() + timeout;
        let mut noise_lines = 0usize;
        let mut noise_bytes = 0usize;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(SyncError::PeerUnavailable {
                    member: peer_member.to_owned(),
                    detail: format!("timed out waiting for stream greeting from {peer_member}"),
                });
            }
            let line = self
                .read_raw_line(peer_member, remaining, None)
                .await?;
            if let Ok(Value::Object(map)) = serde_json::from_slice::<Value>(&line) {
                if let Some(error_val) = map.get("error")
                    && let Ok(body) = serde_json::from_value::<ErrorBody>(error_val.clone())
                {
                    return Err(map_error_body(peer_member, body));
                }
                if let Some(proto) = map.get("protocol").and_then(Value::as_u64) {
                    if proto == u64::from(STREAM_PROTOCOL) {
                        return Ok(());
                    }
                    return Err(SyncError::Unsupported {
                        reason: format!(
                            "peer {peer_member:?} speaks stream protocol {proto}, expected {STREAM_PROTOCOL}"
                        ),
                        hint: Some(format!(
                            "Update legion on {peer_member} (or this machine) so both speak the same stream protocol."
                        )),
                    });
                }
            }
            noise_lines += 1;
            noise_bytes += line.len();
            if noise_lines > MOST_NOISE_LINES || noise_bytes > MOST_NOISE_BYTES {
                return Err(SyncError::Unsupported {
                    reason: format!(
                        "peer {peer_member:?} sent non-protocol output instead of a stream greeting"
                    ),
                    hint: Some(format!(
                        "Update legion on {peer_member} or silence login banners for non-interactive SSH sessions."
                    )),
                });
            }
        }
    }

    async fn send_json_line<T: serde::Serialize>(
        &mut self,
        peer_member: &str,
        value: &T,
        timeout: Duration,
    ) -> legion_sync::Result<()> {
        let mut bytes = serde_json::to_vec(value).map_err(|err| SyncError::CorruptData {
            context: format!("encode sync request for {peer_member}"),
            detail: err.to_string(),
        })?;
        bytes.push(b'\n');
        tokio::time::timeout(timeout, async {
            self.stdin.write_all(&bytes).await?;
            self.stdin.flush().await
        })
        .await
        .map_err(|_| SyncError::Timeout {
            operation: format!("write sync request to {peer_member}"),
            detail: format!("timed out after {}s", timeout.as_secs()),
        })?
        .map_err(|err| SyncError::PeerUnavailable {
            member: peer_member.to_owned(),
            detail: format!("stream write failed: {err}"),
        })
    }

    async fn read_json_line(
        &mut self,
        peer_member: &str,
        timeout: Duration,
        cancel: Option<&AtomicBool>,
    ) -> legion_sync::Result<Vec<u8>> {
        loop {
            let line = self.read_raw_line(peer_member, timeout, cancel).await?;
            if let Ok(Value::Object(ref map)) = serde_json::from_slice::<Value>(&line)
                && map.get("busy").and_then(Value::as_bool) == Some(true)
            {
                continue;
            }
            return Ok(line);
        }
    }

    async fn read_raw_line(
        &mut self,
        peer_member: &str,
        timeout: Duration,
        cancel: Option<&AtomicBool>,
    ) -> legion_sync::Result<Vec<u8>> {
        let deadline = Instant::now() + timeout;
        let mut buf = Vec::new();
        loop {
            if cancel.is_some_and(|c| c.load(Ordering::Relaxed)) {
                return Err(SyncError::Cancelled {
                    operation: format!("sync stream read from {peer_member}"),
                });
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(SyncError::Timeout {
                    operation: format!("sync stream read from {peer_member}"),
                    detail: format!("timed out after {}s", timeout.as_secs()),
                });
            }
            let slice = if cancel.is_some() {
                left.min(CANCEL_POLL_SLICE)
            } else {
                left
            };
            match tokio::time::timeout(slice, self.stdout.fill_buf()).await {
                Err(_) if cancel.is_some() => continue,
                Err(_) => {
                    return Err(SyncError::Timeout {
                        operation: format!("sync stream read from {peer_member}"),
                        detail: format!("timed out after {}s", timeout.as_secs()),
                    });
                }
                Ok(Err(err)) => {
                    return Err(SyncError::PeerUnavailable {
                        member: peer_member.to_owned(),
                        detail: format!("stream read failed: {err}"),
                    });
                }
                Ok(Ok(avail)) => {
                    if avail.is_empty() {
                        if buf.is_empty() {
                            let (code, stderr) = self.handle.collect_exit_info(CLOSE_GRACE).await;
                            let detail = if stderr.is_empty() {
                                format!("stream ended unexpectedly (exit {code:?})")
                            } else {
                                format!("stream ended unexpectedly: {stderr}")
                            };
                            return Err(SyncError::PeerUnavailable {
                                member: peer_member.to_owned(),
                                detail,
                            });
                        }
                        return Ok(buf);
                    }
                    let (take_len, done) = match avail.iter().position(|b| *b == b'\n') {
                        Some(pos) => (pos + 1, true),
                        None => (avail.len(), false),
                    };
                    if buf.len().saturating_add(take_len) > MAX_REPLY_LINE_BYTES {
                        return Err(SyncError::CorruptData {
                            context: format!("reply from {peer_member}"),
                            detail: format!("line exceeded {MAX_REPLY_LINE_BYTES} bytes"),
                        });
                    }
                    buf.extend_from_slice(&avail[..take_len]);
                    self.stdout.consume(take_len);
                    if done {
                        return Ok(buf);
                    }
                }
            }
        }
    }

    async fn close_gracefully(mut self) {
        let _ = self
            .send_json_line("peer", &SessionRequest::Close, Duration::from_secs(1))
            .await;
        let _ = self.stdin.shutdown().await;
        drop(self.stdin);
        drop(self.stdout);
        self.handle.close(CLOSE_GRACE).await;
    }

    async fn abort(self) {
        drop(self.stdin);
        drop(self.stdout);
        self.handle.close(CLOSE_GRACE).await;
    }
}

impl FleetSyncPeer {
    /// Creates a new peer client for pulling `folder_id` from `peer_member`.
    pub fn new(
        fleet: Fleet,
        config: Arc<ConfigStore>,
        local_member: impl Into<String>,
        peer_member: impl Into<String>,
        folder_id: impl Into<String>,
        runtime: Handle,
        cancel: CancellationToken,
    ) -> Self {
        Self {
            fleet,
            config,
            local_member: local_member.into(),
            peer_member: peer_member.into(),
            folder_id: folder_id.into(),
            runtime,
            cancel,
            session: Arc::new(Mutex::new(None)),
        }
    }

    /// Gracefully closes any pooled `sync.session` pipe held by this peer.
    pub fn close(&self) {
        let conn = self
            .session
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .take();
        if let Some(conn) = conn {
            self.runtime.block_on(conn.close_gracefully());
        }
    }

    fn take_or_open(&self, timeout: Duration) -> legion_sync::Result<SyncSessionConn> {
        if let Some(conn) = self
            .session
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .take()
        {
            return Ok(conn);
        }
        self.runtime.block_on(SyncSessionConn::open(
            &self.fleet,
            &self.config,
            &self.local_member,
            &self.peer_member,
            &self.folder_id,
            timeout,
            self.cancel.clone(),
        ))
    }

    fn put_back(&self, conn: SyncSessionConn) {
        let old = self
            .session
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .replace(conn);
        if let Some(old) = old {
            self.runtime.block_on(old.abort());
        }
    }
}

impl Drop for FleetSyncPeer {
    fn drop(&mut self) {
        let conn = self
            .session
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .take();
        if let Some(conn) = conn {
            // Avoid nested block_on if dropped from inside an async context.
            if Handle::try_current().is_ok() {
                self.runtime.spawn(conn.abort());
            } else {
                self.runtime.block_on(conn.abort());
            }
        }
    }
}

impl Peer for FleetSyncPeer {
    fn member_id(&self) -> &str {
        &self.peer_member
    }

    fn handshake(&self, caller: &HandshakeHello) -> legion_sync::Result<PeerHandshake> {
        let mut conn = self.take_or_open(DEFAULT_CONTROL_TIMEOUT)?;
        let req = SessionRequest::Handshake {
            hello: caller.clone(),
        };
        let res = self.runtime.block_on(async {
            conn.send_json_line(&self.peer_member, &req, DEFAULT_CONTROL_TIMEOUT)
                .await?;
            let line = conn
                .read_json_line(&self.peer_member, DEFAULT_CONTROL_TIMEOUT, None)
                .await?;
            let reply: SyncIndexReply = parse_reply_or_error(&self.peer_member, &line)?;
            match reply {
                SyncIndexReply::Handshake { handshake } => Ok(handshake),
                SyncIndexReply::Page { .. } => Err(SyncError::CorruptData {
                    context: format!("handshake from {}", self.peer_member),
                    detail: "peer returned records page instead of handshake".to_owned(),
                }),
            }
        });
        match res {
            Ok(hs) => {
                self.put_back(conn);
                Ok(hs)
            }
            Err(err) => {
                if is_fatal_session_error(&err) {
                    self.runtime.block_on(conn.abort());
                } else {
                    self.put_back(conn);
                }
                Err(err)
            }
        }
    }

    fn records_since(
        &self,
        since_incarnation: u64,
        since_seq: u64,
        limit: usize,
        cancel: Option<&AtomicBool>,
    ) -> legion_sync::Result<RecordsPage> {
        let capped = limit.clamp(1, MAX_PAGE_RECORDS);
        let mut conn = self.take_or_open(DEFAULT_CONTROL_TIMEOUT)?;
        let req = SessionRequest::Index {
            since_incarnation,
            since_seq,
            limit: capped,
        };
        let res = self.runtime.block_on(async {
            conn.send_json_line(&self.peer_member, &req, DEFAULT_CONTROL_TIMEOUT)
                .await?;
            let line = conn
                .read_json_line(&self.peer_member, DEFAULT_CONTROL_TIMEOUT, cancel)
                .await?;
            let reply: SyncIndexReply = parse_reply_or_error(&self.peer_member, &line)?;
            match reply {
                SyncIndexReply::Page { page } => Ok(page),
                SyncIndexReply::Handshake { .. } => Err(SyncError::CorruptData {
                    context: format!("records_since from {}", self.peer_member),
                    detail: "peer returned handshake instead of records page".to_owned(),
                }),
            }
        });
        match res {
            Ok(page) => {
                self.put_back(conn);
                Ok(page)
            }
            Err(err) => {
                if is_fatal_session_error(&err) {
                    self.runtime.block_on(conn.abort());
                } else {
                    self.put_back(conn);
                }
                Err(err)
            }
        }
    }

    fn fetch_file(
        &self,
        path: &str,
        expected_hash: Hash,
        options: &FetchOptions,
    ) -> legion_sync::Result<Box<dyn Read + Send>> {
        let timeout = options.timeout.max(Duration::from_secs(1));
        let mut conn = self.take_or_open(timeout)?;
        let req = SessionRequest::Fetch {
            path: path.to_owned(),
            expected_hash,
            offset: options.offset,
        };
        let cancel_ref = options.cancel.as_deref();
        let ready_res = self.runtime.block_on(async {
            conn.send_json_line(&self.peer_member, &req, timeout)
                .await?;
            let line = conn
                .read_json_line(&self.peer_member, timeout, cancel_ref)
                .await?;
            parse_reply_or_error::<SyncFetchReady>(&self.peer_member, &line)
        });
        let ready = match ready_res {
            Ok(ready) => ready,
            Err(err) => {
                if is_fatal_session_error(&err) {
                    self.runtime.block_on(conn.abort());
                } else {
                    self.put_back(conn);
                }
                return Err(err);
            }
        };

        if ready.remaining == 0 {
            self.put_back(conn);
            return Ok(Box::new(io::empty()));
        }

        Ok(Box::new(SessionFileReader {
            conn: Some(conn),
            slot: Arc::clone(&self.session),
            runtime: self.runtime.clone(),
            peer_member: self.peer_member.clone(),
            remaining: ready.remaining,
            timeout,
            cancel: options.cancel.clone(),
            broken: false,
        }))
    }
}

struct SessionFileReader {
    conn: Option<SyncSessionConn>,
    slot: Arc<Mutex<Option<SyncSessionConn>>>,
    runtime: Handle,
    peer_member: String,
    remaining: u64,
    timeout: Duration,
    cancel: Option<Arc<AtomicBool>>,
    broken: bool,
}

impl Read for SessionFileReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.remaining == 0 {
            self.return_conn();
            return Ok(0);
        }
        if buf.is_empty() {
            return Ok(0);
        }
        if self
            .cancel
            .as_ref()
            .is_some_and(|c| c.load(Ordering::Relaxed))
        {
            self.broken = true;
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "sync file fetch cancelled",
            ));
        }

        let want = usize::try_from(self.remaining.min(buf.len() as u64)).unwrap_or(buf.len());
        let Some(conn) = self.conn.as_mut() else {
            return Ok(0);
        };

        let deadline = Instant::now() + self.timeout;
        let peer_member = &self.peer_member;
        let cancel = self.cancel.as_deref();
        let read_res = self.runtime.block_on(async {
            loop {
                if cancel.is_some_and(|c| c.load(Ordering::Relaxed)) {
                    return Err(io::Error::new(
                        io::ErrorKind::Interrupted,
                        "sync file fetch cancelled",
                    ));
                }
                let left = deadline.saturating_duration_since(Instant::now());
                if left.is_zero() {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        format!("timed out reading sync file from {peer_member}"),
                    ));
                }
                let slice = if cancel.is_some() {
                    left.min(CANCEL_POLL_SLICE)
                } else {
                    left
                };
                match tokio::time::timeout(slice, conn.stdout.read(&mut buf[..want])).await {
                    Err(_) if cancel.is_some() => continue,
                    Err(_) => {
                        return Err(io::Error::new(
                            io::ErrorKind::TimedOut,
                            format!("timed out reading sync file from {peer_member}"),
                        ));
                    }
                    Ok(res) => return res,
                }
            }
        });

        match read_res {
            Ok(0) => {
                self.broken = true;
                Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    format!(
                        "peer {} closed sync stream with {} bytes still unread",
                        self.peer_member, self.remaining
                    ),
                ))
            }
            Ok(n) => {
                self.remaining = self.remaining.saturating_sub(n as u64);
                if self.remaining == 0 {
                    self.return_conn();
                }
                Ok(n)
            }
            Err(err) => {
                self.broken = true;
                Err(err)
            }
        }
    }
}

impl SessionFileReader {
    fn return_conn(&mut self) {
        if self.broken || self.remaining != 0 {
            return;
        }
        if let Some(conn) = self.conn.take() {
            let old = self
                .slot
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .replace(conn);
            if let Some(old) = old {
                self.runtime.spawn(old.abort());
            }
        }
    }
}

impl Drop for SessionFileReader {
    fn drop(&mut self) {
        if !self.broken && self.remaining == 0 {
            self.return_conn();
        } else if let Some(conn) = self.conn.take() {
            if Handle::try_current().is_ok() {
                self.runtime.spawn(conn.abort());
            } else {
                self.runtime.block_on(conn.abort());
            }
        }
    }
}

fn parse_reply_or_error<T: serde::de::DeserializeOwned>(
    peer_member: &str,
    line: &[u8],
) -> legion_sync::Result<T> {
    let val: Value = serde_json::from_slice(line).map_err(|err| SyncError::CorruptData {
        context: format!("parse JSON reply from {peer_member}"),
        detail: err.to_string(),
    })?;
    if let Value::Object(ref map) = val
        && let Some(err_val) = map.get("error")
        && let Ok(body) = serde_json::from_value::<ErrorBody>(err_val.clone())
    {
        return Err(map_error_body(peer_member, body));
    }
    serde_json::from_value::<T>(val).map_err(|err| SyncError::CorruptData {
        context: format!("decode reply from {peer_member}"),
        detail: err.to_string(),
    })
}

fn map_error_body(peer_member: &str, body: ErrorBody) -> SyncError {
    match body.code {
        ErrorCode::Unreachable => SyncError::PeerUnavailable {
            member: peer_member.to_owned(),
            detail: body.message,
        },
        ErrorCode::Timeout => SyncError::Timeout {
            operation: format!("sync with {peer_member}"),
            detail: body.message,
        },
        ErrorCode::Denied => SyncError::Denied {
            member: peer_member.to_owned(),
            reason: body.message,
        },
        ErrorCode::Unsupported | ErrorCode::UnknownTool => SyncError::Unsupported {
            reason: body.message,
            hint: body.hint,
        },
        ErrorCode::NotFound => SyncError::NotFound {
            what: body.message,
        },
        ErrorCode::Conflict => SyncError::Conflict {
            path: peer_member.to_owned(),
            reason: body.message,
        },
        ErrorCode::Cancelled => SyncError::Cancelled {
            operation: body.message,
        },
        _ => SyncError::PeerUnavailable {
            member: peer_member.to_owned(),
            detail: body.message,
        },
    }
}

/// Returns `true` when the error implies the underlying stream pipe cannot safely be reused
/// (transport/framing error, timeout, cancel) as opposed to a clean JSON-level application error.
fn is_fatal_session_error(err: &SyncError) -> bool {
    matches!(
        err,
        SyncError::PeerUnavailable { .. }
            | SyncError::Timeout { .. }
            | SyncError::Cancelled { .. }
            | SyncError::CorruptData { .. }
            | SyncError::Unsupported { .. }
    )
}
