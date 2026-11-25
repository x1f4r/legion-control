//! Streaming random-access file reads (`legion stream` `get`) over a [`Link`](crate::Link) session.
//!
//! Each call to [`Link::open_content`](crate::Link::open_content) opens one independent `legion
//! stream` channel (an SSH exec channel on the existing connection, or a local child process) and
//! runs the `get` handshake (`protocol`, `op: "get"`, `path`, `offset`, optional `length`, and
//! `keep_open: true` on windowed transports). Once the remote node replies with `{size, offset,
//! length, mtime}`, [`ContentStream`] validates the reply bounds strictly and yields exact bytes
//! via [`AsyncRead`] with backpressure.
//!
//! On windowed transports such as `russh`, [`ContentStream`] keeps a single `keep_open` channel
//! open for the whole stream and pulls adaptive windows (starting small for fast first byte and
//! growing up to a few MiB while the reader keeps up) with at most one window of pipelined
//! read-ahead, so an unread or paused stream never blocks the shared SSH session loop and produces
//! one coalesced remote audit record. Against older nodes without `keep_open`, it falls back to
//! per-window channels and retries channel refusals with exponential backoff.

use std::future::Future;
use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

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
/// Maximum `{"done": true}` trailer line length on a `keep_open` frame.
const MAX_DONE_LINE: usize = 256;
/// Maximum pre-protocol noise lines skipped before `{"protocol": 1}` (login banners, shell noise).
const MAX_NOISE_LINES: usize = 64;
/// Maximum cumulative bytes of noise skipped before giving up.
const MAX_NOISE_BYTES: usize = 64 * 1024;
/// Read buffer capacity wrapping the underlying stream so handshake lines and bulk reads share one
/// bounded buffer with low lock/syscall overhead.
const STREAM_BUF_BYTES: usize = 256 * 1024;
/// If the reader takes longer than this before starting to drain a window, shrink the adaptive
/// window size back toward `initial_bytes` to avoid over-buffering a paused consumer.
const ADAPTIVE_STALL_THRESHOLD: Duration = Duration::from_millis(500);
/// Initial backoff when retrying a refused channel on a fallback continuation window.
const CHANNEL_REFUSED_INITIAL_BACKOFF: Duration = Duration::from_millis(25);
/// Maximum backoff between retries after a channel refusal.
const CHANNEL_REFUSED_MAX_BACKOFF: Duration = Duration::from_millis(500);

/// Type-erased async reader for a raw `legion stream` pipe.
pub type BoxStreamRead = Box<dyn AsyncRead + Unpin + Send>;
/// Type-erased async writer for a raw `legion stream` pipe.
pub type BoxStreamWrite = Box<dyn AsyncWrite + Unpin + Send>;

/// Adaptive pull-window bounds advertised by a [`StreamOpener`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WindowPolicy {
    /// Initial window size for fast time to first byte and minimal unread buffering.
    pub initial_bytes: u64,
    /// Maximum window size reached while the caller reads continuously.
    pub max_bytes: u64,
}

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
    /// Pull-based window bounds when the transport buffers incoming channel data in memory (such
    /// as `russh`). `None` requests the full range in one go.
    fn window_policy(&self) -> Option<WindowPolicy> {
        None
    }

    /// Opens one fresh `legion stream` channel or child process.
    fn open_stream(
        &self,
        cancel: CancellationToken,
    ) -> Pin<Box<dyn Future<Output = Result<RawStream, StreamOpenError>> + Send + '_>>;
}

struct OpenedChannel {
    reader: BufReader<BoxStreamRead>,
    writer: BoxStreamWrite,
    channel_cancel: CancellationToken,
    guard: Option<Box<dyn Send + Sync>>,
}

struct ActiveChunk {
    reader: BufReader<BoxStreamRead>,
    _writer: BoxStreamWrite,
    chunk_remaining: u64,
    checked_eof: bool,
    chunk_cancel: CancellationToken,
    _guard: Option<Box<dyn Send + Sync>>,
}

impl ActiveChunk {
    fn from_opened(ch: OpenedChannel, chunk_remaining: u64) -> Self {
        Self {
            reader: ch.reader,
            _writer: ch.writer,
            chunk_remaining,
            checked_eof: false,
            chunk_cancel: ch.channel_cancel,
            _guard: ch.guard,
        }
    }
}

impl Drop for ActiveChunk {
    fn drop(&mut self) {
        if self.chunk_remaining > 0 || !self.checked_eof {
            self.chunk_cancel.cancel();
        }
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

#[derive(Clone, Copy, Debug)]
struct PendingWindow {
    offset: u64,
    length: u64,
}

enum KeepOpenPhase {
    Data {
        remaining: u64,
        started: Instant,
    },
    DoneTrailer {
        line: Vec<u8>,
    },
    NextReady {
        expected: PendingWindow,
        line: Vec<u8>,
    },
    ClosingEof,
    Finished,
}

struct KeepOpenState {
    reader: BufReader<BoxStreamRead>,
    writer: Option<BoxStreamWrite>,
    pending_write: Vec<u8>,
    pending_write_pos: usize,
    need_flush: bool,
    phase: KeepOpenPhase,
    in_flight: Option<PendingWindow>,
    next_request_offset: u64,
    logical_end: u64,
    policy: WindowPolicy,
    current_window_bytes: u64,
    machine: String,
    path: String,
    as_principal: Option<String>,
    expected_size: u64,
    expected_mtime: ContentTime,
    expected_file_id: Option<u64>,
    channel_cancel: CancellationToken,
    _guard: Option<Box<dyn Send + Sync>>,
}

impl Drop for KeepOpenState {
    fn drop(&mut self) {
        if !matches!(self.phase, KeepOpenPhase::Finished) {
            self.channel_cancel.cancel();
        }
    }
}

impl KeepOpenState {
    fn drive_pending_write(&mut self, cx: &mut Context<'_>) -> io::Result<()> {
        let Some(writer) = self.writer.as_mut() else {
            self.pending_write.clear();
            self.pending_write_pos = 0;
            self.need_flush = false;
            return Ok(());
        };
        while self.pending_write_pos < self.pending_write.len() {
            match Pin::new(&mut *writer)
                .poll_write(cx, &self.pending_write[self.pending_write_pos..])
            {
                Poll::Pending => return Ok(()),
                Poll::Ready(Err(err)) => return Err(err),
                Poll::Ready(Ok(0)) => {
                    return Err(io::Error::new(
                        io::ErrorKind::WriteZero,
                        "content stream stdin closed while sending next window header",
                    ));
                }
                Poll::Ready(Ok(n)) => {
                    self.pending_write_pos += n;
                }
            }
        }
        if !self.pending_write.is_empty() {
            self.pending_write.clear();
            self.pending_write_pos = 0;
        }
        if self.need_flush {
            match Pin::new(&mut *writer).poll_flush(cx) {
                Poll::Pending => {}
                Poll::Ready(Err(err)) => return Err(err),
                Poll::Ready(Ok(())) => self.need_flush = false,
            }
        }
        Ok(())
    }

    fn enqueue_next_window(&mut self, elapsed: Duration) {
        if self.in_flight.is_some() || self.next_request_offset >= self.logical_end {
            return;
        }
        let next_size = if elapsed < ADAPTIVE_STALL_THRESHOLD {
            let grown = if self.current_window_bytes < 1024 * 1024 {
                (self.current_window_bytes.saturating_mul(4)).min(1024 * 1024)
            } else {
                self.current_window_bytes.saturating_mul(2)
            };
            grown.clamp(self.policy.initial_bytes, self.policy.max_bytes)
        } else {
            (self.current_window_bytes / 2).max(self.policy.initial_bytes)
        };
        self.current_window_bytes = next_size;

        let offset = self.next_request_offset;
        let want = (self.logical_end - offset).min(next_size);
        self.next_request_offset = offset.saturating_add(want);

        let header = build_get_header(
            &self.path,
            offset,
            Some(want),
            self.as_principal.as_deref(),
            true,
        );
        self.pending_write.extend_from_slice(&header);
        self.need_flush = true;
        self.in_flight = Some(PendingWindow {
            offset,
            length: want,
        });
    }
}

struct PerChunkState {
    active: Option<ActiveChunk>,
    next_chunk: Option<NextChunkFuture>,
    continuation: Option<ChunkContinuation>,
}

enum StreamMode {
    KeepOpen(Box<KeepOpenState>),
    PerChunk(Box<PerChunkState>),
}

/// Async byte-range reader returned by [`Link::open_content`](crate::Link::open_content).
///
/// Implements [`AsyncRead`] with backpressure. Dropping or calling [`ContentStream::cancel`]
/// closes the underlying SSH exec channel or local child process immediately without affecting the
/// parent session or other open content streams.
pub struct ContentStream {
    mode: StreamMode,
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
        let cleanly_finished = match &self.mode {
            StreamMode::KeepOpen(state) => matches!(state.phase, KeepOpenPhase::Finished),
            StreamMode::PerChunk(state) => self.remaining == 0 && state.active.is_none(),
        };
        if !cleanly_finished {
            self.cancel.cancel();
        }
    }
}

impl AsyncRead for ContentStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        if this.cancel.is_cancelled() {
            return Poll::Ready(Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "content stream was cancelled",
            )));
        }
        if buf.remaining() == 0 {
            return Poll::Ready(Ok(()));
        }

        match &mut this.mode {
            StreamMode::KeepOpen(state) => poll_keep_open(state, &mut this.remaining, cx, buf),
            StreamMode::PerChunk(state) => poll_per_chunk(
                &mut state.active,
                &mut state.next_chunk,
                &mut state.continuation,
                &mut this.remaining,
                &this.cancel,
                cx,
                buf,
            ),
        }
    }
}

fn poll_keep_open(
    state: &mut KeepOpenState,
    total_remaining: &mut u64,
    cx: &mut Context<'_>,
    buf: &mut ReadBuf<'_>,
) -> Poll<io::Result<()>> {
    loop {
        if let Err(err) = state.drive_pending_write(cx) {
            *total_remaining = 0;
            state.phase = KeepOpenPhase::Finished;
            state.channel_cancel.cancel();
            return Poll::Ready(Err(err));
        }

        match &mut state.phase {
            KeepOpenPhase::Finished => return Poll::Ready(Ok(())),
            KeepOpenPhase::Data { remaining, started } => {
                if *remaining == 0 {
                    state.phase = KeepOpenPhase::DoneTrailer {
                        line: Vec::with_capacity(32),
                    };
                    continue;
                }
                match Pin::new(&mut state.reader).poll_fill_buf(cx) {
                    Poll::Pending => return Poll::Pending,
                    Poll::Ready(Err(err)) => {
                        *total_remaining = 0;
                        state.phase = KeepOpenPhase::Finished;
                        state.channel_cancel.cancel();
                        return Poll::Ready(Err(err));
                    }
                    Poll::Ready(Ok([])) => {
                        let missing = *total_remaining;
                        *total_remaining = 0;
                        state.phase = KeepOpenPhase::Finished;
                        state.channel_cancel.cancel();
                        return Poll::Ready(Err(io::Error::new(
                            io::ErrorKind::UnexpectedEof,
                            format!("content stream ended with {missing} bytes remaining"),
                        )));
                    }
                    Poll::Ready(Ok(avail)) => {
                        let max_read = usize::try_from(*remaining)
                            .unwrap_or(usize::MAX)
                            .min(buf.remaining());
                        let take = avail.len().min(max_read);
                        buf.put_slice(&avail[..take]);
                        Pin::new(&mut state.reader).consume(take);
                        *remaining -= take as u64;
                        *total_remaining -= take as u64;
                        let elapsed = started.elapsed();

                        // Pipeline at most one read-ahead window once the caller starts reading
                        // this window, keeping unread buffering strictly bounded.
                        if state.in_flight.is_none()
                            && state.next_request_offset < state.logical_end
                        {
                            state.enqueue_next_window(elapsed);
                            if let Err(err) = state.drive_pending_write(cx) {
                                *total_remaining = 0;
                                state.phase = KeepOpenPhase::Finished;
                                state.channel_cancel.cancel();
                                return Poll::Ready(Err(err));
                            }
                        }
                        return Poll::Ready(Ok(()));
                    }
                }
            }
            KeepOpenPhase::DoneTrailer { line } => {
                match Pin::new(&mut state.reader).poll_fill_buf(cx) {
                    Poll::Pending => return Poll::Pending,
                    Poll::Ready(Err(err)) => {
                        *total_remaining = 0;
                        state.phase = KeepOpenPhase::Finished;
                        state.channel_cancel.cancel();
                        return Poll::Ready(Err(err));
                    }
                    Poll::Ready(Ok([])) => {
                        *total_remaining = 0;
                        state.phase = KeepOpenPhase::Finished;
                        state.channel_cancel.cancel();
                        return Poll::Ready(Err(io::Error::new(
                            io::ErrorKind::UnexpectedEof,
                            "content stream closed before done frame",
                        )));
                    }
                    Poll::Ready(Ok(avail)) => {
                        if let Some(pos) = avail.iter().position(|&b| b == b'\n') {
                            let take = pos + 1;
                            if line.len() + take > MAX_DONE_LINE {
                                *total_remaining = 0;
                                state.phase = KeepOpenPhase::Finished;
                                state.channel_cancel.cancel();
                                return Poll::Ready(Err(io::Error::new(
                                    io::ErrorKind::InvalidData,
                                    "remote sent extra bytes past announced window length",
                                )));
                            }
                            line.extend_from_slice(&avail[..take]);
                            Pin::new(&mut state.reader).consume(take);
                            if !is_done_frame(line) {
                                *total_remaining = 0;
                                state.phase = KeepOpenPhase::Finished;
                                state.channel_cancel.cancel();
                                return Poll::Ready(Err(io::Error::new(
                                    io::ErrorKind::InvalidData,
                                    "remote sent an invalid done frame",
                                )));
                            }
                            if *total_remaining == 0 {
                                // Drop stdin cleanly so the remote `legion stream` flushes its
                                // coalesced audit entry and exits with EOF.
                                state.writer = None;
                                state.phase = KeepOpenPhase::ClosingEof;
                                continue;
                            }
                            if state.in_flight.is_none() {
                                state.enqueue_next_window(Duration::ZERO);
                                if let Err(err) = state.drive_pending_write(cx) {
                                    *total_remaining = 0;
                                    state.phase = KeepOpenPhase::Finished;
                                    state.channel_cancel.cancel();
                                    return Poll::Ready(Err(err));
                                }
                            }
                            let Some(expected) = state.in_flight.take() else {
                                *total_remaining = 0;
                                state.phase = KeepOpenPhase::Finished;
                                state.channel_cancel.cancel();
                                return Poll::Ready(Err(io::Error::new(
                                    io::ErrorKind::UnexpectedEof,
                                    "content stream ended before delivering all bytes",
                                )));
                            };
                            state.phase = KeepOpenPhase::NextReady {
                                expected,
                                line: Vec::with_capacity(128),
                            };
                            continue;
                        }
                        let n = avail.len();
                        if line.len() + n > MAX_DONE_LINE {
                            *total_remaining = 0;
                            state.phase = KeepOpenPhase::Finished;
                            state.channel_cancel.cancel();
                            return Poll::Ready(Err(io::Error::new(
                                io::ErrorKind::InvalidData,
                                "remote sent more bytes than announced",
                            )));
                        }
                        line.extend_from_slice(avail);
                        Pin::new(&mut state.reader).consume(n);
                    }
                }
            }
            KeepOpenPhase::NextReady { expected, line } => {
                let expected = *expected;
                match Pin::new(&mut state.reader).poll_fill_buf(cx) {
                    Poll::Pending => return Poll::Pending,
                    Poll::Ready(Err(err)) => {
                        *total_remaining = 0;
                        state.phase = KeepOpenPhase::Finished;
                        state.channel_cancel.cancel();
                        return Poll::Ready(Err(err));
                    }
                    Poll::Ready(Ok([])) => {
                        *total_remaining = 0;
                        state.phase = KeepOpenPhase::Finished;
                        state.channel_cancel.cancel();
                        return Poll::Ready(Err(io::Error::new(
                            io::ErrorKind::UnexpectedEof,
                            "content stream closed before answering next window",
                        )));
                    }
                    Poll::Ready(Ok(avail)) => {
                        if let Some(pos) = avail.iter().position(|&b| b == b'\n') {
                            let take = pos + 1;
                            if line.len() + take > MAX_REPLY_LINE {
                                *total_remaining = 0;
                                state.phase = KeepOpenPhase::Finished;
                                state.channel_cancel.cancel();
                                return Poll::Ready(Err(io::Error::new(
                                    io::ErrorKind::InvalidData,
                                    "remote sent an oversized stream reply line",
                                )));
                            }
                            line.extend_from_slice(&avail[..take]);
                            Pin::new(&mut state.reader).consume(take);

                            let Ok(Value::Object(fields)) = serde_json::from_slice::<Value>(line)
                            else {
                                *total_remaining = 0;
                                state.phase = KeepOpenPhase::Finished;
                                state.channel_cancel.cancel();
                                return Poll::Ready(Err(io::Error::new(
                                    io::ErrorKind::InvalidData,
                                    "remote answered stream get with non-JSON output",
                                )));
                            };
                            if fields.len() == 1 && fields.get("busy") == Some(&Value::Bool(true)) {
                                line.clear();
                                continue;
                            }
                            if let Some(error_val) = fields.get("error")
                                && let Ok(body) =
                                    serde_json::from_value::<ErrorBody>(error_val.clone())
                            {
                                *total_remaining = 0;
                                state.phase = KeepOpenPhase::Finished;
                                state.channel_cancel.cancel();
                                let kind = match body.code {
                                    ErrorCode::Denied => io::ErrorKind::PermissionDenied,
                                    ErrorCode::NotFound => io::ErrorKind::NotFound,
                                    ErrorCode::Timeout => io::ErrorKind::TimedOut,
                                    ErrorCode::Cancelled => io::ErrorKind::Interrupted,
                                    _ => io::ErrorKind::Other,
                                };
                                return Poll::Ready(Err(io::Error::new(kind, body.message)));
                            }
                            let Ok(ready) =
                                serde_json::from_value::<GetReadyReply>(Value::Object(fields))
                            else {
                                *total_remaining = 0;
                                state.phase = KeepOpenPhase::Finished;
                                state.channel_cancel.cancel();
                                return Poll::Ready(Err(io::Error::new(
                                    io::ErrorKind::InvalidData,
                                    "remote answered stream get with an unexpected line",
                                )));
                            };
                            let validated = match validate_get_reply(
                                &state.machine,
                                expected.offset,
                                Some(expected.length),
                                ready,
                            ) {
                                Ok(v) => v,
                                Err(err) => {
                                    *total_remaining = 0;
                                    state.phase = KeepOpenPhase::Finished;
                                    state.channel_cancel.cancel();
                                    return Poll::Ready(Err(io::Error::new(
                                        io::ErrorKind::InvalidData,
                                        err.message,
                                    )));
                                }
                            };
                            if validated.size != state.expected_size
                                || validated.mtime != state.expected_mtime
                                || validated.file_id != state.expected_file_id
                            {
                                *total_remaining = 0;
                                state.phase = KeepOpenPhase::Finished;
                                state.channel_cancel.cancel();
                                return Poll::Ready(Err(io::Error::new(
                                    io::ErrorKind::InvalidData,
                                    "file changed on remote node while streaming",
                                )));
                            }
                            state.phase = KeepOpenPhase::Data {
                                remaining: validated.length,
                                started: Instant::now(),
                            };
                            continue;
                        }
                        let n = avail.len();
                        if line.len() + n > MAX_REPLY_LINE {
                            *total_remaining = 0;
                            state.phase = KeepOpenPhase::Finished;
                            state.channel_cancel.cancel();
                            return Poll::Ready(Err(io::Error::new(
                                io::ErrorKind::InvalidData,
                                "remote sent an oversized stream reply line",
                            )));
                        }
                        line.extend_from_slice(avail);
                        Pin::new(&mut state.reader).consume(n);
                    }
                }
            }
            KeepOpenPhase::ClosingEof => match Pin::new(&mut state.reader).poll_fill_buf(cx) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(Err(err)) => {
                    state.phase = KeepOpenPhase::Finished;
                    return Poll::Ready(Err(err));
                }
                Poll::Ready(Ok(extra)) if !extra.is_empty() => {
                    state.phase = KeepOpenPhase::Finished;
                    state.channel_cancel.cancel();
                    return Poll::Ready(Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "remote sent more bytes than announced",
                    )));
                }
                Poll::Ready(Ok(_)) => {
                    state.phase = KeepOpenPhase::Finished;
                    return Poll::Ready(Ok(()));
                }
            },
        }
    }
}

fn poll_per_chunk(
    active_slot: &mut Option<ActiveChunk>,
    next_chunk: &mut Option<NextChunkFuture>,
    continuation: &mut Option<ChunkContinuation>,
    total_remaining: &mut u64,
    cancel: &CancellationToken,
    cx: &mut Context<'_>,
    buf: &mut ReadBuf<'_>,
) -> Poll<io::Result<()>> {
    loop {
        if let Some(active) = active_slot.as_mut() {
            if active.chunk_remaining > 0 {
                match Pin::new(&mut active.reader).poll_fill_buf(cx) {
                    Poll::Pending => return Poll::Pending,
                    Poll::Ready(Err(err)) => {
                        *total_remaining = 0;
                        *active_slot = None;
                        return Poll::Ready(Err(err));
                    }
                    Poll::Ready(Ok([])) => {
                        let missing = *total_remaining;
                        *total_remaining = 0;
                        *active_slot = None;
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
                        *total_remaining -= take as u64;
                        return Poll::Ready(Ok(()));
                    }
                }
            } else if !active.checked_eof {
                match Pin::new(&mut active.reader).poll_fill_buf(cx) {
                    Poll::Pending => return Poll::Pending,
                    Poll::Ready(Err(err)) => {
                        *total_remaining = 0;
                        *active_slot = None;
                        return Poll::Ready(Err(err));
                    }
                    Poll::Ready(Ok(extra)) if !extra.is_empty() => {
                        active.chunk_cancel.cancel();
                        *total_remaining = 0;
                        *active_slot = None;
                        return Poll::Ready(Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "remote sent more bytes than announced",
                        )));
                    }
                    Poll::Ready(Ok(_)) => {
                        active.checked_eof = true;
                        *active_slot = None;
                        if *total_remaining == 0 {
                            return Poll::Ready(Ok(()));
                        }
                    }
                }
            } else {
                *active_slot = None;
            }
        }

        if *total_remaining == 0 {
            return Poll::Ready(Ok(()));
        }

        if next_chunk.is_none() {
            let remaining = *total_remaining;
            let Some(cont) = continuation.as_mut() else {
                *total_remaining = 0;
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
            let chunk_cancel = cancel.child_token();

            *next_chunk = Some(Box::pin(async move {
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
                    chunk_cancel,
                )
                .await
            }));
        }

        if let Some(fut) = next_chunk.as_mut() {
            match fut.as_mut().poll(cx) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(Err(err)) => {
                    *next_chunk = None;
                    *total_remaining = 0;
                    return Poll::Ready(Err(err));
                }
                Poll::Ready(Ok(chunk)) => {
                    *next_chunk = None;
                    *active_slot = Some(chunk);
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
    #[serde(default)]
    keep_open: Option<bool>,
}

struct ValidatedGet {
    size: u64,
    offset: u64,
    length: u64,
    mtime: ContentTime,
    file_id: Option<u64>,
    keep_open: bool,
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
        keep_open: ready.keep_open == Some(true),
    })
}

fn build_get_header(
    path: &str,
    offset: u64,
    length: Option<u64>,
    as_principal: Option<&str>,
    keep_open: bool,
) -> Vec<u8> {
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
    if keep_open {
        header["keep_open"] = json!(true);
    }
    let mut bytes = header.to_string().into_bytes();
    bytes.push(b'\n');
    bytes
}

fn is_done_frame(line: &[u8]) -> bool {
    let Ok(Value::Object(map)) = serde_json::from_slice::<Value>(line) else {
        return false;
    };
    map.len() == 1 && map.get("done") == Some(&Value::Bool(true))
}

/// Runs the `legion stream` `get` handshake on `stream` and returns a positioned [`ContentStream`].
///
/// When `opener` advertises a [`WindowPolicy`], the initial request asks for `keep_open: true`
/// capped at `initial_bytes`. If the remote speaks `keep_open`, subsequent adaptive pull-based
/// windows run over the same channel with one window of read-ahead; otherwise, older remotes fall
/// back to per-window channels with exponential backoff on channel refusals.
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
    let policy = opener
        .as_ref()
        .and_then(|o| o.window_policy())
        .filter(|p| p.initial_bytes > 0 && p.max_bytes >= p.initial_bytes);
    let first_request_len = match policy {
        Some(p) => Some(length.map_or(p.initial_bytes, |n| n.min(p.initial_bytes))),
        None => length,
    };
    let request_keep_open = policy.is_some();

    let (opened, validated) = open_one_chunk(
        stream,
        machine,
        path,
        offset,
        first_request_len,
        as_principal,
        request_keep_open,
        timeout,
        cancel,
    )
    .await?;

    // Compute the full logical range length requested by the caller across all windows.
    let total_available = validated.size - validated.offset;
    let total_length = length.map_or(total_available, |n| n.min(total_available));

    let stream_cancel = CancellationToken::new();
    let active_cancel = opened.channel_cancel.clone();
    let master = stream_cancel.clone();
    tokio::spawn(async move {
        tokio::select! {
            biased;
            () = master.cancelled() => active_cancel.cancel(),
            () = active_cancel.cancelled() => {}
        }
    });

    let mode = if let Some(policy) = policy
        && validated.keep_open
    {
        let logical_end = validated.offset.saturating_add(total_length);
        let next_request_offset = validated.offset.saturating_add(validated.length);
        StreamMode::KeepOpen(Box::new(KeepOpenState {
            reader: opened.reader,
            writer: Some(opened.writer),
            pending_write: Vec::new(),
            pending_write_pos: 0,
            need_flush: false,
            phase: KeepOpenPhase::Data {
                remaining: validated.length,
                started: Instant::now(),
            },
            in_flight: None,
            next_request_offset,
            logical_end,
            policy,
            current_window_bytes: policy.initial_bytes,
            machine: machine.to_owned(),
            path: path.to_owned(),
            as_principal: as_principal.map(str::to_owned),
            expected_size: validated.size,
            expected_mtime: validated.mtime,
            expected_file_id: validated.file_id,
            channel_cancel: opened.channel_cancel,
            _guard: opened.guard,
        }))
    } else {
        let chunk = ActiveChunk::from_opened(opened, validated.length);
        let continuation = match (opener, policy) {
            (Some(opener), Some(p)) if validated.length < total_length => Some(ChunkContinuation {
                opener,
                machine: machine.to_owned(),
                path: path.to_owned(),
                as_principal: as_principal.map(str::to_owned),
                timeout,
                chunk_bytes: p.max_bytes,
                next_offset: validated.offset.saturating_add(validated.length),
                expected_size: validated.size,
                expected_mtime: validated.mtime,
                expected_file_id: validated.file_id,
            }),
            _ => None,
        };
        StreamMode::PerChunk(Box::new(PerChunkState {
            active: Some(chunk),
            next_chunk: None,
            continuation,
        }))
    };

    Ok(ContentStream {
        mode,
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
    let deadline = Instant::now() + timeout;
    let mut backoff = CHANNEL_REFUSED_INITIAL_BACKOFF;
    let raw = loop {
        match opener.open_stream(cancel.clone()).await {
            Ok(raw) => break raw,
            Err(StreamOpenError::ChannelRefused(body)) => {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    return Err(io::Error::other(body.message));
                }
                let delay = backoff.min(remaining);
                backoff = backoff.saturating_mul(2).min(CHANNEL_REFUSED_MAX_BACKOFF);
                tokio::select! {
                    biased;
                    () = cancel.cancelled() => {
                        return Err(io::Error::new(
                            io::ErrorKind::Interrupted,
                            "content stream was cancelled",
                        ));
                    }
                    () = tokio::time::sleep(delay) => {}
                }
            }
            Err(StreamOpenError::DeadSession(body) | StreamOpenError::Failed(body)) => {
                return Err(io::Error::other(body.message));
            }
        }
    };
    let (opened, validated) = open_one_chunk(
        raw,
        &machine,
        &path,
        offset,
        Some(length),
        as_principal.as_deref(),
        false,
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
    Ok(ActiveChunk::from_opened(opened, validated.length))
}

#[allow(clippy::too_many_arguments)]
async fn open_one_chunk(
    stream: RawStream,
    machine: &str,
    path: &str,
    offset: u64,
    length: Option<u64>,
    as_principal: Option<&str>,
    keep_open: bool,
    timeout: Duration,
    cancel: CancellationToken,
) -> Result<(OpenedChannel, ValidatedGet), ErrorBody> {
    let RawStream {
        reader,
        mut writer,
        stderr,
        cancel: chunk_cancel,
        guard,
    } = stream;

    let header_bytes = build_get_header(path, offset, length, as_principal, keep_open);

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
        OpenedChannel {
            reader: buf_reader,
            writer,
            channel_cancel: chunk_cancel,
            guard,
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
        fn window_policy(&self) -> Option<WindowPolicy> {
            self.chunk_bytes.map(|n| WindowPolicy {
                initial_bytes: n,
                max_bytes: n,
            })
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

    #[tokio::test]
    async fn keep_open_reuses_one_channel_with_adaptive_windows_and_bounded_read_ahead() {
        use std::sync::Mutex;

        struct KeepOpenOpener {
            payload: Arc<Vec<u8>>,
            policy: WindowPolicy,
            opened: Arc<AtomicUsize>,
            requested_windows: Arc<Mutex<Vec<(u64, u64)>>>,
        }

        impl StreamOpener for KeepOpenOpener {
            fn window_policy(&self) -> Option<WindowPolicy> {
                Some(self.policy)
            }

            fn open_stream(
                &self,
                _cancel: CancellationToken,
            ) -> Pin<Box<dyn Future<Output = Result<RawStream, StreamOpenError>> + Send + '_>>
            {
                let payload = Arc::clone(&self.payload);
                let opened = Arc::clone(&self.opened);
                let requested_windows = Arc::clone(&self.requested_windows);
                Box::pin(async move {
                    opened.fetch_add(1, Ordering::SeqCst);
                    let (client_in, mut server_out) = tokio::io::duplex(256 * 1024);
                    let (server_in, client_out) = tokio::io::duplex(64 * 1024);
                    let stream_cancel = CancellationToken::new();

                    tokio::spawn(async move {
                        let _ = server_out.write_all(b"{\"protocol\":1}\n").await;
                        let mut reader = BufReader::new(server_in);
                        let mut line = String::new();
                        loop {
                            line.clear();
                            if reader.read_line(&mut line).await.unwrap_or(0) == 0 {
                                break;
                            }
                            let req: Value = serde_json::from_str(&line).unwrap();
                            assert_eq!(req["keep_open"], json!(true));
                            let offset = req["offset"].as_u64().unwrap() as usize;
                            let want = req["length"].as_u64().unwrap() as usize;
                            let avail = payload.len().saturating_sub(offset);
                            let actual = want.min(avail);
                            requested_windows
                                .lock()
                                .unwrap()
                                .push((offset as u64, actual as u64));
                            let ready = json!({
                                "size": payload.len(),
                                "offset": offset,
                                "length": actual,
                                "mtime": { "secs": 1_700_000_123i64, "nanos": 0u32 },
                                "file_id": 77u64,
                                "keep_open": true
                            });
                            if server_out
                                .write_all(format!("{ready}\n").as_bytes())
                                .await
                                .is_err()
                            {
                                break;
                            }
                            if server_out
                                .write_all(&payload[offset..offset + actual])
                                .await
                                .is_err()
                            {
                                break;
                            }
                            if server_out.write_all(b"{\"done\":true}\n").await.is_err() {
                                break;
                            }
                            let _ = server_out.flush().await;
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

        let payload: Vec<u8> = (0..512 * 1024usize).map(|i| (i % 251) as u8).collect();
        let opened = Arc::new(AtomicUsize::new(0));
        let requested_windows = Arc::new(Mutex::new(Vec::new()));
        let opener = Arc::new(KeepOpenOpener {
            payload: Arc::new(payload.clone()),
            policy: WindowPolicy {
                initial_bytes: 16 * 1024,
                max_bytes: 128 * 1024,
            },
            opened: Arc::clone(&opened),
            requested_windows: Arc::clone(&requested_windows),
        });
        let link = make_mock_link("tower", "tower", opener, Arc::new(AtomicUsize::new(0)));

        let mut stream = link
            .open_content("tower", "/video.mp4", 0, None)
            .await
            .unwrap();
        // Before any bytes are read, only the initial 16 KiB window was requested (zero read-ahead).
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert_eq!(requested_windows.lock().unwrap().len(), 1);
        assert_eq!(requested_windows.lock().unwrap()[0], (0, 16 * 1024));

        // Reading 1 KiB inside Window 0 triggers at most one read-ahead window (Window 1).
        let mut head = [0u8; 1024];
        stream.read_exact(&mut head).await.unwrap();
        assert_eq!(&head[..], &payload[..1024]);
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert_eq!(
            requested_windows.lock().unwrap().len(),
            2,
            "must pipeline at most one window ahead while paused mid-window"
        );

        // Drain the rest of the 512 KiB stream and verify exact bytes, single channel, and growth.
        let mut rest = Vec::new();
        stream.read_to_end(&mut rest).await.unwrap();
        let mut full = head.to_vec();
        full.extend_from_slice(&rest);
        assert_eq!(full, payload);
        assert_eq!(opened.load(Ordering::SeqCst), 1);
        let windows = requested_windows.lock().unwrap().clone();
        assert_eq!(windows[0].1, 16 * 1024);
        assert!(
            windows.iter().any(|&(_, len)| len == 128 * 1024),
            "expected adaptive window to grow to max_bytes (128 KiB): {windows:?}"
        );
    }

    #[tokio::test]
    async fn later_window_channel_refused_retries_with_backoff_and_succeeds() {
        struct FlakyWindowOpener {
            payload: Arc<Vec<u8>>,
            attempts: Arc<AtomicUsize>,
        }

        impl StreamOpener for FlakyWindowOpener {
            fn window_policy(&self) -> Option<WindowPolicy> {
                Some(WindowPolicy {
                    initial_bytes: 16 * 1024,
                    max_bytes: 16 * 1024,
                })
            }

            fn open_stream(
                &self,
                _cancel: CancellationToken,
            ) -> Pin<Box<dyn Future<Output = Result<RawStream, StreamOpenError>> + Send + '_>>
            {
                let payload = Arc::clone(&self.payload);
                let attempts = Arc::clone(&self.attempts);
                Box::pin(async move {
                    let n = attempts.fetch_add(1, Ordering::SeqCst);
                    // Succeed on attempt 0 (window 1), refuse attempts 1 and 2 (window 2), then succeed.
                    if n == 1 || n == 2 {
                        return Err(StreamOpenError::ChannelRefused(ErrorBody::new(
                            ErrorCode::Busy,
                            "MaxSessions reached",
                        )));
                    }
                    let (client_in, mut server_out) = tokio::io::duplex(32 * 1024);
                    let (server_in, client_out) = tokio::io::duplex(8 * 1024);
                    tokio::spawn(async move {
                        let mut reader = BufReader::new(server_in);
                        let mut line = String::new();
                        if reader.read_line(&mut line).await.unwrap_or(0) == 0 {
                            return;
                        }
                        let req: Value = serde_json::from_str(&line).unwrap();
                        let offset = req["offset"].as_u64().unwrap() as usize;
                        let len = req["length"].as_u64().unwrap() as usize;
                        let actual = len.min(payload.len() - offset);
                        let _ = server_out.write_all(b"{\"protocol\":1}\n").await;
                        let ready = json!({
                            "size": payload.len(),
                            "offset": offset,
                            "length": actual,
                            "mtime": { "secs": 1_700_000_123i64, "nanos": 0u32 }
                        });
                        let _ = server_out.write_all(format!("{ready}\n").as_bytes()).await;
                        let _ = server_out
                            .write_all(&payload[offset..offset + actual])
                            .await;
                        let _ = server_out.shutdown().await;
                    });
                    Ok(RawStream {
                        reader: Box::new(client_in),
                        writer: Box::new(client_out),
                        stderr: None,
                        cancel: CancellationToken::new(),
                        guard: None,
                    })
                })
            }
        }

        let payload: Vec<u8> = (0..40 * 1024usize).map(|i| (i % 199) as u8).collect();
        let attempts = Arc::new(AtomicUsize::new(0));
        let opener = Arc::new(FlakyWindowOpener {
            payload: Arc::new(payload.clone()),
            attempts: Arc::clone(&attempts),
        });
        let link = make_mock_link("tower", "tower", opener, Arc::new(AtomicUsize::new(0)));

        let mut stream = link
            .open_content("tower", "/legacy.bin", 0, None)
            .await
            .unwrap();
        let mut out = Vec::new();
        stream.read_to_end(&mut out).await.unwrap();
        assert_eq!(out, payload);
        // 3 windows (16 KiB + 16 KiB + 8 KiB) + 2 refused retries = 5 attempts.
        assert_eq!(attempts.load(Ordering::SeqCst), 5);
    }
}
