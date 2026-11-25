//! Resumable bulk-pipe file uploads (`legion stream` `put`) over a [`Link`](crate::Link) session.
//!
//! Each call to [`Link::open_put`](crate::Link::open_put) opens an independent `legion stream`
//! channel (an SSH exec channel on the existing connection, or a local child process) and runs the
//! `put` handshake (`protocol`, `op: "put"`, `path`, `size`, `hash`, optional `mode` and `mtime`).
//! The remote node replies with `{"offset": N}` indicating how many bytes already exist in its
//! partial file; the caller skips `N` bytes, streams the remaining `size - N` bytes via
//! [`AsyncWrite`], and awaits BLAKE3 verification and atomic rename in [`PutStream::finish`].

use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use legion_proto::{ErrorBody, ErrorCode};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio_util::sync::CancellationToken;

use crate::content::{BoxStreamRead, BoxStreamWrite, ContentTime, RawStream};
use crate::stderr::StderrTail;

/// Stream protocol version spoken by `legion stream`.
const STREAM_PROTOCOL: u32 = 1;
/// Stream capability name advertised by nodes whose `put` supports atomic `no_replace: true`.
pub const CAP_PUT_NO_REPLACE: &str = "put.no_replace";
/// Maximum reply line length accepted during the `put` handshake and verification reply.
const MAX_REPLY_LINE: usize = 1024 * 1024;
/// Maximum pre-protocol noise lines skipped before `{"protocol": 1}` (login banners, shell noise).
const MAX_NOISE_LINES: usize = 64;
/// Maximum cumulative bytes of noise skipped before giving up.
const MAX_NOISE_BYTES: usize = 64 * 1024;
/// Read buffer capacity wrapping the underlying stream for JSON reply lines.
const STREAM_BUF_BYTES: usize = 16 * 1024;
/// Minimum idle reply timeout while waiting for `put` verification heartbeats (`legion stream`
/// emits `{"busy": true}` every 10 seconds while hashing large files).
const MIN_PUT_FINISH_IDLE_TIMEOUT: Duration = Duration::from_secs(30);
/// Maximum number of `{"busy": true}` heartbeats tolerated during the initial `put` handshake.
const MAX_PUT_READY_BUSY_HEARTBEATS: usize = 16;
/// Maximum number of `{"busy": true}` heartbeats tolerated while waiting for `put` verification
/// (`60 * 10s = 10 minutes`), preventing a peer that spams heartbeats from holding a slot forever.
const MAX_PUT_FINISH_BUSY_HEARTBEATS: usize = 60;
/// Hard wall-clock cap on waiting for `put` verification regardless of `{"busy": true}` heartbeats.
const MAX_PUT_FINISH_TOTAL_TIMEOUT: Duration = Duration::from_secs(10 * 60);

/// Request metadata for uploading a file via [`Link::open_put`](crate::Link::open_put).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PutRequest {
    /// Destination file path on the target machine (absolute or `~`-prefixed).
    pub path: String,
    /// Exact total size of the file in bytes.
    pub size: u64,
    /// Expected BLAKE3 content hash (`blake3:<64 lowercase hex>`).
    pub hash: String,
    /// Optional Unix file mode bits to apply once verified (for example `0o644`).
    pub mode: Option<u32>,
    /// Optional file modification timestamp to apply once verified.
    pub mtime: Option<ContentTime>,
    /// When `true`, sends `"no_replace": true` in the `put` header so machines advertising
    /// [`CAP_PUT_NO_REPLACE`] refuse to overwrite an existing destination (returning
    /// [`ErrorCode::Exists`]) both on open and on final atomic placement. Older machines ignore
    /// this field and do not confirm it.
    pub no_replace: bool,
}

/// Verified upload outcome returned by [`PutStream::finish`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PutResult {
    /// Verified final size on the remote machine in bytes.
    pub size: u64,
    /// Verified BLAKE3 content hash reported by the remote machine (`blake3:<64 lowercase hex>`).
    pub hash: String,
}

/// Validates a `blake3:<64 lowercase hex>` digest string before opening a `put` stream.
pub(crate) fn validate_blake3_hash(hash: &str, machine: &str) -> Result<(), ErrorBody> {
    let Some(hex) = hash.strip_prefix("blake3:") else {
        return Err(ErrorBody::new(
            ErrorCode::InvalidArgs,
            "Upload hash must be formatted as `blake3:<64 lowercase hex>`.",
        )
        .with_hint("Compute the BLAKE3 digest of the source file before opening a put stream.")
        .with_machine(machine));
    };
    let valid_hex = hex.len() == 64
        && hex
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
    if !valid_hex {
        return Err(ErrorBody::new(
            ErrorCode::InvalidArgs,
            "Upload hash must contain 64 lowercase hexadecimal characters after `blake3:`.",
        )
        .with_hint("Compute the lowercase BLAKE3 hex digest before opening a put stream.")
        .with_machine(machine));
    }
    Ok(())
}

/// Async resumable file upload stream returned by [`Link::open_put`](crate::Link::open_put).
///
/// After the `put` handshake completes, [`PutStream::offset`] reports how many bytes the target
/// machine already holds in its partial file; the caller skips that many bytes from the local
/// source, streams the remaining `size - offset` bytes through [`AsyncWrite`], and calls
/// [`PutStream::finish`] to wait for BLAKE3 verification and atomic rename. Dropping or calling
/// [`PutStream::cancel`] closes the underlying channel immediately while leaving any partial file
/// on disk so a later upload can resume.
pub struct PutStream {
    reader: BufReader<BoxStreamRead>,
    writer: Option<BoxStreamWrite>,
    stderr: Option<Arc<StderrTail>>,
    machine: String,
    expected_size: u64,
    expected_hash: String,
    offset: u64,
    remaining: u64,
    no_replace_confirmed: bool,
    stream_capabilities: Vec<String>,
    timeout: Duration,
    cancel: CancellationToken,
    channel_cancel: CancellationToken,
    _guard: Option<Box<dyn Send + Sync>>,
    _flight: Option<Box<dyn Send + Sync>>,
}

impl PutStream {
    /// Byte offset the target machine already has on disk and expects the caller to resume from.
    pub fn offset(&self) -> u64 {
        self.offset
    }

    /// Total declared file size in bytes.
    pub fn size(&self) -> u64 {
        self.expected_size
    }

    /// Remaining bytes that must still be written before calling [`PutStream::finish`].
    pub fn remaining(&self) -> u64 {
        self.remaining
    }

    /// Returns `true` when the remote machine confirmed atomic no-replace enforcement for this
    /// upload (via `"no_replace": true` in its ready reply or [`CAP_PUT_NO_REPLACE`] in its stream
    /// greeting when requested).
    pub fn no_replace_confirmed(&self) -> bool {
        self.no_replace_confirmed
    }

    /// Stream capabilities advertised by the remote machine on this stream channel.
    pub fn stream_capabilities(&self) -> &[String] {
        &self.stream_capabilities
    }

    /// Cancels this upload stream and closes its underlying SSH channel or child process
    /// immediately.
    pub fn cancel(&self) {
        self.cancel.cancel();
        self.channel_cancel.cancel();
    }

    pub(crate) fn attach_flight(&mut self, guard: Box<dyn Send + Sync>) {
        self._flight = Some(guard);
    }

    /// Flushes the upload stream, signals EOF, and waits for the remote machine to verify the
    /// BLAKE3 hash and atomically rename the file into place.
    pub async fn finish(mut self) -> Result<PutResult, ErrorBody> {
        if self.cancel.is_cancelled() {
            self.channel_cancel.cancel();
            return Err(ErrorBody::new(
                ErrorCode::Cancelled,
                format!("Upload to {} was cancelled.", self.machine),
            )
            .with_machine(&self.machine));
        }
        if self.remaining > 0 {
            let remaining = self.remaining;
            self.channel_cancel.cancel();
            return Err(ErrorBody::new(
                ErrorCode::InvalidArgs,
                format!(
                    "Upload to {} cannot finish with {remaining} bytes still unwritten.",
                    self.machine
                ),
            )
            .with_hint(
                "Write all remaining bytes from the reported resume offset before finishing.",
            )
            .with_machine(&self.machine));
        }

        let idle_timeout = self.timeout.max(MIN_PUT_FINISH_IDLE_TIMEOUT);
        if let Some(mut writer) = self.writer.take() {
            let flush_fut = async {
                writer.flush().await.map_err(|err| {
                    put_io_error(&self.machine, "flushing upload stream", &err, &self.stderr)
                })?;
                writer.shutdown().await.map_err(|err| {
                    put_io_error(&self.machine, "closing upload input", &err, &self.stderr)
                })?;
                Ok::<(), ErrorBody>(())
            };
            tokio::select! {
                biased;
                () = self.cancel.cancelled() => {
                    self.channel_cancel.cancel();
                    return Err(ErrorBody::new(
                        ErrorCode::Cancelled,
                        format!("Upload to {} was cancelled.", self.machine),
                    )
                    .with_machine(&self.machine));
                }
                res = tokio::time::timeout(idle_timeout, flush_fut) => match res {
                    Ok(Ok(())) => {}
                    Ok(Err(err)) => {
                        self.channel_cancel.cancel();
                        return Err(err);
                    }
                    Err(_) => {
                        self.channel_cancel.cancel();
                        return Err(ErrorBody::new(
                            ErrorCode::Timeout,
                            format!(
                                "Timed out flushing upload bytes to {} after {}s.",
                                self.machine,
                                idle_timeout.as_secs_f64()
                            ),
                        )
                        .with_machine(&self.machine));
                    }
                }
            }
        }

        let done = read_put_done(
            &mut self.reader,
            &self.machine,
            &self.stderr,
            idle_timeout,
            &self.cancel,
        )
        .await;
        let done = match done {
            Ok(done) => done,
            Err(err) => {
                self.channel_cancel.cancel();
                return Err(err);
            }
        };
        if !done.ok || done.size != self.expected_size || done.hash != self.expected_hash {
            self.channel_cancel.cancel();
            return Err(ErrorBody::new(
                ErrorCode::Failed,
                format!(
                    "{} returned an upload confirmation that does not match the sent file.",
                    self.machine
                ),
            )
            .with_hint("Retry the upload and inspect the file hash on the destination machine.")
            .with_machine(&self.machine));
        }

        Ok(PutResult {
            size: done.size,
            hash: done.hash,
        })
    }
}

impl Drop for PutStream {
    fn drop(&mut self) {
        self.channel_cancel.cancel();
    }
}

impl AsyncWrite for PutStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        if self.cancel.is_cancelled() {
            self.channel_cancel.cancel();
            return Poll::Ready(Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "upload stream was cancelled",
            )));
        }
        if buf.is_empty() {
            return Poll::Ready(Ok(0));
        }
        if (buf.len() as u64) > self.remaining {
            return Poll::Ready(Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "attempted to write {} bytes with only {} bytes remaining in upload",
                    buf.len(),
                    self.remaining
                ),
            )));
        }
        let Some(writer) = self.writer.as_mut() else {
            return Poll::Ready(Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "upload stream writer is already closed",
            )));
        };
        match Pin::new(writer).poll_write(cx, buf) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Ok(n)) => {
                self.remaining = self.remaining.saturating_sub(n as u64);
                Poll::Ready(Ok(n))
            }
            Poll::Ready(Err(err)) => Poll::Ready(Err(err)),
        }
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        if self.cancel.is_cancelled() {
            self.channel_cancel.cancel();
            return Poll::Ready(Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "upload stream was cancelled",
            )));
        }
        match self.writer.as_mut() {
            Some(writer) => Pin::new(writer).poll_flush(cx),
            None => Poll::Ready(Ok(())),
        }
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.writer.as_mut() {
            Some(writer) => Pin::new(writer).poll_shutdown(cx),
            None => Poll::Ready(Ok(())),
        }
    }
}

#[derive(Debug, Deserialize)]
struct PutReadyReply {
    offset: u64,
    #[serde(default)]
    no_replace: Option<bool>,
}

#[derive(Debug, Deserialize)]
struct PutDoneReply {
    ok: bool,
    size: u64,
    hash: String,
}

/// Runs the `legion stream` `put` handshake on `stream` and returns a positioned [`PutStream`].
pub(crate) async fn handshake_put(
    stream: RawStream,
    machine: &str,
    request: &PutRequest,
    as_principal: Option<&str>,
    timeout: Duration,
    cancel: CancellationToken,
    on_greeting_capabilities: Option<&(dyn Fn(Vec<String>) + Send + Sync)>,
) -> Result<PutStream, ErrorBody> {
    let RawStream {
        reader,
        mut writer,
        stderr,
        cancel: channel_cancel,
        park: _,
        guard,
    } = stream;

    let mut header = json!({
        "protocol": STREAM_PROTOCOL,
        "op": "put",
        "path": request.path,
        "size": request.size,
        "hash": request.hash,
    });
    if let Some(mode) = request.mode {
        header["mode"] = json!(mode);
    }
    if let Some(mtime) = request.mtime {
        header["mtime"] = json!({ "secs": mtime.secs, "nanos": mtime.nanos });
    }
    if request.no_replace {
        header["no_replace"] = json!(true);
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
            .map_err(|err| put_io_error(machine, "writing stream put header", &err, &stderr))?;
        writer
            .flush()
            .await
            .map_err(|err| put_io_error(machine, "flushing stream put header", &err, &stderr))?;
        read_put_ready(&mut buf_reader, machine, &stderr, on_greeting_capabilities).await
    };

    let (ready, mut stream_capabilities) = tokio::select! {
        biased;
        () = cancel.cancelled() => {
            channel_cancel.cancel();
            return Err(ErrorBody::new(
                ErrorCode::Cancelled,
                format!("Opening upload to {machine} was cancelled."),
            )
            .with_machine(machine));
        }
        res = tokio::time::timeout(timeout, handshake) => match res {
            Ok(Ok(ready)) => ready,
            Ok(Err(err)) => {
                channel_cancel.cancel();
                return Err(err);
            }
            Err(_) => {
                channel_cancel.cancel();
                let mut err = ErrorBody::new(
                    ErrorCode::Timeout,
                    format!(
                        "Timed out waiting for {machine} to answer stream put after {}s.",
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

    if ready.offset > request.size {
        channel_cancel.cancel();
        return Err(ErrorBody::new(
            ErrorCode::Failed,
            format!(
                "{machine} answered stream put with resume offset {} past file size {}.",
                ready.offset, request.size
            ),
        )
        .with_machine(machine));
    }

    let no_replace_confirmed = ready.no_replace == Some(true)
        || (request.no_replace && stream_capabilities.iter().any(|c| c == CAP_PUT_NO_REPLACE));
    if ready.no_replace == Some(true)
        && !stream_capabilities.iter().any(|c| c == CAP_PUT_NO_REPLACE)
    {
        stream_capabilities.push(CAP_PUT_NO_REPLACE.to_owned());
    }

    let stream_cancel = cancel.child_token();
    let active_cancel = channel_cancel.clone();
    let watch_cancel = stream_cancel.clone();
    tokio::spawn(async move {
        tokio::select! {
            biased;
            () = watch_cancel.cancelled() => active_cancel.cancel(),
            () = active_cancel.cancelled() => {}
        }
    });

    Ok(PutStream {
        reader: buf_reader,
        writer: Some(writer),
        stderr,
        machine: machine.to_owned(),
        expected_size: request.size,
        expected_hash: request.hash.clone(),
        offset: ready.offset,
        remaining: request.size - ready.offset,
        no_replace_confirmed,
        stream_capabilities,
        timeout,
        cancel: stream_cancel,
        channel_cancel,
        _guard: guard,
        _flight: None,
    })
}

async fn read_put_ready<R: AsyncRead + Unpin>(
    reader: &mut BufReader<R>,
    machine: &str,
    stderr: &Option<Arc<StderrTail>>,
    on_greeting_capabilities: Option<&(dyn Fn(Vec<String>) + Send + Sync)>,
) -> Result<(PutReadyReply, Vec<String>), ErrorBody> {
    let mut greeted = false;
    let mut greeting_caps = Vec::new();
    let mut noise_lines = 0usize;
    let mut noise_bytes = 0usize;
    let mut busy_heartbeats = 0usize;
    let mut line = Vec::new();

    loop {
        line.clear();
        let n = read_bounded_line(reader, &mut line, MAX_REPLY_LINE)
            .await
            .map_err(|err| put_io_error(machine, "reading stream put reply", &err, stderr))?;
        if n == 0 {
            let mut err = ErrorBody::new(
                ErrorCode::Unreachable,
                format!("The upload stream to {machine} closed before answering."),
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
                    greeting_caps = parse_greeting_capabilities(&fields);
                    if let Some(cb) = on_greeting_capabilities {
                        cb(greeting_caps.clone());
                    }
                    greeted = true;
                    continue;
                }
            } else if fields.len() == 1 && fields.get("busy") == Some(&Value::Bool(true)) {
                busy_heartbeats += 1;
                if busy_heartbeats > MAX_PUT_READY_BUSY_HEARTBEATS {
                    return Err(ErrorBody::new(
                        ErrorCode::Timeout,
                        format!(
                            "Timed out waiting for {machine} to start the upload after repeated busy replies."
                        ),
                    )
                    .with_hint("Try again once the machine is no longer busy.")
                    .with_machine(machine));
                }
                continue;
            } else {
                if let Some(error_val) = fields.get("error")
                    && let Ok(body) = serde_json::from_value::<ErrorBody>(error_val.clone())
                {
                    return Err(crate::strip_remote_sent(body).with_machine(machine));
                }
                if let Ok(ready) = serde_json::from_value::<PutReadyReply>(Value::Object(fields)) {
                    return Ok((ready, greeting_caps));
                }
                return Err(ErrorBody::new(
                    ErrorCode::Failed,
                    format!("{machine} answered stream put with an unexpected line."),
                )
                .with_hint("Update legion on that machine so both ends speak the same protocol.")
                .with_machine(machine));
            }
        }

        if greeted {
            return Err(ErrorBody::new(
                ErrorCode::Failed,
                format!("{machine} answered stream put with non-JSON output."),
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

pub(crate) fn parse_greeting_capabilities(fields: &serde_json::Map<String, Value>) -> Vec<String> {
    fields
        .get("capabilities")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

async fn read_put_done<R: AsyncRead + Unpin>(
    reader: &mut BufReader<R>,
    machine: &str,
    stderr: &Option<Arc<StderrTail>>,
    idle_timeout: Duration,
    cancel: &CancellationToken,
) -> Result<PutDoneReply, ErrorBody> {
    read_put_done_with_limits(
        reader,
        machine,
        stderr,
        idle_timeout,
        MAX_PUT_FINISH_TOTAL_TIMEOUT,
        MAX_PUT_FINISH_BUSY_HEARTBEATS,
        cancel,
    )
    .await
}

async fn read_put_done_with_limits<R: AsyncRead + Unpin>(
    reader: &mut BufReader<R>,
    machine: &str,
    stderr: &Option<Arc<StderrTail>>,
    idle_timeout: Duration,
    total_timeout: Duration,
    max_busy_heartbeats: usize,
    cancel: &CancellationToken,
) -> Result<PutDoneReply, ErrorBody> {
    let started = Instant::now();
    let mut busy_heartbeats = 0usize;
    let mut line = Vec::new();
    loop {
        let Some(remaining) = total_timeout.checked_sub(started.elapsed()) else {
            return Err(ErrorBody::new(
                ErrorCode::Timeout,
                format!(
                    "Timed out waiting for {machine} to finish verifying the upload after {}s.",
                    total_timeout.as_secs_f64()
                ),
            )
            .with_hint("Retry the upload once the machine is no longer busy.")
            .with_machine(machine));
        };
        if remaining.is_zero() {
            return Err(ErrorBody::new(
                ErrorCode::Timeout,
                format!(
                    "Timed out waiting for {machine} to finish verifying the upload after {}s.",
                    total_timeout.as_secs_f64()
                ),
            )
            .with_hint("Retry the upload once the machine is no longer busy.")
            .with_machine(machine));
        }
        let step_timeout = idle_timeout.min(remaining);
        line.clear();
        let read_res = tokio::select! {
            biased;
            () = cancel.cancelled() => {
                return Err(ErrorBody::new(
                    ErrorCode::Cancelled,
                    format!("Upload to {machine} was cancelled."),
                )
                .with_machine(machine));
            }
            res = tokio::time::timeout(
                step_timeout,
                read_bounded_line(reader, &mut line, MAX_REPLY_LINE),
            ) => match res {
                Ok(inner) => inner.map_err(|err| {
                    put_io_error(machine, "reading upload verification reply", &err, stderr)
                })?,
                Err(_) => {
                    let mut err = ErrorBody::new(
                        ErrorCode::Timeout,
                        format!(
                            "Timed out waiting for {machine} to verify the upload after {}s of silence.",
                            step_timeout.as_secs_f64()
                        ),
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
            }
        };

        if read_res == 0 {
            let mut err = ErrorBody::new(
                ErrorCode::Unreachable,
                format!("The upload stream to {machine} closed before confirming verification."),
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

        let Ok(Value::Object(fields)) = serde_json::from_slice::<Value>(&line) else {
            return Err(ErrorBody::new(
                ErrorCode::Failed,
                format!("{machine} answered upload verification with non-JSON output."),
            )
            .with_machine(machine));
        };
        if fields.len() == 1 && fields.get("busy") == Some(&Value::Bool(true)) {
            busy_heartbeats += 1;
            if busy_heartbeats > max_busy_heartbeats {
                return Err(ErrorBody::new(
                    ErrorCode::Timeout,
                    format!(
                        "Timed out waiting for {machine} to verify the upload after repeated busy replies."
                    ),
                )
                .with_hint("Retry the upload once the machine is no longer busy.")
                .with_machine(machine));
            }
            continue;
        }
        if let Some(error_val) = fields.get("error")
            && let Ok(body) = serde_json::from_value::<ErrorBody>(error_val.clone())
        {
            return Err(crate::strip_remote_sent(body).with_machine(machine));
        }
        if let Ok(done) = serde_json::from_value::<PutDoneReply>(Value::Object(fields)) {
            return Ok(done);
        }
        return Err(ErrorBody::new(
            ErrorCode::Failed,
            format!("{machine} answered upload verification with an unexpected line."),
        )
        .with_hint("Update legion on that machine so both ends speak the same protocol.")
        .with_machine(machine));
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

fn put_io_error(
    machine: &str,
    stage: &str,
    io_err: &io::Error,
    stderr: &Option<Arc<StderrTail>>,
) -> ErrorBody {
    let mut err = ErrorBody::new(
        ErrorCode::Unreachable,
        format!("Upload stream to {machine} failed while {stage}: {io_err}."),
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

    use std::future::Future;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    use legion_proto::{HelloResult, PROTOCOL, Response, VERSION};
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

    use super::*;
    use crate::content::{StreamOpenError, StreamOpener};
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
    enum PutServerMode {
        Accept {
            resume_offset: u64,
            heartbeats: usize,
        },
        OffsetPastSize,
        HashMismatch,
        Deny,
        WrongDoneHash,
    }

    struct InMemoryPutOpener {
        mode: PutServerMode,
        received: Arc<std::sync::Mutex<Vec<u8>>>,
        cancelled: Arc<AtomicBool>,
    }

    impl StreamOpener for InMemoryPutOpener {
        fn open_stream(
            &self,
            _cancel: CancellationToken,
        ) -> Pin<Box<dyn Future<Output = Result<RawStream, StreamOpenError>> + Send + '_>> {
            let mode = self.mode;
            let received = Arc::clone(&self.received);
            let cancelled = Arc::clone(&self.cancelled);
            Box::pin(async move {
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
                    assert_eq!(req["op"], "put");
                    let size = req["size"].as_u64().unwrap();
                    let hash = req["hash"].as_str().unwrap().to_owned();

                    let _ = server_out.write_all(b"Welcome banner\n").await;
                    let _ = server_out.write_all(b"{\"protocol\":1}\n").await;
                    match mode {
                        PutServerMode::Deny => {
                            let err = json!({
                                "error": ErrorBody::new(ErrorCode::Denied, "fs.write is not allowed")
                            });
                            let _ = server_out.write_all(format!("{err}\n").as_bytes()).await;
                            return;
                        }
                        PutServerMode::OffsetPastSize => {
                            let ready = json!({ "offset": size + 1 });
                            let _ = server_out.write_all(format!("{ready}\n").as_bytes()).await;
                            return;
                        }
                        PutServerMode::Accept {
                            resume_offset,
                            heartbeats,
                        } => {
                            let ready = json!({ "offset": resume_offset });
                            let _ = server_out.write_all(format!("{ready}\n").as_bytes()).await;
                            let mut buf = vec![0u8; 4096];
                            let mut got = Vec::new();
                            let need = (size - resume_offset) as usize;
                            while got.len() < need {
                                tokio::select! {
                                    biased;
                                    () = watch_cancel.cancelled() => {
                                        cancelled.store(true, Ordering::SeqCst);
                                        return;
                                    }
                                    res = reader.read(&mut buf) => match res {
                                        Ok(0) => break,
                                        Ok(n) => got.extend_from_slice(&buf[..n]),
                                        Err(_) => return,
                                    }
                                }
                            }
                            *received.lock().unwrap() = got;
                            for _ in 0..heartbeats {
                                let _ = server_out.write_all(b"{\"busy\":true}\n").await;
                            }
                            let done = json!({ "ok": true, "size": size, "hash": hash });
                            let _ = server_out.write_all(format!("{done}\n").as_bytes()).await;
                        }
                        PutServerMode::HashMismatch => {
                            let _ = server_out.write_all(b"{\"offset\":0}\n").await;
                            let mut drain = Vec::new();
                            let _ = reader.read_to_end(&mut drain).await;
                            let err = json!({
                                "error": ErrorBody::new(ErrorCode::Conflict, "hash mismatch")
                            });
                            let _ = server_out.write_all(format!("{err}\n").as_bytes()).await;
                        }
                        PutServerMode::WrongDoneHash => {
                            let _ = server_out.write_all(b"{\"offset\":0}\n").await;
                            let mut drain = Vec::new();
                            let _ = reader.read_to_end(&mut drain).await;
                            let done = json!({
                                "ok": true,
                                "size": size,
                                "hash": "blake3:ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff"
                            });
                            let _ = server_out.write_all(format!("{done}\n").as_bytes()).await;
                        }
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
                                node_explicit: None,
                                node_id: None,
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

    const TEST_HASH: &str =
        "blake3:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    #[tokio::test]
    async fn open_put_uploads_resumes_handles_busy_and_validates_inputs() {
        let payload: Vec<u8> = (0..40_000usize).map(|i| (i % 251) as u8).collect();
        let received = Arc::new(std::sync::Mutex::new(Vec::new()));
        let link = make_mock_link(
            "tower",
            "tower",
            Arc::new(InMemoryPutOpener {
                mode: PutServerMode::Accept {
                    resume_offset: 10_000,
                    heartbeats: 3,
                },
                received: Arc::clone(&received),
                cancelled: Arc::new(AtomicBool::new(false)),
            }),
            Arc::new(AtomicUsize::new(0)),
        );

        let err = link
            .open_put(
                "tower",
                PutRequest {
                    path: String::new(),
                    size: 10,
                    hash: TEST_HASH.to_owned(),
                    mode: None,
                    mtime: None,
                    no_replace: false,
                },
            )
            .await
            .err()
            .unwrap();
        assert_eq!(err.code, ErrorCode::InvalidArgs);

        let err = link
            .open_put(
                "tower",
                PutRequest {
                    path: "/tmp/a.bin".to_owned(),
                    size: 10,
                    hash: "sha256:deadbeef".to_owned(),
                    mode: None,
                    mtime: None,
                    no_replace: false,
                },
            )
            .await
            .err()
            .unwrap();
        assert_eq!(err.code, ErrorCode::InvalidArgs);

        let err = link
            .open_put(
                "pi",
                PutRequest {
                    path: "/tmp/a.bin".to_owned(),
                    size: 10,
                    hash: TEST_HASH.to_owned(),
                    mode: None,
                    mtime: None,
                    no_replace: false,
                },
            )
            .await
            .err()
            .unwrap();
        assert_eq!(err.code, ErrorCode::NotFound);

        let mut put = link
            .open_put(
                "tower",
                PutRequest {
                    path: "/home/me/photo.jpg".to_owned(),
                    size: payload.len() as u64,
                    hash: TEST_HASH.to_owned(),
                    mode: Some(0o644),
                    mtime: Some(ContentTime {
                        secs: 1_700_000_000,
                        nanos: 0,
                    }),
                    no_replace: false,
                },
            )
            .await
            .unwrap();
        assert_eq!(put.offset(), 10_000);
        assert_eq!(put.size(), 40_000);
        assert_eq!(put.remaining(), 30_000);

        let too_big = vec![0u8; 30_001];
        let write_err = put.write(&too_big).await.unwrap_err();
        assert_eq!(write_err.kind(), io::ErrorKind::InvalidInput);
        assert_eq!(put.remaining(), 30_000);

        put.write_all(&payload[10_000..]).await.unwrap();
        assert_eq!(put.remaining(), 0);
        let done = put.finish().await.unwrap();
        assert_eq!(done.size, 40_000);
        assert_eq!(done.hash, TEST_HASH);
        assert_eq!(*received.lock().unwrap(), payload[10_000..]);
    }

    #[tokio::test]
    async fn open_put_rejects_server_errors_bad_offset_mismatched_done_and_cancel() {
        for (mode, expected_code) in [
            (PutServerMode::Deny, ErrorCode::Denied),
            (PutServerMode::OffsetPastSize, ErrorCode::Failed),
        ] {
            let link = make_mock_link(
                "tower",
                "tower",
                Arc::new(InMemoryPutOpener {
                    mode,
                    received: Arc::new(std::sync::Mutex::new(Vec::new())),
                    cancelled: Arc::new(AtomicBool::new(false)),
                }),
                Arc::new(AtomicUsize::new(0)),
            );
            let err = link
                .open_put(
                    "tower",
                    PutRequest {
                        path: "/home/me/a.bin".to_owned(),
                        size: 16,
                        hash: TEST_HASH.to_owned(),
                        mode: None,
                        mtime: None,
                        no_replace: false,
                    },
                )
                .await
                .err()
                .unwrap();
            assert_eq!(err.code, expected_code, "{err}");
        }

        for (mode, expected_code) in [
            (PutServerMode::HashMismatch, ErrorCode::Conflict),
            (PutServerMode::WrongDoneHash, ErrorCode::Failed),
        ] {
            let link = make_mock_link(
                "tower",
                "tower",
                Arc::new(InMemoryPutOpener {
                    mode,
                    received: Arc::new(std::sync::Mutex::new(Vec::new())),
                    cancelled: Arc::new(AtomicBool::new(false)),
                }),
                Arc::new(AtomicUsize::new(0)),
            );
            let mut put = link
                .open_put(
                    "tower",
                    PutRequest {
                        path: "/home/me/a.bin".to_owned(),
                        size: 16,
                        hash: TEST_HASH.to_owned(),
                        mode: None,
                        mtime: None,
                        no_replace: false,
                    },
                )
                .await
                .unwrap();
            put.write_all(&[7u8; 16]).await.unwrap();
            let err = put.finish().await.unwrap_err();
            assert_eq!(err.code, expected_code, "{err}");
        }

        let cancelled = Arc::new(AtomicBool::new(false));
        let link = make_mock_link(
            "tower",
            "tower",
            Arc::new(InMemoryPutOpener {
                mode: PutServerMode::Accept {
                    resume_offset: 0,
                    heartbeats: 0,
                },
                received: Arc::new(std::sync::Mutex::new(Vec::new())),
                cancelled: Arc::clone(&cancelled),
            }),
            Arc::new(AtomicUsize::new(0)),
        );
        let mut put = link
            .open_put(
                "tower",
                PutRequest {
                    path: "/home/me/a.bin".to_owned(),
                    size: 100,
                    hash: TEST_HASH.to_owned(),
                    mode: None,
                    mtime: None,
                    no_replace: false,
                },
            )
            .await
            .unwrap();
        put.write_all(&[1u8; 10]).await.unwrap();
        let err = put.finish().await.unwrap_err();
        assert_eq!(err.code, ErrorCode::InvalidArgs);

        let cancel_token = CancellationToken::new();
        let mut put2 = link
            .open_put_with_cancel(
                "tower",
                PutRequest {
                    path: "/home/me/b.bin".to_owned(),
                    size: 100,
                    hash: TEST_HASH.to_owned(),
                    mode: None,
                    mtime: None,
                    no_replace: false,
                },
                cancel_token.clone(),
            )
            .await
            .unwrap();
        cancel_token.cancel();
        let write_err = put2.write(&[2u8; 10]).await.unwrap_err();
        assert_eq!(write_err.kind(), io::ErrorKind::Interrupted);
        drop(put2);
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(cancelled.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn read_put_done_bounds_endless_busy_heartbeats_by_count_and_total_deadline() {
        // Endless {"busy":true} stream fails promptly once heartbeat count exceeds limit.
        let (client, mut server) = tokio::io::duplex(4096);
        let producer = tokio::spawn(async move {
            for _ in 0..20 {
                if server.write_all(b"{\"busy\":true}\n").await.is_err() {
                    break;
                }
            }
        });
        let mut reader = BufReader::new(client);
        let cancel = CancellationToken::new();
        let err = read_put_done_with_limits(
            &mut reader,
            "tower",
            &None,
            Duration::from_secs(5),
            Duration::from_secs(5),
            3,
            &cancel,
        )
        .await
        .unwrap_err();
        assert_eq!(err.code, ErrorCode::Timeout);
        assert!(err.message.contains("repeated busy replies"));
        drop(reader);
        let _ = producer.await;

        // Slow periodic {"busy":true} stream also fails once total deadline expires.
        let (client2, mut server2) = tokio::io::duplex(4096);
        let producer2 = tokio::spawn(async move {
            for _ in 0..20 {
                tokio::time::sleep(Duration::from_millis(30)).await;
                if server2.write_all(b"{\"busy\":true}\n").await.is_err() {
                    break;
                }
            }
        });
        let mut reader2 = BufReader::new(client2);
        let err2 = read_put_done_with_limits(
            &mut reader2,
            "tower",
            &None,
            Duration::from_secs(5),
            Duration::from_millis(80),
            100,
            &cancel,
        )
        .await
        .unwrap_err();
        assert_eq!(err2.code, ErrorCode::Timeout);
        drop(reader2);
        let _ = producer2.await;
    }

    struct CapabilityPutOpener {
        greeting_caps: Option<Vec<String>>,
        reply_no_replace: Option<bool>,
        reject_exists: Arc<AtomicBool>,
        seen_headers: Arc<std::sync::Mutex<Vec<Value>>>,
    }

    impl StreamOpener for CapabilityPutOpener {
        fn open_stream(
            &self,
            _cancel: CancellationToken,
        ) -> Pin<Box<dyn Future<Output = Result<RawStream, StreamOpenError>> + Send + '_>> {
            let greeting_caps = self.greeting_caps.clone();
            let reply_no_replace = self.reply_no_replace;
            let reject_exists = Arc::clone(&self.reject_exists);
            let seen_headers = Arc::clone(&self.seen_headers);
            Box::pin(async move {
                let (client_in, mut server_out) = tokio::io::duplex(16 * 1024);
                let (server_in, client_out) = tokio::io::duplex(16 * 1024);
                let stream_cancel = CancellationToken::new();

                tokio::spawn(async move {
                    let mut reader = BufReader::new(server_in);
                    let mut line = String::new();
                    if reader.read_line(&mut line).await.unwrap_or(0) == 0 {
                        return;
                    }
                    let req: Value = serde_json::from_str(&line).unwrap();
                    seen_headers.lock().unwrap().push(req.clone());

                    let mut greeting = json!({ "protocol": 1 });
                    if let Some(caps) = greeting_caps {
                        greeting["capabilities"] = json!(caps);
                    }
                    let _ = server_out
                        .write_all(format!("{greeting}\n").as_bytes())
                        .await;

                    if reject_exists.load(Ordering::SeqCst) {
                        let err = json!({
                            "error": ErrorBody::new(ErrorCode::Exists, "destination already exists")
                        });
                        let _ = server_out.write_all(format!("{err}\n").as_bytes()).await;
                        let _ = server_out.shutdown().await;
                        return;
                    }

                    let mut ready = json!({ "offset": 0 });
                    if let Some(nr) = reply_no_replace {
                        ready["no_replace"] = json!(nr);
                    }
                    let _ = server_out.write_all(format!("{ready}\n").as_bytes()).await;

                    let size = req["size"].as_u64().unwrap();
                    let hash = req["hash"].as_str().unwrap().to_owned();
                    let mut drain = Vec::new();
                    let _ = reader.read_to_end(&mut drain).await;
                    let done = json!({ "ok": true, "size": size, "hash": hash });
                    let _ = server_out.write_all(format!("{done}\n").as_bytes()).await;
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

    #[tokio::test]
    async fn open_put_tracks_no_replace_capability_across_greeting_reply_and_exists_errors() {
        // 1. Machine advertising `put.no_replace` in greeting caches support even when the first
        //    upload collides with ErrorCode::Exists.
        let reject_exists = Arc::new(AtomicBool::new(true));
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let link = make_mock_link(
            "tower",
            "tower",
            Arc::new(CapabilityPutOpener {
                greeting_caps: Some(vec![CAP_PUT_NO_REPLACE.to_owned()]),
                reply_no_replace: Some(true),
                reject_exists: Arc::clone(&reject_exists),
                seen_headers: Arc::clone(&seen),
            }),
            Arc::new(AtomicUsize::new(0)),
        );
        assert_eq!(link.stream_supports("tower", CAP_PUT_NO_REPLACE), None);

        let err = link
            .open_put(
                "tower",
                PutRequest {
                    path: "/home/me/report.pdf".to_owned(),
                    size: 4,
                    hash: TEST_HASH.to_owned(),
                    mode: None,
                    mtime: None,
                    no_replace: true,
                },
            )
            .await
            .err()
            .unwrap();
        assert_eq!(err.code, ErrorCode::Exists);
        assert_eq!(
            link.stream_supports("tower", CAP_PUT_NO_REPLACE),
            Some(true)
        );
        assert_eq!(seen.lock().unwrap()[0]["no_replace"], Value::Bool(true));

        reject_exists.store(false, Ordering::SeqCst);
        let mut put = link
            .open_put(
                "tower",
                PutRequest {
                    path: "/home/me/report (2).pdf".to_owned(),
                    size: 4,
                    hash: TEST_HASH.to_owned(),
                    mode: None,
                    mtime: None,
                    no_replace: true,
                },
            )
            .await
            .unwrap();
        assert!(put.no_replace_confirmed());
        assert!(
            put.stream_capabilities()
                .contains(&CAP_PUT_NO_REPLACE.to_owned())
        );
        put.write_all(b"abcd").await.unwrap();
        put.finish().await.unwrap();

        // 2. Machine advertising `no_replace: true` only on PutReady (no greeting list) still
        //    confirms and caches `put.no_replace`.
        let seen2 = Arc::new(std::sync::Mutex::new(Vec::new()));
        let link2 = make_mock_link(
            "tower",
            "tower",
            Arc::new(CapabilityPutOpener {
                greeting_caps: None,
                reply_no_replace: Some(true),
                reject_exists: Arc::new(AtomicBool::new(false)),
                seen_headers: Arc::clone(&seen2),
            }),
            Arc::new(AtomicUsize::new(0)),
        );
        let mut put2 = link2
            .open_put(
                "tower",
                PutRequest {
                    path: "/home/me/a.bin".to_owned(),
                    size: 4,
                    hash: TEST_HASH.to_owned(),
                    mode: None,
                    mtime: None,
                    no_replace: true,
                },
            )
            .await
            .unwrap();
        assert!(put2.no_replace_confirmed());
        assert_eq!(
            link2.stream_supports("tower", CAP_PUT_NO_REPLACE),
            Some(true)
        );
        put2.write_all(b"abcd").await.unwrap();
        put2.finish().await.unwrap();

        // 3. Older machine omitting both greeting capabilities and PutReady.no_replace caches
        //    Some(false) and omits `no_replace` when requested with `no_replace: false`.
        let seen3 = Arc::new(std::sync::Mutex::new(Vec::new()));
        let link3 = make_mock_link(
            "tower",
            "tower",
            Arc::new(CapabilityPutOpener {
                greeting_caps: None,
                reply_no_replace: None,
                reject_exists: Arc::new(AtomicBool::new(false)),
                seen_headers: Arc::clone(&seen3),
            }),
            Arc::new(AtomicUsize::new(0)),
        );
        let mut put3 = link3
            .open_put(
                "tower",
                PutRequest {
                    path: "/home/me/old.bin".to_owned(),
                    size: 4,
                    hash: TEST_HASH.to_owned(),
                    mode: None,
                    mtime: None,
                    no_replace: false,
                },
            )
            .await
            .unwrap();
        assert!(!put3.no_replace_confirmed());
        assert_eq!(
            link3.stream_supports("tower", CAP_PUT_NO_REPLACE),
            Some(false)
        );
        assert!(seen3.lock().unwrap()[0].get("no_replace").is_none());
        put3.write_all(b"abcd").await.unwrap();
        put3.finish().await.unwrap();
    }
}
