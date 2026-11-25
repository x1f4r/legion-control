//! Streaming random-access file reads (`legion stream` `get`) over a [`Link`](crate::Link) session.
//!
//! Each call to [`Link::open_content`](crate::Link::open_content) opens an independent `legion
//! stream` channel (an SSH exec channel on the existing connection, or a local child process) and
//! runs the `get` handshake (`protocol`, `op: "get"`, `path`, `offset`, optional `length`). Once
//! the remote node replies with `{size, offset, length, mtime}`, [`ContentStream`] validates the
//! reply bounds strictly, yields exactly `length` bytes via [`AsyncRead`] with backpressure (and
//! chunked windows on SSH transports so an unread stream never blocks other channels on the
//! shared connection), and closes the channel immediately when cancelled or dropped.

use std::future::Future;
use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use legion_proto::{ErrorBody, ErrorCode};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::io::{AsyncBufRead, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader, ReadBuf};
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
    /// Opening one channel was refused (for example sshd `MaxSessions`), while the underlying
    /// shared session remains healthy and must not be discarded.
    ChannelRefused(ErrorBody),
    /// Opening failed for a reason that should be surfaced directly to the caller.
    Failed(ErrorBody),
}

/// Factory exposed by a [`TransportHandle`](crate::TransportHandle) to open independent
/// `legion stream` channels over the same underlying connection or local binary.
pub trait StreamOpener: Send + Sync + 'static {
    /// Maximum bytes requested per underlying channel when the transport buffers incoming channel
    /// data in a fixed window (such as `russh`). `None` requests the full range on one channel.
    fn max_chunk_bytes(&self) -> Option<u64> {
        None
    }

    /// Opens one fresh `legion stream` channel or child process.
    fn open_stream(
        &self,
        cancel: CancellationToken,
    ) -> Pin<Box<dyn Future<Output = Result<RawStream, StreamOpenError>> + Send + '_>>;
}

struct ActiveChunk {
    reader: BufReader<BoxStreamRead>,
    _writer: BoxStreamWrite,
    chunk_remaining: u64,
    checked_eof: bool,
    chunk_cancel: CancellationToken,
    _guard: Option<Box<dyn Send + Sync>>,
}

impl Drop for ActiveChunk {
    fn drop(&mut self) {
        self.chunk_cancel.cancel();
    }
}

struct ChunkContinuation {
    opener: Arc<dyn StreamOpener>,
    machine: String,
    path: String,
    as_principal: Option<String>,
    timeout: Duration,
    chunk_bytes: u64,
    next_offset: u64,
    expected_size: u64,
    expected_mtime: ContentTime,
    expected_file_id: Option<u64>,
}

type NextChunkFuture = Pin<Box<dyn Future<Output = Result<ActiveChunk, io::Error>> + Send>>;

/// Async byte-range reader returned by [`Link::open_content`](crate::Link::open_content).
///
/// Implements [`AsyncRead`] with backpressure. Dropping or calling [`ContentStream::cancel`]
/// closes the underlying SSH exec channel or local child process immediately without affecting the
/// parent session or other open content streams.
pub struct ContentStream {
    active: Option<ActiveChunk>,
    next_chunk: Option<NextChunkFuture>,
    continuation: Option<ChunkContinuation>,
    size: u64,
    offset: u64,
    length: u64,
    remaining: u64,
    mtime: ContentTime,
    file_id: Option<u64>,
    cancel: CancellationToken,
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

    /// Platform file identity (derived from device and inode on Unix) when reported by the node.
    pub fn file_id(&self) -> Option<u64> {
        self.file_id
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
        if buf.remaining() == 0 {
            return Poll::Ready(Ok(()));
        }

        loop {
            if let Some(active) = self.active.as_mut() {
                if active.chunk_remaining > 0 {
                    match Pin::new(&mut active.reader).poll_fill_buf(cx) {
                        Poll::Pending => return Poll::Pending,
                        Poll::Ready(Err(err)) => {
                            self.remaining = 0;
                            self.active = None;
                            return Poll::Ready(Err(err));
                        }
                        Poll::Ready(Ok([])) => {
                            let missing = self.remaining;
                            self.remaining = 0;
                            self.active = None;
                            return Poll::Ready(Err(io::Error::new(
                                io::ErrorKind::UnexpectedEof,
                                format!("content stream ended with {missing} bytes remaining"),
                            )));
                        }
                        Poll::Ready(Ok(avail)) => {
                            let max_read = usize::try_from(active.chunk_remaining)
                                .unwrap_or(usize::MAX)
                                .min(buf.remaining());
                            let take = avail.len().min(max_read);
                            buf.put_slice(&avail[..take]);
                            Pin::new(&mut active.reader).consume(take);
                            active.chunk_remaining -= take as u64;
                            self.remaining -= take as u64;
                            return Poll::Ready(Ok(()));
                        }
                    }
                } else if !active.checked_eof {
                    // Verify the remote closed cleanly after the exact announced byte count and
                    // never forward trailing bytes past `length`.
                    match Pin::new(&mut active.reader).poll_fill_buf(cx) {
                        Poll::Pending => return Poll::Pending,
                        Poll::Ready(Err(err)) => {
                            self.remaining = 0;
                            self.active = None;
                            return Poll::Ready(Err(err));
                        }
                        Poll::Ready(Ok(extra)) if !extra.is_empty() => {
                            active.chunk_cancel.cancel();
                            self.remaining = 0;
                            self.active = None;
                            return Poll::Ready(Err(io::Error::new(
                                io::ErrorKind::InvalidData,
                                "remote sent more bytes than announced",
                            )));
                        }
                        Poll::Ready(Ok(_)) => {
                            active.checked_eof = true;
                            self.active = None;
                            if self.remaining == 0 {
                                return Poll::Ready(Ok(()));
                            }
                            // Continue loop to fetch the next bounded chunk.
                        }
                    }
                } else {
                    self.active = None;
                }
            }

            if self.remaining == 0 {
                return Poll::Ready(Ok(()));
            }

            if self.next_chunk.is_none() {
                let remaining = self.remaining;
                let Some(cont) = self.continuation.as_mut() else {
                    self.remaining = 0;
                    return Poll::Ready(Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "content stream ended before delivering all bytes",
                    )));
                };
                let want = remaining.min(cont.chunk_bytes);
                let next_offset = cont.next_offset;
                cont.next_offset = cont.next_offset.saturating_add(want);
                let opener = Arc::clone(&cont.opener);
                let machine = cont.machine.clone();
                let path = cont.path.clone();
                let as_principal = cont.as_principal.clone();
                let timeout = cont.timeout;
                let expected_size = cont.expected_size;
                let expected_mtime = cont.expected_mtime;
                let expected_file_id = cont.expected_file_id;
                let cancel = self.cancel.child_token();

                self.next_chunk = Some(Box::pin(async move {
                    open_continuation_chunk(
                        opener,
                        machine,
                        path,
                        next_offset,
                        want,
                        as_principal,
                        timeout,
                        expected_size,
                        expected_mtime,
                        expected_file_id,
                        cancel,
                    )
                    .await
                }));
            }

            if let Some(fut) = self.next_chunk.as_mut() {
                match fut.as_mut().poll(cx) {
                    Poll::Pending => return Poll::Pending,
                    Poll::Ready(Err(err)) => {
                        self.next_chunk = None;
                        self.remaining = 0;
                        return Poll::Ready(Err(err));
                    }
                    Poll::Ready(Ok(chunk)) => {
                        self.next_chunk = None;
                        self.active = Some(chunk);
                    }
                }
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
    #[serde(default)]
    file_id: Option<u64>,
}

struct ValidatedGet {
    size: u64,
    offset: u64,
    length: u64,
    mtime: ContentTime,
    file_id: Option<u64>,
}

fn validate_get_reply(
    machine: &str,
    req_offset: u64,
    req_length: Option<u64>,
    ready: GetReadyReply,
) -> Result<ValidatedGet, ErrorBody> {
    let (Some(reply_length), Some(mtime)) = (ready.length, ready.mtime) else {
        return Err(ErrorBody::new(
            ErrorCode::Unsupported,
            format!(
                "{machine} answered stream get without length and mtime (older legion version)."
            ),
        )
        .with_hint("Update legion on that machine so it supports random-access content reads.")
        .with_machine(machine));
    };

    if ready.offset != req_offset {
        return Err(ErrorBody::new(
            ErrorCode::Failed,
            format!(
                "{machine} answered stream get with offset {}, expected {req_offset}.",
                ready.offset
            ),
        )
        .with_machine(machine));
    }
    if ready.offset > ready.size {
        return Err(ErrorBody::new(
            ErrorCode::Failed,
            format!(
                "{machine} answered stream get with offset {} past reported file size {}.",
                ready.offset, ready.size
            ),
        )
        .with_machine(machine));
    }
    let available = ready.size - ready.offset;
    let expected_length = req_length.map_or(available, |n| n.min(available));
    if reply_length != expected_length {
        return Err(ErrorBody::new(
            ErrorCode::Failed,
            format!(
                "{machine} answered stream get with length {reply_length}, expected {expected_length}."
            ),
        )
        .with_machine(machine));
    }
    if ready
        .offset
        .checked_add(reply_length)
        .is_none_or(|end| end > ready.size)
    {
        return Err(ErrorBody::new(
            ErrorCode::Failed,
            format!("{machine} answered stream get with overflowing range metadata."),
        )
        .with_machine(machine));
    }

    Ok(ValidatedGet {
        size: ready.size,
        offset: ready.offset,
        length: reply_length,
        mtime,
        file_id: ready.file_id,
    })
}

/// Runs the `legion stream` `get` handshake on `stream` and returns a positioned [`ContentStream`].
///
/// When `opener` advertises a `max_chunk_bytes` window, the initial request is capped to that
/// window and subsequent chunks are fetched lazily on the same opener as the caller reads.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn handshake_get(
    stream: RawStream,
    opener: Option<Arc<dyn StreamOpener>>,
    machine: &str,
    path: &str,
    offset: u64,
    length: Option<u64>,
    as_principal: Option<&str>,
    timeout: Duration,
    cancel: CancellationToken,
) -> Result<ContentStream, ErrorBody> {
    let chunk_cap = opener
        .as_ref()
        .and_then(|o| o.max_chunk_bytes())
        .filter(|&cap| cap > 0);
    let first_request_len = match chunk_cap {
        Some(cap) => Some(length.map_or(cap, |n| n.min(cap))),
        None => length,
    };

    let (chunk, validated) = open_one_chunk(
        stream,
        machine,
        path,
        offset,
        first_request_len,
        as_principal,
        timeout,
        cancel,
    )
    .await?;

    // Compute the full logical range length requested by the caller across all chunks.
    let total_available = validated.size - validated.offset;
    let total_length = length.map_or(total_available, |n| n.min(total_available));

    let stream_cancel = CancellationToken::new();
    // Tie the active chunk's cancellation to the stream's master cancellation token.
    let active_cancel = chunk.chunk_cancel.clone();
    let master = stream_cancel.clone();
    tokio::spawn(async move {
        tokio::select! {
            biased;
            () = master.cancelled() => active_cancel.cancel(),
            () = active_cancel.cancelled() => {}
        }
    });

    let continuation = match (opener, chunk_cap) {
        (Some(opener), Some(chunk_bytes)) if validated.length < total_length => {
            Some(ChunkContinuation {
                opener,
                machine: machine.to_owned(),
                path: path.to_owned(),
                as_principal: as_principal.map(str::to_owned),
                timeout,
                chunk_bytes,
                next_offset: validated.offset.saturating_add(validated.length),
                expected_size: validated.size,
                expected_mtime: validated.mtime,
                expected_file_id: validated.file_id,
            })
        }
        _ => None,
    };

    Ok(ContentStream {
        active: Some(chunk),
        next_chunk: None,
        continuation,
        size: validated.size,
        offset: validated.offset,
        length: total_length,
        remaining: total_length,
        mtime: validated.mtime,
        file_id: validated.file_id,
        cancel: stream_cancel,
        _flight: None,
    })
}

#[allow(clippy::too_many_arguments)]
async fn open_continuation_chunk(
    opener: Arc<dyn StreamOpener>,
    machine: String,
    path: String,
    offset: u64,
    length: u64,
    as_principal: Option<String>,
    timeout: Duration,
    expected_size: u64,
    expected_mtime: ContentTime,
    expected_file_id: Option<u64>,
    cancel: CancellationToken,
) -> Result<ActiveChunk, io::Error> {
    let raw = opener
        .open_stream(cancel.clone())
        .await
        .map_err(|err| match err {
            StreamOpenError::DeadSession(body)
            | StreamOpenError::ChannelRefused(body)
            | StreamOpenError::Failed(body) => io::Error::other(body.message),
        })?;
    let (chunk, validated) = open_one_chunk(
        raw,
        &machine,
        &path,
        offset,
        Some(length),
        as_principal.as_deref(),
        timeout,
        cancel,
    )
    .await
    .map_err(|err| io::Error::other(err.message))?;

    if validated.size != expected_size
        || validated.mtime != expected_mtime
        || validated.file_id != expected_file_id
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "file changed on remote node while streaming",
        ));
    }
    Ok(chunk)
}

#[allow(clippy::too_many_arguments)]
async fn open_one_chunk(
    stream: RawStream,
    machine: &str,
    path: &str,
    offset: u64,
    length: Option<u64>,
    as_principal: Option<&str>,
    timeout: Duration,
    cancel: CancellationToken,
) -> Result<(ActiveChunk, ValidatedGet), ErrorBody> {
    let RawStream {
        reader,
        mut writer,
        stderr,
        cancel: chunk_cancel,
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
            chunk_cancel.cancel();
            return Err(ErrorBody::new(
                ErrorCode::Cancelled,
                format!("Opening content on {machine} was cancelled."),
            )
            .with_machine(machine));
        }
        res = tokio::time::timeout(timeout, handshake) => match res {
            Ok(Ok(ready)) => ready,
            Ok(Err(err)) => {
                chunk_cancel.cancel();
                return Err(err);
            }
            Err(_) => {
                chunk_cancel.cancel();
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

    let validated = match validate_get_reply(machine, offset, length, ready) {
        Ok(v) => v,
        Err(err) => {
            chunk_cancel.cancel();
            return Err(err);
        }
    };

    Ok((
        ActiveChunk {
            reader: buf_reader,
            _writer: writer,
            chunk_remaining: validated.length,
            checked_eof: false,
            chunk_cancel,
            _guard: guard,
        },
        validated,
    ))
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
    use tokio::io::AsyncBufReadExt;

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

    #[derive(Clone, Copy)]
    enum Misbehavior {
        None,
        WrongOffset,
        WrongLengthTooLarge,
        OverflowRange,
        ExtraBytes(usize),
        TruncateAt(usize),
        RefuseChannel,
    }

    struct InMemoryStreamOpener {
        payload: Arc<Vec<u8>>,
        opened: Arc<AtomicUsize>,
        cancelled: Arc<AtomicBool>,
        banner_noise: bool,
        chunk_bytes: Option<u64>,
        misbehavior: Misbehavior,
    }

    impl StreamOpener for InMemoryStreamOpener {
        fn max_chunk_bytes(&self) -> Option<u64> {
            self.chunk_bytes
        }

        fn open_stream(
            &self,
            _cancel: CancellationToken,
        ) -> Pin<Box<dyn Future<Output = Result<RawStream, StreamOpenError>> + Send + '_>> {
            let payload = Arc::clone(&self.payload);
            let opened = Arc::clone(&self.opened);
            let cancelled = Arc::clone(&self.cancelled);
            let banner_noise = self.banner_noise;
            let misbehavior = self.misbehavior;
            Box::pin(async move {
                opened.fetch_add(1, Ordering::SeqCst);
                if matches!(misbehavior, Misbehavior::RefuseChannel) {
                    return Err(StreamOpenError::ChannelRefused(
                        ErrorBody::new(ErrorCode::Busy, "MaxSessions reached").with_details(
                            json!({"reason": "channel_open_failure", "retryable": true}),
                        ),
                    ));
                }
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
                    let ready = match misbehavior {
                        Misbehavior::WrongOffset => json!({
                            "size": payload.len(),
                            "offset": offset + 1,
                            "length": actual_len,
                            "mtime": { "secs": 1_700_000_123i64, "nanos": 456_000_000u32 }
                        }),
                        Misbehavior::WrongLengthTooLarge => json!({
                            "size": payload.len(),
                            "offset": offset,
                            "length": actual_len + 10,
                            "mtime": { "secs": 1_700_000_123i64, "nanos": 456_000_000u32 }
                        }),
                        Misbehavior::OverflowRange => json!({
                            "size": u64::MAX,
                            "offset": offset,
                            "length": u64::MAX,
                            "mtime": { "secs": 1_700_000_123i64, "nanos": 456_000_000u32 }
                        }),
                        _ => json!({
                            "size": payload.len(),
                            "offset": offset,
                            "length": actual_len,
                            "mtime": { "secs": 1_700_000_123i64, "nanos": 456_000_000u32 },
                            "file_id": 42u64
                        }),
                    };
                    let _ = server_out.write_all(format!("{ready}\n").as_bytes()).await;
                    let slice = &payload[offset..offset + actual_len];
                    let to_send = match misbehavior {
                        Misbehavior::TruncateAt(limit) => &slice[..limit.min(slice.len())],
                        _ => slice,
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
                    if let Misbehavior::ExtraBytes(extra) = misbehavior {
                        let _ = server_out.write_all(&vec![0xEE; extra]).await;
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
        session_opens: Arc<AtomicUsize>,
    ) -> Link {
        let hello_node = hello_node.to_owned();
        let opener: SessionOpener = Arc::new(move |entry, _, cancel| {
            let hello_node = hello_node.clone();
            let stream_opener = Arc::clone(&stream_opener);
            let session_opens = Arc::clone(&session_opens);
            Box::pin(async move {
                session_opens.fetch_add(1, Ordering::SeqCst);
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
            chunk_bytes: Some(32 * 1024),
            misbehavior: Misbehavior::None,
        });
        let link = make_mock_link("tower", "tower", opener, Arc::new(AtomicUsize::new(0)));

        // Unknown machine is refused with NotFound and never falls back to tower.
        let err = link
            .open_content("pi", "/etc/hosts", 0, None)
            .await
            .err()
            .unwrap();
        assert_eq!(err.code, ErrorCode::NotFound);

        // Two concurrent ranges open at once on the same session (s2 spans >1 chunk).
        let mut s1 = link
            .open_content("tower", "/big.bin", 1000, Some(32_000))
            .await
            .unwrap();
        let mut s2 = link
            .open_content("tower", "/big.bin", 50_000, Some(70_000))
            .await
            .unwrap();
        assert_eq!(s1.size(), payload.len() as u64);
        assert_eq!(s1.offset(), 1000);
        assert_eq!(s1.length(), 32_000);
        assert_eq!(s1.file_id(), Some(42));
        assert_eq!(s2.length(), 70_000);
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
        assert_eq!(b2, &payload[50_000..120_000]);
        // s1 used 1 chunk; s2 (70_000 bytes with 32 KiB chunks) used 3 chunks.
        assert_eq!(opened.load(Ordering::SeqCst), 4);

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
                payload: Arc::new(payload),
                opened: Arc::clone(&mismatch_opened),
                cancelled: Arc::new(AtomicBool::new(false)),
                banner_noise: false,
                chunk_bytes: None,
                misbehavior: Misbehavior::None,
            }),
            Arc::new(AtomicUsize::new(0)),
        );
        let err = mismatch_link
            .open_content("tower", "/big.bin", 0, Some(10))
            .await
            .err()
            .unwrap();
        assert_eq!(err.code, ErrorCode::Denied);
        assert_eq!(mismatch_opened.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn misbehaving_remote_replies_and_extra_or_short_bytes_are_rejected() {
        let payload = Arc::new(vec![0xABu8; 8192]);

        for misbehavior in [
            Misbehavior::WrongOffset,
            Misbehavior::WrongLengthTooLarge,
            Misbehavior::OverflowRange,
        ] {
            let link = make_mock_link(
                "tower",
                "tower",
                Arc::new(InMemoryStreamOpener {
                    payload: Arc::clone(&payload),
                    opened: Arc::new(AtomicUsize::new(0)),
                    cancelled: Arc::new(AtomicBool::new(false)),
                    banner_noise: false,
                    chunk_bytes: None,
                    misbehavior,
                }),
                Arc::new(AtomicUsize::new(0)),
            );
            let err = link
                .open_content("tower", "/f.bin", 10, Some(100))
                .await
                .err()
                .unwrap();
            assert_eq!(err.code, ErrorCode::Failed, "{err}");
        }

        // Truncated remote yields UnexpectedEof.
        let trunc_link = make_mock_link(
            "tower",
            "tower",
            Arc::new(InMemoryStreamOpener {
                payload: Arc::clone(&payload),
                opened: Arc::new(AtomicUsize::new(0)),
                cancelled: Arc::new(AtomicBool::new(false)),
                banner_noise: false,
                chunk_bytes: None,
                misbehavior: Misbehavior::TruncateAt(500),
            }),
            Arc::new(AtomicUsize::new(0)),
        );
        let mut trunc = trunc_link
            .open_content("tower", "/f.bin", 0, Some(4096))
            .await
            .unwrap();
        let mut buf = Vec::new();
        let err = trunc.read_to_end(&mut buf).await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);

        // Extra bytes beyond announced length are never forwarded and fail with InvalidData.
        let extra_link = make_mock_link(
            "tower",
            "tower",
            Arc::new(InMemoryStreamOpener {
                payload,
                opened: Arc::new(AtomicUsize::new(0)),
                cancelled: Arc::new(AtomicBool::new(false)),
                banner_noise: false,
                chunk_bytes: None,
                misbehavior: Misbehavior::ExtraBytes(64),
            }),
            Arc::new(AtomicUsize::new(0)),
        );
        let mut extra = extra_link
            .open_content("tower", "/f.bin", 0, Some(100))
            .await
            .unwrap();
        let mut out = Vec::new();
        let err = extra.read_to_end(&mut out).await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert_eq!(out.len(), 100, "extra bytes must never be forwarded");
        assert!(out.iter().all(|&b| b == 0xAB));
    }

    #[tokio::test]
    async fn channel_refused_returns_busy_without_discarding_healthy_session() {
        let session_opens = Arc::new(AtomicUsize::new(0));
        let link = make_mock_link(
            "tower",
            "tower",
            Arc::new(InMemoryStreamOpener {
                payload: Arc::new(vec![1u8; 64]),
                opened: Arc::new(AtomicUsize::new(0)),
                cancelled: Arc::new(AtomicBool::new(false)),
                banner_noise: false,
                chunk_bytes: None,
                misbehavior: Misbehavior::RefuseChannel,
            }),
            Arc::clone(&session_opens),
        );

        // Open healthy session first.
        let _ = link.hello("tower").await.unwrap();
        assert_eq!(session_opens.load(Ordering::SeqCst), 1);

        // Channel refusal returns retryable Busy and must NOT invalidate/reopen the session.
        let err = link
            .open_content("tower", "/a.bin", 0, Some(10))
            .await
            .err()
            .unwrap();
        assert_eq!(err.code, ErrorCode::Busy, "{err}");
        assert_eq!(session_opens.load(Ordering::SeqCst), 1);

        // Subsequent RPC still uses the exact same live session.
        let _ = link.hello("tower").await.unwrap();
        assert_eq!(session_opens.load(Ordering::SeqCst), 1);
    }
}
