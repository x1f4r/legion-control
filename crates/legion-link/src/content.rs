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

use std::collections::VecDeque;
use std::future::Future;
use std::io;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};
use std::time::{Duration, Instant};

use legion_proto::{ErrorBody, ErrorCode};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::io::{AsyncBufRead, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader, ReadBuf};
use tokio::time::Sleep;
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
/// Initial backoff when retrying a refused channel on a fallback continuation window or reopen.
const CHANNEL_REFUSED_INITIAL_BACKOFF: Duration = Duration::from_millis(25);
/// Maximum backoff between retries after a channel refusal.
const CHANNEL_REFUSED_MAX_BACKOFF: Duration = Duration::from_millis(500);
/// Reader inactivity duration after which a `keep_open` stream parks its channel and frees its
/// SSH slot while retaining already-buffered payload bytes.
pub(crate) const DEFAULT_IDLE_PARK_TIMEOUT: Duration = Duration::from_secs(2);
/// Maximum wait for forward progress from a live remote channel while the reader is actively
/// polling (never armed while a stream is paused or parked).
pub(crate) const DEFAULT_WINDOW_PROGRESS_TIMEOUT: Duration = Duration::from_secs(30);
/// Maximum consecutive `{"busy": true}` lines tolerated before failing the stream.
pub(crate) const DEFAULT_MAX_BUSY_LINES: usize = 32;
/// Maximum consecutive unparked channel reopens without delivering payload bytes.
const MAX_CONSECUTIVE_REOPENS: usize = 3;
/// Maximum read-ahead payload bytes retained in memory while a content stream is parked (64 KiB).
pub(crate) const PARKED_BUFFER_CAP: usize = 64 * 1024;
/// Default maximum number of parked content streams retained per [`Link`](crate::Link).
pub const DEFAULT_MAX_PARKED_STREAMS: usize = 256;

/// Internal tuning knobs for [`ContentStream`] parking, progress timeouts, and busy-line bounds.
#[derive(Clone)]
pub(crate) struct ContentStreamOptions {
    pub idle_park_timeout: Duration,
    pub progress_timeout: Duration,
    pub max_busy_lines: usize,
    pub park_pool: Option<Arc<ParkedStreamPool>>,
}

impl Default for ContentStreamOptions {
    fn default() -> Self {
        Self {
            idle_park_timeout: DEFAULT_IDLE_PARK_TIMEOUT,
            progress_timeout: DEFAULT_WINDOW_PROGRESS_TIMEOUT,
            max_busy_lines: DEFAULT_MAX_BUSY_LINES,
            park_pool: None,
        }
    }
}

struct ParkedEntry {
    id: u64,
    evicted: Arc<AtomicBool>,
    live_channel: Arc<Mutex<LiveChannel>>,
}

#[derive(Default)]
struct ParkedPoolInner {
    next_id: u64,
    /// Ordered from least recently parked (front) to most recently parked (back).
    entries: VecDeque<ParkedEntry>,
}

/// Per-[`Link`](crate::Link) LRU cap for parked content streams.
///
/// When more than `max_parked` streams park on one link, the least recently parked stream is
/// closed: its retained read-ahead buffer is freed immediately and its next read fails cleanly
/// with [`io::ErrorKind::PermissionDenied`] (the same error shape as an expired stream).
pub(crate) struct ParkedStreamPool {
    max_parked: usize,
    inner: Mutex<ParkedPoolInner>,
}

impl ParkedStreamPool {
    pub(crate) fn new(max_parked: usize) -> Self {
        Self {
            max_parked,
            inner: Mutex::new(ParkedPoolInner::default()),
        }
    }

    fn register(&self, evicted: Arc<AtomicBool>, live_channel: Arc<Mutex<LiveChannel>>) -> u64 {
        if self.max_parked == 0 {
            evicted.store(true, Ordering::Release);
            live_channel
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .clear_all();
            return 0;
        }
        let mut to_evict = Vec::new();
        let id = {
            let mut inner = self.inner.lock().unwrap_or_else(|p| p.into_inner());
            inner.next_id = inner.next_id.wrapping_add(1).max(1);
            let id = inner.next_id;
            inner.entries.push_back(ParkedEntry {
                id,
                evicted,
                live_channel,
            });
            while inner.entries.len() > self.max_parked {
                if let Some(oldest) = inner.entries.pop_front() {
                    to_evict.push(oldest);
                }
            }
            id
        };
        for oldest in to_evict {
            oldest.evicted.store(true, Ordering::Release);
            oldest
                .live_channel
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .clear_all();
        }
        id
    }

    fn remove(&self, id: u64) {
        if id == 0 {
            return;
        }
        let mut inner = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(pos) = inner.entries.iter().position(|e| e.id == id) {
            inner.entries.remove(pos);
        }
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.inner
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .entries
            .len()
    }
}

struct LiveChannel {
    reader: Option<BufReader<BoxStreamRead>>,
    writer: Option<BoxStreamWrite>,
    /// Remaining payload bytes in the current `KeepOpenPhase::Data` window (0 otherwise).
    data_window_remaining: u64,
    /// Small read-ahead slice (at most [`PARKED_BUFFER_CAP`] = 64 KiB) retained while parked
    /// after dropping the full 256 KiB `BufReader`, underlying channel queue, and stdin duplex.
    parked_head: Vec<u8>,
    parked_pos: usize,
}

impl LiveChannel {
    fn new(
        reader: BufReader<BoxStreamRead>,
        writer: BoxStreamWrite,
        data_window_remaining: u64,
    ) -> Self {
        Self {
            reader: Some(reader),
            writer: Some(writer),
            data_window_remaining,
            parked_head: Vec::new(),
            parked_pos: 0,
        }
    }

    fn trim_on_park(&mut self) {
        self.writer = None;
        if let Some(buf_reader) = self.reader.take() {
            let buffered = buf_reader.buffer();
            let avail_payload = usize::try_from(self.data_window_remaining).unwrap_or(usize::MAX);
            let keep = buffered.len().min(avail_payload).min(PARKED_BUFFER_CAP);
            if keep > 0 {
                let mut head = Vec::with_capacity(keep);
                head.extend_from_slice(&buffered[..keep]);
                self.parked_head = head;
                self.parked_pos = 0;
            } else {
                self.parked_head = Vec::new();
                self.parked_pos = 0;
            }
            drop(buf_reader);
        }
        self.data_window_remaining = 0;
    }

    fn clear_all(&mut self) {
        self.reader = None;
        self.writer = None;
        self.data_window_remaining = 0;
        self.parked_head = Vec::new();
        self.parked_pos = 0;
    }

    #[cfg(test)]
    fn retained_bytes(&self) -> usize {
        let parked = self.parked_head.capacity();
        let active = if self.reader.is_some() {
            STREAM_BUF_BYTES
        } else {
            0
        };
        parked + active
    }
}

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
    /// Cancellation token that immediately aborts this stream channel or child process when
    /// cancelled.
    pub cancel: CancellationToken,
    /// Optional park token that closes the underlying channel and releases its concurrency slot
    /// while leaving already-buffered payload bytes readable until EOF.
    pub park: Option<CancellationToken>,
    /// Optional RAII lifecycle guard dropped when the stream parks, finishes, or is dropped.
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
    park_cancel: Option<CancellationToken>,
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

struct ReopenedKeepOpen {
    opened: OpenedChannel,
    validated: ValidatedGet,
    cursor_offset: u64,
}

type ReopenFuture = Pin<Box<dyn Future<Output = Result<ReopenedKeepOpen, io::Error>> + Send>>;

struct ParkActivity {
    started: Instant,
    last_poll_ms: AtomicU64,
    waiting_for_remote: AtomicBool,
    probe_pending: AtomicBool,
    parked: AtomicBool,
    evicted: Arc<AtomicBool>,
    park_id: AtomicU64,
    park_pool: Option<Arc<ParkedStreamPool>>,
    watchdog_cancel: CancellationToken,
    waker: Mutex<Option<Waker>>,
    guard: Mutex<Option<Box<dyn Send + Sync>>>,
}

impl ParkActivity {
    fn start(
        park_cancel: Option<CancellationToken>,
        guard: Option<Box<dyn Send + Sync>>,
        live_channel: Arc<Mutex<LiveChannel>>,
        park_pool: Option<Arc<ParkedStreamPool>>,
        idle_park_timeout: Duration,
    ) -> Arc<Self> {
        let evicted = Arc::new(AtomicBool::new(false));
        let activity = Arc::new(Self {
            started: Instant::now(),
            last_poll_ms: AtomicU64::new(0),
            waiting_for_remote: AtomicBool::new(false),
            probe_pending: AtomicBool::new(false),
            parked: AtomicBool::new(false),
            evicted: Arc::clone(&evicted),
            park_id: AtomicU64::new(0),
            park_pool: park_pool.clone(),
            watchdog_cancel: CancellationToken::new(),
            waker: Mutex::new(None),
            guard: Mutex::new(guard),
        });
        if let Some(park_token) = park_cancel
            && !idle_park_timeout.is_zero()
        {
            let weak = Arc::downgrade(&activity);
            let cancel = activity.watchdog_cancel.clone();
            let threshold_ms = u64::try_from(idle_park_timeout.as_millis()).unwrap_or(u64::MAX);
            // Probe half-way through the idle window when a read previously returned Poll::Pending:
            // an active reader re-polls immediately and advances `last_poll_ms`, whereas a read
            // future dropped by `timeout(read)` or `select!` never re-polls and parks on schedule.
            let probe_ms = (threshold_ms / 2).max(1);
            let tick = (idle_park_timeout / 4)
                .clamp(Duration::from_millis(15), Duration::from_millis(250));
            tokio::spawn(async move {
                loop {
                    tokio::select! {
                        biased;
                        () = cancel.cancelled() => break,
                        () = tokio::time::sleep(tick) => {
                            let Some(act) = weak.upgrade() else {
                                break;
                            };
                            let now_ms = u64::try_from(act.started.elapsed().as_millis())
                                .unwrap_or(u64::MAX);
                            let last_ms = act.last_poll_ms.load(Ordering::Acquire);
                            let idle_ms = now_ms.saturating_sub(last_ms);
                            if act.waiting_for_remote.load(Ordering::Acquire) {
                                if !act.probe_pending.load(Ordering::Acquire) {
                                    if idle_ms >= probe_ms {
                                        act.probe_pending.store(true, Ordering::Release);
                                        let waker = act
                                            .waker
                                            .lock()
                                            .unwrap_or_else(|p| p.into_inner())
                                            .clone();
                                        if let Some(waker) = waker {
                                            waker.wake();
                                        }
                                    }
                                    continue;
                                }
                                if idle_ms < threshold_ms {
                                    continue;
                                }
                                act.waiting_for_remote.store(false, Ordering::Release);
                                act.probe_pending.store(false, Ordering::Release);
                            } else if idle_ms < threshold_ms {
                                continue;
                            }
                            let _ = act
                                .waker
                                .lock()
                                .unwrap_or_else(|p| p.into_inner())
                                .take();
                            // Signal graceful channel EOF first, then trim the stream's buffers
                            // down to <= PARKED_BUFFER_CAP (64 KiB), register in the per-link
                            // LRU parked pool, publish `parked = true`, and release the channel
                            // concurrency slot guard.
                            park_token.cancel();
                            live_channel
                                .lock()
                                .unwrap_or_else(|p| p.into_inner())
                                .trim_on_park();
                            if let Some(pool) = &park_pool {
                                let id = pool.register(
                                    Arc::clone(&evicted),
                                    Arc::clone(&live_channel),
                                );
                                act.park_id.store(id, Ordering::Release);
                            }
                            act.parked.store(true, Ordering::Release);
                            let _ = act
                                .guard
                                .lock()
                                .unwrap_or_else(|p| p.into_inner())
                                .take();
                            break;
                        }
                    }
                }
            });
        }
        activity
    }

    fn touch_ms(&self) {
        let now_ms = u64::try_from(self.started.elapsed().as_millis()).unwrap_or(u64::MAX);
        self.last_poll_ms.store(now_ms, Ordering::Release);
        self.probe_pending.store(false, Ordering::Release);
        if self.waiting_for_remote.swap(false, Ordering::AcqRel) {
            let _ = self.waker.lock().unwrap_or_else(|p| p.into_inner()).take();
        }
    }

    fn mark_waiting(&self, waker: &Waker) {
        let mut slot = self.waker.lock().unwrap_or_else(|p| p.into_inner());
        match slot.as_mut() {
            Some(existing) => existing.clone_from(waker),
            None => *slot = Some(waker.clone()),
        }
        drop(slot);
        self.probe_pending.store(false, Ordering::Release);
        self.waiting_for_remote.store(true, Ordering::Release);
    }

    fn is_parked(&self) -> bool {
        self.parked.load(Ordering::Acquire)
    }

    fn is_evicted(&self) -> bool {
        self.evicted.load(Ordering::Acquire)
    }

    fn stop_and_release(&self) {
        self.watchdog_cancel.cancel();
        self.waiting_for_remote.store(false, Ordering::Release);
        self.probe_pending.store(false, Ordering::Release);
        let id = self.park_id.swap(0, Ordering::AcqRel);
        if id != 0
            && let Some(pool) = &self.park_pool
        {
            pool.remove(id);
        }
        let _ = self.waker.lock().unwrap_or_else(|p| p.into_inner()).take();
        let _ = self.guard.lock().unwrap_or_else(|p| p.into_inner()).take();
    }
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
    Reopening(ReopenFuture),
    ClosingEof,
    Finished,
}

struct KeepOpenState {
    live_channel: Arc<Mutex<LiveChannel>>,
    pending_write: Vec<u8>,
    pending_write_pos: usize,
    need_flush: bool,
    phase: KeepOpenPhase,
    in_flight: Option<PendingWindow>,
    next_request_offset: u64,
    logical_end: u64,
    policy: WindowPolicy,
    current_window_bytes: u64,
    opener: Option<Arc<dyn StreamOpener>>,
    timeout: Duration,
    options: ContentStreamOptions,
    machine: String,
    path: String,
    as_principal: Option<String>,
    expected_size: u64,
    expected_mtime: ContentTime,
    expected_file_id: Option<u64>,
    stream_cancel: CancellationToken,
    channel_cancel: CancellationToken,
    activity: Arc<ParkActivity>,
    consecutive_reopens: usize,
    progress_deadline: Option<Pin<Box<Sleep>>>,
    busy_count: usize,
}

impl Drop for KeepOpenState {
    fn drop(&mut self) {
        let was_parked = self.activity.is_parked();
        self.activity.stop_and_release();
        self.live_channel
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clear_all();
        if !matches!(self.phase, KeepOpenPhase::Finished) && !was_parked {
            self.channel_cancel.cancel();
        }
    }
}

fn is_reopenable_io_error(err: &io::Error) -> bool {
    matches!(
        err.kind(),
        io::ErrorKind::BrokenPipe
            | io::ErrorKind::ConnectionReset
            | io::ErrorKind::ConnectionAborted
            | io::ErrorKind::UnexpectedEof
            | io::ErrorKind::WriteZero
    )
}

fn poll_progress_deadline(
    deadline: &mut Option<Pin<Box<Sleep>>>,
    timeout: Duration,
    machine: &str,
    cx: &mut Context<'_>,
) -> Result<(), io::Error> {
    if timeout.is_zero() {
        return Ok(());
    }
    let sleep = deadline.get_or_insert_with(|| Box::pin(tokio::time::sleep(timeout)));
    if sleep.as_mut().poll(cx).is_ready() {
        *deadline = None;
        return Err(io::Error::new(
            io::ErrorKind::TimedOut,
            format!(
                "timed out waiting for content stream progress from {machine} after {}s",
                timeout.as_secs_f64()
            ),
        ));
    }
    Ok(())
}

impl KeepOpenState {
    fn fail_stream(&mut self, total_remaining: &mut u64) {
        *total_remaining = 0;
        self.phase = KeepOpenPhase::Finished;
        self.progress_deadline = None;
        let was_parked = self.activity.is_parked();
        self.activity.stop_and_release();
        self.live_channel
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clear_all();
        if !was_parked {
            self.channel_cancel.cancel();
        }
    }

    fn begin_reopen(
        &mut self,
        total_remaining: u64,
        fallback_err: io::Error,
    ) -> Result<(), io::Error> {
        if self.stream_cancel.is_cancelled() {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "content stream was cancelled",
            ));
        }
        let Some(opener) = self.opener.clone() else {
            return Err(fallback_err);
        };
        let was_parked = self.activity.is_parked();
        if !was_parked {
            if self.consecutive_reopens >= MAX_CONSECUTIVE_REOPENS {
                return Err(fallback_err);
            }
            self.consecutive_reopens += 1;
        }
        self.activity.stop_and_release();
        if !was_parked {
            self.channel_cancel.cancel();
        }
        self.live_channel
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clear_all();
        self.pending_write.clear();
        self.pending_write_pos = 0;
        self.need_flush = false;
        self.in_flight = None;
        self.progress_deadline = None;
        self.busy_count = 0;

        let cursor_offset = self.logical_end.saturating_sub(total_remaining);
        let want = total_remaining.min(self.policy.initial_bytes);
        let machine = self.machine.clone();
        let path = self.path.clone();
        let as_principal = self.as_principal.clone();
        let timeout = self.timeout;
        let max_busy_lines = self.options.max_busy_lines;
        let expected_size = self.expected_size;
        let expected_mtime = self.expected_mtime;
        let expected_file_id = self.expected_file_id;
        let cancel = self.stream_cancel.child_token();

        self.phase = KeepOpenPhase::Reopening(Box::pin(async move {
            reopen_keep_open_channel(
                opener,
                machine,
                path,
                cursor_offset,
                want,
                as_principal,
                timeout,
                max_busy_lines,
                expected_size,
                expected_mtime,
                expected_file_id,
                cancel,
            )
            .await
        }));
        Ok(())
    }

    fn has_writer(&self) -> bool {
        self.live_channel
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .writer
            .is_some()
    }

    fn clear_writer(&self) {
        self.live_channel
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .writer = None;
    }

    fn drive_pending_write(&mut self, cx: &mut Context<'_>) -> io::Result<()> {
        let mut live = self.live_channel.lock().unwrap_or_else(|p| p.into_inner());
        let Some(writer) = live.writer.as_mut() else {
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
        if self.in_flight.is_some()
            || self.next_request_offset >= self.logical_end
            || !self.has_writer()
            || self.activity.is_parked()
        {
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
    machine: String,
    progress_timeout: Duration,
    progress_deadline: Option<Pin<Box<Sleep>>>,
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

    #[cfg(test)]
    fn retained_buffer_bytes(&self) -> usize {
        match &self.mode {
            StreamMode::KeepOpen(state) => state
                .live_channel
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .retained_bytes(),
            StreamMode::PerChunk(state) => state.active.as_ref().map_or(0, |_| STREAM_BUF_BYTES),
        }
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
            StreamMode::PerChunk(state) => {
                poll_per_chunk(state, &mut this.remaining, &this.cancel, cx, buf)
            }
        }
    }
}

fn poll_keep_open(
    state: &mut KeepOpenState,
    total_remaining: &mut u64,
    cx: &mut Context<'_>,
    buf: &mut ReadBuf<'_>,
) -> Poll<io::Result<()>> {
    state.activity.touch_ms();

    loop {
        if state.activity.is_evicted() {
            state.fail_stream(total_remaining);
            return Poll::Ready(Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "parked content stream was closed due to parked stream limit",
            )));
        }

        if let KeepOpenPhase::Reopening(fut) = &mut state.phase {
            match fut.as_mut().poll(cx) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(Err(err)) => {
                    state.fail_stream(total_remaining);
                    return Poll::Ready(Err(err));
                }
                Poll::Ready(Ok(reopened)) => {
                    let active_cancel = reopened.opened.channel_cancel.clone();
                    let master = state.stream_cancel.clone();
                    tokio::spawn(async move {
                        tokio::select! {
                            biased;
                            () = master.cancelled() => active_cancel.cancel(),
                            () = active_cancel.cancelled() => {}
                        }
                    });
                    {
                        let mut live = state.live_channel.lock().unwrap_or_else(|p| p.into_inner());
                        *live = LiveChannel::new(
                            reopened.opened.reader,
                            reopened.opened.writer,
                            reopened.validated.length,
                        );
                    }
                    state.channel_cancel = reopened.opened.channel_cancel;
                    state.activity = ParkActivity::start(
                        reopened.opened.park_cancel,
                        reopened.opened.guard,
                        Arc::clone(&state.live_channel),
                        state.options.park_pool.clone(),
                        state.options.idle_park_timeout,
                    );
                    state.activity.touch_ms();
                    state.next_request_offset = reopened
                        .cursor_offset
                        .saturating_add(reopened.validated.length);
                    state.current_window_bytes = state.policy.initial_bytes;
                    state.busy_count = 0;
                    state.progress_deadline = None;
                    state.phase = KeepOpenPhase::Data {
                        remaining: reopened.validated.length,
                        started: Instant::now(),
                    };
                    continue;
                }
            }
        }

        // If the watchdog parked and trimmed this stream, drain any retained <= 64 KiB read-ahead
        // first, then transparently reopen at the exact byte offset.
        {
            let mut live = state.live_channel.lock().unwrap_or_else(|p| p.into_inner());
            if live.reader.is_none() {
                if live.parked_pos < live.parked_head.len() {
                    let avail = &live.parked_head[live.parked_pos..];
                    let max_read = usize::try_from(*total_remaining)
                        .unwrap_or(usize::MAX)
                        .min(buf.remaining());
                    let take = avail.len().min(max_read);
                    buf.put_slice(&avail[..take]);
                    live.parked_pos += take;
                    *total_remaining -= take as u64;
                    if live.parked_pos >= live.parked_head.len() {
                        live.parked_head = Vec::new();
                        live.parked_pos = 0;
                    }
                    state.progress_deadline = None;
                    state.consecutive_reopens = 0;
                    state.activity.touch_ms();
                    return Poll::Ready(Ok(()));
                }
                drop(live);
                if *total_remaining == 0 {
                    state.phase = KeepOpenPhase::Finished;
                    state.activity.stop_and_release();
                    return Poll::Ready(Ok(()));
                }
                let fallback = io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "parked content stream could not reopen",
                );
                match state.begin_reopen(*total_remaining, fallback) {
                    Ok(()) => continue,
                    Err(err) => {
                        state.fail_stream(total_remaining);
                        return Poll::Ready(Err(err));
                    }
                }
            }
        }

        if !state.activity.is_parked()
            && state.has_writer()
            && let Err(err) = state.drive_pending_write(cx)
        {
            if is_reopenable_io_error(&err) && *total_remaining > 0 {
                // Drop writer so any already-buffered bytes drain first, or reopen immediately if
                // we have no buffered data phase left.
                state.clear_writer();
                state.pending_write.clear();
                state.pending_write_pos = 0;
                state.need_flush = false;
                state.in_flight = None;
            } else {
                state.fail_stream(total_remaining);
                return Poll::Ready(Err(err));
            }
        }

        match &mut state.phase {
            KeepOpenPhase::Finished => return Poll::Ready(Ok(())),
            KeepOpenPhase::Reopening(_) => continue,
            KeepOpenPhase::Data { remaining, started } => {
                if *remaining == 0 {
                    state
                        .live_channel
                        .lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .data_window_remaining = 0;
                    if *total_remaining == 0 && state.activity.is_parked() {
                        state.phase = KeepOpenPhase::Finished;
                        state.activity.stop_and_release();
                        return Poll::Ready(Ok(()));
                    }
                    state.phase = KeepOpenPhase::DoneTrailer {
                        line: Vec::with_capacity(32),
                    };
                    continue;
                }
                let data_poll = {
                    let mut live = state.live_channel.lock().unwrap_or_else(|p| p.into_inner());
                    live.data_window_remaining = *remaining;
                    match live.reader.as_mut() {
                        None => None,
                        Some(reader) => match Pin::new(&mut *reader).poll_fill_buf(cx) {
                            Poll::Pending => Some(Poll::Pending),
                            Poll::Ready(Err(err)) => Some(Poll::Ready(Err(err))),
                            Poll::Ready(Ok([])) => Some(Poll::Ready(Ok(0usize))),
                            Poll::Ready(Ok(avail)) => {
                                let max_read = usize::try_from(*remaining)
                                    .unwrap_or(usize::MAX)
                                    .min(buf.remaining());
                                let take = avail.len().min(max_read);
                                buf.put_slice(&avail[..take]);
                                Pin::new(&mut *reader).consume(take);
                                *remaining -= take as u64;
                                *total_remaining -= take as u64;
                                live.data_window_remaining = *remaining;
                                Some(Poll::Ready(Ok(take)))
                            }
                        },
                    }
                };
                match data_poll {
                    None => continue,
                    Some(Poll::Pending) => {
                        if let Err(err) = poll_progress_deadline(
                            &mut state.progress_deadline,
                            state.options.progress_timeout,
                            &state.machine,
                            cx,
                        ) {
                            state.fail_stream(total_remaining);
                            return Poll::Ready(Err(err));
                        }
                        state.activity.mark_waiting(cx.waker());
                        return Poll::Pending;
                    }
                    Some(Poll::Ready(Err(err))) => {
                        if is_reopenable_io_error(&err)
                            && *total_remaining > 0
                            && state.begin_reopen(*total_remaining, err).is_ok()
                        {
                            continue;
                        }
                        state.fail_stream(total_remaining);
                        return Poll::Ready(Err(io::Error::new(
                            io::ErrorKind::UnexpectedEof,
                            "content stream ended unexpectedly",
                        )));
                    }
                    Some(Poll::Ready(Ok(0))) => {
                        let missing = *total_remaining;
                        let fallback = io::Error::new(
                            io::ErrorKind::UnexpectedEof,
                            format!("content stream ended with {missing} bytes remaining"),
                        );
                        match state.begin_reopen(missing, fallback) {
                            Ok(()) => continue,
                            Err(err) => {
                                state.fail_stream(total_remaining);
                                return Poll::Ready(Err(err));
                            }
                        }
                    }
                    Some(Poll::Ready(Ok(_))) => {
                        let elapsed = started.elapsed();
                        state.progress_deadline = None;
                        state.consecutive_reopens = 0;
                        state.activity.touch_ms();

                        // Pipeline at most one read-ahead window once the caller starts reading
                        // this window, keeping unread buffering strictly bounded.
                        if !state.activity.is_parked()
                            && state.has_writer()
                            && state.in_flight.is_none()
                            && state.next_request_offset < state.logical_end
                        {
                            state.enqueue_next_window(elapsed);
                            if let Err(err) = state.drive_pending_write(cx) {
                                if is_reopenable_io_error(&err) {
                                    state.clear_writer();
                                    state.pending_write.clear();
                                    state.pending_write_pos = 0;
                                    state.need_flush = false;
                                    state.in_flight = None;
                                } else {
                                    state.fail_stream(total_remaining);
                                    return Poll::Ready(Err(err));
                                }
                            }
                        }
                        return Poll::Ready(Ok(()));
                    }
                }
            }
            KeepOpenPhase::DoneTrailer { line } => {
                enum LinePoll {
                    Missing,
                    Pending,
                    Err(io::Error),
                    Eof,
                    LineReady,
                    Oversized,
                    More,
                }
                let polled = {
                    let mut live = state.live_channel.lock().unwrap_or_else(|p| p.into_inner());
                    live.data_window_remaining = 0;
                    match live.reader.as_mut() {
                        None => LinePoll::Missing,
                        Some(reader) => match Pin::new(&mut *reader).poll_fill_buf(cx) {
                            Poll::Pending => LinePoll::Pending,
                            Poll::Ready(Err(err)) => LinePoll::Err(err),
                            Poll::Ready(Ok([])) => LinePoll::Eof,
                            Poll::Ready(Ok(avail)) => {
                                if let Some(pos) = avail.iter().position(|&b| b == b'\n') {
                                    let take = pos + 1;
                                    if line.len() + take > MAX_DONE_LINE {
                                        LinePoll::Oversized
                                    } else {
                                        line.extend_from_slice(&avail[..take]);
                                        Pin::new(&mut *reader).consume(take);
                                        LinePoll::LineReady
                                    }
                                } else {
                                    let n = avail.len();
                                    if line.len() + n > MAX_DONE_LINE {
                                        LinePoll::Oversized
                                    } else {
                                        line.extend_from_slice(avail);
                                        Pin::new(&mut *reader).consume(n);
                                        LinePoll::More
                                    }
                                }
                            }
                        },
                    }
                };
                match polled {
                    LinePoll::Missing => continue,
                    LinePoll::More => continue,
                    LinePoll::Pending => {
                        if let Err(err) = poll_progress_deadline(
                            &mut state.progress_deadline,
                            state.options.progress_timeout,
                            &state.machine,
                            cx,
                        ) {
                            state.fail_stream(total_remaining);
                            return Poll::Ready(Err(err));
                        }
                        state.activity.mark_waiting(cx.waker());
                        return Poll::Pending;
                    }
                    LinePoll::Err(err) => {
                        if is_reopenable_io_error(&err) && *total_remaining > 0 {
                            match state.begin_reopen(*total_remaining, err) {
                                Ok(()) => continue,
                                Err(e) => {
                                    state.fail_stream(total_remaining);
                                    return Poll::Ready(Err(e));
                                }
                            }
                        }
                        state.fail_stream(total_remaining);
                        return Poll::Ready(Err(err));
                    }
                    LinePoll::Eof => {
                        if *total_remaining > 0 {
                            let fallback = io::Error::new(
                                io::ErrorKind::UnexpectedEof,
                                "content stream closed before done frame",
                            );
                            match state.begin_reopen(*total_remaining, fallback) {
                                Ok(()) => continue,
                                Err(err) => {
                                    state.fail_stream(total_remaining);
                                    return Poll::Ready(Err(err));
                                }
                            }
                        }
                        if state.activity.is_parked() {
                            state.phase = KeepOpenPhase::Finished;
                            state.activity.stop_and_release();
                            return Poll::Ready(Ok(()));
                        }
                        state.fail_stream(total_remaining);
                        return Poll::Ready(Err(io::Error::new(
                            io::ErrorKind::UnexpectedEof,
                            "content stream closed before done frame",
                        )));
                    }
                    LinePoll::Oversized => {
                        state.fail_stream(total_remaining);
                        return Poll::Ready(Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "remote sent extra bytes past announced window length",
                        )));
                    }
                    LinePoll::LineReady => {
                        state.progress_deadline = None;
                        if !is_done_frame(line) {
                            state.fail_stream(total_remaining);
                            return Poll::Ready(Err(io::Error::new(
                                io::ErrorKind::InvalidData,
                                "remote sent an invalid done frame",
                            )));
                        }
                        if *total_remaining == 0 {
                            if state.activity.is_parked() {
                                state.phase = KeepOpenPhase::Finished;
                                state.activity.stop_and_release();
                                return Poll::Ready(Ok(()));
                            }
                            // Drop stdin cleanly so the remote `legion stream` flushes its
                            // coalesced audit entry and exits with EOF.
                            state.clear_writer();
                            state.phase = KeepOpenPhase::ClosingEof;
                            continue;
                        }
                        if state.in_flight.is_none() {
                            if state.activity.is_parked() || !state.has_writer() {
                                let fallback = io::Error::new(
                                    io::ErrorKind::UnexpectedEof,
                                    "content stream channel closed before next window",
                                );
                                match state.begin_reopen(*total_remaining, fallback) {
                                    Ok(()) => continue,
                                    Err(err) => {
                                        state.fail_stream(total_remaining);
                                        return Poll::Ready(Err(err));
                                    }
                                }
                            }
                            state.enqueue_next_window(Duration::ZERO);
                            if let Err(err) = state.drive_pending_write(cx) {
                                if is_reopenable_io_error(&err) {
                                    match state.begin_reopen(*total_remaining, err) {
                                        Ok(()) => continue,
                                        Err(e) => {
                                            state.fail_stream(total_remaining);
                                            return Poll::Ready(Err(e));
                                        }
                                    }
                                }
                                state.fail_stream(total_remaining);
                                return Poll::Ready(Err(err));
                            }
                        }
                        let Some(expected) = state.in_flight.take() else {
                            state.fail_stream(total_remaining);
                            return Poll::Ready(Err(io::Error::new(
                                io::ErrorKind::UnexpectedEof,
                                "content stream ended before delivering all bytes",
                            )));
                        };
                        state.busy_count = 0;
                        state.phase = KeepOpenPhase::NextReady {
                            expected,
                            line: Vec::with_capacity(128),
                        };
                        continue;
                    }
                }
            }
            KeepOpenPhase::NextReady { expected, line } => {
                let expected = *expected;
                enum ReadyPoll {
                    Missing,
                    Pending,
                    Err(io::Error),
                    Eof,
                    LineReady,
                    Oversized,
                    More,
                }
                let polled = {
                    let mut live = state.live_channel.lock().unwrap_or_else(|p| p.into_inner());
                    live.data_window_remaining = 0;
                    match live.reader.as_mut() {
                        None => ReadyPoll::Missing,
                        Some(reader) => match Pin::new(&mut *reader).poll_fill_buf(cx) {
                            Poll::Pending => ReadyPoll::Pending,
                            Poll::Ready(Err(err)) => ReadyPoll::Err(err),
                            Poll::Ready(Ok([])) => ReadyPoll::Eof,
                            Poll::Ready(Ok(avail)) => {
                                if let Some(pos) = avail.iter().position(|&b| b == b'\n') {
                                    let take = pos + 1;
                                    if line.len() + take > MAX_REPLY_LINE {
                                        ReadyPoll::Oversized
                                    } else {
                                        line.extend_from_slice(&avail[..take]);
                                        Pin::new(&mut *reader).consume(take);
                                        ReadyPoll::LineReady
                                    }
                                } else {
                                    let n = avail.len();
                                    if line.len() + n > MAX_REPLY_LINE {
                                        ReadyPoll::Oversized
                                    } else {
                                        line.extend_from_slice(avail);
                                        Pin::new(&mut *reader).consume(n);
                                        ReadyPoll::More
                                    }
                                }
                            }
                        },
                    }
                };
                match polled {
                    ReadyPoll::Missing => continue,
                    ReadyPoll::More => continue,
                    ReadyPoll::Pending => {
                        if let Err(err) = poll_progress_deadline(
                            &mut state.progress_deadline,
                            state.options.progress_timeout,
                            &state.machine,
                            cx,
                        ) {
                            state.fail_stream(total_remaining);
                            return Poll::Ready(Err(err));
                        }
                        state.activity.mark_waiting(cx.waker());
                        return Poll::Pending;
                    }
                    ReadyPoll::Err(err) => {
                        if is_reopenable_io_error(&err) && *total_remaining > 0 {
                            match state.begin_reopen(*total_remaining, err) {
                                Ok(()) => continue,
                                Err(e) => {
                                    state.fail_stream(total_remaining);
                                    return Poll::Ready(Err(e));
                                }
                            }
                        }
                        state.fail_stream(total_remaining);
                        return Poll::Ready(Err(err));
                    }
                    ReadyPoll::Eof => {
                        let fallback = io::Error::new(
                            io::ErrorKind::UnexpectedEof,
                            "content stream closed before answering next window",
                        );
                        match state.begin_reopen(*total_remaining, fallback) {
                            Ok(()) => continue,
                            Err(err) => {
                                state.fail_stream(total_remaining);
                                return Poll::Ready(Err(err));
                            }
                        }
                    }
                    ReadyPoll::Oversized => {
                        state.fail_stream(total_remaining);
                        return Poll::Ready(Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "remote sent an oversized stream reply line",
                        )));
                    }
                    ReadyPoll::LineReady => {
                        let Ok(Value::Object(fields)) = serde_json::from_slice::<Value>(line)
                        else {
                            state.fail_stream(total_remaining);
                            return Poll::Ready(Err(io::Error::new(
                                io::ErrorKind::InvalidData,
                                "remote answered stream get with non-JSON output",
                            )));
                        };
                        if fields.len() == 1 && fields.get("busy") == Some(&Value::Bool(true)) {
                            state.busy_count += 1;
                            if state.busy_count > state.options.max_busy_lines {
                                let machine = state.machine.clone();
                                state.fail_stream(total_remaining);
                                return Poll::Ready(Err(io::Error::other(format!(
                                    "{machine} sent too many consecutive busy responses while streaming"
                                ))));
                            }
                            line.clear();
                            continue;
                        }
                        state.busy_count = 0;
                        state.progress_deadline = None;
                        if let Some(error_val) = fields.get("error")
                            && let Ok(body) = serde_json::from_value::<ErrorBody>(error_val.clone())
                        {
                            state.fail_stream(total_remaining);
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
                            state.fail_stream(total_remaining);
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
                                state.fail_stream(total_remaining);
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
                            state.fail_stream(total_remaining);
                            return Poll::Ready(Err(io::Error::new(
                                io::ErrorKind::InvalidData,
                                "file changed on remote node while streaming",
                            )));
                        }
                        state
                            .live_channel
                            .lock()
                            .unwrap_or_else(|p| p.into_inner())
                            .data_window_remaining = validated.length;
                        state.phase = KeepOpenPhase::Data {
                            remaining: validated.length,
                            started: Instant::now(),
                        };
                        continue;
                    }
                }
            }
            KeepOpenPhase::ClosingEof => {
                enum ClosePoll {
                    Missing,
                    Pending,
                    Err(io::Error),
                    Extra,
                    Done,
                }
                let polled = {
                    let mut live = state.live_channel.lock().unwrap_or_else(|p| p.into_inner());
                    live.data_window_remaining = 0;
                    match live.reader.as_mut() {
                        None => ClosePoll::Missing,
                        Some(reader) => match Pin::new(&mut *reader).poll_fill_buf(cx) {
                            Poll::Pending => ClosePoll::Pending,
                            Poll::Ready(Err(err)) => ClosePoll::Err(err),
                            Poll::Ready(Ok(extra)) if !extra.is_empty() => ClosePoll::Extra,
                            Poll::Ready(Ok(_)) => ClosePoll::Done,
                        },
                    }
                };
                match polled {
                    ClosePoll::Missing => {
                        state.progress_deadline = None;
                        state.phase = KeepOpenPhase::Finished;
                        state.activity.stop_and_release();
                        return Poll::Ready(Ok(()));
                    }
                    ClosePoll::Pending => {
                        if let Err(err) = poll_progress_deadline(
                            &mut state.progress_deadline,
                            state.options.progress_timeout,
                            &state.machine,
                            cx,
                        ) {
                            state.fail_stream(total_remaining);
                            return Poll::Ready(Err(err));
                        }
                        state.activity.mark_waiting(cx.waker());
                        return Poll::Pending;
                    }
                    ClosePoll::Err(err) => {
                        state.fail_stream(total_remaining);
                        return Poll::Ready(Err(err));
                    }
                    ClosePoll::Extra => {
                        state.fail_stream(total_remaining);
                        return Poll::Ready(Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "remote sent more bytes than announced",
                        )));
                    }
                    ClosePoll::Done => {
                        state.progress_deadline = None;
                        state.phase = KeepOpenPhase::Finished;
                        state.activity.stop_and_release();
                        return Poll::Ready(Ok(()));
                    }
                }
            }
        }
    }
}

fn poll_per_chunk(
    state: &mut PerChunkState,
    total_remaining: &mut u64,
    cancel: &CancellationToken,
    cx: &mut Context<'_>,
    buf: &mut ReadBuf<'_>,
) -> Poll<io::Result<()>> {
    let active_slot = &mut state.active;
    let next_chunk = &mut state.next_chunk;
    let continuation = &mut state.continuation;
    let machine = &state.machine;
    let progress_timeout = state.progress_timeout;
    let progress_deadline = &mut state.progress_deadline;
    loop {
        if let Some(active) = active_slot.as_mut() {
            if active.chunk_remaining > 0 {
                match Pin::new(&mut active.reader).poll_fill_buf(cx) {
                    Poll::Pending => {
                        if let Err(err) =
                            poll_progress_deadline(progress_deadline, progress_timeout, machine, cx)
                        {
                            *total_remaining = 0;
                            *active_slot = None;
                            return Poll::Ready(Err(err));
                        }
                        return Poll::Pending;
                    }
                    Poll::Ready(Err(err)) => {
                        *progress_deadline = None;
                        *total_remaining = 0;
                        *active_slot = None;
                        return Poll::Ready(Err(err));
                    }
                    Poll::Ready(Ok([])) => {
                        *progress_deadline = None;
                        let missing = *total_remaining;
                        *total_remaining = 0;
                        *active_slot = None;
                        return Poll::Ready(Err(io::Error::new(
                            io::ErrorKind::UnexpectedEof,
                            format!("content stream ended with {missing} bytes remaining"),
                        )));
                    }
                    Poll::Ready(Ok(avail)) => {
                        *progress_deadline = None;
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
                    Poll::Pending => {
                        if let Err(err) =
                            poll_progress_deadline(progress_deadline, progress_timeout, machine, cx)
                        {
                            *total_remaining = 0;
                            *active_slot = None;
                            return Poll::Ready(Err(err));
                        }
                        return Poll::Pending;
                    }
                    Poll::Ready(Err(err)) => {
                        *progress_deadline = None;
                        *total_remaining = 0;
                        *active_slot = None;
                        return Poll::Ready(Err(err));
                    }
                    Poll::Ready(Ok(extra)) if !extra.is_empty() => {
                        *progress_deadline = None;
                        active.chunk_cancel.cancel();
                        *total_remaining = 0;
                        *active_slot = None;
                        return Poll::Ready(Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "remote sent more bytes than announced",
                        )));
                    }
                    Poll::Ready(Ok(_)) => {
                        *progress_deadline = None;
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
pub(crate) async fn handshake_get_with_options(
    stream: RawStream,
    opener: Option<Arc<dyn StreamOpener>>,
    machine: &str,
    path: &str,
    offset: u64,
    length: Option<u64>,
    as_principal: Option<&str>,
    timeout: Duration,
    cancel: CancellationToken,
    options: ContentStreamOptions,
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
        options.max_busy_lines,
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
        let live_channel = Arc::new(Mutex::new(LiveChannel::new(
            opened.reader,
            opened.writer,
            validated.length,
        )));
        let activity = ParkActivity::start(
            opened.park_cancel,
            opened.guard,
            Arc::clone(&live_channel),
            options.park_pool.clone(),
            options.idle_park_timeout,
        );
        StreamMode::KeepOpen(Box::new(KeepOpenState {
            live_channel,
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
            opener,
            timeout,
            options,
            machine: machine.to_owned(),
            path: path.to_owned(),
            as_principal: as_principal.map(str::to_owned),
            expected_size: validated.size,
            expected_mtime: validated.mtime,
            expected_file_id: validated.file_id,
            stream_cancel: stream_cancel.clone(),
            channel_cancel: opened.channel_cancel,
            activity,
            consecutive_reopens: 0,
            progress_deadline: None,
            busy_count: 0,
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
            machine: machine.to_owned(),
            progress_timeout: options.progress_timeout,
            progress_deadline: None,
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
    })
}

#[allow(clippy::too_many_arguments)]
async fn reopen_keep_open_channel(
    opener: Arc<dyn StreamOpener>,
    machine: String,
    path: String,
    cursor_offset: u64,
    length: u64,
    as_principal: Option<String>,
    timeout: Duration,
    max_busy_lines: usize,
    expected_size: u64,
    expected_mtime: ContentTime,
    expected_file_id: Option<u64>,
    cancel: CancellationToken,
) -> Result<ReopenedKeepOpen, io::Error> {
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
                let kind = match body.code {
                    ErrorCode::Denied => io::ErrorKind::PermissionDenied,
                    ErrorCode::NotFound => io::ErrorKind::NotFound,
                    ErrorCode::Timeout => io::ErrorKind::TimedOut,
                    ErrorCode::Cancelled => io::ErrorKind::Interrupted,
                    _ => io::ErrorKind::Other,
                };
                return Err(io::Error::new(kind, body.message));
            }
        }
    };

    let (opened, validated) = open_one_chunk(
        raw,
        &machine,
        &path,
        cursor_offset,
        Some(length),
        as_principal.as_deref(),
        true,
        timeout,
        max_busy_lines,
        cancel,
    )
    .await
    .map_err(|err| {
        let kind = match err.code {
            ErrorCode::Denied => io::ErrorKind::PermissionDenied,
            ErrorCode::NotFound => io::ErrorKind::NotFound,
            ErrorCode::Timeout => io::ErrorKind::TimedOut,
            ErrorCode::Cancelled => io::ErrorKind::Interrupted,
            _ => io::ErrorKind::Other,
        };
        io::Error::new(kind, err.message)
    })?;

    if validated.size != expected_size
        || validated.mtime != expected_mtime
        || validated.file_id != expected_file_id
    {
        opened.channel_cancel.cancel();
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "file changed on remote node while streaming",
        ));
    }

    Ok(ReopenedKeepOpen {
        opened,
        validated,
        cursor_offset,
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
        DEFAULT_MAX_BUSY_LINES,
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
    max_busy_lines: usize,
    cancel: CancellationToken,
) -> Result<(OpenedChannel, ValidatedGet), ErrorBody> {
    let RawStream {
        reader,
        mut writer,
        stderr,
        cancel: chunk_cancel,
        park: park_cancel,
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
        read_get_ready(&mut buf_reader, machine, &stderr, max_busy_lines).await
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
            park_cancel,
            guard,
        },
        validated,
    ))
}

async fn read_get_ready<R: AsyncRead + Unpin>(
    reader: &mut BufReader<R>,
    machine: &str,
    stderr: &Option<Arc<StderrTail>>,
    max_busy_lines: usize,
) -> Result<GetReadyReply, ErrorBody> {
    let mut greeted = false;
    let mut noise_lines = 0usize;
    let mut noise_bytes = 0usize;
    let mut busy_lines = 0usize;
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
                busy_lines += 1;
                if busy_lines > max_busy_lines {
                    return Err(ErrorBody::new(
                        ErrorCode::Busy,
                        format!(
                            "{machine} sent too many consecutive busy responses while opening content stream."
                        ),
                    )
                    .with_hint("Retry once the remote node is less busy.")
                    .with_machine(machine));
                }
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
                    park: None,
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
                        park: None,
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
                        park: None,
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

    #[tokio::test]
    async fn keep_open_parks_idle_channel_frees_slot_reopens_and_detects_change() {
        struct ParkSlotGuard(Arc<AtomicUsize>);
        impl Drop for ParkSlotGuard {
            fn drop(&mut self) {
                self.0.fetch_sub(1, Ordering::SeqCst);
            }
        }

        struct ParkableOpener {
            payload: Arc<Vec<u8>>,
            mtime_secs: Arc<std::sync::atomic::AtomicI64>,
            active_slots: Arc<AtomicUsize>,
            opened: Arc<AtomicUsize>,
        }

        impl StreamOpener for ParkableOpener {
            fn window_policy(&self) -> Option<WindowPolicy> {
                Some(WindowPolicy {
                    initial_bytes: 16 * 1024,
                    max_bytes: 32 * 1024,
                })
            }

            fn open_stream(
                &self,
                _cancel: CancellationToken,
            ) -> Pin<Box<dyn Future<Output = Result<RawStream, StreamOpenError>> + Send + '_>>
            {
                let payload = Arc::clone(&self.payload);
                let mtime_secs = Arc::clone(&self.mtime_secs);
                let active_slots = Arc::clone(&self.active_slots);
                let opened = Arc::clone(&self.opened);
                Box::pin(async move {
                    opened.fetch_add(1, Ordering::SeqCst);
                    active_slots.fetch_add(1, Ordering::SeqCst);
                    let guard = Box::new(ParkSlotGuard(Arc::clone(&active_slots)));

                    let (client_in, mut server_out) = tokio::io::duplex(128 * 1024);
                    let (server_in, client_out) = tokio::io::duplex(16 * 1024);
                    let channel_cancel = CancellationToken::new();
                    let park_cancel = CancellationToken::new();
                    let rx_cancel = channel_cancel.clone();
                    let rx_park = park_cancel.clone();

                    tokio::spawn(async move {
                        let _ = server_out.write_all(b"{\"protocol\":1}\n").await;
                        let mut reader = BufReader::new(server_in);
                        let mut line = String::new();
                        loop {
                            line.clear();
                            let n = tokio::select! {
                                biased;
                                () = rx_cancel.cancelled() => break,
                                () = rx_park.cancelled() => break,
                                res = reader.read_line(&mut line) => res.unwrap_or(0),
                            };
                            if n == 0 {
                                break;
                            }
                            let req: Value = serde_json::from_str(&line).unwrap();
                            let offset = req["offset"].as_u64().unwrap() as usize;
                            let len = req["length"].as_u64().unwrap() as usize;
                            let actual = len.min(payload.len() - offset);
                            let ready = json!({
                                "size": payload.len(),
                                "offset": offset,
                                "length": actual,
                                "mtime": { "secs": mtime_secs.load(Ordering::SeqCst), "nanos": 0u32 },
                                "file_id": 77u64,
                                "keep_open": true,
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
                        cancel: channel_cancel,
                        park: Some(park_cancel),
                        guard: Some(guard),
                    })
                })
            }
        }

        let payload: Vec<u8> = (0..96 * 1024usize).map(|i| (i % 239) as u8).collect();
        let mtime_secs = Arc::new(std::sync::atomic::AtomicI64::new(1_700_000_123));
        let active_slots = Arc::new(AtomicUsize::new(0));
        let opened = Arc::new(AtomicUsize::new(0));
        let opener: Arc<dyn StreamOpener> = Arc::new(ParkableOpener {
            payload: Arc::new(payload.clone()),
            mtime_secs: Arc::clone(&mtime_secs),
            active_slots: Arc::clone(&active_slots),
            opened: Arc::clone(&opened),
        });

        let opts = ContentStreamOptions {
            idle_park_timeout: Duration::from_millis(60),
            progress_timeout: Duration::from_millis(120),
            max_busy_lines: 4,
            ..ContentStreamOptions::default()
        };
        let raw = opener.open_stream(CancellationToken::new()).await.unwrap();
        let mut stream = handshake_get_with_options(
            raw,
            Some(Arc::clone(&opener)),
            "tower",
            "/movie.webm",
            0,
            None,
            None,
            Duration::from_secs(5),
            CancellationToken::new(),
            opts.clone(),
        )
        .await
        .unwrap();
        assert_eq!(active_slots.load(Ordering::SeqCst), 1);

        // Read 4 KiB, then pause for 200ms (> idle_park_timeout 60ms AND > progress_timeout 120ms):
        // the channel must park, free its slot, keep buffered bytes, and NEVER fire progress timeout.
        let mut head = [0u8; 4096];
        stream.read_exact(&mut head).await.unwrap();
        assert_eq!(&head[..], &payload[..4096]);
        tokio::time::sleep(Duration::from_millis(220)).await;
        assert_eq!(
            active_slots.load(Ordering::SeqCst),
            0,
            "idle parked stream must release its channel slot"
        );

        // Resume: buffered bytes drain first, then the stream transparently reopens at exact offset.
        let mut rest = Vec::new();
        stream.read_to_end(&mut rest).await.unwrap();
        let mut full = head.to_vec();
        full.extend_from_slice(&rest);
        assert_eq!(full, payload);
        assert!(
            opened.load(Ordering::SeqCst) >= 2,
            "expected transparent reopen after parking"
        );
        assert_eq!(active_slots.load(Ordering::SeqCst), 0);

        // Now verify file-changed detection on reopen: open, park, mutate mtime, resume -> error.
        let raw2 = opener.open_stream(CancellationToken::new()).await.unwrap();
        let mut stream2 = handshake_get_with_options(
            raw2,
            Some(Arc::clone(&opener)),
            "tower",
            "/movie.webm",
            0,
            None,
            None,
            Duration::from_secs(5),
            CancellationToken::new(),
            opts,
        )
        .await
        .unwrap();
        tokio::time::sleep(Duration::from_millis(160)).await;
        assert_eq!(active_slots.load(Ordering::SeqCst), 0);
        mtime_secs.store(1_700_000_999, Ordering::SeqCst);

        let mut sink = Vec::new();
        let err = stream2.read_to_end(&mut sink).await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData, "{err}");
        assert!(err.to_string().contains("file changed"), "{err}");
    }

    #[tokio::test]
    async fn keep_open_reopens_transparently_after_remote_idle_close() {
        struct OneWindowThenCloseOpener {
            payload: Arc<Vec<u8>>,
            opened: Arc<AtomicUsize>,
        }

        impl StreamOpener for OneWindowThenCloseOpener {
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
                let opened = Arc::clone(&self.opened);
                Box::pin(async move {
                    opened.fetch_add(1, Ordering::SeqCst);
                    let (client_in, mut server_out) = tokio::io::duplex(64 * 1024);
                    let (server_in, client_out) = tokio::io::duplex(16 * 1024);
                    tokio::spawn(async move {
                        let _ = server_out.write_all(b"{\"protocol\":1}\n").await;
                        let mut reader = BufReader::new(server_in);
                        let mut line = String::new();
                        if reader.read_line(&mut line).await.unwrap_or(0) == 0 {
                            return;
                        }
                        let req: Value = serde_json::from_str(&line).unwrap();
                        let offset = req["offset"].as_u64().unwrap() as usize;
                        let len = req["length"].as_u64().unwrap() as usize;
                        let actual = len.min(payload.len() - offset);
                        let ready = json!({
                            "size": payload.len(),
                            "offset": offset,
                            "length": actual,
                            "mtime": { "secs": 1_700_000_123i64, "nanos": 0u32 },
                            "file_id": 88u64,
                            "keep_open": true,
                        });
                        let _ = server_out.write_all(format!("{ready}\n").as_bytes()).await;
                        let _ = server_out
                            .write_all(&payload[offset..offset + actual])
                            .await;
                        let _ = server_out.write_all(b"{\"done\":true}\n").await;
                        // Simulate remote idle-timeout exit right after one window.
                        let _ = server_out.shutdown().await;
                    });
                    Ok(RawStream {
                        reader: Box::new(client_in),
                        writer: Box::new(client_out),
                        stderr: None,
                        cancel: CancellationToken::new(),
                        park: None,
                        guard: None,
                    })
                })
            }
        }

        let payload: Vec<u8> = (0..48 * 1024usize).map(|i| (i % 197) as u8).collect();
        let opened = Arc::new(AtomicUsize::new(0));
        let opener: Arc<dyn StreamOpener> = Arc::new(OneWindowThenCloseOpener {
            payload: Arc::new(payload.clone()),
            opened: Arc::clone(&opened),
        });
        let raw = opener.open_stream(CancellationToken::new()).await.unwrap();
        let mut stream = handshake_get_with_options(
            raw,
            Some(opener),
            "tower",
            "/clip.webm",
            0,
            None,
            None,
            Duration::from_secs(5),
            CancellationToken::new(),
            ContentStreamOptions {
                idle_park_timeout: Duration::from_secs(60),
                progress_timeout: Duration::from_secs(5),
                max_busy_lines: 4,
                ..ContentStreamOptions::default()
            },
        )
        .await
        .unwrap();

        let mut out = Vec::new();
        stream.read_to_end(&mut out).await.unwrap();
        assert_eq!(out, payload);
        assert_eq!(opened.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn progress_timeout_and_busy_line_cap_stop_misbehaving_remote() {
        let opts = ContentStreamOptions {
            idle_park_timeout: Duration::from_secs(60),
            progress_timeout: Duration::from_millis(100),
            max_busy_lines: 3,
            ..ContentStreamOptions::default()
        };

        // 1. Mid-window stall triggers progress timeout.
        {
            let (client_in, mut server_out) = tokio::io::duplex(32 * 1024);
            let (server_in, client_out) = tokio::io::duplex(8 * 1024);
            tokio::spawn(async move {
                let mut r = BufReader::new(server_in);
                let mut line = String::new();
                let _ = r.read_line(&mut line).await;
                let _ = server_out.write_all(b"{\"protocol\":1}\n").await;
                let _ = server_out
                    .write_all(b"{\"size\":1024,\"offset\":0,\"length\":1024,\"mtime\":{\"secs\":10,\"nanos\":0},\"keep_open\":true}\n")
                    .await;
                let _ = server_out.write_all(&[0xAA; 128]).await;
                let _ = server_out.flush().await;
                // Stall forever without sending remaining bytes.
                tokio::time::sleep(Duration::from_secs(30)).await;
            });
            let raw = RawStream {
                reader: Box::new(client_in),
                writer: Box::new(client_out),
                stderr: None,
                cancel: CancellationToken::new(),
                park: None,
                guard: None,
            };
            let mut stream = handshake_get_with_options(
                raw,
                None,
                "tower",
                "/stall.bin",
                0,
                None,
                None,
                Duration::from_secs(5),
                CancellationToken::new(),
                opts.clone(),
            )
            .await
            .unwrap();
            let mut out = Vec::new();
            let err = stream.read_to_end(&mut out).await.unwrap_err();
            assert_eq!(err.kind(), io::ErrorKind::TimedOut, "{err}");
            assert_eq!(out.len(), 128);
        }

        // 2. Infinite {"busy": true} on initial handshake fails with ErrorCode::Busy.
        {
            let (client_in, mut server_out) = tokio::io::duplex(32 * 1024);
            let (server_in, client_out) = tokio::io::duplex(8 * 1024);
            tokio::spawn(async move {
                let mut r = BufReader::new(server_in);
                let mut line = String::new();
                let _ = r.read_line(&mut line).await;
                let _ = server_out.write_all(b"{\"protocol\":1}\n").await;
                for _ in 0..10 {
                    if server_out.write_all(b"{\"busy\":true}\n").await.is_err() {
                        break;
                    }
                }
            });
            let raw = RawStream {
                reader: Box::new(client_in),
                writer: Box::new(client_out),
                stderr: None,
                cancel: CancellationToken::new(),
                park: None,
                guard: None,
            };
            let err = handshake_get_with_options(
                raw,
                None,
                "tower",
                "/busy.bin",
                0,
                None,
                None,
                Duration::from_secs(5),
                CancellationToken::new(),
                opts.clone(),
            )
            .await
            .err()
            .expect("expected busy cap error");
            assert_eq!(err.code, ErrorCode::Busy, "{err}");
        }

        // 3. Infinite {"busy": true} on a subsequent keep_open window fails cleanly.
        {
            struct StaticPolicyOpener;
            impl StreamOpener for StaticPolicyOpener {
                fn window_policy(&self) -> Option<WindowPolicy> {
                    Some(WindowPolicy {
                        initial_bytes: 128,
                        max_bytes: 128,
                    })
                }
                fn open_stream(
                    &self,
                    _cancel: CancellationToken,
                ) -> Pin<Box<dyn Future<Output = Result<RawStream, StreamOpenError>> + Send + '_>>
                {
                    Box::pin(async {
                        Err(StreamOpenError::Failed(ErrorBody::new(
                            ErrorCode::Failed,
                            "unused",
                        )))
                    })
                }
            }

            let (client_in, mut server_out) = tokio::io::duplex(32 * 1024);
            let (server_in, client_out) = tokio::io::duplex(8 * 1024);
            tokio::spawn(async move {
                let mut r = BufReader::new(server_in);
                let mut line = String::new();
                let _ = r.read_line(&mut line).await;
                let _ = server_out.write_all(b"{\"protocol\":1}\n").await;
                let _ = server_out
                    .write_all(b"{\"size\":256,\"offset\":0,\"length\":128,\"mtime\":{\"secs\":10,\"nanos\":0},\"keep_open\":true}\n")
                    .await;
                let _ = server_out.write_all(&[0xBB; 128]).await;
                let _ = server_out.write_all(b"{\"done\":true}\n").await;
                line.clear();
                let _ = r.read_line(&mut line).await;
                for _ in 0..10 {
                    if server_out.write_all(b"{\"busy\":true}\n").await.is_err() {
                        break;
                    }
                }
            });
            let raw = RawStream {
                reader: Box::new(client_in),
                writer: Box::new(client_out),
                stderr: None,
                cancel: CancellationToken::new(),
                park: None,
                guard: None,
            };
            let mut stream = handshake_get_with_options(
                raw,
                Some(Arc::new(StaticPolicyOpener)),
                "tower",
                "/busy2.bin",
                0,
                None,
                None,
                Duration::from_secs(5),
                CancellationToken::new(),
                opts,
            )
            .await
            .unwrap();
            let mut out = Vec::new();
            let err = stream.read_to_end(&mut out).await.unwrap_err();
            assert!(
                err.to_string().contains("too many consecutive busy"),
                "{err}"
            );
        }
    }

    #[tokio::test]
    async fn abandoned_pending_read_does_not_block_parking() {
        struct SlotGuard(Arc<AtomicUsize>);
        impl Drop for SlotGuard {
            fn drop(&mut self) {
                self.0.fetch_sub(1, Ordering::SeqCst);
            }
        }

        struct DelayedBodyOpener {
            payload: Arc<Vec<u8>>,
            first_body_delay: Duration,
            active_slots: Arc<AtomicUsize>,
            opened: Arc<AtomicUsize>,
        }

        impl StreamOpener for DelayedBodyOpener {
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
                let delay = self.first_body_delay;
                let active_slots = Arc::clone(&self.active_slots);
                let opened = Arc::clone(&self.opened);
                Box::pin(async move {
                    let seq = opened.fetch_add(1, Ordering::SeqCst);
                    active_slots.fetch_add(1, Ordering::SeqCst);
                    let guard = Box::new(SlotGuard(Arc::clone(&active_slots)));

                    let (client_in, mut server_out) = tokio::io::duplex(64 * 1024);
                    let (server_in, client_out) = tokio::io::duplex(16 * 1024);
                    let channel_cancel = CancellationToken::new();
                    let park_cancel = CancellationToken::new();
                    let rx_cancel = channel_cancel.clone();
                    let rx_park = park_cancel.clone();

                    tokio::spawn(async move {
                        let _ = server_out.write_all(b"{\"protocol\":1}\n").await;
                        let mut reader = BufReader::new(server_in);
                        let mut line = String::new();
                        loop {
                            line.clear();
                            let n = tokio::select! {
                                biased;
                                () = rx_cancel.cancelled() => break,
                                () = rx_park.cancelled() => break,
                                res = reader.read_line(&mut line) => res.unwrap_or(0),
                            };
                            if n == 0 {
                                break;
                            }
                            let req: Value = serde_json::from_str(&line).unwrap();
                            let offset = req["offset"].as_u64().unwrap() as usize;
                            let len = req["length"].as_u64().unwrap() as usize;
                            let actual = len.min(payload.len() - offset);
                            let ready = json!({
                                "size": payload.len(),
                                "offset": offset,
                                "length": actual,
                                "mtime": { "secs": 1_700_000_555i64, "nanos": 0u32 },
                                "file_id": 88u64,
                                "keep_open": true,
                            });
                            if server_out
                                .write_all(format!("{ready}\n").as_bytes())
                                .await
                                .is_err()
                            {
                                break;
                            }
                            let _ = server_out.flush().await;
                            // Delay body bytes on the first channel so the caller's read returns
                            // Poll::Pending and sets `waiting_for_remote`.
                            if seq == 0 {
                                tokio::select! {
                                    biased;
                                    () = rx_cancel.cancelled() => break,
                                    () = rx_park.cancelled() => break,
                                    () = tokio::time::sleep(delay) => {}
                                }
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
                        cancel: channel_cancel,
                        park: Some(park_cancel),
                        guard: Some(guard),
                    })
                })
            }
        }

        let payload: Vec<u8> = (0..32 * 1024usize).map(|i| (i % 241) as u8).collect();
        let idle_park = Duration::from_millis(80);
        let opts = ContentStreamOptions {
            idle_park_timeout: idle_park,
            progress_timeout: Duration::from_secs(5),
            max_busy_lines: 4,
            ..ContentStreamOptions::default()
        };

        // 1. A read abandoned via `timeout(read)` while pending must NOT block parking.
        {
            let active_slots = Arc::new(AtomicUsize::new(0));
            let opened = Arc::new(AtomicUsize::new(0));
            let opener: Arc<dyn StreamOpener> = Arc::new(DelayedBodyOpener {
                payload: Arc::new(payload.clone()),
                first_body_delay: Duration::from_secs(30),
                active_slots: Arc::clone(&active_slots),
                opened: Arc::clone(&opened),
            });
            let raw = opener.open_stream(CancellationToken::new()).await.unwrap();
            let mut stream = handshake_get_with_options(
                raw,
                Some(Arc::clone(&opener)),
                "tower",
                "/slow.bin",
                0,
                None,
                None,
                Duration::from_secs(5),
                CancellationToken::new(),
                opts.clone(),
            )
            .await
            .unwrap();
            assert_eq!(active_slots.load(Ordering::SeqCst), 1);

            let mut buf = [0u8; 4096];
            let timed_out =
                tokio::time::timeout(Duration::from_millis(25), stream.read(&mut buf)).await;
            assert!(
                timed_out.is_err(),
                "first read should time out while pending"
            );

            // Stream must park within the normal idle park window (~80 ms + modest scheduler slack).
            let deadline = Instant::now() + Duration::from_millis(400);
            while active_slots.load(Ordering::SeqCst) != 0 && Instant::now() < deadline {
                tokio::time::sleep(Duration::from_millis(15)).await;
            }
            assert_eq!(
                active_slots.load(Ordering::SeqCst),
                0,
                "abandoned pending read must not block parking"
            );

            // Resuming after park reopens transparently and yields exact payload.
            let mut out = Vec::new();
            stream.read_to_end(&mut out).await.unwrap();
            assert_eq!(out, payload);
            assert!(opened.load(Ordering::SeqCst) >= 2);
        }

        // 2. An actively waiting reader (future NOT dropped) must NOT be prematurely parked while
        //    waiting for a slow remote (220 ms > idle_park_timeout 80 ms).
        {
            let active_slots = Arc::new(AtomicUsize::new(0));
            let opened = Arc::new(AtomicUsize::new(0));
            let opener: Arc<dyn StreamOpener> = Arc::new(DelayedBodyOpener {
                payload: Arc::new(payload.clone()),
                first_body_delay: Duration::from_millis(220),
                active_slots: Arc::clone(&active_slots),
                opened: Arc::clone(&opened),
            });
            let raw = opener.open_stream(CancellationToken::new()).await.unwrap();
            let mut stream = handshake_get_with_options(
                raw,
                Some(Arc::clone(&opener)),
                "tower",
                "/slow-active.bin",
                0,
                None,
                None,
                Duration::from_secs(5),
                CancellationToken::new(),
                opts,
            )
            .await
            .unwrap();

            let mut out = Vec::new();
            stream.read_to_end(&mut out).await.unwrap();
            assert_eq!(out, payload);
            assert_eq!(
                opened.load(Ordering::SeqCst),
                1,
                "active reader waiting on slow remote must not be prematurely parked"
            );
        }
    }

    #[tokio::test]
    async fn parked_streams_trim_buffers_cap_lru_and_stay_cheap_at_scale() {
        fn pattern_byte(offset: u64) -> u8 {
            ((offset.wrapping_mul(31).wrapping_add(17)) % 251) as u8
        }

        #[cfg(target_os = "linux")]
        fn rss_bytes() -> u64 {
            let Ok(statm) = std::fs::read_to_string("/proc/self/statm") else {
                return 0;
            };
            let pages: u64 = statm
                .split_whitespace()
                .nth(1)
                .and_then(|s| s.parse().ok())
                .unwrap_or(0);
            pages.saturating_mul(4096)
        }
        #[cfg(not(target_os = "linux"))]
        fn rss_bytes() -> u64 {
            0
        }

        struct SlotGuard(Arc<AtomicUsize>);
        impl Drop for SlotGuard {
            fn drop(&mut self) {
                self.0.fetch_sub(1, Ordering::SeqCst);
            }
        }

        struct LargeWindowOpener {
            file_size: u64,
            active_slots: Arc<AtomicUsize>,
        }

        impl StreamOpener for LargeWindowOpener {
            fn window_policy(&self) -> Option<WindowPolicy> {
                Some(WindowPolicy {
                    initial_bytes: 256 * 1024,
                    max_bytes: 4 * 1024 * 1024,
                })
            }

            fn open_stream(
                &self,
                _cancel: CancellationToken,
            ) -> Pin<Box<dyn Future<Output = Result<RawStream, StreamOpenError>> + Send + '_>>
            {
                let file_size = self.file_size;
                let active_slots = Arc::clone(&self.active_slots);
                Box::pin(async move {
                    active_slots.fetch_add(1, Ordering::SeqCst);
                    let guard = Box::new(SlotGuard(Arc::clone(&active_slots)));
                    // Large duplex buffer matching SSH's in-flight window queue.
                    let (client_in, mut server_out) = tokio::io::duplex(4 * 1024 * 1024);
                    let (server_in, client_out) = tokio::io::duplex(64 * 1024);
                    let channel_cancel = CancellationToken::new();
                    let park_cancel = CancellationToken::new();
                    let rx_cancel = channel_cancel.clone();
                    let rx_park = park_cancel.clone();

                    tokio::spawn(async move {
                        let _ = server_out.write_all(b"{\"protocol\":1}\n").await;
                        let mut reader = BufReader::new(server_in);
                        let mut line = String::new();
                        let mut chunk = vec![0u8; 64 * 1024];
                        loop {
                            line.clear();
                            let n = tokio::select! {
                                biased;
                                () = rx_cancel.cancelled() => break,
                                () = rx_park.cancelled() => break,
                                res = reader.read_line(&mut line) => res.unwrap_or(0),
                            };
                            if n == 0 {
                                break;
                            }
                            let req: Value = serde_json::from_str(&line).unwrap();
                            let offset = req["offset"].as_u64().unwrap();
                            let len = req["length"].as_u64().unwrap();
                            let actual = len.min(file_size.saturating_sub(offset));
                            let ready = json!({
                                "size": file_size,
                                "offset": offset,
                                "length": actual,
                                "mtime": { "secs": 1_700_000_777i64, "nanos": 0u32 },
                                "file_id": 99u64,
                                "keep_open": true,
                            });
                            if server_out
                                .write_all(format!("{ready}\n").as_bytes())
                                .await
                                .is_err()
                            {
                                break;
                            }
                            let mut pos = offset;
                            let end = offset + actual;
                            let mut broken = false;
                            while pos < end {
                                let step = usize::try_from(end - pos).unwrap().min(chunk.len());
                                for (i, b) in chunk[..step].iter_mut().enumerate() {
                                    *b = pattern_byte(pos + i as u64);
                                }
                                let write_ok = tokio::select! {
                                    biased;
                                    () = rx_cancel.cancelled() => false,
                                    () = rx_park.cancelled() => false,
                                    res = server_out.write_all(&chunk[..step]) => res.is_ok(),
                                };
                                if !write_ok {
                                    broken = true;
                                    break;
                                }
                                pos += step as u64;
                            }
                            if broken {
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
                        cancel: channel_cancel,
                        park: Some(park_cancel),
                        guard: Some(guard),
                    })
                })
            }
        }

        async fn read_exact_pattern(stream: &mut ContentStream, start: u64, len: usize) {
            let mut scratch = vec![0u8; 64 * 1024];
            let mut done = 0usize;
            while done < len {
                let want = (len - done).min(scratch.len());
                let n = stream.read(&mut scratch[..want]).await.unwrap();
                assert!(n > 0, "unexpected EOF at offset {}", start + done as u64);
                for (i, &b) in scratch[..n].iter().enumerate() {
                    let off = start + (done + i) as u64;
                    assert_eq!(b, pattern_byte(off), "byte mismatch at offset {off}");
                }
                done += n;
            }
        }

        // Scenario 1: 200 streams parked after reading 6 MiB each (grows window to 4 MiB).
        {
            let file_size = 8 * 1024 * 1024u64;
            let active_slots = Arc::new(AtomicUsize::new(0));
            let opener: Arc<dyn StreamOpener> = Arc::new(LargeWindowOpener {
                file_size,
                active_slots: Arc::clone(&active_slots),
            });
            let pool = Arc::new(ParkedStreamPool::new(DEFAULT_MAX_PARKED_STREAMS));
            let opts = ContentStreamOptions {
                idle_park_timeout: Duration::from_millis(60),
                progress_timeout: Duration::from_secs(5),
                max_busy_lines: 4,
                park_pool: Some(Arc::clone(&pool)),
            };
            let rss_before = rss_bytes();
            let mut streams = Vec::with_capacity(200);
            for batch in 0..25 {
                let mut futs = Vec::with_capacity(8);
                for j in 0..8 {
                    let opener = Arc::clone(&opener);
                    let opts = opts.clone();
                    let idx = batch * 8 + j;
                    futs.push(tokio::spawn(async move {
                        let raw = opener.open_stream(CancellationToken::new()).await.unwrap();
                        let mut s = handshake_get_with_options(
                            raw,
                            Some(opener),
                            "tower",
                            &format!("/video-{idx}.webm"),
                            0,
                            None,
                            None,
                            Duration::from_secs(5),
                            CancellationToken::new(),
                            opts,
                        )
                        .await
                        .unwrap();
                        read_exact_pattern(&mut s, 0, 6 * 1024 * 1024).await;
                        s
                    }));
                }
                for fut in futs {
                    streams.push(fut.await.unwrap());
                }
                // Wait for this batch of 8 to park before opening the next batch.
                let deadline = Instant::now() + Duration::from_secs(2);
                while active_slots.load(Ordering::SeqCst) != 0 && Instant::now() < deadline {
                    tokio::time::sleep(Duration::from_millis(15)).await;
                }
                assert_eq!(active_slots.load(Ordering::SeqCst), 0);
            }
            assert_eq!(pool.len(), 200);
            let total_retained: usize = streams
                .iter()
                .map(ContentStream::retained_buffer_bytes)
                .sum();
            let max_one = streams
                .iter()
                .map(ContentStream::retained_buffer_bytes)
                .max()
                .unwrap_or(0);
            let rss_after = rss_bytes();
            eprintln!(
                "200 parked streams (after 6 MiB each): total_retained={} bytes ({:.2} MiB) max_per_stream={} bytes rss_delta={:.2} MiB",
                total_retained,
                total_retained as f64 / (1024.0 * 1024.0),
                max_one,
                rss_after.saturating_sub(rss_before) as f64 / (1024.0 * 1024.0),
            );
            assert!(
                max_one <= PARKED_BUFFER_CAP,
                "each parked stream must retain <= 64 KiB, saw {max_one}"
            );
            assert!(
                total_retained <= 200 * PARKED_BUFFER_CAP,
                "200 parked streams retained too much buffer: {total_retained} bytes"
            );

            // Spot-check byte-exact resume across the parked streams.
            for idx in [0usize, 99, 199] {
                read_exact_pattern(&mut streams[idx], 6 * 1024 * 1024, 2 * 1024 * 1024).await;
                let mut eof = [0u8; 8];
                assert_eq!(streams[idx].read(&mut eof).await.unwrap(), 0);
            }
            drop(streams);
            assert_eq!(pool.len(), 0);
        }

        // Scenario 2: 300 streams parked after reading 300 KiB each with default 256 cap.
        {
            let file_size = 1024 * 1024u64;
            let active_slots = Arc::new(AtomicUsize::new(0));
            let opener: Arc<dyn StreamOpener> = Arc::new(LargeWindowOpener {
                file_size,
                active_slots: Arc::clone(&active_slots),
            });
            let pool = Arc::new(ParkedStreamPool::new(DEFAULT_MAX_PARKED_STREAMS));
            let opts = ContentStreamOptions {
                idle_park_timeout: Duration::from_millis(50),
                progress_timeout: Duration::from_secs(5),
                max_busy_lines: 4,
                park_pool: Some(Arc::clone(&pool)),
            };
            let rss_before = rss_bytes();
            let mut streams = Vec::with_capacity(300);
            for batch in 0..30 {
                let mut futs = Vec::with_capacity(10);
                for j in 0..10 {
                    let opener = Arc::clone(&opener);
                    let opts = opts.clone();
                    let idx = batch * 10 + j;
                    futs.push(tokio::spawn(async move {
                        let raw = opener.open_stream(CancellationToken::new()).await.unwrap();
                        let mut s = handshake_get_with_options(
                            raw,
                            Some(opener),
                            "tower",
                            &format!("/clip-{idx}.webm"),
                            0,
                            None,
                            None,
                            Duration::from_secs(5),
                            CancellationToken::new(),
                            opts,
                        )
                        .await
                        .unwrap();
                        read_exact_pattern(&mut s, 0, 300 * 1024).await;
                        s
                    }));
                }
                for fut in futs {
                    streams.push(fut.await.unwrap());
                }
                let deadline = Instant::now() + Duration::from_secs(2);
                while active_slots.load(Ordering::SeqCst) != 0 && Instant::now() < deadline {
                    tokio::time::sleep(Duration::from_millis(15)).await;
                }
                assert_eq!(active_slots.load(Ordering::SeqCst), 0);
            }
            assert_eq!(
                pool.len(),
                DEFAULT_MAX_PARKED_STREAMS,
                "parked pool must cap at 256 streams"
            );
            let total_retained: usize = streams
                .iter()
                .map(ContentStream::retained_buffer_bytes)
                .sum();
            let rss_after = rss_bytes();
            eprintln!(
                "300 parked streams (after 300 KiB each, cap 256): pool_len={} total_retained={} bytes ({:.2} MiB) rss_delta={:.2} MiB",
                pool.len(),
                total_retained,
                total_retained as f64 / (1024.0 * 1024.0),
                rss_after.saturating_sub(rss_before) as f64 / (1024.0 * 1024.0),
            );
            assert!(
                total_retained <= DEFAULT_MAX_PARKED_STREAMS * PARKED_BUFFER_CAP,
                "300 parked streams (cap 256) retained too much buffer: {total_retained} bytes"
            );

            // Exactly 44 oldest streams were evicted LRU: 0 bytes retained and clean
            // PermissionDenied error on next read. Streams in strictly earlier batches (0..40)
            // must all be evicted, and the remaining 4 evicted streams come from the boundary
            // batch (40..48) whose 8 watchdogs parked concurrently.
            let mut evicted_indices = Vec::new();
            for (idx, s) in streams.iter_mut().enumerate() {
                if s.retained_buffer_bytes() == 0 {
                    evicted_indices.push(idx);
                    let mut probe = [0u8; 64];
                    let err = s.read(&mut probe).await.unwrap_err();
                    assert_eq!(err.kind(), io::ErrorKind::PermissionDenied, "{err}");
                }
            }
            assert_eq!(evicted_indices.len(), 44);
            for idx in 0..40 {
                assert!(
                    evicted_indices.contains(&idx),
                    "stream {idx} from an earlier batch must have been evicted LRU"
                );
            }
            assert!(
                evicted_indices.iter().all(|&idx| idx < 48),
                "only streams from oldest batches (0..48) may be evicted LRU, got {evicted_indices:?}"
            );

            // Surviving streams resume byte-exact.
            for idx in [48usize, 170, 299] {
                read_exact_pattern(&mut streams[idx], 300 * 1024, (1024 - 300) * 1024).await;
                let mut eof = [0u8; 8];
                assert_eq!(streams[idx].read(&mut eof).await.unwrap(), 0);
            }
            drop(streams);
            assert_eq!(pool.len(), 0);
        }
    }

    #[tokio::test]
    async fn channel_refused_with_free_permits_reconnects_once_per_minute() {
        struct SessionScopedOpener {
            session_id: usize,
            refuse_sessions: Arc<Mutex<Vec<usize>>>,
            payload: Arc<Vec<u8>>,
        }

        impl StreamOpener for SessionScopedOpener {
            fn open_stream(
                &self,
                _cancel: CancellationToken,
            ) -> Pin<Box<dyn Future<Output = Result<RawStream, StreamOpenError>> + Send + '_>>
            {
                let sid = self.session_id;
                let refuse = self
                    .refuse_sessions
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .contains(&sid);
                let payload = Arc::clone(&self.payload);
                Box::pin(async move {
                    if refuse {
                        return Err(StreamOpenError::ChannelRefused(
                            ErrorBody::new(
                                ErrorCode::Busy,
                                "The SSH server refused to open another stream channel.",
                            )
                            .with_details(json!({
                                "reason": "channel_open_failure",
                                "permits_free": true,
                                "retryable": true,
                            })),
                        ));
                    }
                    let (client_in, mut server_out) = tokio::io::duplex(16 * 1024);
                    let (server_in, client_out) = tokio::io::duplex(8 * 1024);
                    tokio::spawn(async move {
                        let mut r = BufReader::new(server_in);
                        let mut line = String::new();
                        if r.read_line(&mut line).await.unwrap_or(0) == 0 {
                            return;
                        }
                        let _ = server_out.write_all(b"{\"protocol\":1}\n").await;
                        let ready = json!({
                            "size": payload.len(),
                            "offset": 0u64,
                            "length": payload.len(),
                            "mtime": { "secs": 1_700_000_000i64, "nanos": 0u32 },
                            "file_id": 42u64,
                        });
                        let _ = server_out.write_all(format!("{ready}\n").as_bytes()).await;
                        let _ = server_out.write_all(&payload).await;
                        let _ = server_out.shutdown().await;
                    });
                    Ok(RawStream {
                        reader: Box::new(client_in),
                        writer: Box::new(client_out),
                        stderr: None,
                        cancel: CancellationToken::new(),
                        park: None,
                        guard: None,
                    })
                })
            }
        }

        let session_opens = Arc::new(AtomicUsize::new(0));
        let refuse_sessions = Arc::new(Mutex::new(vec![1usize]));
        let payload = Arc::new(b"fresh-session-content".to_vec());

        let make_link = |cooldown: Duration| {
            let opens = Arc::clone(&session_opens);
            let refuses = Arc::clone(&refuse_sessions);
            let data = Arc::clone(&payload);
            let mut config = LinkConfig::default();
            config.upsert_node(NodeLink::local("tower", "/bin/true"));
            let opener: SessionOpener = Arc::new(move |entry, _key, cancel| {
                let sid = opens.fetch_add(1, Ordering::SeqCst) + 1;
                let stream_opener: Arc<dyn StreamOpener> = Arc::new(SessionScopedOpener {
                    session_id: sid,
                    refuse_sessions: Arc::clone(&refuses),
                    payload: Arc::clone(&data),
                });
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
                                    node: "tower".to_owned(),
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
                                    format!("{}\n", serde_json::to_string(&reply).unwrap())
                                        .as_bytes(),
                                )
                                .await;
                            line.clear();
                        }
                    });
                    Session::open_transport(
                        client_in,
                        client_out,
                        Box::new(FakeTransport {
                            opener: stream_opener,
                        }),
                        &entry.node,
                        None,
                        Duration::from_secs(5),
                        cancel,
                    )
                    .await
                    .map_err(|err| err.into_error())
                })
            });
            Link::with_opener(
                config,
                LinkOptions {
                    channel_refused_reconnect_cooldown: cooldown,
                    ..LinkOptions::default()
                },
                opener,
            )
        };

        // 1. Default 60 s cooldown: session 1 refuses with free permits -> reconnects ONCE to
        //    session 2 and succeeds; when session 2 also refuses within 60 s, returns Busy without
        //    opening session 3.
        let link = make_link(Duration::from_secs(60));
        let _ = link.hello("tower").await.unwrap();
        assert_eq!(session_opens.load(Ordering::SeqCst), 1);

        let mut s = link.open_content("tower", "/a.bin", 0, None).await.unwrap();
        let mut out = Vec::new();
        s.read_to_end(&mut out).await.unwrap();
        assert_eq!(out, *payload);
        assert_eq!(
            session_opens.load(Ordering::SeqCst),
            2,
            "expected one reconnect when sshd refused channels with free permits"
        );

        // Poison session 2 too: within the 60 s cooldown it must NOT reconnect again.
        refuse_sessions.lock().unwrap().push(2);
        let err = link
            .open_content("tower", "/b.bin", 0, None)
            .await
            .err()
            .unwrap();
        assert_eq!(err.code, ErrorCode::Busy, "{err}");
        assert_eq!(
            session_opens.load(Ordering::SeqCst),
            2,
            "reconnect must be rate-limited to once per cooldown window"
        );
        // Existing RPC session remains usable.
        let _ = link.hello("tower").await.unwrap();
        assert_eq!(session_opens.load(Ordering::SeqCst), 2);

        // 2. With a short cooldown, once the cooldown elapses a new refusal reconnects once more.
        session_opens.store(0, Ordering::SeqCst);
        *refuse_sessions.lock().unwrap() = vec![1, 2];
        let fast_link = make_link(Duration::from_millis(50));
        let _ = fast_link.hello("tower").await.unwrap();
        assert_eq!(session_opens.load(Ordering::SeqCst), 1);
        // Session 1 refuses -> reconnects to session 2, which also refuses -> Busy (session_opens=2).
        let err1 = fast_link
            .open_content("tower", "/c.bin", 0, None)
            .await
            .err()
            .unwrap();
        assert_eq!(err1.code, ErrorCode::Busy, "{err1}");
        assert_eq!(session_opens.load(Ordering::SeqCst), 2);
        // Wait past cooldown -> session 2 refuses -> reconnects to session 3 -> succeeds!
        tokio::time::sleep(Duration::from_millis(70)).await;
        let mut s3 = fast_link
            .open_content("tower", "/d.bin", 0, None)
            .await
            .unwrap();
        let mut out3 = Vec::new();
        s3.read_to_end(&mut out3).await.unwrap();
        assert_eq!(out3, *payload);
        assert_eq!(session_opens.load(Ordering::SeqCst), 3);
    }
}
