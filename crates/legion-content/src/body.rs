//! Streaming HTTP body with bounded chunk buffering and backpressure.

use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};

use bytes::{Bytes, BytesMut};
use http_body::{Body, Frame, SizeHint};
use tokio::io::{AsyncRead, ReadBuf};

/// Chunk size read per poll so arbitrarily large files stream in constant memory.
const CHUNK_SIZE: usize = 64 * 1024;

/// Type-erased async reader yielded by a [`ContentSource`](crate::ContentSource).
pub type BoxAsyncRead = Box<dyn AsyncRead + Send + Unpin>;

/// HTTP response body produced by [`handle_request`](crate::handle_request).
///
/// Streaming bodies read at most 64 KiB per frame poll, propagate backpressure from the HTTP
/// transport to the underlying reader, and drop the reader immediately if the client disconnects.
pub struct ContentBody {
    kind: BodyKind,
}

enum BodyKind {
    Empty,
    Fixed(Option<Bytes>),
    Stream {
        reader: BoxAsyncRead,
        remaining: u64,
        buf: BytesMut,
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
        if length == 0 {
            return Self::empty();
        }
        let initial_cap = usize::try_from(length)
            .unwrap_or(CHUNK_SIZE)
            .min(CHUNK_SIZE);
        Self {
            kind: BodyKind::Stream {
                reader,
                remaining: length,
                buf: BytesMut::with_capacity(initial_cap),
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
                buf,
            } => {
                if *remaining == 0 {
                    return Poll::Ready(None);
                }
                let want = usize::try_from(*remaining)
                    .unwrap_or(CHUNK_SIZE)
                    .min(CHUNK_SIZE);
                buf.clear();
                if buf.capacity() < want {
                    buf.reserve(want);
                }
                buf.resize(want, 0);
                let mut read_buf = ReadBuf::new(&mut buf[..want]);
                match Pin::new(reader.as_mut()).poll_read(cx, &mut read_buf) {
                    Poll::Pending => Poll::Pending,
                    Poll::Ready(Err(err)) => {
                        *remaining = 0;
                        Poll::Ready(Some(Err(err)))
                    }
                    Poll::Ready(Ok(())) => {
                        let n = read_buf.filled().len();
                        if n == 0 {
                            let missing = *remaining;
                            *remaining = 0;
                            return Poll::Ready(Some(Err(io::Error::new(
                                io::ErrorKind::UnexpectedEof,
                                format!("content stream ended with {missing} bytes remaining"),
                            ))));
                        }
                        *remaining -= n as u64;
                        let chunk = buf.split_to(n).freeze();
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
            BodyKind::Stream { remaining, .. } => *remaining == 0,
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
