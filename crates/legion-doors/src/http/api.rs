//! The JSON API: `GET /v1/health`, `GET /v1/tools` and `POST /v1/call`.

use std::convert::Infallible;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use axum::body::{Body, Bytes};
use axum::extract::{Request, State};
use axum::http::header::{ACCEPT, CACHE_CONTROL, CONTENT_TYPE, HeaderValue};
use axum::http::{HeaderMap, Response, StatusCode};
use http_body::Frame;
use legion_core::proto::{ErrorBody, ErrorCode, ToolsResult};
use legion_core::{Call, Door, Event, EventSink, RouteOutcome, VERSION};
use serde::Deserialize;
use serde_json::{Map, Value, json};
use tokio::sync::mpsc;
use tokio_util::sync::DropGuard;

use super::{KEEP_ALIVE, Shared, read_body, reply};
use crate::mcp::catalog;
use crate::{Authenticated, too_many_calls};

/// Events waiting to be written to an event stream. A tool producing output faster than the
/// client reads it loses some of it rather than the server's memory; the result still says
/// everything the call did.
const QUEUE: usize = 256;

/// `GET /v1/health`: that the server is up, and nothing a stranger could use.
pub(crate) async fn health(State(shared): State<Arc<Shared>>) -> Response<Body> {
    reply::json(
        StatusCode::OK,
        &json!({"ok": true, "node": shared.node.name(), "version": VERSION}),
    )
}

/// `GET /v1/tools`: the tools the token's principal may use. A policy that cannot be read lists
/// nothing and says why, as every door fails closed.
pub(crate) async fn tools(State(shared): State<Arc<Shared>>, request: Request) -> Response<Body> {
    let Some(Authenticated { principal, .. }) = request.extensions().get::<Authenticated>() else {
        return reply::error(&no_principal());
    };
    match crate::grant(&shared.node, Some(principal)) {
        Ok(grant) => {
            let tools = shared
                .node
                .specs()
                .into_iter()
                .filter(|spec| grant.allows(spec))
                .collect();
            reply::json(StatusCode::OK, &ToolsResult { tools })
        }
        Err(error) => reply::error(&error),
    }
}

/// The body of `POST /v1/call`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CallBody {
    tool: String,
    #[serde(default)]
    args: Option<Map<String, Value>>,
    #[serde(default)]
    on: Option<Value>,
    /// Honoured only when the token's role may delegate; the node decides.
    #[serde(default, rename = "as")]
    as_principal: Option<String>,
}

/// `POST /v1/call`: runs one call as the token's principal and answers with its result, or, when
/// the client accepts `text/event-stream`, streams its events and then its result.
///
/// The call runs on a task of its own, so it still ends in the audit log when the client goes
/// away; going away cancels it.
pub(crate) async fn call(State(shared): State<Arc<Shared>>, request: Request) -> Response<Body> {
    let Some(Authenticated {
        principal, expires, ..
    }) = request.extensions().get::<Authenticated>().cloned()
    else {
        return reply::error(&no_principal());
    };
    let streaming = accepts_events(request.headers());
    let bytes = match read_body(request.into_body()).await {
        Ok(bytes) => bytes,
        Err(response) => return *response,
    };
    let body: CallBody = match serde_json::from_slice(&bytes) {
        Ok(body) => body,
        Err(error) => {
            return reply::error(
                &ErrorBody::new(
                    ErrorCode::InvalidArgs,
                    format!("The request body is not a call: {error}."),
                )
                .with_hint(
                    "Send a JSON object such as {\"tool\": \"meta.whoami\", \"args\": {}}, with on to choose machines.",
                ),
            );
        }
    };
    let on = match catalog::target(body.on.unwrap_or(Value::Null)) {
        Ok(on) => on,
        Err(error) => {
            return reply::error(
                &ErrorBody::new(ErrorCode::InvalidArgs, format!("on: {error}."))
                    .with_hint("Give on as a machine's name, several names separated by commas or as a list, or \"all\", or leave it out to run on this machine."),
            );
        }
    };
    let Ok(permit) = Arc::clone(&shared.link.limit).try_acquire_owned() else {
        return reply::json(
            StatusCode::SERVICE_UNAVAILABLE,
            &too_many_calls(&shared.node),
        );
    };

    let cancel = shared.link.shutdown.child_token();
    let (events, queue) = if streaming {
        let (sender, queue) = mpsc::channel(QUEUE);
        let sink = EventSink::with_events(move |event| {
            let _ = sender.try_send(event);
        });
        (sink, Some(queue))
    } else {
        (EventSink::discard(), None)
    };
    let call = Call {
        tool: body.tool,
        args: Value::Object(body.args.unwrap_or_default()),
        as_principal: body.as_principal,
        door: Door::Http,
        pinned: Some(principal),
        cancel: cancel.clone(),
        events,
        hops: 0,
        credential_expires: expires,
    };
    let node = shared.node.clone();
    let router = Arc::clone(&shared.router);
    let running = shared.calls.spawn(async move {
        let _permit = permit;
        router.call(&node, call, on).await
    });
    // Dropped with the handler or the stream when the client goes away, which cancels the call.
    let gone = cancel.drop_guard();

    let Some(mut queue) = queue else {
        let outcome = running.await.unwrap_or_else(|error| stopped(&error));
        gone.disarm();
        return reply::outcome(outcome);
    };

    let (frames, stream) = mpsc::channel::<Bytes>(QUEUE);
    shared.calls.spawn(async move {
        tokio::pin!(running);
        let mut quiet = tokio::time::interval(KEEP_ALIVE);
        quiet.reset();
        let outcome = loop {
            tokio::select! {
                outcome = &mut running => break outcome.unwrap_or_else(|error| stopped(&error)),
                _ = quiet.tick() => {
                    // A comment, which clients skip: writing it is how a vanished client is found.
                    let _ = frames.send(Bytes::from_static(b": keep-alive\n\n")).await;
                }
                Some(event) = queue.recv() => {
                    // A client that has gone away has nobody to tell; its stream's guard has
                    // cancelled the call, which ends this loop.
                    let _ = frames.send(event_frame(&event)).await;
                }
            }
        };
        while let Ok(event) = queue.try_recv() {
            let _ = frames.send(event_frame(&event)).await;
        }
        let _ = frames.send(final_frame(outcome)).await;
    });
    let mut response = Response::new(Body::new(EventStream {
        frames: stream,
        _gone: gone,
    }));
    let headers = response.headers_mut();
    headers.insert(CONTENT_TYPE, HeaderValue::from_static("text/event-stream"));
    headers.insert(CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    response
}

/// Whether the client asked for the call's events as they happen.
fn accepts_events(headers: &HeaderMap) -> bool {
    headers
        .get_all(ACCEPT)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .any(|item| {
            item.split(';')
                .next()
                .is_some_and(|kind| kind.trim().eq_ignore_ascii_case("text/event-stream"))
        })
}

/// One event as a server-sent event.
fn event_frame(event: &Event) -> Bytes {
    let data = match &event.machine {
        Some(machine) => {
            json!({"kind": event.kind.as_str(), "data": event.data, "machine": machine})
        }
        None => json!({"kind": event.kind.as_str(), "data": event.data}),
    };
    Bytes::from(format!("event: event\ndata: {data}\n\n"))
}

/// The call's end as a server-sent event: `result` with what the call returned, or `error` with
/// the error, or with a fan-out's results when every machine failed.
fn final_frame(outcome: RouteOutcome) -> Bytes {
    let outcome = reply::for_http(outcome);
    let failed = outcome.all_failed();
    let (name, data) = match outcome.into_wire() {
        Ok(value) if failed => ("error", value.to_string()),
        Ok(value) => ("result", value.to_string()),
        Err(error) => (
            "error",
            serde_json::to_string(&error).unwrap_or_else(|_| {
                json!({"code": "internal", "message": error.message}).to_string()
            }),
        ),
    };
    Bytes::from(format!("event: {name}\ndata: {data}\n\n"))
}

fn stopped(error: &tokio::task::JoinError) -> RouteOutcome {
    RouteOutcome::One(Err(ErrorBody::new(
        ErrorCode::Internal,
        format!("The call stopped before it replied: {error}."),
    )))
}

/// The gate puts a principal on every request it lets through to here, so its absence is a
/// mistake in the server, refused rather than served as anyone.
fn no_principal() -> ErrorBody {
    ErrorBody::new(
        ErrorCode::Internal,
        "The request reached a call without a principal.",
    )
}

/// A response body of server-sent events, which cancels its call when it is dropped before the
/// call has finished: that is what happens when the client goes away.
struct EventStream {
    frames: mpsc::Receiver<Bytes>,
    _gone: DropGuard,
}

impl http_body::Body for EventStream {
    type Data = Bytes;
    type Error = Infallible;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, Infallible>>> {
        self.frames
            .poll_recv(cx)
            .map(|frame| frame.map(|bytes| Ok(Frame::data(bytes))))
    }
}

#[cfg(test)]
mod tests {
    use legion_core::proto::EventKind;

    use super::*;

    #[test]
    fn events_and_the_end_are_server_sent_events() {
        let frame = event_frame(&Event {
            kind: EventKind::Stdout,
            data: json!("a\nb"),
            machine: Some("pi".to_owned()),
        });
        assert_eq!(
            frame,
            "event: event\ndata: {\"kind\":\"stdout\",\"data\":\"a\\nb\",\"machine\":\"pi\"}\n\n"
        );
        assert_eq!(
            final_frame(RouteOutcome::One(Ok(json!({"v": 1})))),
            "event: result\ndata: {\"v\":1}\n\n"
        );
        let failed = final_frame(RouteOutcome::One(Err(ErrorBody::new(
            ErrorCode::Busy,
            "Busy.",
        ))));
        assert!(failed.starts_with(b"event: error\ndata: {\"code\":\"busy\""));
    }

    #[test]
    fn the_event_stream_is_asked_for_by_accept() {
        let with = |accept: &'static str| {
            let mut headers = HeaderMap::new();
            headers.insert(ACCEPT, HeaderValue::from_static(accept));
            accepts_events(&headers)
        };
        assert!(with("text/event-stream"));
        assert!(with("application/json, Text/Event-Stream;q=0.5"));
        assert!(!with("application/json"));
        assert!(!accepts_events(&HeaderMap::new()));
    }
}
