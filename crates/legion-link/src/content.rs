//! Streaming random-access file reads (`legion stream` `get`) over a [`Link`](crate::Link) session.
//!
//! Each call to [`Link::open_content`](crate::Link::open_content) opens an independent `legion
//! stream` channel (an SSH exec channel on the existing connection, or a local child process) and
//! runs the `get` handshake (`protocol`, `op: "get"`, `path`, `offset`, optional `length`). Once
//! the remote node replies with `{size, offset, length, mtime}`, [`ContentStream`] yields exactly
//! `length` bytes via [`AsyncRead`] with backpressure and closes the channel immediately when
//! cancelled or dropped.

use std::future::Future;
use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use legion_proto::{ErrorBody, ErrorCode};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader, ReadBuf};
use tokio_util::sync::CancellationToken;

use crate::stderr::StderrTail;

/// Stream protocol version spoken by `legion stream`.
const STREAM_PROTOCOL: u32 = 1;
/// Maximum reply line length accepted during the `get` handshake.
const MAX_REPLY_LINE: usize = 1024 * 1024;
/// Maximum pre-protocol noise lines skipped before `{"protocol": 1}` (login banners, shell noise).
const MAX_NOISE_LINES: usize = 64;
/// Maximum cumulative bytes of noise skipped before giving up.
const MAX_NOISE_BYTES: usize = 64 * 1024;
/// Read buffer capacity wrapping the underlying stream so handshake bytes and body reads share one
/// bounded buffer with backpressure.
const STREAM_BUF_BYTES: usize = 64 * 1024;

/// Type-erased async reader for a raw `legion stream` pipe.
pub type BoxStreamRead = Box<dyn AsyncRead + Unpin + Send>;
/// Type-erased async writer for a raw `legion stream` pipe.
pub type BoxStreamWrite = Box<dyn AsyncWrite + Unpin + Send>;

/// File modification timestamp reported by `legion stream`'s `get` reply.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ContentTime {
    /// Whole seconds since the Unix epoch (negative before 1970-01-01T00:00:00Z).
    pub secs: i64,
    /// Sub-second nanoseconds in `0..1_000_000_000`.
    pub nanos: u32,
}

/// A raw bidirectional byte stream opened to `legion stream` on a node.
pub struct RawStream {
    /// Async stdout from the remote `legion stream`.
    pub reader: BoxStreamRead,
    /// Async stdin to the remote `legion stream`.
    pub writer: BoxStreamWrite,
    /// Optional bounded stderr collector for diagnostics.
    pub stderr: Option<Arc<StderrTail>>,
    /// Cancellation token that immediately closes this stream channel or child process when
    /// cancelled.
    pub cancel: CancellationToken,
    /// Optional RAII lifecycle guard dropped when the stream finishes or is dropped.
    pub guard: Option<Box<dyn Send + Sync>>,
}

/// Failure opening a raw `legion stream` channel on a session's transport.
#[derive(Debug)]
pub enum StreamOpenError {
    /// The underlying shared session died before any bytes were written; reopening the session and
    /// retrying once is safe.
    DeadSession(ErrorBody),
    /// Opening failed for a reason that should be surfaced directly to the caller.
    Failed(ErrorBody),
}

/// Factory exposed by a [`TransportHandle`](crate::TransportHandle) to open independent
/// `legion stream` channels over the same underlying connection or local binary.
pub trait StreamOpener: Send + Sync + 'static {
    /// Opens one fresh `legion stream` channel or child process.
    fn open_stream(
        &self,
        cancel: CancellationToken,
    ) -> Pin<Box<dyn Future<Output = Result<RawStream, StreamOpenError>> + Send + '_>>;
}

/// Async byte-range reader returned by [`Link::open_content`](crate::Link::open_content).
///
/// Implements [`AsyncRead`] with backpressure. Dropping or calling [`ContentStream::cancel`]
/// closes the underlying SSH exec channel or local child process immediately without affecting the
/// parent session or other open content streams.
pub struct ContentStream {
    reader: BufReader<BoxStreamRead>,
    _writer: BoxStreamWrite,
    size: u64,
    offset: u64,
    length: u64,
    remaining: u64,
    mtime: ContentTime,
    cancel: CancellationToken,
    _guard: Option<Box<dyn Send + Sync>>,
    _flight: Option<Box<dyn Send + Sync>>,
}

impl ContentStream {
    /// Total file size in bytes when the file was opened on the remote node.
    pub fn size(&self) -> u64 {
        self.size
    }

    /// Starting byte offset of this stream.
    pub fn offset(&self) -> u64 {
        self.offset
    }

    /// Exact number of bytes this stream will yield.
    pub fn length(&self) -> u64 {
        self.length
    }

    /// Remaining unread bytes in this stream.
    pub fn remaining(&self) -> u64 {
        self.remaining
    }

    /// File modification timestamp captured when the file was opened.
    pub fn mtime(&self) -> ContentTime {
        self.mtime
    }

    /// Cancels this stream and closes its underlying SSH channel or child process immediately.
    pub fn cancel(&self) {
        self.cancel.cancel();
    }

    pub(crate) fn attach_flight(&mut self, guard: Box<dyn Send + Sync>) {
        self._flight = Some(guard);
    }
}

impl Drop for ContentStream {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

impl AsyncRead for ContentStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        if self.cancel.is_cancelled() {
            return Poll::Ready(Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "content stream was cancelled",
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
        match Pin::new(&mut self.reader).poll_read(cx, &mut sub) {
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
                        format!("content stream ended with {missing} bytes remaining"),
                    )));
                }
                self.remaining -= n as u64;
                Poll::Ready(Ok(()))
            }
        }
    }
}

#[derive(Debug, Deserialize)]
struct GetReadyReply {
    size: u64,
    #[serde(default)]
    offset: u64,
    #[serde(default)]
    length: Option<u64>,
    #[serde(default)]
    mtime: Option<ContentTime>,
}

/// Runs the `legion stream` `get` handshake on `stream` and returns a positioned [`ContentStream`].
#[allow(clippy::too_many_arguments)]
pub(crate) async fn handshake_get(
    stream: RawStream,
    machine: &str,
    path: &str,
    offset: u64,
    length: Option<u64>,
    as_principal: Option<&str>,
    timeout: Duration,
    cancel: CancellationToken,
) -> Result<ContentStream, ErrorBody> {
    let RawStream {
        reader,
        mut writer,
        stderr,
        cancel: stream_cancel,
        guard,
    } = stream;

    let mut header = json!({
        "protocol": STREAM_PROTOCOL,
        "op": "get",
        "path": path,
        "offset": offset,
    });
    if let Some(len) = length {
        header["length"] = json!(len);
    }
    if let Some(principal) = as_principal {
        header["as"] = json!(principal);
    }
    let mut header_bytes = header.to_string().into_bytes();
    header_bytes.push(b'\n');

    let mut buf_reader = BufReader::with_capacity(STREAM_BUF_BYTES, reader);
    let handshake = async {
        writer
            .write_all(&header_bytes)
            .await
            .map_err(|err| stream_io_error(machine, "writing stream header", &err, &stderr))?;
        writer
            .flush()
            .await
            .map_err(|err| stream_io_error(machine, "flushing stream header", &err, &stderr))?;
        read_get_ready(&mut buf_reader, machine, &stderr).await
    };

    let ready = tokio::select! {
        biased;
        () = cancel.cancelled() => {
            stream_cancel.cancel();
            return Err(ErrorBody::new(
                ErrorCode::Cancelled,
                format!("Opening content on {machine} was cancelled."),
            )
            .with_machine(machine));
        }
        res = tokio::time::timeout(timeout, handshake) => match res {
            Ok(Ok(ready)) => ready,
            Ok(Err(err)) => {
                stream_cancel.cancel();
                return Err(err);
            }
            Err(_) => {
                stream_cancel.cancel();
                let mut err = ErrorBody::new(
                    ErrorCode::Timeout,
                    format!(
                        "Timed out waiting for {machine} to answer stream get after {}s.",
                        timeout.as_secs_f64()
                    ),
                )
                .with_machine(machine);
                if let Some(tail) = &stderr {
                    let text = tail.text();
                    if !text.is_empty() {
                        err = err.with_hint(format!("Remote stderr: {text}"));
                    }
                }
                return Err(err);
            }
        }
    };

    let (Some(reply_length), Some(mtime)) = (ready.length, ready.mtime) else {
        stream_cancel.cancel();
        return Err(ErrorBody::new(
            ErrorCode::Unsupported,
            format!(
                "{machine} answered stream get without length and mtime (older legion version)."
            ),
        )
        .with_hint("Update legion on that machine so it supports random-access content reads.")
        .with_machine(machine));
    };

    Ok(ContentStream {
        reader: buf_reader,
        _writer: writer,
        size: ready.size,
        offset: ready.offset,
        length: reply_length,
        remaining: reply_length,
        mtime,
        cancel: stream_cancel,
        _guard: guard,
        _flight: None,
    })
}

async fn read_get_ready<R: AsyncRead + Unpin>(
    reader: &mut BufReader<R>,
    machine: &str,
    stderr: &Option<Arc<StderrTail>>,
) -> Result<GetReadyReply, ErrorBody> {
    let mut greeted = false;
    let mut noise_lines = 0usize;
    let mut noise_bytes = 0usize;
    let mut line = Vec::new();

    loop {
        line.clear();
        let n = read_bounded_line(reader, &mut line, MAX_REPLY_LINE)
            .await
            .map_err(|err| stream_io_error(machine, "reading stream reply", &err, stderr))?;
        if n == 0 {
            let mut err = ErrorBody::new(
                ErrorCode::Unreachable,
                format!("The stream to {machine} closed before answering."),
            )
            .with_machine(machine);
            if let Some(tail) = stderr {
                let text = tail.text();
                if !text.is_empty() {
                    err = err.with_hint(format!("Remote stderr: {text}"));
                }
            }
            return Err(err);
        }

        if let Ok(Value::Object(fields)) = serde_json::from_slice::<Value>(&line) {
            if !greeted {
                if let Some(spoken) = fields.get("protocol") {
                    if spoken.as_u64() != Some(u64::from(STREAM_PROTOCOL)) {
                        return Err(ErrorBody::new(
                            ErrorCode::Unsupported,
                            format!(
                                "{machine} speaks stream protocol {spoken}, and this client speaks stream protocol {STREAM_PROTOCOL}."
                            ),
                        )
                        .with_hint("Update legion so both sides speak the same stream protocol.")
                        .with_machine(machine));
                    }
                    greeted = true;
                    continue;
                }
            } else if fields.len() == 1 && fields.get("busy") == Some(&Value::Bool(true)) {
                continue;
            } else {
                if let Some(error_val) = fields.get("error")
                    && let Ok(body) = serde_json::from_value::<ErrorBody>(error_val.clone())
                {
                    return Err(crate::strip_remote_sent(body).with_machine(machine));
                }
                if let Ok(ready) = serde_json::from_value::<GetReadyReply>(Value::Object(fields)) {
                    return Ok(ready);
                }
                return Err(ErrorBody::new(
                    ErrorCode::Failed,
                    format!("{machine} answered stream get with an unexpected line."),
                )
                .with_hint("Update legion on that machine so both ends speak the same protocol.")
                .with_machine(machine));
            }
        }

        if greeted {
            return Err(ErrorBody::new(
                ErrorCode::Failed,
                format!("{machine} answered stream get with non-JSON output."),
            )
            .with_machine(machine));
        }

        noise_lines += 1;
        noise_bytes += line.len();
        if noise_lines > MAX_NOISE_LINES || noise_bytes > MAX_NOISE_BYTES {
            return Err(ErrorBody::new(
                ErrorCode::Unreachable,
                format!(
                    "{machine} sent too much non-protocol output before starting legion stream."
                ),
            )
            .with_hint("Check the remote account's shell profile or forced command.")
            .with_machine(machine)
            .with_details(json!({ "reason": crate::REASON_NON_PROTOCOL_OUTPUT })));
        }
    }
}

async fn read_bounded_line<R: AsyncRead + Unpin>(
    reader: &mut BufReader<R>,
    out: &mut Vec<u8>,
    limit: usize,
) -> io::Result<usize> {
    let mut total = 0usize;
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            return Ok(total);
        }
        if let Some(pos) = available.iter().position(|&b| b == b'\n') {
            let take = pos + 1;
            if total + take > limit {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "stream reply line exceeded maximum length",
                ));
            }
            out.extend_from_slice(&available[..take]);
            reader.consume(take);
            return Ok(total + take);
        }
        let len = available.len();
        if total + len > limit {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "stream reply line exceeded maximum length",
            ));
        }
        out.extend_from_slice(available);
        reader.consume(len);
        total += len;
    }
}

fn stream_io_error(
    machine: &str,
    stage: &str,
    io_err: &io::Error,
    stderr: &Option<Arc<StderrTail>>,
) -> ErrorBody {
    let mut err = ErrorBody::new(
        ErrorCode::Unreachable,
        format!("Stream to {machine} failed while {stage}: {io_err}."),
    )
    .with_machine(machine);
    if let Some(tail) = stderr {
        let text = tail.text();
        if !text.is_empty() {
            err = err.with_hint(format!("Remote stderr: {text}"));
        }
    }
    err
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    use legion_proto::{HelloResult, PROTOCOL, Response, VERSION};
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

    use super::*;
    use crate::{Link, LinkConfig, LinkOptions, NodeLink, Session, SessionOpener, TransportHandle};

    struct FakeTransport {
        opener: Arc<dyn StreamOpener>,
    }

    impl TransportHandle for FakeTransport {
        fn collect_exit_info(
            &mut self,
            _wait_limit: Duration,
        ) -> Pin<Box<dyn Future<Output = (Option<i32>, String)> + Send + '_>> {
            Box::pin(async { (Some(0), String::new()) })
        }

        fn close(self: Box<Self>, _grace: Duration) -> Pin<Box<dyn Future<Output = ()> + Send>> {
            Box::pin(async {})
        }

        fn stream_opener(&self) -> Option<Arc<dyn StreamOpener>> {
            Some(Arc::clone(&self.opener))
        }
    }

    struct InMemoryStreamOpener {
        payload: Arc<Vec<u8>>,
        opened: Arc<AtomicUsize>,
        cancelled: Arc<AtomicBool>,
        banner_noise: bool,
        truncate_at: Option<usize>,
    }

    impl StreamOpener for InMemoryStreamOpener {
        fn open_stream(
            &self,
            _cancel: CancellationToken,
        ) -> Pin<Box<dyn Future<Output = Result<RawStream, StreamOpenError>> + Send + '_>> {
            let payload = Arc::clone(&self.payload);
            let opened = Arc::clone(&self.opened);
            let cancelled = Arc::clone(&self.cancelled);
            let banner_noise = self.banner_noise;
            let truncate_at = self.truncate_at;
            Box::pin(async move {
                opened.fetch_add(1, Ordering::SeqCst);
                let (client_in, mut server_out) = tokio::io::duplex(16 * 1024);
                let (server_in, client_out) = tokio::io::duplex(16 * 1024);
                let stream_cancel = CancellationToken::new();
                let watch_cancel = stream_cancel.clone();

                tokio::spawn(async move {
                    let mut reader = BufReader::new(server_in);
                    let mut line = String::new();
                    if reader.read_line(&mut line).await.unwrap_or(0) == 0 {
                        return;
                    }
                    let req: Value = serde_json::from_str(&line).unwrap();
                    let offset = req["offset"].as_u64().unwrap_or(0) as usize;
                    let req_len = req
                        .get("length")
                        .and_then(Value::as_u64)
                        .map(|n| n as usize);
                    if banner_noise {
                        let _ = server_out.write_all(b"Welcome banner\n").await;
                    }
                    let _ = server_out.write_all(b"{\"protocol\":1}\n").await;
                    let _ = server_out.write_all(b"{\"busy\":true}\n").await;
                    if offset > payload.len() {
                        let err = json!({
                            "error": ErrorBody::new(ErrorCode::Conflict, "offset past end")
                                .with_details(json!({"requested": offset, "actual": payload.len()}))
                        });
                        let _ = server_out.write_all(format!("{err}\n").as_bytes()).await;
                        return;
                    }
                    let avail = payload.len() - offset;
                    let actual_len = req_len.map_or(avail, |n| n.min(avail));
                    let ready = json!({
                        "size": payload.len(),
                        "offset": offset,
                        "length": actual_len,
                        "mtime": { "secs": 1_700_000_123i64, "nanos": 456_000_000u32 }
                    });
                    let _ = server_out.write_all(format!("{ready}\n").as_bytes()).await;
                    let slice = &payload[offset..offset + actual_len];
                    let to_send = match truncate_at {
                        Some(limit) => &slice[..limit.min(slice.len())],
                        None => slice,
                    };
                    let mut pos = 0;
                    while pos < to_send.len() {
                        let end = (pos + 4096).min(to_send.len());
                        tokio::select! {
                            biased;
                            () = watch_cancel.cancelled() => {
                                cancelled.store(true, Ordering::SeqCst);
                                return;
                            }
                            res = server_out.write_all(&to_send[pos..end]) => {
                                if res.is_err() {
                                    cancelled.store(true, Ordering::SeqCst);
                                    return;
                                }
                            }
                        }
                        pos = end;
                    }
                    let _ = server_out.shutdown().await;
                });

                Ok(RawStream {
                    reader: Box::new(client_in),
                    writer: Box::new(client_out),
                    stderr: None,
                    cancel: stream_cancel,
                    guard: None,
                })
            })
        }
    }

    fn make_mock_link(
        node_name: &str,
        hello_node: &str,
        stream_opener: Arc<dyn StreamOpener>,
    ) -> Link {
        let hello_node = hello_node.to_owned();
        let opener: SessionOpener = Arc::new(move |entry, _, cancel| {
            let hello_node = hello_node.clone();
            let stream_opener = Arc::clone(&stream_opener);
            Box::pin(async move {
                let (client_in, mut server_out) = tokio::io::duplex(16 * 1024);
                let (server_in, client_out) = tokio::io::duplex(16 * 1024);
                tokio::spawn(async move {
                    let mut reader = BufReader::new(server_in);
                    let mut line = String::new();
                    while reader.read_line(&mut line).await.unwrap_or(0) > 0 {
                        let req: legion_proto::Request = serde_json::from_str(&line).unwrap();
                        let reply = Response::ok(
                            req.id,
                            serde_json::to_value(HelloResult {
                                node: hello_node.clone(),
                                version: VERSION.to_owned(),
                                build: None,
                                protocol: PROTOCOL,
                                os: "linux".to_owned(),
                                arch: "x86_64".to_owned(),
                                systems: vec![],
                                system: None,
                                boot_id: None,
                            })
                            .unwrap(),
                        );
                        let _ = server_out
                            .write_all(
                                format!("{}\n", serde_json::to_string(&reply).unwrap()).as_bytes(),
                            )
                            .await;
                        line.clear();
                    }
                });
                let transport = Box::new(FakeTransport {
                    opener: stream_opener,
                });
                Session::open_transport(
                    client_in,
                    client_out,
                    transport,
                    &entry.node,
                    None,
                    Duration::from_secs(5),
                    cancel,
                )
                .await
                .map_err(|err| err.into_error())
            })
        });
        let config = LinkConfig {
            nodes: vec![NodeLink::ssh(
                node_name,
                crate::SshTarget {
                    hosts: vec!["192.0.2.1".to_owned()],
                    port: 22,
                    user: "me".to_owned(),
                    host_keys: vec!["ssh-ed25519 AAAA".to_owned()],
                },
                Some("phone".to_owned()),
            )],
            ..LinkConfig::default()
        };
        Link::with_opener(config, LinkOptions::default(), opener)
    }

    #[tokio::test]
    async fn open_content_streams_ranges_concurrently_and_enforces_identity_and_cancel() {
        let payload: Vec<u8> = (0..128 * 1024usize).map(|i| (i % 251) as u8).collect();
        let opened = Arc::new(AtomicUsize::new(0));
        let cancelled = Arc::new(AtomicBool::new(false));
        let opener = Arc::new(InMemoryStreamOpener {
            payload: Arc::new(payload.clone()),
            opened: Arc::clone(&opened),
            cancelled: Arc::clone(&cancelled),
            banner_noise: true,
            truncate_at: None,
        });
        let link = make_mock_link("tower", "tower", opener);

        // Unknown machine is refused with NotFound and never falls back to tower.
        let err = link
            .open_content("pi", "/etc/hosts", 0, None)
            .await
            .err()
            .unwrap();
        assert_eq!(err.code, ErrorCode::NotFound);

        // Two concurrent ranges open at once on the same session.
        let mut s1 = link
            .open_content("tower", "/big.bin", 1000, Some(32_000))
            .await
            .unwrap();
        let mut s2 = link
            .open_content("tower", "/big.bin", 50_000, Some(40_000))
            .await
            .unwrap();
        assert_eq!(opened.load(Ordering::SeqCst), 2);
        assert_eq!(s1.size(), payload.len() as u64);
        assert_eq!(s1.offset(), 1000);
        assert_eq!(s1.length(), 32_000);
        assert_eq!(
            s1.mtime(),
            ContentTime {
                secs: 1_700_000_123,
                nanos: 456_000_000
            }
        );

        let (mut b1, mut b2) = (Vec::new(), Vec::new());
        let (r1, r2) = tokio::join!(s1.read_to_end(&mut b1), s2.read_to_end(&mut b2));
        r1.unwrap();
        r2.unwrap();
        assert_eq!(b1, &payload[1000..33_000]);
        assert_eq!(b2, &payload[50_000..90_000]);

        // Cancelling mid-stream stops reading and cancels the channel.
        let mut s3 = link
            .open_content("tower", "/big.bin", 0, None)
            .await
            .unwrap();
        let mut head = [0u8; 64];
        s3.read_exact(&mut head).await.unwrap();
        s3.cancel();
        let err = s3.read(&mut head).await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::Interrupted);
        drop(s3);
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(cancelled.load(Ordering::SeqCst));

        // Node identity mismatch is refused before opening any stream.
        let mismatch_opened = Arc::new(AtomicUsize::new(0));
        let mismatch_link = make_mock_link(
            "tower",
            "impostor",
            Arc::new(InMemoryStreamOpener {
                payload: Arc::new(payload.clone()),
                opened: Arc::clone(&mismatch_opened),
                cancelled: Arc::new(AtomicBool::new(false)),
                banner_noise: false,
                truncate_at: None,
            }),
        );
        let err = mismatch_link
            .open_content("tower", "/big.bin", 0, Some(10))
            .await
            .err()
            .unwrap();
        assert_eq!(err.code, ErrorCode::Denied);
        assert_eq!(mismatch_opened.load(Ordering::SeqCst), 0);

        // Truncated remote stream yields UnexpectedEof rather than silent truncation.
        let trunc_link = make_mock_link(
            "tower",
            "tower",
            Arc::new(InMemoryStreamOpener {
                payload: Arc::new(payload),
                opened: Arc::new(AtomicUsize::new(0)),
                cancelled: Arc::new(AtomicBool::new(false)),
                banner_noise: false,
                truncate_at: Some(500),
            }),
        );
        let mut trunc = trunc_link
            .open_content("tower", "/big.bin", 0, Some(4096))
            .await
            .unwrap();
        let mut buf = Vec::new();
        let err = trunc.read_to_end(&mut buf).await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);
    }
}
