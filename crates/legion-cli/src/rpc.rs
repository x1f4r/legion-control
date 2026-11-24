//! `legion rpc`: newline-delimited JSON-RPC 2.0 over a byte stream, normally stdin and stdout.
//!
//! Calls run concurrently, each under its own id; `cancel` names the id of the call to stop. A line
//! that is not JSON, or not a request, is answered with an error and the session goes on. At the
//! end of the input the session waits for the calls still running, writes their replies and ends.

use std::collections::HashMap;
use std::io;
use std::sync::{Arc, Mutex};

use legion_core::proto::{
    CallParams, CancelParams, CancelResult, ErrorBody, ErrorCode, EventParams, HelloParams,
    INTERNAL_ERROR, INVALID_PARAMS, INVALID_REQUEST, Id, METHOD_NOT_FOUND, Notification,
    PARSE_ERROR, Request, Response, RpcError, TOOL_ERROR, ToolsResult, method,
};
use legion_core::{Call, CancellationToken, Door, EventSink, Node, Router};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::mpsc::{self, UnboundedSender};
use tokio::task::JoinSet;

/// The longest line read. JSON carries control, never bulk data, so a longer line is a client
/// gone wrong, and is refused rather than read into memory without end.
const LONGEST_LINE: usize = 16 * 1024 * 1024;

/// Serves one session until `input` ends and every call has replied.
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
    let (lines, mut outgoing) = mpsc::unbounded_channel::<String>();
    // One task owns the output, so replies and events from concurrent calls never interleave
    // within a line.
    let writer = tokio::spawn(async move {
        let mut output = output;
        while let Some(mut line) = outgoing.recv().await {
            line.push('\n');
            output.write_all(line.as_bytes()).await?;
            output.flush().await?;
        }
        Ok::<(), io::Error>(())
    });

    let session = Session {
        node,
        router,
        pinned,
        out: lines,
        running: Arc::new(Mutex::new(HashMap::new())),
    };
    let mut calls = JoinSet::new();
    let mut input = BufReader::new(input);
    let mut line = Vec::new();
    // Input that fails ends the session like input that ends, so the calls already running still
    // finish, reply and reach the audit log.
    let read_failure = loop {
        match read_line(&mut input, &mut line, LONGEST_LINE).await {
            Ok(Line::End) => break None,
            Ok(Line::TooLong) => session.reply(
                None,
                Err(RpcError::new(
                    PARSE_ERROR,
                    ErrorBody::new(
                        ErrorCode::InvalidArgs,
                        format!("A line longer than {LONGEST_LINE} bytes was skipped."),
                    )
                    .with_hint("Send bulk data with legion stream, not inside a request."),
                )),
            ),
            Ok(Line::Read) => session.handle(&line, &mut calls),
            Err(error) => break Some(error),
        }
    };
    while calls.join_next().await.is_some() {}
    drop(session);
    let written = writer
        .await
        .map_err(|error| io::Error::other(format!("the output task stopped: {error}")))?;
    match read_failure {
        Some(error) => Err(error),
        None => written,
    }
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
    out: UnboundedSender<String>,
    running: Arc<Mutex<HashMap<Id, CancellationToken>>>,
}

impl Session {
    fn send<T: Serialize>(&self, message: &T) {
        send(&self.out, message);
    }

    fn reply(&self, id: Option<Id>, result: Result<Value, RpcError>) {
        self.send(&response(id, result));
    }

    fn handle(&self, line: &[u8], calls: &mut JoinSet<()>) {
        if line.iter().all(u8::is_ascii_whitespace) {
            return;
        }
        let value: Value = match serde_json::from_slice(line) {
            Ok(value) => value,
            Err(error) => {
                return self.reply(
                    None,
                    Err(RpcError::new(
                        PARSE_ERROR,
                        ErrorBody::new(
                            ErrorCode::InvalidArgs,
                            format!("The line is not JSON: {error}."),
                        )
                        .with_hint("Send one JSON-RPC 2.0 request per line."),
                    )),
                );
            }
        };
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
                );
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
                self.reply(Some(id), result);
            }
            method::TOOLS => {
                let result = to_value(&ToolsResult {
                    tools: self.node.specs(),
                });
                self.reply(Some(id), result);
            }
            method::CANCEL => {
                let result = params::<CancelParams>(request.params).and_then(|params| {
                    self.cancel(&params.id);
                    to_value(&CancelResult {})
                });
                self.reply(Some(id), result);
            }
            method::CALL => match params::<CallParams>(request.params) {
                Ok(params) => self.start(id, params, calls),
                Err(error) => self.reply(Some(id), Err(error)),
            },
            other => self.reply(
                Some(id),
                Err(RpcError::new(
                    METHOD_NOT_FOUND,
                    ErrorBody::new(
                        ErrorCode::Unsupported,
                        format!("This node has no method called {other:?}."),
                    )
                    .with_hint("The methods are hello, tools, call and cancel."),
                )),
            ),
        }
    }

    /// Cancels the call with this id, if it is still running; a call that already replied has
    /// nothing left to cancel.
    fn cancel(&self, id: &Id) {
        let running = self
            .running
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(token) = running.get(id) {
            token.cancel();
        }
    }

    fn start(&self, id: Id, params: CallParams, calls: &mut JoinSet<()>) {
        let cancel = CancellationToken::new();
        {
            let mut running = self
                .running
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if running.contains_key(&id) {
                drop(running);
                return self.reply(
                    Some(id.clone()),
                    Err(RpcError::new(
                        INVALID_REQUEST,
                        ErrorBody::new(
                            ErrorCode::Conflict,
                            format!("A call with the id {id} is still running."),
                        )
                        .with_hint("Give every call in a session an id of its own."),
                    )),
                );
            }
            running.insert(id.clone(), cancel.clone());
        }

        let events = {
            let out = self.out.clone();
            let id = id.clone();
            EventSink::new(move |kind, data| {
                let params = EventParams {
                    id: id.clone(),
                    kind,
                    data,
                };
                if let Ok(params) = serde_json::to_value(params) {
                    send(&out, &Notification::new(method::EVENT, params));
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
        };
        let node = self.node.clone();
        let router = Arc::clone(&self.router);
        let out = self.out.clone();
        let running = Arc::clone(&self.running);
        calls.spawn(async move {
            let result = router.call(&node, call, params.on).await;
            running
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .remove(&id);
            let result = result.map_err(|error| RpcError::new(TOOL_ERROR, error));
            send(&out, &response(Some(id), result));
        });
    }
}

fn response(id: Option<Id>, result: Result<Value, RpcError>) -> Response {
    match result {
        Ok(value) => Response::ok(id, value),
        Err(error) => Response::error(id, error),
    }
}

/// Queues one message for the output. A session whose output has gone has nobody to tell, so a
/// failed send is dropped.
fn send<T: Serialize>(out: &UnboundedSender<String>, message: &T) {
    if let Ok(line) = serde_json::to_string(message) {
        let _ = out.send(line);
    }
}

fn params<T: DeserializeOwned>(params: Value) -> Result<T, RpcError> {
    let params = if params.is_null() {
        Value::Object(serde_json::Map::new())
    } else {
        params
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
    use super::*;

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
