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
//! - `POST /v1/content`: `{path, on?}` issues a short-lived ticket URL
//!   (`/v1/content/<ticket>/<file name>`) for random-access file reads (`GET`/`HEAD` with
//!   single-range support, safe headers, and no bearer token required on the ticket URL).
//! - `/mcp`: MCP over streamable HTTP, the same door `legion mcp` serves over stdio.
//!   `?compact=1` (or `Legion-Compact: 1`) serves the three compact tools.

mod api;
mod content;
mod gate;
mod reply;
mod sessions;
mod ui;

use std::future::Future;
use std::io::{self, Write as _};
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use axum::body::{Body, Bytes};
use axum::extract::{Request, State};
use axum::http::header::{
    ACCESS_CONTROL_ALLOW_HEADERS, ACCESS_CONTROL_ALLOW_METHODS, ACCESS_CONTROL_ALLOW_ORIGIN,
    ACCESS_CONTROL_EXPOSE_HEADERS, ACCESS_CONTROL_MAX_AGE, ALLOW, CONTENT_SECURITY_POLICY,
    CONTENT_TYPE, HeaderValue, REFERRER_POLICY, VARY, WWW_AUTHENTICATE, X_CONTENT_TYPE_OPTIONS,
};
use axum::http::{Method, Response, StatusCode};
use axum::middleware::{self, Next};
use axum::routing::{any, get, post};
use hyper_util::rt::{TokioIo, TokioTimer};
use hyper_util::service::TowerToHyperService;
use legion_content::{ConcurrencyLimits, ContentLimiter, TicketTable};
use legion_core::config::NodeFile;
use legion_core::proto::{ErrorBody, ErrorCode};
use legion_core::tokens::Tokens;
use legion_core::{CANCEL_GRACE, Node, Router};
use rmcp::model::ClientJsonRpcMessage;
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::session::{SessionId, SessionManager};
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Semaphore;
use tokio::time::Sleep;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

use self::content::ContentAuditCoalescer;
use self::gate::Refusal;
use self::sessions::{Abandonable, SessionRef, Sessions};
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

/// How often an event stream with nothing to say says so, so that a client that went away is
/// noticed, and its call cancelled, within this long.
pub(crate) const KEEP_ALIVE: Duration = Duration::from_secs(3);

/// How often sessions are checked for a token that went and for idleness.
const SWEEP: Duration = Duration::from_secs(10);

/// The header that carries an MCP session's id.
const SESSION_HEADER: &str = "mcp-session-id";

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

/// Concurrency, slow-reader, and audit-coalescing limits for `/v1/content/{ticket}/{name}` streams.
#[derive(Clone, Copy, Debug)]
pub struct ContentStreamLimits {
    /// Per-ticket, per-holder, and global concurrent stream caps.
    pub concurrency: ConcurrencyLimits,
    /// Maximum duration a content stream write may stall without draining any bytes.
    pub write_idle_timeout: Duration,
    /// Window over which minimum sustained write progress is checked while streaming.
    pub min_rate_window: Duration,
    /// Minimum bytes written during [`Self::min_rate_window`] (default 40 KiB / 10s = 4 KiB/s).
    pub min_rate_bytes: u64,
    /// Coalescing window for per-ticket `content.read` audit records (default 60s).
    pub audit_coalesce_window: Duration,
    /// Interval between background session/ticket/audit sweeps (default 10s).
    pub sweep_interval: Duration,
    /// Interval between independent mid-stream revocation and policy checks on active content
    /// responses (default 1s).
    pub liveness_check_interval: Duration,
}

impl Default for ContentStreamLimits {
    fn default() -> Self {
        Self {
            concurrency: ConcurrencyLimits::default(),
            write_idle_timeout: Duration::from_secs(15),
            min_rate_window: Duration::from_secs(10),
            min_rate_bytes: 40 * 1024,
            audit_coalesce_window: content::DEFAULT_AUDIT_COALESCE_WINDOW,
            sweep_interval: SWEEP,
            liveness_check_interval: Duration::from_secs(1),
        }
    }
}

/// Socket send-buffer cap applied when a TCP connection streams a content response (128 KiB),
/// bounding kernel-buffered bytes on revocation without hurting loopback or LAN throughput.
pub(crate) const CONTENT_SOCKET_SNDBUF: usize = 128 * 1024;

/// Request extension injected per TCP connection so streaming content bodies can enable socket
/// send-buffer bounding, write-stall / minimum-rate enforcement, and immediate RST abort on
/// revocation.
#[derive(Clone)]
pub(crate) struct ConnectionControl {
    pub(crate) active: Arc<AtomicUsize>,
    pub(crate) abort: CancellationToken,
}

/// The HTTP door over one node.
pub struct HttpDoor {
    node: Node,
    router: Arc<dyn Router>,
    full: Arc<crate::mcp::catalog::CatalogCache>,
    compact: Arc<crate::mcp::catalog::CatalogCache>,
    content_limits: ContentStreamLimits,
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
            content_limits: ContentStreamLimits::default(),
        })
    }

    /// Overrides the default content stream concurrency and slow-reader timeouts (useful in tests).
    pub fn with_content_limits(mut self, limits: ContentStreamLimits) -> Self {
        self.content_limits = limits;
        self
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
        let mcp = |compact: bool, catalog: Arc<crate::mcp::catalog::CatalogCache>| {
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
                .with_max_request_body_bytes(MAX_BODY)
                // Something is written this often, so a client that vanished is noticed.
                .with_sse_keep_alive(Some(KEEP_ALIVE));
            let mut sessions = LocalSessionManager::default();
            sessions.session_config.keep_alive = Some(sessions::IDLE);
            let sessions = Arc::new(sessions);
            let service = StreamableHttpService::new(
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
                Arc::clone(&sessions),
                config,
            );
            (service, sessions)
        };
        let (mcp_full, managers_full) = mcp(false, Arc::clone(&self.full));
        let (mcp_compact, managers_compact) = mcp(true, Arc::clone(&self.compact));
        let content_limits = self.content_limits;
        let shared = Arc::new(Shared {
            mcp_full,
            mcp_compact,
            managers: [managers_full, managers_compact],
            sessions: Sessions::default(),
            content_tickets: Arc::new(TicketTable::new()),
            content_limiter: ContentLimiter::with_limits(content_limits.concurrency),
            content_audit: ContentAuditCoalescer::with_window(content_limits.audit_coalesce_window),
            content_liveness_interval: content_limits.liveness_check_interval,
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
            .route("/v1/content", post(content::create))
            .route("/v1/content/{ticket}/{name}", any(content::serve))
            .route("/mcp", any(serve_mcp))
            .fallback(not_found)
            .layer(middleware::from_fn_with_state(Arc::clone(&shared), gate))
            .with_state(Arc::clone(&shared));

        // Ends sessions whose token went or that went idle, even when no request comes to notice.
        let sweeping = Arc::clone(&shared);
        let sweep_interval = content_limits.sweep_interval;
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    () = sweeping.link.shutdown.cancelled() => break,
                    () = tokio::time::sleep(sweep_interval) => sweeping.end_stale_sessions().await,
                }
            }
        });

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
            let active_streams = Arc::new(AtomicUsize::new(0));
            let conn_abort = CancellationToken::new();
            let monitored = MonitoredTcpStream::new(
                stream,
                Arc::clone(&active_streams),
                conn_abort.clone(),
                content_limits.write_idle_timeout,
                content_limits.min_rate_window,
                content_limits.min_rate_bytes,
            );
            let service = InjectConnectionFlag {
                inner: TowerToHyperService::new(app.clone()),
                flag: ConnectionControl {
                    active: active_streams,
                    abort: conn_abort.clone(),
                },
            };
            let closing = closing.clone();
            connections.spawn(async move {
                let _slot = slot;
                let mut builder = hyper::server::conn::http1::Builder::new();
                builder
                    .timer(TokioTimer::new())
                    .header_read_timeout(HEADER_TIMEOUT);
                let connection = builder.serve_connection(TokioIo::new(monitored), service);
                tokio::pin!(connection);
                tokio::select! {
                    biased;
                    () = conn_abort.cancelled() => {
                        // Drop `connection` immediately so `MonitoredTcpStream::drop` applies
                        // SO_LINGER(0) (TCP RST) to discard kernel-buffered bytes and drops the
                        // active content body, buffers, and remote stream pipe at once.
                    }
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
        shared.content_audit.flush_all(&shared.node).await;
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
    /// The session managers behind the full and the compact endpoint, in that order.
    managers: [Arc<LocalSessionManager>; 2],
    /// Which token opened each session.
    sessions: Sessions,
    /// Active content tickets bound to bearer tokens.
    content_tickets: Arc<TicketTable>,
    /// Per-ticket, per-holder, and global concurrent stream limiter.
    content_limiter: ContentLimiter,
    /// Coalesced per-ticket `content.read` audit tracker.
    content_audit: ContentAuditCoalescer,
    /// Interval between independent mid-stream liveness/revocation checks.
    content_liveness_interval: Duration,
}

impl Shared {
    fn manager(&self, compact: bool) -> &LocalSessionManager {
        &self.managers[usize::from(compact)]
    }

    async fn close(&self, ended: Vec<SessionRef>) {
        for session in ended {
            let id: SessionId = session.id.into();
            let _ = self.manager(session.compact).close_session(&id).await;
        }
    }

    /// Ends the sessions and content tickets of tokens that were removed, replaced or have
    /// expired, flushes coalesced content audit entries, and sweeps idle ones.
    async fn end_stale_sessions(&self) {
        let tokens = self.node.config().get::<Tokens>().ok();
        let now = jiff::Timestamp::now();
        let removed = self
            .content_tickets
            .sweep_holders(|holder| content::holder_is_live(holder, None, tokens.as_deref(), now));
        self.content_audit.flush_removed(&self.node, &removed).await;
        self.content_audit
            .flush_missing(&self.node, |hash| self.content_tickets.contains_hash(hash))
            .await;
        self.content_audit.flush_due(&self.node).await;
        let ended = self.sessions.stale(tokens.as_deref());
        self.close(ended).await;
    }
}

/// Hyper service adapter that injects [`ConnectionControl`] into every request on a TCP
/// connection so streaming content responses can activate socket-level stall/rate limits, send
/// buffer bounds, and immediate revocation abort.
struct InjectConnectionFlag {
    inner: TowerToHyperService<axum::Router>,
    flag: ConnectionControl,
}

impl hyper::service::Service<axum::http::Request<hyper::body::Incoming>> for InjectConnectionFlag {
    type Response = <TowerToHyperService<axum::Router> as hyper::service::Service<
        axum::http::Request<hyper::body::Incoming>,
    >>::Response;
    type Error = <TowerToHyperService<axum::Router> as hyper::service::Service<
        axum::http::Request<hyper::body::Incoming>,
    >>::Error;
    type Future = <TowerToHyperService<axum::Router> as hyper::service::Service<
        axum::http::Request<hyper::body::Incoming>,
    >>::Future;

    fn call(&self, mut req: axum::http::Request<hyper::body::Incoming>) -> Self::Future {
        req.extensions_mut().insert(self.flag.clone());
        self.inner.call(req)
    }
}

/// Wraps a [`TcpStream`] to enforce a write-idle timeout and minimum sustained progress rate while
/// a content response is actively streaming on the connection.
///
/// Only time spent blocked on a full client socket counts toward the minimum-rate window, so a
/// slow upstream source never causes a fast client to be disconnected. When no content stream is
/// active on the connection, reads, writes, and vectored writes pass straight through to the inner
/// [`TcpStream`].
struct MonitoredTcpStream {
    inner: TcpStream,
    active: Arc<AtomicUsize>,
    abort: CancellationToken,
    sndbuf_applied: bool,
    write_idle_timeout: Duration,
    min_rate_window: Duration,
    min_rate_bytes: u64,
    monitoring_active: bool,
    blocked_since: Option<Instant>,
    blocked_accum: Duration,
    rate_window_bytes: u64,
    stall_sleep: Option<Pin<Box<Sleep>>>,
    rate_sleep: Option<Pin<Box<Sleep>>>,
}

impl Drop for MonitoredTcpStream {
    fn drop(&mut self) {
        if self.abort.is_cancelled() {
            let _ = socket2::SockRef::from(&self.inner).set_linger(Some(Duration::ZERO));
        }
    }
}

impl MonitoredTcpStream {
    fn new(
        inner: TcpStream,
        active: Arc<AtomicUsize>,
        abort: CancellationToken,
        write_idle_timeout: Duration,
        min_rate_window: Duration,
        min_rate_bytes: u64,
    ) -> Self {
        Self {
            inner,
            active,
            abort,
            sndbuf_applied: false,
            write_idle_timeout,
            min_rate_window,
            min_rate_bytes,
            monitoring_active: false,
            blocked_since: None,
            blocked_accum: Duration::ZERO,
            rate_window_bytes: 0,
            stall_sleep: None,
            rate_sleep: None,
        }
    }

    fn reset_monitoring(&mut self) {
        self.monitoring_active = false;
        self.blocked_since = None;
        self.blocked_accum = Duration::ZERO;
        self.rate_window_bytes = 0;
        self.stall_sleep = None;
        self.rate_sleep = None;
    }

    fn monitor_write(
        &mut self,
        cx: &mut Context<'_>,
        write_op: impl FnOnce(&mut TcpStream, &mut Context<'_>) -> Poll<io::Result<usize>>,
    ) -> Poll<io::Result<usize>> {
        if self.abort.is_cancelled() {
            return Poll::Ready(Err(io::Error::new(
                io::ErrorKind::ConnectionReset,
                "content stream was revoked",
            )));
        }
        if self.active.load(Ordering::Acquire) == 0 {
            if self.monitoring_active {
                self.reset_monitoring();
            }
            return write_op(&mut self.inner, cx);
        }
        if !self.sndbuf_applied {
            let _ = socket2::SockRef::from(&self.inner)
                .set_send_buffer_size(CONTENT_SOCKET_SNDBUF);
            self.sndbuf_applied = true;
        }
        self.monitoring_active = true;

        let now = Instant::now();
        match write_op(&mut self.inner, cx) {
            Poll::Ready(Ok(n)) => {
                if let Some(since) = self.blocked_since.take() {
                    self.blocked_accum = self
                        .blocked_accum
                        .saturating_add(now.saturating_duration_since(since));
                    self.stall_sleep = None;
                    self.rate_sleep = None;
                    self.rate_window_bytes = self.rate_window_bytes.saturating_add(n as u64);
                } else if !self.blocked_accum.is_zero() {
                    self.rate_window_bytes = self.rate_window_bytes.saturating_add(n as u64);
                }
                if self.blocked_accum >= self.min_rate_window {
                    if self.rate_window_bytes < self.min_rate_bytes {
                        return Poll::Ready(Err(io::Error::new(
                            io::ErrorKind::TimedOut,
                            "content stream transfer rate fell below minimum threshold",
                        )));
                    }
                    self.blocked_accum = Duration::ZERO;
                    self.rate_window_bytes = 0;
                }
                Poll::Ready(Ok(n))
            }
            Poll::Ready(Err(err)) => Poll::Ready(Err(err)),
            Poll::Pending => {
                let since = *self.blocked_since.get_or_insert(now);
                let write_idle_timeout = self.write_idle_timeout;
                let stall = self
                    .stall_sleep
                    .get_or_insert_with(|| Box::pin(tokio::time::sleep(write_idle_timeout)));
                if stall.as_mut().poll(cx).is_ready() {
                    return Poll::Ready(Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "content stream stalled waiting for client to read",
                    )));
                }

                let total_blocked = self
                    .blocked_accum
                    .saturating_add(now.saturating_duration_since(since));
                if total_blocked >= self.min_rate_window {
                    if self.rate_window_bytes < self.min_rate_bytes {
                        return Poll::Ready(Err(io::Error::new(
                            io::ErrorKind::TimedOut,
                            "content stream transfer rate fell below minimum threshold",
                        )));
                    }
                    self.blocked_accum = Duration::ZERO;
                    self.rate_window_bytes = 0;
                    self.blocked_since = Some(now);
                    self.rate_sleep = None;
                }

                let current_since = self.blocked_since.unwrap_or(now);
                let remaining_window = self.min_rate_window.saturating_sub(
                    self.blocked_accum
                        .saturating_add(now.saturating_duration_since(current_since)),
                );
                let rate_sleep = self
                    .rate_sleep
                    .get_or_insert_with(|| Box::pin(tokio::time::sleep(remaining_window)));
                if rate_sleep.as_mut().poll(cx).is_ready() {
                    if self.rate_window_bytes < self.min_rate_bytes {
                        return Poll::Ready(Err(io::Error::new(
                            io::ErrorKind::TimedOut,
                            "content stream transfer rate fell below minimum threshold",
                        )));
                    }
                    self.blocked_accum = Duration::ZERO;
                    self.rate_window_bytes = 0;
                    self.blocked_since = Some(now);
                    let next = Box::pin(tokio::time::sleep(self.min_rate_window));
                    self.rate_sleep = Some(next);
                    if let Some(sleep) = self.rate_sleep.as_mut() {
                        let _ = sleep.as_mut().poll(cx);
                    }
                }
                Poll::Pending
            }
        }
    }
}

impl AsyncRead for MonitoredTcpStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

impl AsyncWrite for MonitoredTcpStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        self.get_mut()
            .monitor_write(cx, |inner, cx| Pin::new(inner).poll_write(cx, buf))
    }

    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        self.get_mut().monitor_write(cx, |inner, cx| {
            Pin::new(inner).poll_write_vectored(cx, bufs)
        })
    }

    fn is_write_vectored(&self) -> bool {
        self.inner.is_write_vectored()
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
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
    let raw_path = request.uri().path().to_owned();
    let logged_path: String = content::redact_log_path(&raw_path)
        .chars()
        .take(200)
        .collect();
    // A node.toml that does not parse adds no hosts and no origins: loopback still works.
    let config = shared
        .node
        .config()
        .get::<NodeFile>()
        .map(|file| file.serve.clone())
        .unwrap_or_default();
    let mut principal = None;
    let mut served_content = false;
    // The origin of a page allowed to call, which every response past the origin check names, so
    // the browser shows the page its errors as well as its results.
    let mut accepted_origin = None;
    let mut response = if let Some(problem) = gate::malformed(request.headers(), request.uri()) {
        reply::refusal(StatusCode::BAD_REQUEST, ErrorCode::InvalidArgs, problem)
    } else if !gate::host_allowed(request.headers(), shared.port, &config) {
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
    } else {
        accepted_origin = gate::listed_origin(request.headers(), &config);
        if method == Method::OPTIONS && accepted_origin.is_some() {
            // A browser's preflight carries no token; a listed origin is told what it may send.
            preflight()
        } else if method == Method::GET && raw_path == "/v1/health" {
            next.run(request).await
        } else if (method == Method::GET || method == Method::HEAD)
            && content::is_content_read_path(&raw_path)
        {
            // Media elements (`<img>`, `<video>`, `<audio>`) cannot send `Authorization` and send
            // no `Origin`. Host and Origin guards above have already run; `legion-content`
            // authenticates the 256-bit ticket, re-verifies the token and `fs.read` policy, and
            // sets its own fixed headers (`nosniff`, `CORP`, `no-referrer`, `no-store`, and
            // unconditional `sandbox` CSP with `frame-ancestors 'none'`).
            served_content = true;
            next.run(request).await
        } else if config.ui
            && (method == Method::GET || method == Method::HEAD)
            && let Some(asset) = ui::serve(
                request.uri().path(),
                request.headers(),
                method == Method::HEAD,
            )
        {
            asset
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
                    principal = Some(found.principal.clone());
                    request.extensions_mut().insert(found);
                    next.run(request).await
                }
                Err(refusal) => {
                    tokio::time::sleep(REFUSAL_DELAY).await;
                    unauthorized(&refusal)
                }
            }
        }
    };
    let headers = response.headers_mut();
    if !served_content {
        headers.insert(
            CONTENT_SECURITY_POLICY,
            HeaderValue::from_static(ui::CONTENT_SECURITY_POLICY),
        );
        headers.insert(X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
        headers.insert(REFERRER_POLICY, HeaderValue::from_static("no-referrer"));
    }
    if let Some(origin) = accepted_origin {
        headers.insert(ACCESS_CONTROL_ALLOW_ORIGIN, origin);
        headers.insert(
            ACCESS_CONTROL_EXPOSE_HEADERS,
            HeaderValue::from_static(
                "Mcp-Session-Id, WWW-Authenticate, Content-Range, Accept-Ranges, ETag, Content-Disposition",
            ),
        );
        headers.append(VARY, HeaderValue::from_static("Origin"));
    }
    log(&format!(
        "{method} {logged_path} {} {} {}ms",
        response.status().as_u16(),
        principal.as_deref().unwrap_or("-"),
        started.elapsed().as_millis()
    ));
    response
}

/// The answer to a listed origin's preflight: the methods and request headers this API takes.
/// Never a wildcard, and never credentials: the token travels in `Authorization`, which a page
/// sets itself, so a browser's cookies have no part in it.
fn preflight() -> Response<Body> {
    let mut response = Response::new(Body::empty());
    *response.status_mut() = StatusCode::NO_CONTENT;
    let headers = response.headers_mut();
    headers.insert(
        ACCESS_CONTROL_ALLOW_METHODS,
        HeaderValue::from_static("GET, HEAD, POST, DELETE"),
    );
    headers.insert(
        ACCESS_CONTROL_ALLOW_HEADERS,
        HeaderValue::from_static(
            "Authorization, Content-Type, Accept, Range, If-Range, Mcp-Session-Id, MCP-Protocol-Version, Mcp-Method, Mcp-Name, Last-Event-ID, Legion-Compact",
        ),
    );
    headers.insert(ACCESS_CONTROL_MAX_AGE, HeaderValue::from_static("600"));
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
/// server, and so is every method but `POST` and `DELETE`, answered here rather than by the SDK,
/// which would offer `GET`. The body is read here, bounded and with a deadline, before the SDK sees the request.
async fn serve_mcp(State(shared): State<Arc<Shared>>, request: Request) -> Response<Body> {
    if request.method() != Method::POST && request.method() != Method::DELETE {
        let mut response = reply::refusal(
            StatusCode::METHOD_NOT_ALLOWED,
            ErrorCode::Unsupported,
            "MCP here takes POST, and DELETE to end a session; this server offers no event stream of its own.",
        );
        response
            .headers_mut()
            .insert(ALLOW, HeaderValue::from_static("POST, DELETE"));
        return response;
    }
    let Some(who) = request.extensions().get::<Authenticated>().cloned() else {
        return reply::error(&ErrorBody::new(
            ErrorCode::Internal,
            "The request reached MCP without a principal.",
        ));
    };
    let compact = wants_compact(&request);
    shared.end_stale_sessions().await;
    // A session that is not this token's is answered exactly as one that does not exist.
    let session = match request.headers().get(SESSION_HEADER).map(|id| id.to_str()) {
        None => None,
        Some(Ok(id)) if shared.sessions.admit(id, &who, compact) => Some(id.to_owned()),
        Some(_) => return session_not_found(),
    };
    let deleting = request.method() == Method::DELETE;
    let (parts, body) = request.into_parts();
    let bytes = match read_body(body).await {
        Ok(bytes) => bytes,
        Err(response) => return *response,
    };
    let mut reservation = None;
    if session.is_none() && opens_session(&bytes) {
        match shared.sessions.reserve(&who, compact) {
            Some((place, ended)) => {
                reservation = Some(place);
                shared.close(ended).await;
            }
            None => {
                return reply::json(
                    StatusCode::SERVICE_UNAVAILABLE,
                    &ErrorBody::new(
                        ErrorCode::Busy,
                        "The server holds as many MCP sessions as it allows.",
                    )
                    .with_hint(
                        "End sessions that are no longer needed, or wait for idle ones to expire.",
                    ),
                );
            }
        }
    }
    // Only a request posted inside a session can be cancelled by leaving it; a DELETE ends the
    // session whatever its body says.
    let request_id = session
        .as_ref()
        .filter(|_| !deleting)
        .and_then(|_| request_id(&bytes));
    let request = Request::from_parts(parts, Body::from(bytes));
    let service = if compact {
        &shared.mcp_compact
    } else {
        &shared.mcp_full
    };
    let response = service.handle(request).await;
    // Only a successful event stream answers the request; any other reply (a 406 or a 400 for a
    // request that reused an id) is not the running call's, and leaving it cancels nothing.
    let streaming = response.status() == StatusCode::OK
        && response
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|kind| kind.to_str().ok())
            .is_some_and(|kind| kind.starts_with("text/event-stream"));
    if let (Some(id), Some(request_id), true) = (&session, request_id, streaming) {
        // A client that goes away while a request in its session is answered has cancelled it.
        let manager = Arc::clone(&shared.managers[usize::from(compact)]);
        let id: SessionId = id.as_str().into();
        return response.map(|body| {
            Body::new(Abandonable::new(body, move || {
                let cancel = serde_json::json!({
                    "jsonrpc": "2.0",
                    "method": "notifications/cancelled",
                    "params": {"requestId": request_id, "reason": "The client went away."}
                });
                let (Ok(handle), Ok(cancel)) = (
                    tokio::runtime::Handle::try_current(),
                    serde_json::from_value::<ClientJsonRpcMessage>(cancel),
                ) else {
                    return;
                };
                handle.spawn(async move {
                    let _ = manager.accept_message(&id, cancel).await;
                });
            }))
        });
    }
    match &session {
        None => {
            // A failed handshake opened nothing, and dropping the reservation gives its place back.
            if let (Some(place), Some(id)) = (
                reservation,
                response
                    .headers()
                    .get(SESSION_HEADER)
                    .and_then(|id| id.to_str().ok()),
            ) {
                place.bind(id.to_owned());
            }
        }
        Some(id) if deleting && response.status().is_success() => shared.sessions.forget(id),
        Some(_) => {}
    }
    response.map(Body::new)
}

/// What the SDK answers for a session it does not know, word for word, so a session another
/// token holds cannot be told from one that does not exist.
fn session_not_found() -> Response<Body> {
    let mut response = Response::new(Body::from("Not Found: Session not found"));
    *response.status_mut() = StatusCode::NOT_FOUND;
    response
}

/// The id of a request, when the message is one rather than a notification or a reply.
fn request_id(body: &[u8]) -> Option<serde_json::Value> {
    let message = serde_json::from_slice::<serde_json::Value>(body).ok()?;
    message.get("method")?;
    message.get("id").cloned()
}

/// Whether a message opens a session: an `initialize` request.
fn opens_session(body: &[u8]) -> bool {
    serde_json::from_slice::<serde_json::Value>(body).is_ok_and(|message| {
        message.get("method").and_then(|method| method.as_str()) == Some("initialize")
    })
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
    read_body_limited(body, MAX_BODY).await
}

/// Like [`read_body`] with a caller-specified maximum byte size `limit`.
pub(crate) async fn read_body_limited(
    body: Body,
    limit: usize,
) -> Result<Bytes, Box<Response<Body>>> {
    let too_large = || {
        Box::new(reply::refusal(
            StatusCode::PAYLOAD_TOO_LARGE,
            ErrorCode::InvalidArgs,
            &format!("The request body is larger than {limit} bytes, or ended before its length."),
        ))
    };
    // A body whose declared length is already too large is refused before a byte of it is read.
    let declared = http_body::Body::size_hint(&body).lower();
    if usize::try_from(declared).map_or(true, |declared| declared > limit) {
        return Err(too_large());
    }
    match tokio::time::timeout(BODY_TIMEOUT, axum::body::to_bytes(body, limit)).await {
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
            .with_hint("The paths are GET /v1/health, GET /v1/tools, POST /v1/call, POST /v1/content and /mcp."),
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

    #[tokio::test]
    async fn slow_content_reader_is_dropped_by_write_idle_timeout_while_api_stays_responsive() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("legion");
        let user_home = dir.path().join("user");
        std::fs::create_dir_all(home.join("config")).unwrap();
        std::fs::create_dir_all(home.join("state")).unwrap();
        std::fs::create_dir_all(&user_home).unwrap();

        // Create a 16 MiB file so the kernel TCP send buffer fills when the client stops reading.
        let big_file = user_home.join("big.bin");
        std::fs::write(&big_file, vec![b'Z'; 16 * 1024 * 1024]).unwrap();

        let paths = legion_core::Paths::under(&home);
        std::fs::write(paths.config_file("node.toml"), "name = \"tower\"\n").unwrap();
        std::fs::write(
            paths.config_file("policy.toml"),
            "[principals.phone]\nrole = \"observer\"\n[roles.observer]\nallow = [\"effect:read\"]\n",
        )
        .unwrap();

        let node = Node::new(paths, legion_tools::registry());
        let minted = node
            .call(legion_core::Call::new(
                legion_core::Door::Cli,
                "token.add",
                serde_json::json!({ "name": "phone", "principal": "phone" }),
            ))
            .await
            .unwrap();
        let raw_token = minted["token"].as_str().unwrap().to_owned();
        let door = HttpDoor::new(node, Arc::new(legion_core::Local))
            .unwrap()
            .with_content_limits(ContentStreamLimits {
                concurrency: ConcurrencyLimits {
                    max_per_ticket: 2,
                    max_per_holder: 4,
                    max_global: 8,
                    retry_after_secs: 3,
                },
                write_idle_timeout: Duration::from_millis(350),
                min_rate_window: Duration::from_secs(5),
                min_rate_bytes: 40 * 1024,
                ..ContentStreamLimits::default()
            });

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let shutdown = CancellationToken::new();
        let stop = shutdown.clone();
        let server_task = tokio::spawn(async move {
            let _ = door.serve(listener, stop.cancelled()).await;
        });

        // 1. Mint a ticket via POST /v1/content.
        let body = serde_json::json!({ "path": big_file.to_string_lossy() }).to_string();
        let mut s = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        let req = format!(
            "POST /v1/content HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAuthorization: Bearer {raw_token}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        s.write_all(req.as_bytes()).await.unwrap();
        let mut resp_bytes = Vec::new();
        s.read_to_end(&mut resp_bytes).await.unwrap();
        let resp_text = String::from_utf8(resp_bytes).unwrap();
        let (_, json_body) = resp_text.split_once("\r\n\r\n").unwrap();
        let created: serde_json::Value = serde_json::from_str(json_body).unwrap();
        let content_url = created["url"].as_str().unwrap().to_owned();

        // 2. Open 2 stalled readers (filling max_per_ticket = 2) that send GET and never read.
        let mut stalled = Vec::new();
        for _ in 0..2 {
            let mut conn = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
            let get = format!(
                "GET {content_url} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n"
            );
            conn.write_all(get.as_bytes()).await.unwrap();
            stalled.push(conn);
        }
        tokio::time::sleep(Duration::from_millis(80)).await;

        // 3. A 3rd concurrent request gets 429 Too Many Requests + Retry-After + Connection: close.
        {
            let mut conn = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
            let get = format!(
                "GET {content_url} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n"
            );
            conn.write_all(get.as_bytes()).await.unwrap();
            let mut reply = Vec::new();
            conn.read_to_end(&mut reply).await.unwrap();
            let reply = String::from_utf8_lossy(&reply);
            assert!(reply.starts_with("HTTP/1.1 429 "), "{reply}");
            assert!(
                reply.to_ascii_lowercase().contains("retry-after: 3"),
                "{reply}"
            );
            assert!(
                reply.to_ascii_lowercase().contains("connection: close"),
                "{reply}"
            );
        }

        // 4. Meanwhile, GET /v1/health stays fast and responsive.
        {
            let mut conn = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
            let req = format!(
                "GET /v1/health HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n"
            );
            conn.write_all(req.as_bytes()).await.unwrap();
            let mut reply = Vec::new();
            conn.read_to_end(&mut reply).await.unwrap();
            let reply = String::from_utf8_lossy(&reply);
            assert!(reply.starts_with("HTTP/1.1 200 "), "{reply}");
        }

        // 5. After write_idle_timeout (350ms), the server drops the stalled connections and frees
        //    their permits so a fresh range read succeeds even while the stalled sockets remain open.
        tokio::time::sleep(Duration::from_millis(650)).await;
        {
            let mut conn = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
            let get = format!(
                "GET {content_url} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nRange: bytes=0-15\r\nConnection: close\r\n\r\n"
            );
            conn.write_all(get.as_bytes()).await.unwrap();
            let mut reply = Vec::new();
            conn.read_to_end(&mut reply).await.unwrap();
            let reply = String::from_utf8_lossy(&reply);
            assert!(
                reply.starts_with("HTTP/1.1 206 "),
                "expected stalled streams to time out and free slots: {reply}"
            );
        }

        drop(stalled);
        shutdown.cancel();
        server_task.await.unwrap();
    }

    #[tokio::test]
    async fn content_audit_coalescer_flushes_on_holder_revocation_and_graceful_shutdown() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("legion");
        let user_home = dir.path().join("user");
        std::fs::create_dir_all(home.join("config")).unwrap();
        std::fs::create_dir_all(home.join("state")).unwrap();
        std::fs::create_dir_all(&user_home).unwrap();

        let file_a = user_home.join("a.bin");
        let file_b = user_home.join("b.bin");
        std::fs::write(&file_a, vec![b'A'; 4096]).unwrap();
        std::fs::write(&file_b, vec![b'B'; 4096]).unwrap();

        let paths = legion_core::Paths::under(&home);
        std::fs::write(paths.config_file("node.toml"), "name = \"tower\"\n").unwrap();
        std::fs::write(
            paths.config_file("policy.toml"),
            "[principals.phone]\nrole = \"observer\"\n[roles.observer]\nallow = [\"effect:read\"]\n",
        )
        .unwrap();

        let node = Node::new(paths.clone(), legion_tools::registry());
        let tok_a = node
            .call(legion_core::Call::new(
                legion_core::Door::Cli,
                "token.add",
                serde_json::json!({ "name": "tok-a", "principal": "phone" }),
            ))
            .await
            .unwrap()["token"]
            .as_str()
            .unwrap()
            .to_owned();
        let tok_b = node
            .call(legion_core::Call::new(
                legion_core::Door::Cli,
                "token.add",
                serde_json::json!({ "name": "tok-b", "principal": "phone" }),
            ))
            .await
            .unwrap()["token"]
            .as_str()
            .unwrap()
            .to_owned();

        let door = HttpDoor::new(node.clone(), Arc::new(legion_core::Local)).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let shutdown = CancellationToken::new();
        let stop = shutdown.clone();
        let server_task = tokio::spawn(async move {
            let _ = door.serve(listener, stop.cancelled()).await;
        });

        async fn mint(port: u16, token: &str, path: &std::path::Path) -> String {
            let body = serde_json::json!({ "path": path.to_string_lossy() }).to_string();
            let mut s = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
            let req = format!(
                "POST /v1/content HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAuthorization: Bearer {token}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            s.write_all(req.as_bytes()).await.unwrap();
            let mut raw = Vec::new();
            s.read_to_end(&mut raw).await.unwrap();
            let text = String::from_utf8(raw).unwrap();
            let (_, json_body) = text.split_once("\r\n\r\n").unwrap();
            serde_json::from_str::<serde_json::Value>(json_body).unwrap()["url"]
                .as_str()
                .unwrap()
                .to_owned()
        }

        async fn get_range(port: u16, url: &str, range: &str) -> u16 {
            let mut s = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
            let req = format!(
                "GET {url} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nRange: {range}\r\nConnection: close\r\n\r\n"
            );
            s.write_all(req.as_bytes()).await.unwrap();
            let mut raw = Vec::new();
            s.read_to_end(&mut raw).await.unwrap();
            let line = String::from_utf8_lossy(&raw);
            line.split_whitespace()
                .nth(1)
                .unwrap()
                .parse::<u16>()
                .unwrap()
        }

        let read_audit = || -> Vec<serde_json::Value> {
            std::fs::read_to_string(paths.audit_log())
                .unwrap_or_default()
                .lines()
                .filter(|l| !l.trim().is_empty())
                .map(|l| serde_json::from_str(l).unwrap())
                .collect()
        };

        let url_a = mint(port, &tok_a, &file_a).await;
        let url_b = mint(port, &tok_b, &file_b).await;

        // Three range reads on each ticket: 100 + 200 + 300 = 600 bytes.
        for range in ["bytes=0-99", "bytes=100-299", "bytes=300-599"] {
            assert_eq!(get_range(port, &url_a, range).await, 206);
            assert_eq!(get_range(port, &url_b, range).await, 206);
        }

        // Revoking tok-a and hitting its ticket flushes tok-a's coalesced audit entry immediately
        // while the server is still running (without waiting for the 60s window).
        node.call(legion_core::Call::new(
            legion_core::Door::Cli,
            "token.remove",
            serde_json::json!({ "name": "tok-a" }),
        ))
        .await
        .unwrap();
        assert_eq!(get_range(port, &url_a, "bytes=0-9").await, 404);

        let mid_audit = read_audit();
        let a_reads: Vec<&serde_json::Value> = mid_audit
            .iter()
            .filter(|e| {
                e["tool"] == "content.read"
                    && e["outcome"] == "ok"
                    && e["args"]["path"] == file_a.to_string_lossy().as_ref()
            })
            .collect();
        assert_eq!(a_reads.len(), 1, "{mid_audit:?}");
        assert_eq!(a_reads[0]["args"]["requests"], serde_json::json!(3));
        assert_eq!(a_reads[0]["args"]["bytes_served"], serde_json::json!(600));

        // Graceful shutdown flushes tok-b's pending coalesced audit entry before serve() exits.
        shutdown.cancel();
        server_task.await.unwrap();

        let final_audit = read_audit();
        let b_reads: Vec<&serde_json::Value> = final_audit
            .iter()
            .filter(|e| {
                e["tool"] == "content.read"
                    && e["outcome"] == "ok"
                    && e["args"]["path"] == file_b.to_string_lossy().as_ref()
            })
            .collect();
        assert_eq!(b_reads.len(), 1, "{final_audit:?}");
        assert_eq!(b_reads[0]["args"]["requests"], serde_json::json!(3));
        assert_eq!(b_reads[0]["args"]["bytes_served"], serde_json::json!(600));
    }

    #[tokio::test]
    async fn monitored_tcp_stream_forwards_vectored_writes_and_exempts_slow_sources() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let client_fut = TcpStream::connect(addr);
        let accept_fut = listener.accept();
        let (client, server) = tokio::join!(client_fut, accept_fut);
        let mut client = client.unwrap();
        let (server, _) = server.unwrap();

        let active = Arc::new(AtomicUsize::new(0));
        let mut monitored = MonitoredTcpStream::new(
            server,
            Arc::clone(&active),
            CancellationToken::new(),
            Duration::from_secs(5),
            Duration::from_millis(120),
            64 * 1024,
        );
        assert!(
            monitored.is_write_vectored(),
            "MonitoredTcpStream must advertise vectored writes from TcpStream"
        );

        // Inactive connection forwards vectored writes directly.
        let slices = [
            io::IoSlice::new(b"HTTP/1.1 200 OK\r\n\r\n"),
            io::IoSlice::new(b"{\"ok\":true}"),
        ];
        let written = monitored.write_vectored(&slices).await.unwrap();
        assert_eq!(written, b"HTTP/1.1 200 OK\r\n\r\n{\"ok\":true}".len());
        let mut recv = vec![0u8; written];
        client.read_exact(&mut recv).await.unwrap();
        assert_eq!(recv, b"HTTP/1.1 200 OK\r\n\r\n{\"ok\":true}");

        // Active content stream with a slow source (1 KiB every 60ms over 420ms, far slower than
        // 64 KiB / 120ms) and a fast client must NEVER trip the minimum-rate timeout because the
        // client socket never blocks.
        active.store(1, Ordering::Release);
        let reader = tokio::spawn(async move {
            let mut total = 0usize;
            let mut buf = [0u8; 4096];
            loop {
                let n = client.read(&mut buf).await.unwrap();
                if n == 0 {
                    break;
                }
                total += n;
            }
            total
        });
        let chunk = [b'S'; 1024];
        for _ in 0..7 {
            monitored.write_all(&chunk).await.unwrap();
            tokio::time::sleep(Duration::from_millis(60)).await;
        }
        monitored.shutdown().await.unwrap();
        assert_eq!(reader.await.unwrap(), 7 * 1024);
    }

    #[tokio::test]
    async fn monitored_tcp_stream_disconnects_slow_trickling_client() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let client_fut = TcpStream::connect(addr);
        let accept_fut = listener.accept();
        let (client, server) = tokio::join!(client_fut, accept_fut);
        let mut client = client.unwrap();
        let (server, _) = server.unwrap();

        let active = Arc::new(AtomicUsize::new(1));
        // Long write_idle_timeout (5s) so only the minimum-rate rule (200ms / 512 KiB) can fire
        // while the client trickles 512 bytes every 50ms.
        let mut monitored = MonitoredTcpStream::new(
            server,
            active,
            CancellationToken::new(),
            Duration::from_secs(5),
            Duration::from_millis(200),
            512 * 1024,
        );

        let trickle = tokio::spawn(async move {
            let mut buf = [0u8; 512];
            for _ in 0..20 {
                tokio::time::sleep(Duration::from_millis(50)).await;
                if client.read(&mut buf).await.unwrap_or(0) == 0 {
                    break;
                }
            }
        });

        let payload = vec![b'X'; 64 * 1024];
        let mut err = None;
        for _ in 0..512 {
            match monitored.write_all(&payload).await {
                Ok(()) => {}
                Err(e) => {
                    err = Some(e);
                    break;
                }
            }
        }
        let err = err.expect("trickling client must be disconnected by minimum-rate rule");
        assert_eq!(err.kind(), io::ErrorKind::TimedOut, "{err}");
        trickle.abort();
    }
}
