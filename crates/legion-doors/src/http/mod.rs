//! The HTTP door, `legion serve`: the node's tools for a device that cannot start a process of its
//! own, such as a phone, a hosted assistant or a script on another machine.
//!
//! It listens on loopback only. Publishing it to the private network is Tailscale Serve's job,
//! which brings its own TLS and its own idea of who may connect; a wider bind address would bring
//! neither. Every request but `GET /v1/health` carries a bearer token from `tokens.toml`, which
//! stands for one principal, and every call acts as that principal whatever its body says, unless
//! the principal's role may delegate.
//!
//! Before a token is even looked at, the `Host` must be this server (a page that rebinds a name of
//! its own to 127.0.0.1 is refused) and an `Origin`, if there is one, must be listed in
//! `node.toml`, so a web page open on the machine cannot use the server through the browser.
//!
//! Paths:
//!
//! - `GET /v1/health`: `{ok, node, version}`, without a token.
//! - `GET /v1/tools`: `{tools: [ToolSpec]}`, the tools the principal may use.
//! - `POST /v1/call`: `{tool, args, on?}` runs a call. The tool's result comes back with 200, a
//!   failure as the error object with a status that fits its code. With
//!   `Accept: text/event-stream` the call's events stream as they happen, then its end.
//! - `/mcp`: MCP over streamable HTTP, the same door `legion mcp` serves over stdio.
//!   `?compact=1` (or `Legion-Compact: 1`) serves the three compact tools.

mod api;
mod gate;
mod reply;

use std::future::Future;
use std::io::{self, Write as _};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::body::{Body, Bytes};
use axum::extract::{Request, State};
use axum::http::header::{ALLOW, HeaderValue, WWW_AUTHENTICATE};
use axum::http::{Method, Response, StatusCode};
use axum::middleware::{self, Next};
use axum::routing::{any, get, post};
use hyper_util::rt::{TokioIo, TokioTimer};
use hyper_util::service::TowerToHyperService;
use legion_core::config::NodeFile;
use legion_core::proto::{ErrorBody, ErrorCode};
use legion_core::tokens::Tokens;
use legion_core::{CANCEL_GRACE, Node, Router};
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use tokio::net::TcpListener;
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

use self::gate::Refusal;
use crate::Authenticated;
use crate::mcp::{HttpLink, McpDoor};

/// Where `legion serve` listens unless told otherwise.
pub const DEFAULT_LISTEN: &str = "127.0.0.1:7117";

/// The largest request body read, for a call or an MCP message. A call's arguments are small; file
/// content beyond this belongs to `legion stream`, not to JSON.
pub const MAX_BODY: usize = 4 * 1024 * 1024;

/// How many calls run at once, across both the JSON API and MCP. One more is refused as busy
/// rather than queued, so a client that floods the server learns so at once.
const MAX_CALLS: usize = 64;

/// How many connections are open at once; the next waits to be accepted.
const MAX_CONNECTIONS: usize = 256;

/// How long a client has to send a request's headers, and then its body. A client that stalls
/// longer is disconnected rather than holding a connection for ever.
const HEADER_TIMEOUT: Duration = Duration::from_secs(30);
const BODY_TIMEOUT: Duration = Duration::from_secs(30);

/// How long a request with a missing or wrong token waits before it is refused, so guessing is
/// slow; a token holder never waits.
const REFUSAL_DELAY: Duration = Duration::from_millis(200);

/// How long shutting down waits for connections to close and calls to be recorded: the time a
/// cancelled call has to stop, and a little more.
const DRAIN: Duration = CANCEL_GRACE.saturating_add(Duration::from_secs(2));

/// The header that asks for compact MCP, for clients that cannot add a query to their URL.
const COMPACT_HEADER: &str = "legion-compact";

/// The address `--listen` gives, refused unless it is on this machine.
///
/// `localhost` is taken as 127.0.0.1, so the server does not depend on what a resolver says.
pub fn listen_address(text: &str) -> Result<SocketAddr, ErrorBody> {
    let spelled = match text.rsplit_once(':') {
        Some((host, port)) if host.eq_ignore_ascii_case("localhost") => format!("127.0.0.1:{port}"),
        _ => text.to_owned(),
    };
    let address: SocketAddr = spelled.parse().map_err(|_| {
        ErrorBody::new(
            ErrorCode::InvalidArgs,
            format!("{text} is not an address to listen on."),
        )
        .with_hint(format!(
            "Give an address and a port, such as {DEFAULT_LISTEN}."
        ))
    })?;
    if !address.ip().is_loopback() {
        return Err(ErrorBody::new(
            ErrorCode::Denied,
            format!(
                "legion serve listens on this machine only, and {} is not a loopback address.",
                address.ip()
            ),
        )
        .with_hint(format!(
            "Publishing it beyond this machine is Tailscale Serve's job: listen on 127.0.0.1:{port} and run `tailscale serve --bg --https=8443 http://127.0.0.1:{port}`.",
            port = address.port()
        )));
    }
    Ok(address)
}

/// The HTTP door over one node.
pub struct HttpDoor {
    node: Node,
    router: Arc<dyn Router>,
    full: Arc<crate::mcp::catalog::Catalog>,
    compact: Arc<crate::mcp::catalog::Catalog>,
}

impl HttpDoor {
    /// A door serving `node`'s tools, running each call through `router`.
    ///
    /// Fails when the registry cannot be served over MCP as it is, as `legion mcp` would.
    pub fn new(node: Node, router: Arc<dyn Router>) -> Result<HttpDoor, ErrorBody> {
        Ok(HttpDoor {
            full: McpDoor::catalog(&node, false)?,
            compact: McpDoor::catalog(&node, true)?,
            node,
            router,
        })
    }

    /// Serves connections from `listener` until `shutdown` completes, then stops accepting,
    /// cancels every running call, waits for them to reach the audit log and closes the router's
    /// sessions.
    ///
    /// The listener must be bound to a loopback address; [`listen_address`] is how the command
    /// line makes sure.
    pub async fn serve(
        self,
        listener: TcpListener,
        shutdown: impl Future<Output = ()>,
    ) -> io::Result<()> {
        let local = listener.local_addr()?;
        if !local.ip().is_loopback() {
            return Err(io::Error::other(format!(
                "{local} is not a loopback address"
            )));
        }
        let link = HttpLink {
            shutdown: CancellationToken::new(),
            limit: Arc::new(Semaphore::new(MAX_CALLS)),
        };
        let calls = TaskTracker::new();
        let mcp = |compact: bool, catalog: Arc<crate::mcp::catalog::Catalog>| {
            let (node, router, link, calls) = (
                self.node.clone(),
                Arc::clone(&self.router),
                link.clone(),
                calls.clone(),
            );
            let config = StreamableHttpServerConfig::default()
                // The gate has checked Host and Origin for every path already.
                .disable_allowed_hosts()
                .disable_allowed_origins()
                .with_cancellation_token(link.shutdown.child_token())
                .with_max_request_body_bytes(MAX_BODY);
            StreamableHttpService::new(
                move || {
                    Ok(McpDoor::for_http(
                        node.clone(),
                        Arc::clone(&router),
                        compact,
                        Arc::clone(&catalog),
                        link.clone(),
                        calls.clone(),
                    ))
                },
                Arc::new(LocalSessionManager::default()),
                config,
            )
        };
        let shared = Arc::new(Shared {
            mcp_full: mcp(false, Arc::clone(&self.full)),
            mcp_compact: mcp(true, Arc::clone(&self.compact)),
            node: self.node.clone(),
            router: Arc::clone(&self.router),
            port: local.port(),
            link,
            calls,
        });
        let app = axum::Router::new()
            .route("/v1/health", get(api::health))
            .route("/v1/tools", get(api::tools))
            .route("/v1/call", post(api::call))
            .route("/mcp", any(serve_mcp))
            .fallback(not_found)
            .layer(middleware::from_fn_with_state(Arc::clone(&shared), gate))
            .with_state(Arc::clone(&shared));

        let connections = TaskTracker::new();
        let closing = CancellationToken::new();
        let slots = Arc::new(Semaphore::new(MAX_CONNECTIONS));
        tokio::pin!(shutdown);
        loop {
            let slot = tokio::select! {
                () = &mut shutdown => break,
                slot = Arc::clone(&slots).acquire_owned() => slot,
            };
            let Ok(slot) = slot else { break };
            let stream = tokio::select! {
                () = &mut shutdown => break,
                accepted = listener.accept() => match accepted {
                    Ok((stream, _)) => stream,
                    Err(error) => {
                        // Out of file descriptors, most likely; waiting lets some close.
                        log(&format!("accepting a connection failed: {error}"));
                        tokio::time::sleep(Duration::from_millis(100)).await;
                        continue;
                    }
                },
            };
            let service = TowerToHyperService::new(app.clone());
            let closing = closing.clone();
            connections.spawn(async move {
                let _slot = slot;
                let mut builder = hyper::server::conn::http1::Builder::new();
                builder
                    .timer(TokioTimer::new())
                    .header_read_timeout(HEADER_TIMEOUT);
                let connection = builder.serve_connection(TokioIo::new(stream), service);
                tokio::pin!(connection);
                tokio::select! {
                    _ = connection.as_mut() => {}
                    () = closing.cancelled() => {
                        connection.as_mut().graceful_shutdown();
                        let _ = connection.await;
                    }
                }
            });
        }

        drop(listener);
        shared.link.shutdown.cancel();
        closing.cancel();
        connections.close();
        let _ = tokio::time::timeout(DRAIN, connections.wait()).await;
        shared.calls.close();
        let _ = tokio::time::timeout(DRAIN, shared.calls.wait()).await;
        self.router.shutdown().await;
        Ok(())
    }
}

/// What every request handler shares.
pub(crate) struct Shared {
    node: Node,
    router: Arc<dyn Router>,
    /// The port the server listens on, which a loopback `Host` must name.
    port: u16,
    link: HttpLink,
    /// Every call still running, so shutting down can wait for them to be recorded.
    calls: TaskTracker,
    mcp_full: StreamableHttpService<McpDoor, LocalSessionManager>,
    mcp_compact: StreamableHttpService<McpDoor, LocalSessionManager>,
}

/// Every request passes here first: the Host, the Origin and then the token are checked, and one
/// line about the request goes to stderr. Neither the token nor the body is ever logged.
async fn gate(
    State(shared): State<Arc<Shared>>,
    mut request: Request,
    next: Next,
) -> Response<Body> {
    let started = Instant::now();
    let method = request.method().clone();
    let path: String = request.uri().path().chars().take(200).collect();
    // A node.toml that does not parse adds no hosts and no origins: loopback still works.
    let config = shared
        .node
        .config()
        .get::<NodeFile>()
        .map(|file| file.serve.clone())
        .unwrap_or_default();
    let mut principal = None;
    let response = if !gate::host_allowed(request.headers(), request.uri(), shared.port, &config) {
        reply::refusal(
            StatusCode::MISDIRECTED_REQUEST,
            ErrorCode::Denied,
            "This server does not answer to that host name.",
        )
    } else if !gate::origin_allowed(request.headers(), &config) {
        reply::refusal(
            StatusCode::FORBIDDEN,
            ErrorCode::Denied,
            "Pages from that origin may not use this server.",
        )
    } else if method == Method::GET && path == "/v1/health" {
        next.run(request).await
    } else {
        let tokens = shared
            .node
            .config()
            .get::<Tokens>()
            .unwrap_or_else(|error| {
                log(&format!(
                    "every token is refused until tokens.toml is mended: {}",
                    error.message
                ));
                Arc::new(Tokens::default())
            });
        match gate::principal(request.headers(), &tokens) {
            Ok(found) => {
                principal = Some(found.clone());
                request.extensions_mut().insert(Authenticated(found));
                next.run(request).await
            }
            Err(refusal) => {
                tokio::time::sleep(REFUSAL_DELAY).await;
                unauthorized(&refusal)
            }
        }
    };
    log(&format!(
        "{method} {path} {} {} {}ms",
        response.status().as_u16(),
        principal.as_deref().unwrap_or("-"),
        started.elapsed().as_millis()
    ));
    response
}

/// 401, saying a token is needed or that the one given will not do, and never which of the two
/// reasons a token will not do.
fn unauthorized(refusal: &Refusal) -> Response<Body> {
    let (message, challenge) = match refusal {
        Refusal::NoToken => (
            "This request needs a bearer token.",
            "Bearer realm=\"legion\"",
        ),
        _ => (
            "The bearer token is invalid or expired.",
            "Bearer realm=\"legion\", error=\"invalid_token\"",
        ),
    };
    let mut response = reply::json(
        StatusCode::UNAUTHORIZED,
        &ErrorBody::new(ErrorCode::Denied, message).with_hint(
            "Send Authorization: Bearer <token>, with a token the owner of the machine made with token.add.",
        ),
    );
    response
        .headers_mut()
        .insert(WWW_AUTHENTICATE, HeaderValue::from_static(challenge));
    response
}

/// `/mcp`: the MCP door over streamable HTTP.
///
/// The server offers no stream of its own, so `GET` is 405 as the specification asks of such a
/// server. The body is read here, bounded and with a deadline, before the SDK sees the request.
async fn serve_mcp(State(shared): State<Arc<Shared>>, request: Request) -> Response<Body> {
    if request.method() == Method::GET {
        let mut response = reply::refusal(
            StatusCode::METHOD_NOT_ALLOWED,
            ErrorCode::Unsupported,
            "This server offers no event stream of its own; send MCP messages with POST.",
        );
        response
            .headers_mut()
            .insert(ALLOW, HeaderValue::from_static("POST, DELETE"));
        return response;
    }
    let compact = wants_compact(&request);
    let (parts, body) = request.into_parts();
    let bytes = match read_body(body).await {
        Ok(bytes) => bytes,
        Err(response) => return *response,
    };
    let request = Request::from_parts(parts, Body::from(bytes));
    let service = if compact {
        &shared.mcp_compact
    } else {
        &shared.mcp_full
    };
    service.handle(request).await.map(Body::new)
}

/// Whether the client asked for compact MCP, with `?compact=1` or the `Legion-Compact` header.
fn wants_compact(request: &Request) -> bool {
    let yes = |value: &str| matches!(value.trim(), "1" | "true" | "yes");
    let by_query = request.uri().query().is_some_and(|query| {
        query.split('&').any(|pair| match pair.split_once('=') {
            Some((name, value)) => name == "compact" && yes(value),
            None => pair == "compact",
        })
    });
    let by_header = request
        .headers()
        .get(COMPACT_HEADER)
        .and_then(|value| value.to_str().ok())
        .is_some_and(yes);
    by_query || by_header
}

/// A request's body, refused with 413 when it is larger than [`MAX_BODY`] and with 408 when the
/// client takes longer than [`BODY_TIMEOUT`] to send it.
pub(crate) async fn read_body(body: Body) -> Result<Bytes, Box<Response<Body>>> {
    let too_large = || {
        Box::new(reply::refusal(
            StatusCode::PAYLOAD_TOO_LARGE,
            ErrorCode::InvalidArgs,
            &format!(
                "The request body is larger than {MAX_BODY} bytes, or ended before its length."
            ),
        ))
    };
    match tokio::time::timeout(BODY_TIMEOUT, axum::body::to_bytes(body, MAX_BODY)).await {
        Ok(Ok(bytes)) => Ok(bytes),
        Ok(Err(_)) => Err(too_large()),
        Err(_) => Err(Box::new(reply::refusal(
            StatusCode::REQUEST_TIMEOUT,
            ErrorCode::Timeout,
            "The request body took too long to arrive.",
        ))),
    }
}

async fn not_found() -> Response<Body> {
    reply::json(
        StatusCode::NOT_FOUND,
        &ErrorBody::new(ErrorCode::NotFound, "There is nothing at this path.")
            .with_hint("The paths are GET /v1/health, GET /v1/tools, POST /v1/call and /mcp."),
    )
}

/// One line on stderr, for the person who started the server.
fn log(line: &str) {
    let _ = io::stderr()
        .lock()
        .write_all(format!("legion serve: {line}\n").as_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_loopback_is_listened_on() {
        assert_eq!(
            listen_address("127.0.0.1:7117").unwrap(),
            "127.0.0.1:7117".parse().unwrap()
        );
        assert_eq!(
            listen_address("localhost:0").unwrap(),
            "127.0.0.1:0".parse().unwrap()
        );
        assert!(listen_address("[::1]:7117").is_ok());
        for wide in ["0.0.0.0:7117", "[::]:7117", "192.0.2.1:7117"] {
            let error = listen_address(wide).unwrap_err();
            assert_eq!(error.code, ErrorCode::Denied, "{wide}");
            let hint = error.hint.unwrap();
            assert!(
                hint.contains("tailscale serve --bg --https=8443 http://127.0.0.1:7117"),
                "{hint}"
            );
        }
        assert_eq!(
            listen_address("tower:x").unwrap_err().code,
            ErrorCode::InvalidArgs
        );
    }
}
