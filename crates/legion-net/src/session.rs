//! Multiplexed newline-delimited JSON-RPC 2.0 client session over any byte stream or connector
//! [`Pipe`].
//!
//! Opening performs the `hello` handshake while skipping bounded pre-protocol noise (SSH banners,
//! shell profile output). Once open, concurrent calls share one pipe keyed by request id, events
//! route to the matching call's [`EventSink`], cancellation sends `cancel` and waits boundedly for
//! the remote reply, and process death fails every in-flight call with `unreachable`.

use std::collections::HashMap;
use std::io;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use legion_core::{CANCEL_GRACE, CancellationToken, Event, EventSink, VERSION};
use legion_proto::{
    CallParams, CancelParams, ErrorBody, ErrorCode, EventParams, HelloParams, HelloResult, Id,
    Notification, Outcome, PROTOCOL, Request, Response, method,
};
use serde_json::{Value, json};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

use crate::connector::{Pipe, PipeHandle, StderrTail};

/// Maximum JSON-RPC line length accepted after handshake (matches `legion rpc`).
const LONGEST_LINE: usize = 16 * 1024 * 1024;
/// Maximum non-JSON-RPC noise lines skipped before the `hello` reply.
const MAX_NOISE_LINES: usize = 64;
/// Maximum cumulative bytes of noise skipped before the `hello` reply.
const MAX_NOISE_BYTES: usize = 64 * 1024;
/// How long a cancelled call waits for the remote node's reply after sending `cancel`: the remote
/// node gives its cancelled tool [`CANCEL_GRACE`] to clean up and reply, plus transport slack so
/// the final reply still arrives cleanly.
const CANCEL_REPLY_TIMEOUT: Duration = CANCEL_GRACE.saturating_add(Duration::from_secs(1));
/// Grace window for clean EOF exit when closing a session.
const CLOSE_GRACE: Duration = Duration::from_millis(500);
/// Brief wait for process exit + stderr drain when a pipe closes unexpectedly.
const EXIT_COLLECT_WAIT: Duration = Duration::from_millis(200);

/// Identity learned during the `hello` handshake.
#[derive(Clone, Debug, PartialEq)]
pub struct SessionInfo {
    /// The remote node's `hello` reply.
    pub hello: HelloResult,
    /// Which configured `[[systems]]` entry answered, when the machine has named systems.
    pub system: Option<String>,
}

/// Outcome of a call attempted on a [`Session`], distinguishing whether bytes ever reached the
/// remote pipe so the pool knows whether retrying once is safe.
#[derive(Clone, Debug)]
pub enum CallError {
    /// The session was already dead or failed before any byte of the request was written.
    /// Reopening and retrying once is safe because the remote never saw the call.
    NotWritten(ErrorBody),
    /// The session broke after the request was written; the remote may or may not have executed it.
    LostInFlight(ErrorBody),
    /// The remote node answered with an error (or the call was cancelled); the session is healthy.
    Remote(ErrorBody),
}

impl CallError {
    /// Extracts the underlying [`ErrorBody`].
    pub fn into_error(self) -> ErrorBody {
        match self {
            CallError::NotWritten(err) | CallError::LostInFlight(err) | CallError::Remote(err) => {
                err
            }
        }
    }
}

/// Internal classification of a failed handshake so the pool knows whether trying the alternate
/// SSH OS shape could succeed.
#[derive(Debug)]
pub(crate) enum OpenError {
    Unsupported(ErrorBody),
    Cancelled(ErrorBody),
    Unreachable {
        error: ErrorBody,
        retry_other_os: bool,
    },
}

impl OpenError {
    pub(crate) fn into_error(self) -> ErrorBody {
        match self {
            OpenError::Unsupported(err)
            | OpenError::Cancelled(err)
            | OpenError::Unreachable { error: err, .. } => err,
        }
    }
}

struct PendingCall {
    reply: oneshot::Sender<Result<Value, CallError>>,
    events: EventSink,
}

type BoxWriter = Box<dyn AsyncWrite + Unpin + Send>;

/// One live JSON-RPC 2.0 client session to a remote Legion node.
pub struct Session {
    machine: String,
    info: SessionInfo,
    stdin: Arc<tokio::sync::Mutex<Option<BoxWriter>>>,
    pending: Arc<Mutex<HashMap<Id, PendingCall>>>,
    alive: Arc<AtomicBool>,
    next_id: AtomicI64,
    stderr: Option<Arc<StderrTail>>,
    death_error: Arc<Mutex<Option<ErrorBody>>>,
    pipe: Arc<tokio::sync::Mutex<Option<PipeHandle>>>,
    reader_task: Mutex<Option<JoinHandle<()>>>,
}

impl Session {
    /// Opens a session over a spawned connector [`Pipe`], completing the `hello` handshake within
    /// `timeout`.
    pub(crate) async fn open_pipe(
        pipe: Pipe,
        machine: &str,
        system: Option<String>,
        timeout: Duration,
        cancel: CancellationToken,
    ) -> Result<Session, OpenError> {
        let (stdin, stdout, handle) = pipe.into_parts();
        let stderr = handle.stderr_tail();
        Session::open_inner(
            stdout,
            stdin,
            Some(handle),
            Some(stderr),
            machine,
            system,
            timeout,
            cancel,
        )
        .await
    }

    /// Opens a session over arbitrary async reader/writer streams (useful for testing without
    /// spawning processes).
    pub async fn open_io<R, W>(
        reader: R,
        writer: W,
        machine: &str,
        system: Option<String>,
        timeout: Duration,
        cancel: CancellationToken,
    ) -> Result<Session, ErrorBody>
    where
        R: AsyncRead + Unpin + Send + 'static,
        W: AsyncWrite + Unpin + Send + 'static,
    {
        Session::open_inner(reader, writer, None, None, machine, system, timeout, cancel)
            .await
            .map_err(OpenError::into_error)
    }

    #[allow(clippy::too_many_arguments)]
    async fn open_inner<R, W>(
        reader: R,
        mut writer: W,
        mut handle: Option<PipeHandle>,
        stderr: Option<Arc<StderrTail>>,
        machine: &str,
        system: Option<String>,
        timeout: Duration,
        cancel: CancellationToken,
    ) -> Result<Session, OpenError>
    where
        R: AsyncRead + Unpin + Send + 'static,
        W: AsyncWrite + Unpin + Send + 'static,
    {
        if cancel.is_cancelled() {
            if let Some(h) = handle.take() {
                drop(writer);
                h.close(CLOSE_GRACE).await;
            }
            return Err(OpenError::Cancelled(
                ErrorBody::new(
                    ErrorCode::Cancelled,
                    format!("Connecting to {machine} was cancelled."),
                )
                .with_machine(machine),
            ));
        }

        let hello_id = Id::Number(1);
        let hello_params = HelloParams {
            client: "legion".to_owned(),
            version: VERSION.to_owned(),
            protocol: PROTOCOL,
        };
        let hello_req = Request::new(
            hello_id.clone(),
            method::HELLO,
            serde_json::to_value(&hello_params).unwrap_or(Value::Null),
        );
        let mut line = serde_json::to_string(&hello_req).map_err(|err| OpenError::Unreachable {
            error: ErrorBody::new(
                ErrorCode::Internal,
                format!("Could not encode hello request: {err}."),
            )
            .with_machine(machine),
            retry_other_os: false,
        })?;
        line.push('\n');

        let mut buf_reader = BufReader::new(reader);
        let handshake = async {
            if let Err(err) = writer.write_all(line.as_bytes()).await {
                return Err(HandshakeFailure::Io(err));
            }
            if let Err(err) = writer.flush().await {
                return Err(HandshakeFailure::Io(err));
            }
            read_hello_reply(&mut buf_reader, &hello_id, machine).await
        };

        let hello_result = tokio::select! {
            biased;
            () = cancel.cancelled() => {
                drop(writer);
                if let Some(h) = handle.take() {
                    h.close(CLOSE_GRACE).await;
                }
                return Err(OpenError::Cancelled(
                    ErrorBody::new(
                        ErrorCode::Cancelled,
                        format!("Connecting to {machine} was cancelled."),
                    )
                    .with_machine(machine),
                ));
            }
            res = tokio::time::timeout(timeout, handshake) => match res {
                Ok(Ok(hello)) => hello,
                Ok(Err(HandshakeFailure::Unsupported(err))) => {
                    drop(writer);
                    if let Some(h) = handle.take() {
                        h.close(CLOSE_GRACE).await;
                    }
                    return Err(OpenError::Unsupported(err.with_machine(machine)));
                }
                Ok(Err(failure)) => {
                    drop(writer);
                    let (exit_code, stderr_text) = collect_failure_context(&mut handle, &stderr).await;
                    if let Some(h) = handle.take() {
                        h.close(CLOSE_GRACE).await;
                    }
                    let retry_other_os = exit_code != Some(255);
                    return Err(OpenError::Unreachable {
                        error: format_handshake_unreachable(machine, &failure, exit_code, &stderr_text),
                        retry_other_os,
                    });
                }
                Err(_) => {
                    drop(writer);
                    let (exit_code, stderr_text) = collect_failure_context(&mut handle, &stderr).await;
                    if let Some(h) = handle.take() {
                        h.close(CLOSE_GRACE).await;
                    }
                    let mut err = ErrorBody::new(
                        ErrorCode::Unreachable,
                        format!(
                            "Timed out waiting for {machine} to answer hello after {}s.",
                            timeout.as_secs_f64()
                        ),
                    )
                    .with_machine(machine);
                    if !stderr_text.is_empty() {
                        err = err.with_hint(format!("Remote stderr: {stderr_text}"));
                    } else if let Some(code) = exit_code {
                        err = err.with_hint(format!("Connector exited with status {code}."));
                    } else {
                        err = err.with_hint(format!(
                            "Check that {machine} is awake and reachable and that legion is installed there."
                        ));
                    }
                    return Err(OpenError::Unreachable {
                        error: err,
                        retry_other_os: false,
                    });
                }
            }
        };

        let stdin: Arc<tokio::sync::Mutex<Option<BoxWriter>>> =
            Arc::new(tokio::sync::Mutex::new(Some(Box::new(writer))));
        let pending = Arc::new(Mutex::new(HashMap::new()));
        let alive = Arc::new(AtomicBool::new(true));
        let death_error = Arc::new(Mutex::new(None));
        let pipe_arc = Arc::new(tokio::sync::Mutex::new(handle));

        let reader_handle = tokio::spawn(reader_loop(
            buf_reader,
            machine.to_owned(),
            Arc::clone(&stdin),
            Arc::clone(&pending),
            Arc::clone(&alive),
            stderr.clone(),
            Arc::clone(&death_error),
            Arc::clone(&pipe_arc),
        ));

        Ok(Session {
            machine: machine.to_owned(),
            info: SessionInfo {
                hello: hello_result,
                system,
            },
            stdin,
            pending,
            alive,
            next_id: AtomicI64::new(2),
            stderr,
            death_error,
            pipe: pipe_arc,
            reader_task: Mutex::new(Some(reader_handle)),
        })
    }

    /// Information reported by the remote node during `hello`.
    pub fn info(&self) -> &SessionInfo {
        &self.info
    }

    /// Whether the session's pipe is still believed to be open.
    pub fn is_alive(&self) -> bool {
        self.alive.load(Ordering::Acquire)
    }

    /// Sends a live `hello` request over an already-open session to verify connectivity and
    /// measure round-trip latency.
    pub async fn hello(
        &self,
        timeout: Duration,
        cancel: CancellationToken,
    ) -> Result<HelloResult, CallError> {
        let params = HelloParams {
            client: "legion".to_owned(),
            version: VERSION.to_owned(),
            protocol: PROTOCOL,
        };
        let params_value = serde_json::to_value(&params).unwrap_or(Value::Null);
        let fut = self.request(
            method::HELLO,
            params_value,
            cancel,
            EventSink::discard(),
            false,
        );
        match tokio::time::timeout(timeout, fut).await {
            Ok(Ok(value)) => serde_json::from_value::<HelloResult>(value).map_err(|err| {
                CallError::Remote(
                    ErrorBody::new(
                        ErrorCode::Failed,
                        format!("{} returned a malformed hello reply: {err}.", self.machine),
                    )
                    .with_machine(&self.machine),
                )
            }),
            Ok(Err(err)) => Err(err),
            Err(_) => Err(CallError::LostInFlight(
                ErrorBody::new(
                    ErrorCode::Unreachable,
                    format!(
                        "Timed out waiting for {} to answer hello after {}s.",
                        self.machine,
                        timeout.as_secs_f64()
                    ),
                )
                .with_machine(&self.machine),
            )),
        }
    }

    /// Runs one remote `call` on this session, forwarding events to `events` and sending `cancel`
    /// (then waiting boundedly for the remote reply) when `cancel` fires.
    pub async fn call(
        &self,
        params: CallParams,
        cancel: CancellationToken,
        events: EventSink,
    ) -> Result<Value, CallError> {
        let params_value = serde_json::to_value(&params).map_err(|err| {
            CallError::NotWritten(
                ErrorBody::new(
                    ErrorCode::Internal,
                    format!("Could not encode call parameters: {err}."),
                )
                .with_machine(&self.machine),
            )
        })?;
        self.request(method::CALL, params_value, cancel, events, true)
            .await
    }

    async fn request(
        &self,
        rpc_method: &str,
        params: Value,
        cancel: CancellationToken,
        events: EventSink,
        send_cancel_on_abort: bool,
    ) -> Result<Value, CallError> {
        if cancel.is_cancelled() {
            return Err(CallError::Remote(
                ErrorBody::new(
                    ErrorCode::Cancelled,
                    format!(
                        "The call on {} was cancelled before it was sent.",
                        self.machine
                    ),
                )
                .with_machine(&self.machine),
            ));
        }
        if !self.is_alive() {
            return Err(CallError::NotWritten(self.dead_session_error(false)));
        }

        let id = Id::Number(self.next_id.fetch_add(1, Ordering::Relaxed));
        let req = Request::new(id.clone(), rpc_method, params);
        let mut encoded = serde_json::to_string(&req).map_err(|err| {
            CallError::NotWritten(
                ErrorBody::new(
                    ErrorCode::Internal,
                    format!("Could not serialize JSON-RPC request: {err}."),
                )
                .with_machine(&self.machine),
            )
        })?;
        encoded.push('\n');

        let (reply_tx, mut reply_rx) = oneshot::channel();
        {
            let mut map = self
                .pending
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if !self.is_alive() {
                return Err(CallError::NotWritten(self.dead_session_error(false)));
            }
            map.insert(
                id.clone(),
                PendingCall {
                    reply: reply_tx,
                    events,
                },
            );
        }
        let guard = PendingGuard {
            pending: Arc::clone(&self.pending),
            id: id.clone(),
        };

        // Acquire stdin lock while respecting cancellation before any byte is written.
        let mut stdin_guard = tokio::select! {
            biased;
            () = cancel.cancelled() => {
                drop(guard);
                return Err(CallError::Remote(
                    ErrorBody::new(
                        ErrorCode::Cancelled,
                        format!("The call on {} was cancelled before it was sent.", self.machine),
                    )
                    .with_machine(&self.machine),
                ));
            }
            lock = self.stdin.lock() => lock,
        };

        let Some(writer) = stdin_guard.as_mut() else {
            drop(stdin_guard);
            drop(guard);
            return Err(CallError::NotWritten(self.dead_session_error(false)));
        };
        if !self.is_alive() {
            drop(stdin_guard);
            drop(guard);
            return Err(CallError::NotWritten(self.dead_session_error(false)));
        }

        match write_line_tracking(writer.as_mut(), encoded.as_bytes()).await {
            WriteStatus::Written => {
                drop(stdin_guard);
            }
            WriteStatus::NeverWritten(io_err) => {
                *stdin_guard = None;
                self.alive.store(false, Ordering::Release);
                drop(stdin_guard);
                drop(guard);
                return Err(CallError::NotWritten(
                    self.write_failure_error(&io_err, false),
                ));
            }
            WriteStatus::FailedAfterWrite(io_err) => {
                *stdin_guard = None;
                self.alive.store(false, Ordering::Release);
                drop(stdin_guard);
                drop(guard);
                return Err(CallError::LostInFlight(
                    self.write_failure_error(&io_err, true),
                ));
            }
        }

        let outcome = tokio::select! {
            biased;
            reply = &mut reply_rx => match reply {
                Ok(result) => result,
                Err(_) => Err(CallError::LostInFlight(self.dead_session_error(true))),
            },
            () = cancel.cancelled() => {
                if send_cancel_on_abort {
                    self.send_cancel(&id).await;
                    match tokio::time::timeout(CANCEL_REPLY_TIMEOUT, &mut reply_rx).await {
                        Ok(Ok(result)) => result,
                        Ok(Err(_)) => Err(CallError::LostInFlight(self.dead_session_error(true))),
                        Err(_) => Err(CallError::Remote(
                            ErrorBody::new(
                                ErrorCode::Cancelled,
                                format!(
                                    "The call on {} was cancelled and did not reply within {}s.",
                                    self.machine,
                                    CANCEL_REPLY_TIMEOUT.as_secs()
                                ),
                            )
                            .with_machine(&self.machine),
                        )),
                    }
                } else {
                    Err(CallError::Remote(
                        ErrorBody::new(
                            ErrorCode::Cancelled,
                            format!("The request on {} was cancelled.", self.machine),
                        )
                        .with_machine(&self.machine),
                    ))
                }
            }
        };
        drop(guard);
        outcome
    }

    async fn send_cancel(&self, target_id: &Id) {
        let cancel_id = Id::Number(self.next_id.fetch_add(1, Ordering::Relaxed));
        let req = Request::new(
            cancel_id,
            method::CANCEL,
            serde_json::to_value(CancelParams {
                id: target_id.clone(),
            })
            .unwrap_or(Value::Null),
        );
        let Ok(mut line) = serde_json::to_string(&req) else {
            return;
        };
        line.push('\n');
        let _ = tokio::time::timeout(Duration::from_secs(2), async {
            let mut stdin_guard = self.stdin.lock().await;
            if let Some(writer) = stdin_guard.as_mut()
                && writer.write_all(line.as_bytes()).await.is_ok()
            {
                let _ = writer.flush().await;
            }
        })
        .await;
    }

    /// Closes stdin to signal EOF, waits briefly for clean exit, and kills the connector process
    /// if it has not exited.
    pub async fn close(&self) {
        self.alive.store(false, Ordering::Release);
        {
            let mut guard = self.stdin.lock().await;
            guard.take();
        }
        let handle = {
            let mut guard = self.pipe.lock().await;
            guard.take()
        };
        if let Some(handle) = handle {
            handle.close(CLOSE_GRACE).await;
        }
        let reader = self
            .reader_task
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take();
        if let Some(task) = reader {
            task.abort();
        }
    }

    fn dead_session_error(&self, after_write: bool) -> ErrorBody {
        if after_write
            && let Some(existing) = self
                .death_error
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .clone()
        {
            return existing;
        }
        let msg = if after_write {
            format!(
                "The session to {} closed while the call was running; the outcome of the call is unknown.",
                self.machine
            )
        } else {
            format!("The session to {} is closed.", self.machine)
        };
        let mut err = ErrorBody::new(ErrorCode::Unreachable, msg).with_machine(&self.machine);
        if let Some(stderr) = &self.stderr {
            let text = stderr.text();
            if !text.is_empty() {
                err = err.with_hint(format!("Remote stderr: {text}"));
            }
        }
        err
    }

    fn write_failure_error(&self, io_err: &io::Error, after_write: bool) -> ErrorBody {
        let msg = if after_write {
            format!(
                "Writing to {} failed mid-request ({io_err}); the outcome of the call is unknown.",
                self.machine
            )
        } else {
            format!("The session to {} is closed ({io_err}).", self.machine)
        };
        let mut err = ErrorBody::new(ErrorCode::Unreachable, msg).with_machine(&self.machine);
        if let Some(stderr) = &self.stderr {
            let text = stderr.text();
            if !text.is_empty() {
                err = err.with_hint(format!("Remote stderr: {text}"));
            }
        }
        err
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.alive.store(false, Ordering::Release);
        if let Ok(mut guard) = self.stdin.try_lock() {
            guard.take();
        }
        if let Some(task) = self
            .reader_task
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take()
        {
            task.abort();
        }
        // Dropping PipeHandle synchronously waits briefly for EOF exit then kills if needed.
        if let Ok(mut guard) = self.pipe.try_lock() {
            guard.take();
        }
    }
}

struct PendingGuard {
    pending: Arc<Mutex<HashMap<Id, PendingCall>>>,
    id: Id,
}

impl Drop for PendingGuard {
    fn drop(&mut self) {
        self.pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&self.id);
    }
}

enum WriteStatus {
    Written,
    NeverWritten(io::Error),
    FailedAfterWrite(io::Error),
}

async fn write_line_tracking<W: AsyncWrite + Unpin + ?Sized>(
    writer: &mut W,
    bytes: &[u8],
) -> WriteStatus {
    let mut offset = 0;
    while offset < bytes.len() {
        match writer.write(&bytes[offset..]).await {
            Ok(0) => {
                let err = io::Error::new(io::ErrorKind::WriteZero, "pipe closed");
                return if offset == 0 {
                    WriteStatus::NeverWritten(err)
                } else {
                    WriteStatus::FailedAfterWrite(err)
                };
            }
            Ok(n) => offset += n,
            Err(err) => {
                return if offset == 0 {
                    WriteStatus::NeverWritten(err)
                } else {
                    WriteStatus::FailedAfterWrite(err)
                };
            }
        }
    }
    match writer.flush().await {
        Ok(()) => WriteStatus::Written,
        Err(err) => WriteStatus::FailedAfterWrite(err),
    }
}

enum HandshakeFailure {
    Unsupported(ErrorBody),
    Eof,
    TooMuchNoise { lines: usize, bytes: usize },
    BadHelloResult(String),
    RemoteError(ErrorBody),
    Io(io::Error),
}

async fn read_hello_reply<R: AsyncBufRead + Unpin>(
    reader: &mut R,
    hello_id: &Id,
    machine: &str,
) -> Result<HelloResult, HandshakeFailure> {
    let mut line = Vec::new();
    let mut noise_lines = 0usize;
    let mut noise_bytes = 0usize;

    loop {
        match read_line(reader, &mut line, LONGEST_LINE).await {
            Ok(Line::End) => return Err(HandshakeFailure::Eof),
            Ok(Line::TooLong) => {
                return Err(HandshakeFailure::TooMuchNoise {
                    lines: noise_lines + 1,
                    bytes: noise_bytes.saturating_add(LONGEST_LINE),
                });
            }
            Ok(Line::Read) => {
                if line.iter().all(u8::is_ascii_whitespace) {
                    continue;
                }
                if let Ok(response) = serde_json::from_slice::<Response>(&line)
                    && response.id.as_ref() == Some(hello_id)
                {
                    return match response.outcome {
                        Outcome::Result(value) => {
                            let hello: HelloResult = serde_json::from_value(value)
                                .map_err(|e| HandshakeFailure::BadHelloResult(e.to_string()))?;
                            if hello.protocol != PROTOCOL {
                                return Err(HandshakeFailure::Unsupported(
                                    ErrorBody::new(
                                        ErrorCode::Unsupported,
                                        format!(
                                            "{machine} speaks protocol {}, and this node speaks protocol {PROTOCOL}.",
                                            hello.protocol
                                        ),
                                    )
                                    .with_hint(
                                        "Update legion on whichever side is older so both speak the same protocol.",
                                    )
                                    .with_details(json!({
                                        "local_protocol": PROTOCOL,
                                        "remote_protocol": hello.protocol,
                                    })),
                                ));
                            }
                            Ok(hello)
                        }
                        Outcome::Error(rpc_err) => {
                            let body = rpc_err.data.unwrap_or_else(|| {
                                ErrorBody::new(ErrorCode::Failed, rpc_err.message.clone())
                            });
                            if body.code == ErrorCode::Unsupported {
                                let remote_versions = body
                                    .details
                                    .as_ref()
                                    .and_then(|d| d.get("supported"))
                                    .and_then(Value::as_array)
                                    .map(|arr| {
                                        arr.iter()
                                            .filter_map(Value::as_u64)
                                            .map(|v| v.to_string())
                                            .collect::<Vec<_>>()
                                            .join(", ")
                                    })
                                    .filter(|s| !s.is_empty());
                                let message = match remote_versions {
                                    Some(supported) => format!(
                                        "{machine} speaks protocol {supported}, and this node speaks protocol {PROTOCOL}."
                                    ),
                                    None => format!(
                                        "{machine} rejected protocol {PROTOCOL}: {}",
                                        body.message
                                    ),
                                };
                                let mut err = ErrorBody::new(ErrorCode::Unsupported, message)
                                    .with_hint(body.hint.unwrap_or_else(|| {
                                        "Update legion on whichever side is older so both speak the same protocol.".to_owned()
                                    }));
                                if let Some(details) = body.details {
                                    err = err.with_details(details);
                                }
                                Err(HandshakeFailure::Unsupported(err))
                            } else {
                                Err(HandshakeFailure::RemoteError(body))
                            }
                        }
                    };
                }
                noise_lines += 1;
                noise_bytes = noise_bytes.saturating_add(line.len());
                if noise_lines > MAX_NOISE_LINES || noise_bytes > MAX_NOISE_BYTES {
                    return Err(HandshakeFailure::TooMuchNoise {
                        lines: noise_lines,
                        bytes: noise_bytes,
                    });
                }
            }
            Err(err) => return Err(HandshakeFailure::Io(err)),
        }
    }
}

async fn collect_failure_context(
    handle: &mut Option<PipeHandle>,
    stderr: &Option<Arc<StderrTail>>,
) -> (Option<i32>, String) {
    if let Some(h) = handle.as_mut() {
        h.collect_exit_info(EXIT_COLLECT_WAIT).await
    } else {
        let text = stderr.as_ref().map_or_else(String::new, |s| s.text());
        (None, text)
    }
}

fn format_handshake_unreachable(
    machine: &str,
    failure: &HandshakeFailure,
    exit_code: Option<i32>,
    stderr_text: &str,
) -> ErrorBody {
    let message = match failure {
        HandshakeFailure::Eof => match exit_code {
            Some(code) => format!(
                "Could not open a legion session on {machine}: the connection closed before hello (exit status {code})."
            ),
            None => format!(
                "Could not open a legion session on {machine}: the connection closed before hello."
            ),
        },
        HandshakeFailure::TooMuchNoise { lines, bytes } => format!(
            "Could not open a legion session on {machine}: received {lines} non-protocol lines ({bytes} bytes) before hello."
        ),
        HandshakeFailure::BadHelloResult(reason) => format!(
            "Could not open a legion session on {machine}: hello reply was invalid ({reason})."
        ),
        HandshakeFailure::RemoteError(err) => format!(
            "Could not open a legion session on {machine}: {}",
            err.message
        ),
        HandshakeFailure::Io(err) => {
            format!("Could not open a legion session on {machine}: {err}.")
        }
        HandshakeFailure::Unsupported(err) => return err.clone().with_machine(machine),
    };
    let hint = if !stderr_text.is_empty() {
        format!("Remote stderr: {stderr_text}")
    } else {
        format!("Check that {machine} is reachable and that `legion rpc` can be started on it.")
    };
    ErrorBody::new(ErrorCode::Unreachable, message)
        .with_hint(hint)
        .with_machine(machine)
}

#[allow(clippy::too_many_arguments)]
async fn reader_loop<R: AsyncBufRead + Unpin>(
    mut reader: R,
    machine: String,
    stdin: Arc<tokio::sync::Mutex<Option<BoxWriter>>>,
    pending: Arc<Mutex<HashMap<Id, PendingCall>>>,
    alive: Arc<AtomicBool>,
    stderr: Option<Arc<StderrTail>>,
    death_error: Arc<Mutex<Option<ErrorBody>>>,
    pipe: Arc<tokio::sync::Mutex<Option<PipeHandle>>>,
) {
    let mut line = Vec::new();
    let read_err = loop {
        match read_line(&mut reader, &mut line, LONGEST_LINE).await {
            Ok(Line::End) => break None,
            Ok(Line::TooLong) => continue,
            Ok(Line::Read) => {
                if line.iter().all(u8::is_ascii_whitespace) {
                    continue;
                }
                dispatch_incoming(&line, &machine, &pending);
            }
            Err(err) => break Some(err),
        }
    };

    alive.store(false, Ordering::Release);
    {
        let mut guard = stdin.lock().await;
        guard.take();
    }

    let (exit_code, stderr_text) = {
        let mut pipe_guard = pipe.lock().await;
        if let Some(h) = pipe_guard.as_mut() {
            h.collect_exit_info(EXIT_COLLECT_WAIT).await
        } else {
            let text = stderr.as_ref().map_or_else(String::new, |s| s.text());
            (None, text)
        }
    };

    let reason = match (read_err, exit_code) {
        (Some(err), _) => format!("read error: {err}"),
        (None, Some(code)) => format!("exit status {code}"),
        (None, None) => "connection closed".to_owned(),
    };
    let mut err = ErrorBody::new(
        ErrorCode::Unreachable,
        format!(
            "The session to {machine} ended while the call was running ({reason}); the outcome of the call is unknown."
        ),
    )
    .with_machine(&machine);
    if !stderr_text.is_empty() {
        err = err.with_hint(format!("Remote stderr: {stderr_text}"));
    } else {
        err = err.with_hint(format!(
            "Retry the call once {machine} is reachable again; check its state if the operation was not idempotent."
        ));
    }

    *death_error
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(err.clone());

    let stranded: Vec<PendingCall> = {
        let mut map = pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        map.drain().map(|(_, call)| call).collect()
    };
    for call in stranded {
        let _ = call.reply.send(Err(CallError::LostInFlight(err.clone())));
    }
}

fn dispatch_incoming(line: &[u8], machine: &str, pending: &Arc<Mutex<HashMap<Id, PendingCall>>>) {
    let Ok(value) = serde_json::from_slice::<Value>(line) else {
        return;
    };
    // Notification (has `method`, no `id`).
    if value.get("method").is_some() && value.get("id").is_none() {
        if let Ok(notification) = serde_json::from_value::<Notification>(value)
            && notification.method == method::EVENT
            && let Ok(event) = serde_json::from_value::<EventParams>(notification.params)
        {
            let sink = {
                let map = pending
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                map.get(&event.id).map(|entry| entry.events.clone())
            };
            if let Some(sink) = sink {
                sink.emit_event(Event {
                    kind: event.kind,
                    data: event.data,
                    machine: event.machine.or_else(|| Some(machine.to_owned())),
                });
            }
        }
        return;
    }

    // Response (has `id` and `result` or `error`).
    if let Ok(response) = serde_json::from_value::<Response>(value)
        && let Some(id) = response.id
    {
        let entry = {
            let mut map = pending
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            map.remove(&id)
        };
        if let Some(entry) = entry {
            let result = match response.outcome {
                Outcome::Result(value) => Ok(value),
                Outcome::Error(rpc_err) => {
                    let mut body = rpc_err.data.unwrap_or_else(|| {
                        ErrorBody::new(ErrorCode::Failed, rpc_err.message.clone())
                    });
                    if body.machine.is_none() {
                        body = body.with_machine(machine);
                    }
                    Err(CallError::Remote(body))
                }
            };
            let _ = entry.reply.send(result);
        }
    }
}

enum Line {
    Read,
    TooLong,
    End,
}

async fn read_line<R: AsyncBufRead + Unpin>(
    input: &mut R,
    line: &mut Vec<u8>,
    longest: usize,
) -> io::Result<Line> {
    line.clear();
    let mut too_long = false;
    loop {
        let available = input.fill_buf().await?;
        if available.is_empty() {
            return Ok(if too_long {
                Line::TooLong
            } else if line.is_empty() {
                Line::End
            } else {
                Line::Read
            });
        }
        let (chunk, found_end) = match available.iter().position(|byte| *byte == b'\n') {
            Some(end) => (&available[..end], true),
            None => (available, false),
        };
        if !too_long && line.len() + chunk.len() <= longest {
            line.extend_from_slice(chunk);
        } else {
            too_long = true;
            line.clear();
        }
        let used = chunk.len() + usize::from(found_end);
        input.consume(used);
        if found_end {
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            return Ok(if too_long { Line::TooLong } else { Line::Read });
        }
    }
}

#[cfg(test)]
mod tests {
    use legion_proto::{EventKind, RpcError, TOOL_ERROR};
    use tokio::io::AsyncWriteExt;

    use super::*;

    fn hello_ok(node: &str, protocol: u32) -> String {
        let resp = Response::ok(
            Some(Id::Number(1)),
            serde_json::to_value(HelloResult {
                node: node.to_owned(),
                version: "0.1.0".to_owned(),
                protocol,
                os: "linux".to_owned(),
                arch: "x86_64".to_owned(),
                systems: Vec::new(),
            })
            .unwrap(),
        );
        format!("{}\n", serde_json::to_string(&resp).unwrap())
    }

    #[tokio::test]
    async fn skips_noise_lines_before_hello_and_bounds_excessive_noise() {
        let (client, server) = tokio::io::duplex(64 * 1024);
        let (client_r, client_w) = tokio::io::split(client);
        let (server_r, mut server_w) = tokio::io::split(server);

        let server_task = tokio::spawn(async move {
            let mut lines = BufReader::new(server_r).lines();
            let first = lines.next_line().await.unwrap().unwrap();
            assert!(first.contains("\"method\":\"hello\""));
            server_w
                .write_all(b"Welcome to pi!\nLast login: yesterday\n{\"not\":\"jsonrpc\"}\n")
                .await
                .unwrap();
            server_w
                .write_all(hello_ok("pi", PROTOCOL).as_bytes())
                .await
                .unwrap();
            server_w.flush().await.unwrap();
            // Keep stream open until client closes.
            let _ = lines.next_line().await;
        });

        let session = Session::open_io(
            client_r,
            client_w,
            "pi",
            None,
            Duration::from_secs(5),
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(session.info().hello.node, "pi");
        session.close().await;
        server_task.await.unwrap();

        // Excessive noise lines fail with unreachable instead of looping forever.
        let (client, server) = tokio::io::duplex(64 * 1024);
        let (client_r, client_w) = tokio::io::split(client);
        let (server_r, mut server_w) = tokio::io::split(server);
        let noisy_server = tokio::spawn(async move {
            let mut lines = BufReader::new(server_r).lines();
            let _ = lines.next_line().await;
            for i in 0..(MAX_NOISE_LINES + 5) {
                if server_w
                    .write_all(format!("banner line {i}\n").as_bytes())
                    .await
                    .is_err()
                {
                    break;
                }
            }
            let _ = server_w.flush().await;
        });
        let err = Session::open_io(
            client_r,
            client_w,
            "pi",
            None,
            Duration::from_secs(5),
            CancellationToken::new(),
        )
        .await
        .err()
        .unwrap();
        assert_eq!(err.code, ErrorCode::Unreachable);
        assert_eq!(err.machine.as_deref(), Some("pi"));
        let _ = noisy_server.await;
    }

    #[tokio::test]
    async fn protocol_mismatch_fails_with_unsupported_naming_both_versions() {
        // Case 1: remote refuses hello with Unsupported error naming supported versions.
        let (client, server) = tokio::io::duplex(16 * 1024);
        let (client_r, client_w) = tokio::io::split(client);
        let (server_r, mut server_w) = tokio::io::split(server);
        tokio::spawn(async move {
            let mut lines = BufReader::new(server_r).lines();
            let _ = lines.next_line().await;
            let reply = Response::error(
                Some(Id::Number(1)),
                RpcError::new(
                    TOOL_ERROR,
                    ErrorBody::new(ErrorCode::Unsupported, "Protocol 99 only.")
                        .with_details(json!({"supported": [99]})),
                ),
            );
            let _ = server_w
                .write_all(format!("{}\n", serde_json::to_string(&reply).unwrap()).as_bytes())
                .await;
        });
        let err = Session::open_io(
            client_r,
            client_w,
            "tower",
            None,
            Duration::from_secs(5),
            CancellationToken::new(),
        )
        .await
        .err()
        .unwrap();
        assert_eq!(err.code, ErrorCode::Unsupported);
        assert_eq!(err.machine.as_deref(), Some("tower"));
        assert!(err.message.contains("99"), "{}", err.message);
        assert!(
            err.message.contains(&PROTOCOL.to_string()),
            "{}",
            err.message
        );

        // Case 2: remote returns HelloResult claiming a different protocol.
        let (client, server) = tokio::io::duplex(16 * 1024);
        let (client_r, client_w) = tokio::io::split(client);
        let (server_r, mut server_w) = tokio::io::split(server);
        tokio::spawn(async move {
            let mut lines = BufReader::new(server_r).lines();
            let _ = lines.next_line().await;
            let _ = server_w.write_all(hello_ok("tower", 42).as_bytes()).await;
        });
        let err = Session::open_io(
            client_r,
            client_w,
            "tower",
            None,
            Duration::from_secs(5),
            CancellationToken::new(),
        )
        .await
        .err()
        .unwrap();
        assert_eq!(err.code, ErrorCode::Unsupported);
        assert!(err.message.contains("42"), "{}", err.message);
        assert!(
            err.message.contains(&PROTOCOL.to_string()),
            "{}",
            err.message
        );
    }

    #[tokio::test]
    async fn multiplexes_calls_routes_events_and_fails_pending_calls_when_stream_dies() {
        let (client, server) = tokio::io::duplex(64 * 1024);
        let (client_r, client_w) = tokio::io::split(client);
        let (server_r, mut server_w) = tokio::io::split(server);

        let server_task = tokio::spawn(async move {
            let mut lines = BufReader::new(server_r).lines();
            let _ = lines.next_line().await.unwrap().unwrap();
            server_w
                .write_all(hello_ok("pi", PROTOCOL).as_bytes())
                .await
                .unwrap();

            // Read two calls, send event for call 1, reply to call 2, then drop the connection
            // while call 1 is still in flight.
            let line1 = lines.next_line().await.unwrap().unwrap();
            let req1: Request = serde_json::from_str(&line1).unwrap();
            let line2 = lines.next_line().await.unwrap().unwrap();
            let req2: Request = serde_json::from_str(&line2).unwrap();

            let ev = Notification::new(
                method::EVENT,
                serde_json::to_value(EventParams {
                    id: req1.id.clone().unwrap(),
                    kind: EventKind::Progress,
                    data: json!({"step": 1}),
                    machine: None,
                })
                .unwrap(),
            );
            server_w
                .write_all(format!("{}\n", serde_json::to_string(&ev).unwrap()).as_bytes())
                .await
                .unwrap();

            let reply2 = Response::ok(req2.id, json!({"fast": true}));
            server_w
                .write_all(format!("{}\n", serde_json::to_string(&reply2).unwrap()).as_bytes())
                .await
                .unwrap();
            server_w.flush().await.unwrap();
            // Drop server stream abruptly while call 1 is pending.
            drop(server_w);
        });

        let session = Arc::new(
            Session::open_io(
                client_r,
                client_w,
                "pi",
                None,
                Duration::from_secs(5),
                CancellationToken::new(),
            )
            .await
            .unwrap(),
        );

        let seen_events = Arc::new(Mutex::new(Vec::new()));
        let sink_events = Arc::clone(&seen_events);
        let s1 = Arc::clone(&session);
        let call1 = tokio::spawn(async move {
            s1.call(
                CallParams {
                    tool: "test.sleep".into(),
                    args: json!({"seconds": 10}),
                    as_principal: None,
                    on: None,
                    hops: 0,
                },
                CancellationToken::new(),
                EventSink::with_events(move |event| {
                    sink_events
                        .lock()
                        .unwrap()
                        .push((event.kind, event.data, event.machine));
                }),
            )
            .await
        });

        // Give call1 a moment to send first so ordering in the mock server is deterministic.
        tokio::time::sleep(Duration::from_millis(20)).await;
        let fast = session
            .call(
                CallParams {
                    tool: "meta.version".into(),
                    args: json!({}),
                    as_principal: None,
                    on: None,
                    hops: 0,
                },
                CancellationToken::new(),
                EventSink::discard(),
            )
            .await
            .unwrap();
        assert_eq!(fast, json!({"fast": true}));

        let lost = call1.await.unwrap().unwrap_err();
        assert!(matches!(lost, CallError::LostInFlight(_)), "{lost:?}");
        let body = lost.into_error();
        assert_eq!(body.code, ErrorCode::Unreachable);
        assert!(body.message.contains("unknown"), "{}", body.message);
        assert_eq!(body.machine.as_deref(), Some("pi"));
        assert_eq!(
            *seen_events.lock().unwrap(),
            vec![(
                EventKind::Progress,
                json!({"step": 1}),
                Some("pi".to_owned())
            )]
        );

        // Writing to the now-dead session never panics and returns NotWritten.
        let after = session
            .call(
                CallParams {
                    tool: "meta.version".into(),
                    args: json!({}),
                    as_principal: None,
                    on: None,
                    hops: 0,
                },
                CancellationToken::new(),
                EventSink::discard(),
            )
            .await
            .unwrap_err();
        assert!(matches!(after, CallError::NotWritten(_)), "{after:?}");
        server_task.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn cancel_forwards_to_remote_and_waits_through_cancel_grace() {
        // Case 1: remote finishes cleanup near the end of CANCEL_GRACE and returns a reply.
        let (client, server) = tokio::io::duplex(64 * 1024);
        let (client_r, client_w) = tokio::io::split(client);
        let (server_r, mut server_w) = tokio::io::split(server);

        let (called_tx, called_rx) = oneshot::channel();
        let server_task = tokio::spawn(async move {
            let mut lines = BufReader::new(server_r).lines();
            let _ = lines.next_line().await.unwrap().unwrap();
            server_w
                .write_all(hello_ok("pi", PROTOCOL).as_bytes())
                .await
                .unwrap();

            let call_line = lines.next_line().await.unwrap().unwrap();
            let call_req: Request = serde_json::from_str(&call_line).unwrap();
            let _ = called_tx.send(());

            let cancel_line = lines.next_line().await.unwrap().unwrap();
            let cancel_req: Request = serde_json::from_str(&cancel_line).unwrap();
            assert_eq!(cancel_req.method, method::CANCEL);
            let cancel_params: CancelParams = serde_json::from_value(cancel_req.params).unwrap();
            assert_eq!(cancel_params.id, call_req.id.clone().unwrap());

            let cancel_ack = Response::ok(cancel_req.id, json!({}));
            server_w
                .write_all(format!("{}\n", serde_json::to_string(&cancel_ack).unwrap()).as_bytes())
                .await
                .unwrap();
            server_w.flush().await.unwrap();

            // Simulate a remote tool that takes almost the full CANCEL_GRACE to clean up.
            tokio::time::sleep(CANCEL_GRACE - Duration::from_millis(200)).await;
            let reply = Response::ok(call_req.id, json!({"cleaned": true}));
            server_w
                .write_all(format!("{}\n", serde_json::to_string(&reply).unwrap()).as_bytes())
                .await
                .unwrap();
            server_w.flush().await.unwrap();
        });

        let session = Arc::new(
            Session::open_io(
                client_r,
                client_w,
                "pi",
                None,
                Duration::from_secs(5),
                CancellationToken::new(),
            )
            .await
            .unwrap(),
        );
        let cancel = CancellationToken::new();
        let s = Arc::clone(&session);
        let call_cancel = cancel.clone();
        let call_task = tokio::spawn(async move {
            s.call(
                CallParams {
                    tool: "test.sleep".into(),
                    args: json!({"seconds": 30}),
                    as_principal: None,
                    on: None,
                    hops: 1,
                },
                call_cancel,
                EventSink::discard(),
            )
            .await
        });

        called_rx.await.unwrap();
        cancel.cancel();
        let result = call_task.await.unwrap().unwrap();
        assert_eq!(result, json!({"cleaned": true}));
        session.close().await;
        server_task.await.unwrap();

        // Case 2: remote ignores cancel and never replies -> times out with Cancelled after
        // CANCEL_REPLY_TIMEOUT.
        let (client, server) = tokio::io::duplex(64 * 1024);
        let (client_r, client_w) = tokio::io::split(client);
        let (server_r, mut server_w) = tokio::io::split(server);

        let (called_tx, called_rx) = oneshot::channel();
        let hung_server = tokio::spawn(async move {
            let mut lines = BufReader::new(server_r).lines();
            let _ = lines.next_line().await.unwrap().unwrap();
            server_w
                .write_all(hello_ok("pi", PROTOCOL).as_bytes())
                .await
                .unwrap();
            let _ = lines.next_line().await.unwrap().unwrap();
            let _ = called_tx.send(());
            let cancel_line = lines.next_line().await.unwrap().unwrap();
            let cancel_req: Request = serde_json::from_str(&cancel_line).unwrap();
            assert_eq!(cancel_req.method, method::CANCEL);
            // Never reply; keep stream open until client closes.
            let _ = lines.next_line().await;
        });

        let session = Arc::new(
            Session::open_io(
                client_r,
                client_w,
                "pi",
                None,
                Duration::from_secs(5),
                CancellationToken::new(),
            )
            .await
            .unwrap(),
        );
        let cancel = CancellationToken::new();
        let s = Arc::clone(&session);
        let call_cancel = cancel.clone();
        let call_task = tokio::spawn(async move {
            s.call(
                CallParams {
                    tool: "test.sleep".into(),
                    args: json!({"seconds": 30}),
                    as_principal: None,
                    on: None,
                    hops: 1,
                },
                call_cancel,
                EventSink::discard(),
            )
            .await
        });

        called_rx.await.unwrap();
        let cancelled_at = tokio::time::Instant::now();
        cancel.cancel();
        let err = call_task.await.unwrap().unwrap_err().into_error();
        assert_eq!(err.code, ErrorCode::Cancelled);
        assert_eq!(err.machine.as_deref(), Some("pi"));
        assert!(cancelled_at.elapsed() >= CANCEL_REPLY_TIMEOUT);
        session.close().await;
        let _ = hung_server.await;
    }
}
