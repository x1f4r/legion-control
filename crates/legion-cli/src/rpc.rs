//! `legion rpc`: newline-delimited JSON-RPC 2.0 over a byte stream, normally stdin and stdout.
//!
//! Calls run concurrently, each under its own id, up to [`MOST_CALLS`] at once; `cancel` names the
//! id of the call to stop. A line that is not JSON, or not a request, is answered with an error and
//! the session goes on. At the end of the input the session waits for the calls still running,
//! writes their replies and ends. When the output goes away there is nobody left to answer, so the
//! session cancels every call still running and ends once they have stopped.

use std::collections::HashMap;
use std::io;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use legion_core::proto::{
    CallParams, CancelParams, CancelResult, ErrorBody, ErrorCode, EventParams, HelloParams,
    INTERNAL_ERROR, INVALID_PARAMS, INVALID_REQUEST, Id, METHOD_NOT_FOUND, Notification,
    PARSE_ERROR, Request, Response, RpcError, TOOL_ERROR, ToolsParams, ToolsResult, method,
};
use legion_core::{CANCEL_GRACE, Call, CancellationToken, Door, EventSink, Node, Router};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::mpsc::{self, Sender};
use tokio::task::{self, JoinError, JoinSet};

/// The longest line read. JSON carries control, never bulk data, so a longer line is a client
/// gone wrong, and is refused rather than read into memory without end.
const LONGEST_LINE: usize = 16 * 1024 * 1024;

/// The most calls one session runs at once. One more is refused as `busy` rather than queued, so a
/// client that floods the node hears so at once instead of finding its calls waiting unseen.
const MOST_CALLS: usize = 256;

/// The most messages waiting to be written. A reply waits for room, which slows a client that does
/// not read what it is sent; an event that finds none is dropped, since events are a courtesy and
/// a result says everything on its own.
const QUEUED: usize = 256;

/// Bounded wait for cancelled calls to stop and audit when a session is signalled to shut down.
const SHUTDOWN_GRACE: Duration = CANCEL_GRACE.saturating_add(Duration::from_secs(4));

/// Serves one session until `input` ends and every call has replied, or until `output` fails and
/// every call has stopped.
#[cfg_attr(not(test), allow(dead_code))]
pub async fn serve<R, W>(
    input: R,
    output: W,
    node: Node,
    router: Arc<dyn Router>,
    pinned: Option<String>,
) -> io::Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin + Send + 'static,
{
    serve_until(
        input,
        output,
        node,
        router,
        pinned,
        CancellationToken::new(),
    )
    .await
}

/// Serves one session until `input` ends, `output` closes, or `shutdown` is cancelled.
///
/// When `shutdown` fires, every running call is cancelled, drained within [`SHUTDOWN_GRACE`], and
/// the router is shut down cleanly before returning.
pub async fn serve_until<R, W>(
    input: R,
    output: W,
    node: Node,
    router: Arc<dyn Router>,
    pinned: Option<String>,
    shutdown: CancellationToken,
) -> io::Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let (lines, mut outgoing) = mpsc::channel::<String>(QUEUED);
    // One task owns the output, so replies and events from concurrent calls never interleave
    // within a line. When writing fails it ends, and the session sees its queue close.
    let writer = tokio::spawn(async move {
        let mut output = output;
        while let Some(mut line) = outgoing.recv().await {
            line.push('\n');
            output.write_all(line.as_bytes()).await?;
            output.flush().await?;
        }
        Ok::<(), io::Error>(())
    });

    let mut session = Session {
        node,
        router,
        pinned,
        out: lines,
        running: Arc::new(Mutex::new(HashMap::new())),
        calls: JoinSet::new(),
        tasks: HashMap::new(),
    };
    let mut input = BufReader::new(input);
    let mut line = Vec::new();
    // Input that fails ends the session like input that ends, so the calls already running still
    // finish, reply and reach the audit log.
    let ended = loop {
        let next = {
            // Kept across the turns of the inner loop: a read dropped half way would lose what it
            // had read of its line.
            let read = read_line(&mut input, &mut line, LONGEST_LINE);
            tokio::pin!(read);
            loop {
                tokio::select! {
                    biased;
                    () = shutdown.cancelled() => break ReadStep::Signalled,
                    () = session.out.closed() => break ReadStep::OutputGone,
                    Some(joined) = session.calls.join_next_with_id() => session.joined(joined).await,
                    read = &mut read => break ReadStep::Line(read),
                }
            }
        };
        match next {
            ReadStep::Signalled => break Ended::Signalled,
            ReadStep::OutputGone => break Ended::OutputGone,
            ReadStep::Line(Ok(Line::End)) => break Ended::Input(None),
            ReadStep::Line(Err(error)) => break Ended::Input(Some(error)),
            ReadStep::Line(Ok(Line::TooLong)) => {
                session
                    .reply(
                        None,
                        Err(RpcError::new(
                            PARSE_ERROR,
                            ErrorBody::new(
                                ErrorCode::InvalidArgs,
                                format!("A line longer than {LONGEST_LINE} bytes was skipped."),
                            )
                            .with_hint("Send bulk data with legion stream, not inside a request."),
                        )),
                    )
                    .await;
            }
            ReadStep::Line(Ok(Line::Read)) => session.handle(&line).await,
        }
    };
    let mut cancelled = matches!(ended, Ended::OutputGone | Ended::Signalled);
    let mut deadline =
        matches!(ended, Ended::Signalled).then(|| Box::pin(tokio::time::sleep(SHUTDOWN_GRACE)));
    if cancelled {
        session.cancel_all();
    }
    loop {
        tokio::select! {
            biased;
            () = shutdown.cancelled(), if deadline.is_none() => {
                if !cancelled {
                    session.cancel_all();
                    cancelled = true;
                }
                deadline = Some(Box::pin(tokio::time::sleep(SHUTDOWN_GRACE)));
            }
            () = session.out.closed(), if !cancelled => {
                session.cancel_all();
                cancelled = true;
            }
            () = async {
                match deadline.as_mut() {
                    Some(sleep) => sleep.as_mut().await,
                    None => std::future::pending().await,
                }
            }, if deadline.is_some() => {
                session.calls.abort_all();
                break;
            }
            joined = session.calls.join_next_with_id() => match joined {
                Some(joined) => session.joined(joined).await,
                None => break,
            },
        }
    }
    session.router.shutdown().await;
    drop(session);
    let written = writer
        .await
        .map_err(|error| io::Error::other(format!("the output task stopped: {error}")))?;
    match ended {
        Ended::Input(Some(error)) => Err(error),
        Ended::Input(None) | Ended::OutputGone | Ended::Signalled => written,
    }
}

enum ReadStep {
    Signalled,
    OutputGone,
    Line(io::Result<Line>),
}

/// Why a session stopped reading.
enum Ended {
    /// The input ended, or failed with this error.
    Input(Option<io::Error>),
    /// Writing to the output failed.
    OutputGone,
    /// A termination signal asked the process to shut down.
    Signalled,
}

enum Line {
    Read,
    TooLong,
    End,
}

/// Reads one line into `line`, without its ending. A line longer than `longest` is skipped to its
/// end and reported as too long.
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

struct Session {
    node: Node,
    router: Arc<dyn Router>,
    pinned: Option<String>,
    out: Sender<String>,
    running: Arc<Mutex<HashMap<Id, CancellationToken>>>,
    calls: JoinSet<()>,
    /// The id of the call each task runs, so a task that panicked can still be answered.
    tasks: HashMap<task::Id, Id>,
}

impl Session {
    async fn reply(&self, id: Option<Id>, result: Result<Value, RpcError>) {
        send(&self.out, &response(id, result)).await;
    }

    async fn handle(&mut self, line: &[u8]) {
        if line.iter().all(u8::is_ascii_whitespace) {
            return;
        }
        let value: Value = match serde_json::from_slice(line) {
            Ok(value) => value,
            Err(error) => {
                return self
                    .reply(
                        None,
                        Err(RpcError::new(
                            PARSE_ERROR,
                            ErrorBody::new(
                                ErrorCode::InvalidArgs,
                                format!("The line is not JSON: {error}."),
                            )
                            .with_hint("Send one JSON-RPC 2.0 request per line."),
                        )),
                    )
                    .await;
            }
        };
        // Read as absent it would make the request a notification, which is never answered, and
        // the client would wait for its reply for ever.
        if value.get("id").is_some_and(Value::is_null) {
            return self
                .reply(
                    None,
                    Err(RpcError::new(
                        INVALID_REQUEST,
                        ErrorBody::new(
                            ErrorCode::InvalidArgs,
                            "The request's id is null, which no reply could be matched to.",
                        )
                        .with_hint("Give the request a number or a string as its id, or leave id out of a notification, which gets no reply."),
                    )),
                )
                .await;
        }
        let id = value
            .get("id")
            .and_then(|id| serde_json::from_value::<Id>(id.clone()).ok());
        let request: Request = match serde_json::from_value(value) {
            Ok(request) => request,
            Err(error) => {
                return self.reply(
                    id,
                    Err(RpcError::new(
                        INVALID_REQUEST,
                        ErrorBody::new(ErrorCode::InvalidArgs, format!("The line is not a JSON-RPC 2.0 request: {error}."))
                            .with_hint("A request is {\"jsonrpc\": \"2.0\", \"id\": 1, \"method\": \"call\", \"params\": {…}}."),
                    )),
                ).await;
            }
        };

        let Some(id) = request.id else {
            // A notification gets no reply, so only one that needs none is acted on.
            if request.method == method::CANCEL
                && let Ok(params) = params::<CancelParams>(request.params)
            {
                self.cancel(&params.id);
            }
            return;
        };
        match request.method.as_str() {
            method::HELLO => {
                let result = params::<HelloParams>(request.params).and_then(|params| {
                    self.node
                        .hello(&params)
                        .map_err(|error| RpcError::new(TOOL_ERROR, error))
                        .and_then(|hello| to_value(&hello))
                });
                self.reply(Some(id), result).await;
            }
            method::TOOLS => {
                let result = params::<ToolsParams>(request.params).and_then(|_| {
                    to_value(&ToolsResult {
                        tools: self.node.specs(),
                    })
                });
                self.reply(Some(id), result).await;
            }
            method::CANCEL => {
                let result = params::<CancelParams>(request.params).and_then(|params| {
                    self.cancel(&params.id);
                    to_value(&CancelResult {})
                });
                self.reply(Some(id), result).await;
            }
            method::CALL => match params::<CallParams>(request.params) {
                Ok(params) => self.start(id, params).await,
                Err(error) => self.reply(Some(id), Err(error)).await,
            },
            other => {
                self.reply(
                    Some(id),
                    Err(RpcError::new(
                        METHOD_NOT_FOUND,
                        ErrorBody::new(
                            ErrorCode::Unsupported,
                            format!("This node has no method called {other:?}."),
                        )
                        .with_hint("The methods are hello, tools, call and cancel."),
                    )),
                )
                .await;
            }
        }
    }

    fn running(&self) -> std::sync::MutexGuard<'_, HashMap<Id, CancellationToken>> {
        // A poisoned lock means a thread panicked holding it; the map is still a plain map.
        self.running
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Cancels the call with this id, if it is still running; a call that already replied has
    /// nothing left to cancel.
    fn cancel(&self, id: &Id) {
        if let Some(token) = self.running().get(id) {
            token.cancel();
        }
    }

    fn cancel_all(&self) {
        for token in self.running().values() {
            token.cancel();
        }
    }

    async fn start(&mut self, id: Id, params: CallParams) {
        if self.calls.len() >= MOST_CALLS {
            return self
                .reply(
                    Some(id),
                    Err(RpcError::new(
                        TOOL_ERROR,
                        ErrorBody::new(
                            ErrorCode::Busy,
                            format!("This session is already running {MOST_CALLS} calls."),
                        )
                        .with_hint("Wait for one of them to reply, or open another session."),
                    )),
                )
                .await;
        }
        let cancel = CancellationToken::new();
        let taken = {
            let mut running = self.running();
            let taken = running.contains_key(&id);
            if !taken {
                running.insert(id.clone(), cancel.clone());
            }
            taken
        };
        if taken {
            return self
                .reply(
                    Some(id.clone()),
                    Err(RpcError::new(
                        INVALID_REQUEST,
                        ErrorBody::new(
                            ErrorCode::Conflict,
                            format!("A call with the id {id} is still running."),
                        )
                        .with_hint("Give every call in a session an id of its own."),
                    )),
                )
                .await;
        }

        let events = {
            let out = self.out.clone();
            let id = id.clone();
            EventSink::with_events(move |event| {
                let params = EventParams {
                    id: id.clone(),
                    kind: event.kind,
                    data: event.data,
                    machine: event.machine,
                };
                if let Ok(params) = serde_json::to_value(params)
                    && let Ok(line) =
                        serde_json::to_string(&Notification::new(method::EVENT, params))
                {
                    // Dropped when the queue is full: an event is a courtesy, and waiting for room
                    // here would stall the tool that emitted it.
                    let _ = out.try_send(line);
                }
            })
        };
        let call = Call {
            tool: params.tool,
            args: params.args,
            as_principal: params.as_principal,
            door: Door::Rpc,
            pinned: self.pinned.clone(),
            cancel,
            events,
            hops: params.hops,
            credential_expires: None,
        };
        let node = self.node.clone();
        let router = Arc::clone(&self.router);
        let out = self.out.clone();
        let running = Arc::clone(&self.running);
        let reply_id = id.clone();
        let task = self.calls.spawn(async move {
            let result = router.call(&node, call, params.on).await.into_wire();
            running
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .remove(&reply_id);
            let result = result.map_err(|error| RpcError::new(TOOL_ERROR, error));
            send(&out, &response(Some(reply_id), result)).await;
        });
        self.tasks.insert(task.id(), id);
    }

    /// Takes note of a call that has ended. One that replied has nothing left to do; one that
    /// panicked never did, so it is answered here and its id is freed for the next call.
    async fn joined(&mut self, joined: Result<(task::Id, ()), JoinError>) {
        let (task, failure) = match joined {
            Ok((task, ())) => (task, None),
            Err(error) => (error.id(), Some(error)),
        };
        let Some(id) = self.tasks.remove(&task) else {
            return;
        };
        let Some(failure) = failure else {
            return;
        };
        self.running().remove(&id);
        let message = if failure.is_panic() {
            "The call failed inside legion before it could reply."
        } else {
            "The call was stopped before it could reply."
        };
        self.reply(
            Some(id),
            Err(RpcError::new(
                INTERNAL_ERROR,
                ErrorBody::new(ErrorCode::Internal, message)
                    .with_hint("This is a fault in legion; what it wrote on stderr says where."),
            )),
        )
        .await;
    }
}

fn response(id: Option<Id>, result: Result<Value, RpcError>) -> Response {
    match result {
        Ok(value) => Response::ok(id, value),
        Err(error) => Response::error(id, error),
    }
}

/// Queues one message for the output, waiting for room. A session whose output has gone has
/// nobody to tell, so a failed send is dropped.
async fn send<T: Serialize>(out: &Sender<String>, message: &T) {
    if let Ok(line) = serde_json::to_string(message) {
        let _ = out.send(line).await;
    }
}

/// A method's params: absent means `{}`, and anything but an object is refused, since a struct
/// would otherwise take an array as its fields in order.
fn params<T: DeserializeOwned>(params: Value) -> Result<T, RpcError> {
    let params = match params {
        Value::Null => Value::Object(serde_json::Map::new()),
        Value::Object(_) => params,
        other => {
            return Err(RpcError::new(
                INVALID_PARAMS,
                ErrorBody::new(
                    ErrorCode::InvalidArgs,
                    format!(
                        "The params are {}, and every method takes an object.",
                        match other {
                            Value::Bool(_) => "a boolean",
                            Value::Number(_) => "a number",
                            Value::String(_) => "a string",
                            _ => "an array",
                        }
                    ),
                )
                .with_hint("Send params as a JSON object, such as {\"tool\": \"meta.version\"}, or leave them out."),
            ));
        }
    };
    serde_json::from_value(params).map_err(|error| {
        RpcError::new(
            INVALID_PARAMS,
            ErrorBody::new(
                ErrorCode::InvalidArgs,
                format!("The params do not fit the method: {error}."),
            ),
        )
    })
}

fn to_value<T: Serialize>(value: &T) -> Result<Value, RpcError> {
    serde_json::to_value(value).map_err(|error| {
        RpcError::new(
            INTERNAL_ERROR,
            ErrorBody::new(
                ErrorCode::Internal,
                format!("The reply cannot be written as JSON: {error}."),
            ),
        )
    })
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::time::Duration;

    use legion_core::proto::{Effect, EventKind};
    use legion_core::{Ctx, Paths, Registry, ToolError};
    use serde_json::{Map, json};
    use tokio::io::{AsyncReadExt, DuplexStream};
    use tokio::task::JoinHandle;

    use super::*;
    use legion_core::Local;

    type Args = Map<String, Value>;

    async fn echo(_ctx: &Ctx, args: Args) -> Result<Args, ToolError> {
        Ok(args)
    }

    async fn panics(_ctx: &Ctx, _args: Args) -> Result<Args, ToolError> {
        panic!("t.panic panics on purpose");
    }

    /// Waits without a word until it is cancelled.
    async fn wait(ctx: &Ctx, _args: Args) -> Result<Args, ToolError> {
        ctx.cancelled().await;
        Err(ErrorBody::new(
            ErrorCode::Cancelled,
            "t.wait was cancelled.",
        ))
    }

    /// Reports progress every few milliseconds until it is cancelled.
    async fn chatter(ctx: &Ctx, _args: Args) -> Result<Args, ToolError> {
        loop {
            ctx.emit(EventKind::Progress, json!({}));
            let tick = tokio::time::timeout(Duration::from_millis(5), ctx.cancelled()).await;
            if tick.is_ok() {
                return Err(ErrorBody::new(
                    ErrorCode::Cancelled,
                    "t.chatter was cancelled.",
                ));
            }
        }
    }

    /// A session served in this process over in-memory pipes, on a node with tools of its own.
    struct Client {
        dir: tempfile::TempDir,
        input: Option<DuplexStream>,
        output: Option<BufReader<DuplexStream>>,
        session: JoinHandle<io::Result<()>>,
    }

    impl Client {
        fn start() -> Client {
            let dir = tempfile::tempdir().unwrap();
            let mut registry = Registry::new();
            registry.add_fn("t.echo", Effect::Read, "Echoes.", echo);
            registry.add_fn("t.panic", Effect::Read, "Panics.", panics);
            registry.add_fn("t.wait", Effect::Read, "Waits.", wait);
            registry.add_fn("t.chatter", Effect::Read, "Chatters.", chatter);
            let node = Node::new(Paths::under(dir.path()), registry);
            let (input, session_input) = tokio::io::duplex(1 << 16);
            let (session_output, output) = tokio::io::duplex(1 << 16);
            let session = tokio::spawn(serve(
                session_input,
                session_output,
                node,
                Arc::new(Local),
                None,
            ));
            Client {
                dir,
                input: Some(input),
                output: Some(BufReader::new(output)),
                session,
            }
        }

        async fn send(&mut self, message: Value) {
            let input = self.input.as_mut().unwrap();
            let mut line = message.to_string();
            line.push('\n');
            input.write_all(line.as_bytes()).await.unwrap();
        }

        async fn call(&mut self, id: i64, tool: &str) {
            self.send(
                json!({"jsonrpc": "2.0", "id": id, "method": "call", "params": {"tool": tool}}),
            )
            .await;
        }

        async fn next(&mut self) -> Value {
            let output = self.output.as_mut().unwrap();
            let mut line = String::new();
            tokio::time::timeout(Duration::from_secs(20), output.read_line(&mut line))
                .await
                .expect("no line from the session")
                .unwrap();
            serde_json::from_str(&line).unwrap()
        }

        async fn reply_to(&mut self, id: i64) -> Value {
            loop {
                let message = self.next().await;
                if message.get("method").is_none() && message["id"] == json!(id) {
                    return message;
                }
            }
        }

        /// Ends the input and waits for the session to end with it.
        async fn finish(mut self) -> io::Result<()> {
            drop(self.input.take());
            let mut output = self.output.take().unwrap();
            let draining = tokio::spawn(async move {
                let mut rest = Vec::new();
                let _ = output.read_to_end(&mut rest).await;
            });
            let ended = tokio::time::timeout(Duration::from_secs(20), self.session)
                .await
                .expect("the session did not end with its input")
                .unwrap();
            draining.await.unwrap();
            ended
        }

        fn audit(&self) -> Vec<Value> {
            let log: PathBuf = self.dir.path().join("state").join("audit.jsonl");
            std::fs::read_to_string(log)
                .unwrap_or_default()
                .lines()
                .map(|line| serde_json::from_str(line).unwrap())
                .collect()
        }
    }

    #[tokio::test]
    async fn a_call_that_panics_is_answered_and_its_id_can_be_used_again() {
        let mut client = Client::start();
        client.call(1, "t.panic").await;
        let reply = client.reply_to(1).await;
        assert_eq!(reply["error"]["code"], json!(INTERNAL_ERROR));
        assert_eq!(reply["error"]["data"]["code"], json!("internal"));
        client.call(1, "t.echo").await;
        assert_eq!(client.reply_to(1).await["result"], json!({}));
        client.finish().await.unwrap();
    }

    #[tokio::test]
    async fn calls_beyond_the_limit_are_busy_and_finished_ones_make_room() {
        let mut client = Client::start();
        let most = i64::try_from(MOST_CALLS).unwrap();
        // Calls that have ended are drained as they end, so however many there were, they never
        // count against the ones running.
        for id in 0..most + 10 {
            client.call(id, "t.echo").await;
            assert_eq!(client.reply_to(id).await["result"], json!({}));
        }
        for id in 0..most {
            client.call(id, "t.wait").await;
        }
        client.call(most, "t.echo").await;
        let busy = client.reply_to(most).await;
        assert_eq!(busy["error"]["data"]["code"], json!("busy"), "{busy}");
        client
            .send(json!({"jsonrpc": "2.0", "method": "cancel", "params": {"id": 0}}))
            .await;
        assert_eq!(
            client.reply_to(0).await["error"]["data"]["code"],
            json!("cancelled")
        );
        client.call(most, "t.echo").await;
        assert_eq!(client.reply_to(most).await["result"], json!({}));
        for id in 1..most {
            client
                .send(json!({"jsonrpc": "2.0", "method": "cancel", "params": {"id": id}}))
                .await;
        }
        client.finish().await.unwrap();
    }

    #[tokio::test]
    async fn when_the_output_goes_the_running_calls_are_cancelled_and_the_session_ends() {
        let mut client = Client::start();
        client.call(1, "t.chatter").await;
        assert_eq!(client.next().await["method"], json!("event"));
        drop(client.output.take());
        // The input stays open: the session must not wait for it.
        let ended = tokio::time::timeout(Duration::from_secs(20), &mut client.session)
            .await
            .expect("the session went on after its output had gone")
            .unwrap();
        assert!(ended.is_err());
        let audit = client.audit();
        assert_eq!(audit.len(), 1);
        assert_eq!(audit[0]["outcome"], json!("cancelled"));
    }

    #[tokio::test]
    async fn shutdown_token_cancels_running_calls_and_ends_session() {
        let dir = tempfile::tempdir().unwrap();
        let mut registry = Registry::new();
        registry.add_fn("t.chatter", Effect::Read, "Chatters.", chatter);
        let node = Node::new(Paths::under(dir.path()), registry);
        let (mut input, session_input) = tokio::io::duplex(1 << 16);
        let (session_output, output) = tokio::io::duplex(1 << 16);
        let shutdown = CancellationToken::new();
        let session = tokio::spawn(serve_until(
            session_input,
            session_output,
            node,
            Arc::new(Local),
            None,
            shutdown.clone(),
        ));
        let mut output = BufReader::new(output);
        let req =
            json!({"jsonrpc": "2.0", "id": 1, "method": "call", "params": {"tool": "t.chatter"}});
        input
            .write_all(format!("{req}\n").as_bytes())
            .await
            .unwrap();
        let mut first = String::new();
        tokio::time::timeout(Duration::from_secs(10), output.read_line(&mut first))
            .await
            .unwrap()
            .unwrap();
        let ev: Value = serde_json::from_str(&first).unwrap();
        assert_eq!(ev["method"], json!("event"));

        // Signal shutdown while stdin and stdout remain open.
        shutdown.cancel();
        let ended = tokio::time::timeout(Duration::from_secs(10), session)
            .await
            .expect("session did not end after shutdown signal")
            .unwrap();
        assert!(ended.is_ok(), "{ended:?}");
        let log: PathBuf = dir.path().join("state").join("audit.jsonl");
        let audit: Vec<Value> = std::fs::read_to_string(log)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(audit.len(), 1);
        assert_eq!(audit[0]["outcome"], json!("cancelled"));
    }

    async fn lines(input: &[u8], longest: usize) -> Vec<Result<Vec<u8>, &'static str>> {
        let mut reader = BufReader::with_capacity(4, input);
        let mut line = Vec::new();
        let mut found = Vec::new();
        loop {
            match read_line(&mut reader, &mut line, longest).await.unwrap() {
                Line::End => return found,
                Line::TooLong => found.push(Err("too long")),
                Line::Read => found.push(Ok(line.clone())),
            }
        }
    }

    #[tokio::test]
    async fn lines_end_at_newlines_with_or_without_a_carriage_return() {
        let found = lines(b"one\r\ntwo\n\nlast", 100).await;
        assert_eq!(
            found,
            vec![
                Ok(b"one".to_vec()),
                Ok(b"two".to_vec()),
                Ok(Vec::new()),
                Ok(b"last".to_vec())
            ]
        );
    }

    #[tokio::test]
    async fn a_line_too_long_is_skipped_to_its_end_and_the_next_one_read() {
        let found = lines(b"short\nfar too long a line\nok\nunfinished and long", 8).await;
        assert_eq!(
            found,
            vec![
                Ok(b"short".to_vec()),
                Err("too long"),
                Ok(b"ok".to_vec()),
                Err("too long")
            ]
        );
    }
}
