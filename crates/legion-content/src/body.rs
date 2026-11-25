//! Streaming HTTP body with bounded chunk buffering, periodic liveness checks, and backpressure.

use std::any::Any;
use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use bytes::Bytes;
use http_body::{Body, Frame, SizeHint};
use tokio::io::{AsyncBufRead, AsyncRead, BufReader};

/// Chunk size buffered per read so arbitrarily large files stream in constant memory without
/// zero-filling a fresh scratch buffer on every poll.
const CHUNK_SIZE: usize = 64 * 1024;

/// Default byte interval between in-flight revocation and TTL checks (2 MiB).
pub const LIVENESS_CHECK_BYTES: u64 = 2 * 1024 * 1024;

/// Default wall-time interval between in-flight revocation and TTL checks (2 seconds).
pub const LIVENESS_CHECK_INTERVAL: Duration = Duration::from_secs(2);

/// Type-erased async reader yielded by a [`ContentSource`](crate::ContentSource).
pub type BoxAsyncRead = Box<dyn AsyncRead + Send + Unpin>;

/// Periodic liveness callback checked while streaming a [`ContentBody`].
pub(crate) type LiveCheck = Arc<dyn Fn() -> Result<(), io::Error> + Send + Sync>;

/// HTTP response body produced by [`handle_request`](crate::handle_request).
///
/// Streaming bodies read at most 64 KiB per frame poll through a [`BufReader`] (avoiding per-poll
/// zero-fill), propagate backpressure from the HTTP transport to the underlying reader, re-check
/// ticket and policy liveness periodically while streaming, and drop the reader and any RAII
/// permits immediately if the client disconnects.
pub struct ContentBody {
    kind: BodyKind,
}

enum BodyKind {
    Empty,
    Fixed(Option<Bytes>),
    Stream {
        reader: BufReader<BoxAsyncRead>,
        remaining: u64,
        eof_verified: bool,
        live_check: Option<LiveCheck>,
        check_bytes_interval: u64,
        check_time_interval: Duration,
        bytes_since_check: u64,
        last_check: Instant,
        bytes_served: Option<Arc<AtomicU64>>,
        _guards: Vec<Box<dyn Any + Send + Sync>>,
    },
}

impl ContentBody {
    /// An empty response body (used for `HEAD`, `416`, and zero-length files).
    pub fn empty() -> Self {
        Self {
            kind: BodyKind::Empty,
        }
    }

    /// A small buffered response body (used for JSON error replies).
    pub fn from_bytes(bytes: impl Into<Bytes>) -> Self {
        let bytes = bytes.into();
        if bytes.is_empty() {
            Self::empty()
        } else {
            Self {
                kind: BodyKind::Fixed(Some(bytes)),
            }
        }
    }

    /// A streaming body that reads exactly `length` bytes from `reader`.
    ///
    /// If `reader` reaches EOF before `length` bytes have been yielded (for example because the
    /// file shrank mid-stream), polling yields an [`io::ErrorKind::UnexpectedEof`] error so the
    /// HTTP server aborts the response rather than delivering a truncated payload as complete.
    pub fn stream(reader: BoxAsyncRead, length: u64) -> Self {
        Self::stream_with_controls(
            reader,
            length,
            None,
            LIVENESS_CHECK_BYTES,
            LIVENESS_CHECK_INTERVAL,
            None,
            Vec::new(),
        )
    }

    pub(crate) fn stream_with_controls(
        reader: BoxAsyncRead,
        length: u64,
        live_check: Option<LiveCheck>,
        check_bytes_interval: u64,
        check_time_interval: Duration,
        bytes_served: Option<Arc<AtomicU64>>,
        guards: Vec<Box<dyn Any + Send + Sync>>,
    ) -> Self {
        if length == 0 {
            return Self::empty();
        }
        let cap = usize::try_from(length)
            .unwrap_or(CHUNK_SIZE)
            .min(CHUNK_SIZE);
        Self {
            kind: BodyKind::Stream {
                reader: BufReader::with_capacity(cap, reader),
                remaining: length,
                eof_verified: false,
                live_check,
                check_bytes_interval: check_bytes_interval.max(1),
                check_time_interval,
                bytes_since_check: 0,
                last_check: Instant::now(),
                bytes_served,
                _guards: guards,
            },
        }
    }
}

impl Body for ContentBody {
    type Data = Bytes;
    type Error = io::Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        match &mut self.kind {
            BodyKind::Empty => Poll::Ready(None),
            BodyKind::Fixed(slot) => Poll::Ready(slot.take().map(|bytes| Ok(Frame::data(bytes)))),
            BodyKind::Stream {
                reader,
                remaining,
                eof_verified,
                live_check,
                check_bytes_interval,
                check_time_interval,
                bytes_since_check,
                last_check,
                bytes_served,
                ..
            } => {
                if *remaining == 0 {
                    if *eof_verified {
                        return Poll::Ready(None);
                    }
                    // Poll the underlying reader once more at EOF so remote framing trailers
                    // (such as `{"done": true}\n`) are validated and unexpected extra bytes
                    // past the declared length fail with `InvalidData` without being forwarded.
                    return match Pin::new(&mut *reader).poll_fill_buf(cx) {
                        Poll::Pending => Poll::Pending,
                        Poll::Ready(Err(err)) => {
                            *eof_verified = true;
                            Poll::Ready(Some(Err(err)))
                        }
                        Poll::Ready(Ok(avail)) => {
                            *eof_verified = true;
                            if avail.is_empty() {
                                Poll::Ready(None)
                            } else {
                                Poll::Ready(Some(Err(io::Error::new(
                                    io::ErrorKind::InvalidData,
                                    "content stream yielded extra bytes past declared length",
                                ))))
                            }
                        }
                    };
                }

                if let Some(check) = live_check.as_ref()
                    && (*bytes_since_check >= *check_bytes_interval
                        || last_check.elapsed() >= *check_time_interval)
                {
                    if let Err(err) = check() {
                        *remaining = 0;
                        *eof_verified = true;
                        return Poll::Ready(Some(Err(err)));
                    }
                    *bytes_since_check = 0;
                    *last_check = Instant::now();
                }

                let want = usize::try_from(*remaining)
                    .unwrap_or(CHUNK_SIZE)
                    .min(CHUNK_SIZE);
                match Pin::new(&mut *reader).poll_fill_buf(cx) {
                    Poll::Pending => Poll::Pending,
                    Poll::Ready(Err(err)) => {
                        *remaining = 0;
                        *eof_verified = true;
                        Poll::Ready(Some(Err(err)))
                    }
                    Poll::Ready(Ok(avail)) => {
                        if avail.is_empty() {
                            let missing = *remaining;
                            *remaining = 0;
                            *eof_verified = true;
                            return Poll::Ready(Some(Err(io::Error::new(
                                io::ErrorKind::UnexpectedEof,
                                format!("content stream ended with {missing} bytes remaining"),
                            ))));
                        }
                        let take = avail.len().min(want);
                        let chunk = Bytes::copy_from_slice(&avail[..take]);
                        Pin::new(&mut *reader).consume(take);
                        *remaining -= take as u64;
                        *bytes_since_check = bytes_since_check.saturating_add(take as u64);
                        if let Some(counter) = bytes_served {
                            counter.fetch_add(take as u64, Ordering::Relaxed);
                        }
                        Poll::Ready(Some(Ok(Frame::data(chunk))))
                    }
                }
            }
        }
    }

    fn is_end_stream(&self) -> bool {
        match &self.kind {
            BodyKind::Empty => true,
            BodyKind::Fixed(slot) => slot.is_none(),
            BodyKind::Stream {
                remaining,
                eof_verified,
                ..
            } => *remaining == 0 && *eof_verified,
        }
    }

    fn size_hint(&self) -> SizeHint {
        match &self.kind {
            BodyKind::Empty => SizeHint::with_exact(0),
            BodyKind::Fixed(Some(bytes)) => SizeHint::with_exact(bytes.len() as u64),
            BodyKind::Fixed(None) => SizeHint::with_exact(0),
            BodyKind::Stream { remaining, .. } => SizeHint::with_exact(*remaining),
        }
    }
}
