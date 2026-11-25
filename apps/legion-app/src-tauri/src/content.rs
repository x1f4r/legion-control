//! Native in-app content server built on [`legion_content`].
//!
//! Serves media and file bytes from paired machines through [`legion_link::Link::open_content`]
//! behind single-file 256-bit tickets so `<img>`, `<video>`, and `<audio>` elements in the web
//! view play and seek straight from the machine without copying files into memory or exposing the
//! interface origin.
//!
//! - **Linux, macOS, iOS/iPadOS, and Windows** mount `legion-content` on a lazy HTTP/1.1 listener
//!   bound strictly to `127.0.0.1:0` (system-chosen ephemeral port), stopping automatically once
//!   no live tickets or active streams remain for [`DEFAULT_IDLE_SHUTDOWN`].
//! - **Android** enforces `android:usesCleartextTraffic="false"` in `AndroidManifest.xml`, which
//!   causes Android WebView to reject `http://127.0.0.1:<port>` with `ERR_CLEARTEXT_NOT_PERMITTED`.
//!   Android therefore mounts the same `legion-content` handler on the Tauri custom URI scheme
//!   `legion-content` (`http://legion-content.localhost/v1/content/<ticket>/<name>`), answering
//!   open-ended ranges in bounded [`CUSTOM_SCHEME_MAX_RANGE_BYTES`] pieces so `wry` never buffers
//!   whole videos into memory and no cleartext loopback port is opened on the device.

use std::collections::VecDeque;
use std::convert::Infallible;
use std::future::Future;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use http::header::{
    ACCESS_CONTROL_ALLOW_HEADERS, ACCESS_CONTROL_ALLOW_METHODS, ACCESS_CONTROL_ALLOW_ORIGIN,
    ACCESS_CONTROL_EXPOSE_HEADERS, CACHE_CONTROL, CONTENT_LENGTH, CONTENT_RANGE,
    CONTENT_SECURITY_POLICY, CONTENT_TYPE, HOST, HeaderName, HeaderValue, RANGE, REFERRER_POLICY,
    VARY, X_CONTENT_TYPE_OPTIONS,
};
use http::{HeaderMap, Method, Request, Response, StatusCode, Uri};
use http_body::Body as _;
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper_util::rt::TokioIo;
use legion_content::{
    CONTENT_SANDBOX_CSP, ContentBody, ContentLimiter, ContentSource, ContentTime, CorpPolicy,
    GuardRejection, HostOriginGuard, MAX_PATH_BYTES, OpenRangeError, OpenedRange, ServeOptions,
    StreamHooks, Ticket, TicketConfig, TicketRequest, TicketTable, encode_path_segment,
    extract_file_name, handle_request,
};
use legion_link::proto::{ErrorBody as ProtoErrorBody, ErrorCode as ProtoErrorCode};
use serde::{Deserialize, Serialize};
use tokio::net::TcpListener;
use tokio::sync::Mutex as AsyncMutex;
use tokio_util::sync::CancellationToken;

use crate::backend::{AppError, Backend, ErrorCode};

/// Custom URI scheme name registered with Tauri on Android.
pub const CUSTOM_SCHEME_NAME: &str = "legion-content";

/// Authority host `wry` uses on Android for [`CUSTOM_SCHEME_NAME`].
pub const CUSTOM_SCHEME_HOST: &str = "legion-content.localhost";

/// Maximum byte piece answered for one `Range` request over the buffered custom URI scheme (2 MiB).
pub const CUSTOM_SCHEME_MAX_RANGE_BYTES: u64 = 2 * 1024 * 1024;

/// Hard upper bound on a single buffered custom-scheme response body (64 MiB, for unranged images).
pub const MAX_CUSTOM_SCHEME_BODY_BYTES: usize = 64 * 1024 * 1024;

/// Idle duration after all tickets expire or are revoked before the loopback listener shuts down.
pub const DEFAULT_IDLE_SHUTDOWN: Duration = Duration::from_secs(60);

/// Maximum recent content requests retained for diagnostics (never stores ticket secrets).
const MAX_REQUEST_LOG: usize = 64;

const CROSS_ORIGIN_RESOURCE_POLICY: HeaderName =
    HeaderName::from_static("cross-origin-resource-policy");

/// Transport mount used to reach [`AppContentServer`] from the web view.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ContentMountMode {
    /// Loopback HTTP/1.1 listener on `127.0.0.1:0` (Linux, macOS, iOS/iPadOS, Windows).
    Loopback,
    /// Tauri custom URI scheme `legion-content` answering bounded range pieces (Android).
    CustomScheme,
}

impl ContentMountMode {
    /// Returns the content transport mount selected for the current target platform.
    pub const fn current_platform() -> Self {
        #[cfg(target_os = "android")]
        {
            Self::CustomScheme
        }
        #[cfg(not(target_os = "android"))]
        {
            Self::Loopback
        }
    }
}

/// Returns the app's own web view page origins for the current platform (never `"null"`).
pub fn platform_app_origins() -> Vec<String> {
    #[cfg(any(target_os = "android", target_os = "windows"))]
    {
        vec![
            "http://tauri.localhost".to_owned(),
            "https://tauri.localhost".to_owned(),
        ]
    }
    #[cfg(not(any(target_os = "android", target_os = "windows")))]
    {
        vec!["tauri://localhost".to_owned()]
    }
}

/// Result returned by the `content_url` command.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContentUrlResult {
    /// Ticketed content URL for `<img>`, `<video>`, or `<audio>`.
    pub url: String,
    /// Ticket sliding lifetime in seconds.
    pub expires_in: u64,
}

/// Redacted request diagnostic entry (never stores ticket identifiers or credentials).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContentRequestLog {
    /// HTTP method (`GET` or `HEAD`).
    pub method: String,
    /// File name segment from the URL.
    pub file_name: String,
    /// Request `Range` header when present.
    pub range: Option<String>,
    /// HTTP status code emitted.
    pub status: u16,
    /// Response `Content-Range` header when present.
    pub content_range: Option<String>,
    /// Response `Content-Length` header when present.
    pub content_length: Option<u64>,
}

/// Managed Tauri state wrapper for [`AppContentServer`].
pub struct SharedContentServer(pub Arc<AppContentServer>);

struct ListenerHandle {
    port: u16,
    shutdown: CancellationToken,
}

struct ActivityState {
    last_activity: Instant,
}

struct ActiveStreamGuard {
    active_streams: Arc<AtomicUsize>,
    activity: Arc<Mutex<ActivityState>>,
}

impl Drop for ActiveStreamGuard {
    fn drop(&mut self) {
        self.active_streams.fetch_sub(1, Ordering::AcqRel);
        self.activity
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .last_activity = Instant::now();
    }
}

struct LinkContentSource {
    backend: Arc<dyn Backend>,
}

impl ContentSource for LinkContentSource {
    fn open_range<'a>(
        &'a self,
        ticket: &'a Ticket,
        offset: u64,
        length: Option<u64>,
    ) -> Pin<Box<dyn Future<Output = Result<OpenedRange, OpenRangeError>> + Send + 'a>> {
        Box::pin(async move {
            if !self.backend.is_pairing_holder_live(&ticket.holder) {
                return Err(OpenRangeError::TicketRevoked);
            }
            let target = match self.backend.content_target(&ticket.machine) {
                Ok(target) => target,
                Err(_) => return Err(OpenRangeError::TicketRevoked),
            };
            if target.holder != ticket.holder {
                return Err(OpenRangeError::TicketRevoked);
            }

            let stream = target
                .link
                .open_content(&target.link_node, &ticket.path, offset, length)
                .await
                .map_err(map_link_open_error)?;

            Ok(OpenedRange {
                size: stream.size(),
                offset: stream.offset(),
                length: stream.length(),
                mtime: ContentTime::new(stream.mtime().secs, stream.mtime().nanos),
                file_id: stream.file_id(),
                reader: Box::new(stream),
            })
        })
    }

    fn check_live(&self, ticket: &Ticket) -> Result<(), OpenRangeError> {
        if !self.backend.is_pairing_holder_live(&ticket.holder) {
            return Err(OpenRangeError::TicketRevoked);
        }
        Ok(())
    }
}

fn map_link_open_error(err: ProtoErrorBody) -> OpenRangeError {
    if err.code == ProtoErrorCode::Conflict
        && let Some(actual) = err
            .details
            .as_ref()
            .and_then(|d| d.get("actual"))
            .and_then(serde_json::Value::as_u64)
    {
        return OpenRangeError::OutOfRange { size: actual };
    }
    OpenRangeError::Error(err)
}

/// Ticketed content server mounted either on `127.0.0.1:0` (lazy/idle-stopped) or on Android's
/// `legion-content` custom URI scheme.
pub struct AppContentServer {
    backend: Arc<dyn Backend>,
    mode: ContentMountMode,
    tickets: Arc<TicketTable>,
    limiter: ContentLimiter,
    source: Arc<LinkContentSource>,
    allowed_origins: Vec<String>,
    idle_shutdown: Duration,
    listener_lock: AsyncMutex<()>,
    listener: Mutex<Option<ListenerHandle>>,
    active_streams: Arc<AtomicUsize>,
    activity: Arc<Mutex<ActivityState>>,
    request_log: Mutex<VecDeque<ContentRequestLog>>,
}

impl AppContentServer {
    /// Creates a content server using the current platform's mount mode and default TTLs.
    pub fn new(backend: Arc<dyn Backend>) -> Self {
        Self::with_config(
            backend,
            ContentMountMode::current_platform(),
            TicketConfig::default(),
            DEFAULT_IDLE_SHUTDOWN,
            platform_app_origins(),
        )
    }

    /// Creates a content server with explicit mount mode, ticket config, idle shutdown timeout,
    /// and allowed page origins.
    pub fn with_config(
        backend: Arc<dyn Backend>,
        mode: ContentMountMode,
        ticket_config: TicketConfig,
        idle_shutdown: Duration,
        allowed_origins: Vec<String>,
    ) -> Self {
        let source = Arc::new(LinkContentSource {
            backend: Arc::clone(&backend),
        });
        Self {
            backend,
            mode,
            tickets: Arc::new(TicketTable::with_config(ticket_config)),
            limiter: ContentLimiter::default(),
            source,
            allowed_origins,
            idle_shutdown,
            listener_lock: AsyncMutex::new(()),
            listener: Mutex::new(None),
            active_streams: Arc::new(AtomicUsize::new(0)),
            activity: Arc::new(Mutex::new(ActivityState {
                last_activity: Instant::now(),
            })),
            request_log: Mutex::new(VecDeque::with_capacity(MAX_REQUEST_LOG)),
        }
    }

    /// Returns which content mount mode this server uses.
    pub fn mode(&self) -> ContentMountMode {
        self.mode
    }

    /// Returns the currently bound loopback TCP port, or `None` when no loopback listener is running.
    pub fn loopback_port(&self) -> Option<u16> {
        self.listener
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .as_ref()
            .map(|h| h.port)
    }

    /// Returns a snapshot of recent redacted content requests (never includes ticket secrets).
    pub fn recent_requests(&self) -> Vec<ContentRequestLog> {
        self.request_log
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .iter()
            .cloned()
            .collect()
    }

    /// Clears all recorded request diagnostics.
    pub fn clear_request_log(&self) {
        self.request_log
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clear();
    }

    /// Probes `path` on `machine` through `Link::open_content(..., 0, Some(0))`, issues a
    /// pairing-bound 256-bit ticket, starts the loopback listener lazily when needed, and returns
    /// `{ url, expires_in }`.
    pub async fn mint_url(
        self: &Arc<Self>,
        machine: &str,
        path: &str,
        download: bool,
    ) -> Result<ContentUrlResult, AppError> {
        let machine_trimmed = machine.trim();
        if machine_trimmed.is_empty() {
            return Err(
                AppError::new(ErrorCode::InvalidArgs, "Machine name cannot be empty.")
                    .with_hint("Pass the name of a paired machine."),
            );
        }
        if path.is_empty() {
            return Err(
                AppError::new(ErrorCode::InvalidArgs, "File path cannot be empty.")
                    .with_hint("Pass an absolute or `~`-prefixed file path."),
            );
        }
        if path.len() > MAX_PATH_BYTES {
            return Err(AppError::new(
                ErrorCode::InvalidArgs,
                format!("File path exceeds the {MAX_PATH_BYTES}-byte limit."),
            ));
        }
        extract_file_name(path).map_err(AppError::from)?;

        let target = self.backend.content_target(machine_trimmed)?;

        // Probe zero bytes before minting so missing files, directories, denied paths, or offline
        // machines return a structured AppError immediately.
        let probe = target
            .link
            .open_content(&target.link_node, path, 0, Some(0))
            .await
            .map_err(AppError::from)?;
        drop(probe);

        let issued = self
            .tickets
            .issue(TicketRequest {
                holder: target.holder,
                principal: target.principal,
                machine: target.machine,
                path: path.to_owned(),
            })
            .map_err(AppError::from)?;

        self.touch_activity();

        let encoded_name = encode_path_segment(&issued.file_name);
        let suffix = if download { "?download=1" } else { "" };
        let url = match self.mode {
            ContentMountMode::Loopback => {
                let port = self.ensure_loopback_listener().await?;
                format!(
                    "http://127.0.0.1:{port}/v1/content/{}/{encoded_name}{suffix}",
                    issued.id
                )
            }
            ContentMountMode::CustomScheme => {
                format!(
                    "http://{CUSTOM_SCHEME_HOST}/v1/content/{}/{encoded_name}{suffix}",
                    issued.id
                )
            }
        };

        Ok(ContentUrlResult {
            url,
            expires_in: issued.expires_in,
        })
    }

    /// Revokes every ticket belonging to pairings that are no longer live in [`Backend`].
    pub fn revoke_stale_pairings(&self) {
        let _ = self
            .tickets
            .retain_holders(|holder| self.backend.is_pairing_holder_live(holder));
    }

    /// Revokes every issued content ticket (used on store repair and app exit).
    pub fn revoke_all(&self) {
        let _ = self.tickets.retain_holders(|_| false);
    }

    /// Revokes every ticket and immediately stops any active loopback listener.
    pub fn shutdown(&self) {
        self.revoke_all();
        if let Some(handle) = self
            .listener
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take()
        {
            handle.shutdown.cancel();
        }
    }

    /// Handles one Tauri custom URI scheme request (`http://legion-content.localhost/v1/content/...`)
    /// with bounded range pieces and full [`HostOriginGuard`] + `legion-content` security headers.
    pub async fn handle_custom_scheme(&self, request: Request<Vec<u8>>) -> Response<Vec<u8>> {
        let method = request.method().clone();
        let uri = request.uri().clone();
        let mut headers = request.headers().clone();

        // Android WebView's WebResourceRequest omits the transport Host header while carrying the
        // full URI (`http://legion-content.localhost/...`). Enforce that the URI authority is
        // strictly our custom scheme host with no port or userinfo, and supply Host only if absent
        // so any mismatched explicit Host header is rejected by HostOriginGuard.
        let canonical_host = match (
            uri.scheme_str().map(str::to_ascii_lowercase).as_deref(),
            uri.authority().map(|a| a.as_str()),
        ) {
            // `wry` on Android rewrites `http://legion-content.localhost/...` to
            // `legion-content://localhost/...` before invoking the custom-scheme handler.
            (Some(CUSTOM_SCHEME_NAME), Some(auth)) if auth.eq_ignore_ascii_case("localhost") => {
                Some("localhost")
            }
            (Some("http" | "https"), Some(auth))
                if auth.eq_ignore_ascii_case(CUSTOM_SCHEME_HOST) =>
            {
                Some(CUSTOM_SCHEME_HOST)
            }
            _ => None,
        };
        let Some(expected_host) = canonical_host else {
            return collect_bounded_response(
                guard_error_response(GuardRejection::MisdirectedHost, CorpPolicy::CrossOrigin),
                MAX_CUSTOM_SCHEME_BODY_BYTES,
            )
            .await;
        };
        if !headers.contains_key(HOST) {
            headers.insert(HOST, HeaderValue::from_static(expected_host));
        }

        let guard = HostOriginGuard {
            port: 0,
            allowed_hosts: vec![expected_host.to_owned()],
            allowed_origins: self.allowed_origins.clone(),
            allow_same_origin: false,
        };
        let options = ServeOptions {
            corp: CorpPolicy::CrossOrigin,
            max_range_bytes: Some(CUSTOM_SCHEME_MAX_RANGE_BYTES),
            ..ServeOptions::default()
        };

        let response = self
            .handle_checked_request(&guard, &method, &uri, &headers, options)
            .await;
        collect_bounded_response(response, MAX_CUSTOM_SCHEME_BODY_BYTES).await
    }

    fn touch_activity(&self) {
        self.activity
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .last_activity = Instant::now();
    }

    async fn ensure_loopback_listener(self: &Arc<Self>) -> Result<u16, AppError> {
        if let Some(port) = self.loopback_port() {
            return Ok(port);
        }
        let _guard = self.listener_lock.lock().await;
        if let Some(port) = self.loopback_port() {
            return Ok(port);
        }

        let bind_addr = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0));
        let tcp = TcpListener::bind(bind_addr).await.map_err(|err| {
            AppError::new(
                ErrorCode::Internal,
                format!("Could not bind local content listener on 127.0.0.1: {err}"),
            )
        })?;
        let port = tcp
            .local_addr()
            .map_err(|err| {
                AppError::new(
                    ErrorCode::Internal,
                    format!("Could not inspect local content listener address: {err}"),
                )
            })?
            .port();

        let shutdown = CancellationToken::new();
        {
            let mut slot = self
                .listener
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            *slot = Some(ListenerHandle {
                port,
                shutdown: shutdown.clone(),
            });
        }
        self.touch_activity();

        let server = Arc::clone(self);
        let accept_shutdown = shutdown.clone();
        tokio::spawn(async move {
            server.run_accept_loop(tcp, port, accept_shutdown).await;
        });

        let reaper_server = Arc::clone(self);
        tokio::spawn(async move {
            reaper_server.run_idle_reaper(port, shutdown).await;
        });

        Ok(port)
    }

    async fn run_accept_loop(
        self: Arc<Self>,
        tcp: TcpListener,
        port: u16,
        shutdown: CancellationToken,
    ) {
        let guard = Arc::new(HostOriginGuard {
            port,
            allowed_hosts: Vec::new(),
            allowed_origins: self.allowed_origins.clone(),
            allow_same_origin: false,
        });
        let options = ServeOptions {
            corp: CorpPolicy::CrossOrigin,
            max_range_bytes: None,
            ..ServeOptions::default()
        };

        loop {
            let accepted = tokio::select! {
                biased;
                () = shutdown.cancelled() => break,
                res = tcp.accept() => res,
            };
            let Ok((stream, _peer)) = accepted else {
                continue;
            };
            let _ = stream.set_nodelay(true);
            let server = Arc::clone(&self);
            let conn_guard = Arc::clone(&guard);
            let conn_shutdown = shutdown.clone();
            tokio::spawn(async move {
                let io = TokioIo::new(stream);
                let service = service_fn(move |req: Request<Incoming>| {
                    let server = Arc::clone(&server);
                    let guard = Arc::clone(&conn_guard);
                    async move {
                        let method = req.method().clone();
                        let uri = req.uri().clone();
                        let headers = req.headers().clone();
                        let resp = server
                            .handle_checked_request(&guard, &method, &uri, &headers, options)
                            .await;
                        Ok::<Response<ContentBody>, Infallible>(resp)
                    }
                });
                let conn = http1::Builder::new()
                    .keep_alive(true)
                    .serve_connection(io, service);
                tokio::pin!(conn);
                tokio::select! {
                    biased;
                    () = conn_shutdown.cancelled() => {
                        conn.as_mut().graceful_shutdown();
                    }
                    _ = &mut conn => {}
                }
            });
        }
    }

    async fn run_idle_reaper(self: Arc<Self>, port: u16, shutdown: CancellationToken) {
        let tick = (self.idle_shutdown / 4)
            .max(Duration::from_millis(25))
            .min(Duration::from_secs(15));
        loop {
            tokio::select! {
                biased;
                () = shutdown.cancelled() => return,
                () = tokio::time::sleep(tick) => {}
            }
            self.revoke_stale_pairings();
            if self.active_streams.load(Ordering::Acquire) > 0 {
                continue;
            }
            if !self.tickets.is_empty() {
                continue;
            }
            let idle_for = self
                .activity
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .last_activity
                .elapsed();
            if idle_for < self.idle_shutdown {
                continue;
            }
            let mut slot = self
                .listener
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if slot.as_ref().is_some_and(|h| h.port == port)
                && self.active_streams.load(Ordering::Acquire) == 0
                && self.tickets.is_empty()
                && let Some(handle) = slot.take()
            {
                handle.shutdown.cancel();
                return;
            }
        }
    }

    async fn handle_checked_request(
        &self,
        guard: &HostOriginGuard,
        method: &Method,
        uri: &Uri,
        headers: &HeaderMap,
        options: ServeOptions,
    ) -> Response<ContentBody> {
        self.touch_activity();

        let cors_origin = match guard.check(headers, uri) {
            Ok(origin) => origin,
            Err(rejection) => return guard_error_response(rejection, options.corp),
        };

        if *method == Method::OPTIONS {
            return preflight_response(cors_origin, options.corp);
        }

        let Some((ticket_id, file_name)) = parse_content_path(uri.path()) else {
            return not_found_response(options.corp, cors_origin);
        };

        self.active_streams.fetch_add(1, Ordering::AcqRel);
        let active_guard = ActiveStreamGuard {
            active_streams: Arc::clone(&self.active_streams),
            activity: Arc::clone(&self.activity),
        };
        let hooks = StreamHooks {
            limiter: Some(self.limiter.clone()),
            bytes_served: None,
            active_guard: Some(Box::new(active_guard)),
        };

        let mut response = handle_request(
            &self.tickets,
            &self.source,
            method,
            ticket_id,
            file_name,
            uri.query(),
            headers,
            options,
            hooks,
        )
        .await;

        if let Some(origin) = cors_origin {
            apply_cors_headers(response.headers_mut(), origin);
        }

        self.record_request_log(method, file_name, headers, &response);
        response
    }

    fn record_request_log(
        &self,
        method: &Method,
        file_name: &str,
        req_headers: &HeaderMap,
        response: &Response<ContentBody>,
    ) {
        let range = req_headers
            .get(RANGE)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        let status = response.status().as_u16();
        let content_range = response
            .headers()
            .get(CONTENT_RANGE)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        let content_length = response
            .headers()
            .get(CONTENT_LENGTH)
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.parse::<u64>().ok());

        let entry = ContentRequestLog {
            method: method.as_str().to_owned(),
            file_name: file_name.to_owned(),
            range: range.clone(),
            status,
            content_range: content_range.clone(),
            content_length,
        };
        {
            let mut log = self
                .request_log
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if log.len() >= MAX_REQUEST_LOG {
                log.pop_front();
            }
            log.push_back(entry);
        }

        #[cfg(target_os = "android")]
        log::info!(
            target: "legion-content",
            "legion-content: {} {} range={} -> {} content-range={} len={}",
            method.as_str(),
            file_name,
            range.as_deref().unwrap_or("none"),
            status,
            content_range.as_deref().unwrap_or("none"),
            content_length.map_or_else(|| "none".to_owned(), |n| n.to_string()),
        );
        #[cfg(not(target_os = "android"))]
        if std::env::var_os("LEGION_CONTENT_LOG").is_some_and(|v| v == "1") {
            eprintln!(
                "legion-content: {} {} range={} -> {} content-range={} len={}",
                method.as_str(),
                file_name,
                range.as_deref().unwrap_or("none"),
                status,
                content_range.as_deref().unwrap_or("none"),
                content_length.map_or_else(|| "none".to_owned(), |n| n.to_string()),
            );
        }
    }
}

fn parse_content_path(path: &str) -> Option<(&str, &str)> {
    let rest = path.strip_prefix("/v1/content/")?;
    let (ticket_id, file_name) = rest.split_once('/')?;
    if ticket_id.is_empty() || file_name.is_empty() || file_name.contains('/') {
        return None;
    }
    Some((ticket_id, file_name))
}

fn apply_base_security_headers(headers: &mut HeaderMap, corp: CorpPolicy) {
    headers.insert(X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    headers.insert(
        CROSS_ORIGIN_RESOURCE_POLICY,
        match corp {
            CorpPolicy::SameOrigin => HeaderValue::from_static("same-origin"),
            CorpPolicy::CrossOrigin => HeaderValue::from_static("cross-origin"),
        },
    );
    headers.insert(REFERRER_POLICY, HeaderValue::from_static("no-referrer"));
    headers.insert(CACHE_CONTROL, HeaderValue::from_static("private, no-store"));
    headers.insert(
        CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(CONTENT_SANDBOX_CSP),
    );
}

fn apply_cors_headers(headers: &mut HeaderMap, origin: HeaderValue) {
    headers.insert(ACCESS_CONTROL_ALLOW_ORIGIN, origin);
    headers.insert(
        ACCESS_CONTROL_EXPOSE_HEADERS,
        HeaderValue::from_static(
            "Content-Length, Content-Range, Accept-Ranges, ETag, Content-Disposition",
        ),
    );
    headers.insert(VARY, HeaderValue::from_static("Origin"));
}

fn preflight_response(origin: Option<HeaderValue>, corp: CorpPolicy) -> Response<ContentBody> {
    let mut response = Response::new(ContentBody::empty());
    *response.status_mut() = StatusCode::NO_CONTENT;
    apply_base_security_headers(response.headers_mut(), corp);
    if let Some(origin) = origin {
        apply_cors_headers(response.headers_mut(), origin);
        response.headers_mut().insert(
            ACCESS_CONTROL_ALLOW_METHODS,
            HeaderValue::from_static("GET, HEAD, OPTIONS"),
        );
        response.headers_mut().insert(
            ACCESS_CONTROL_ALLOW_HEADERS,
            HeaderValue::from_static("Range, If-Range"),
        );
    }
    response
}

fn guard_error_response(rejection: GuardRejection, corp: CorpPolicy) -> Response<ContentBody> {
    let (status, code, message) = match rejection {
        GuardRejection::Malformed(msg) => {
            (StatusCode::BAD_REQUEST, ProtoErrorCode::InvalidArgs, msg)
        }
        GuardRejection::MisdirectedHost => (
            StatusCode::MISDIRECTED_REQUEST,
            ProtoErrorCode::Denied,
            "The request Host header does not match this content listener.",
        ),
        GuardRejection::ForbiddenOrigin => (
            StatusCode::FORBIDDEN,
            ProtoErrorCode::Denied,
            "The request Origin header is not allowed to access this content listener.",
        ),
    };
    let bytes = serde_json::to_vec(&ProtoErrorBody::new(code, message)).unwrap_or_default();
    let len = bytes.len() as u64;
    let mut response = Response::new(ContentBody::from_bytes(bytes));
    *response.status_mut() = status;
    apply_base_security_headers(response.headers_mut(), corp);
    response
        .headers_mut()
        .insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    response
        .headers_mut()
        .insert(CONTENT_LENGTH, HeaderValue::from(len));
    response
}

fn not_found_response(corp: CorpPolicy, cors_origin: Option<HeaderValue>) -> Response<ContentBody> {
    let bytes = serde_json::to_vec(&ProtoErrorBody::new(
        ProtoErrorCode::NotFound,
        "Content path not found.",
    ))
    .unwrap_or_default();
    let len = bytes.len() as u64;
    let mut response = Response::new(ContentBody::from_bytes(bytes));
    *response.status_mut() = StatusCode::NOT_FOUND;
    apply_base_security_headers(response.headers_mut(), corp);
    response
        .headers_mut()
        .insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    response
        .headers_mut()
        .insert(CONTENT_LENGTH, HeaderValue::from(len));
    if let Some(origin) = cors_origin {
        apply_cors_headers(response.headers_mut(), origin);
    }
    response
}

async fn collect_bounded_response(
    response: Response<ContentBody>,
    max_bytes: usize,
) -> Response<Vec<u8>> {
    let (parts, mut body) = response.into_parts();
    let mut out = Vec::new();
    let mut pinned = Pin::new(&mut body);
    while let Some(frame) = std::future::poll_fn(|cx| pinned.as_mut().poll_frame(cx)).await {
        match frame {
            Ok(frame) => {
                if let Ok(data) = frame.into_data() {
                    if out.len().saturating_add(data.len()) > max_bytes {
                        let msg = serde_json::to_vec(&ProtoErrorBody::new(
                            ProtoErrorCode::Unsupported,
                            "Unranged custom-scheme response exceeds the in-memory size limit.",
                        ))
                        .unwrap_or_default();
                        let len = msg.len() as u64;
                        let mut err_resp = Response::new(msg);
                        *err_resp.status_mut() = StatusCode::PAYLOAD_TOO_LARGE;
                        apply_base_security_headers(
                            err_resp.headers_mut(),
                            CorpPolicy::CrossOrigin,
                        );
                        err_resp
                            .headers_mut()
                            .insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
                        err_resp
                            .headers_mut()
                            .insert(CONTENT_LENGTH, HeaderValue::from(len));
                        return err_resp;
                    }
                    out.extend_from_slice(&data);
                }
            }
            Err(_) => {
                let msg = serde_json::to_vec(&ProtoErrorBody::new(
                    ProtoErrorCode::Failed,
                    "Content stream ended before completing.",
                ))
                .unwrap_or_default();
                let len = msg.len() as u64;
                let mut err_resp = Response::new(msg);
                *err_resp.status_mut() = StatusCode::BAD_GATEWAY;
                apply_base_security_headers(err_resp.headers_mut(), CorpPolicy::CrossOrigin);
                err_resp
                    .headers_mut()
                    .insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
                err_resp
                    .headers_mut()
                    .insert(CONTENT_LENGTH, HeaderValue::from(len));
                return err_resp;
            }
        }
    }
    Response::from_parts(parts, out)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::collections::HashMap;
    use std::sync::atomic::{AtomicBool, Ordering};

    use legion_link::proto::{
        ErrorBody, ErrorCode as ProtoCode, HelloResult, PROTOCOL, Response as ProtoResp, VERSION,
    };
    use legion_link::{
        Link, LinkConfig, LinkOptions, NodeLink, RawStream, Session, SessionOpener, SshTarget,
        StreamOpenError, StreamOpener, TransportHandle,
    };
    use serde_json::{Value, json};
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
    use tokio::net::TcpStream;

    use super::*;
    use crate::backend::{
        AppState, BoxFuture, CallId, CallRequest, CancelResult, ContentTarget, LearnResult,
        RepairResult, UnpairResult,
    };

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

    struct MemoryFilesOpener {
        files: Arc<HashMap<String, Vec<u8>>>,
    }

    impl StreamOpener for MemoryFilesOpener {
        fn open_stream(
            &self,
            _cancel: CancellationToken,
        ) -> Pin<Box<dyn Future<Output = Result<RawStream, StreamOpenError>> + Send + '_>> {
            let files = Arc::clone(&self.files);
            Box::pin(async move {
                let (client_in, mut server_out) = tokio::io::duplex(64 * 1024);
                let (server_in, client_out) = tokio::io::duplex(64 * 1024);
                let cancel = CancellationToken::new();
                tokio::spawn(async move {
                    let mut reader = BufReader::new(server_in);
                    let mut line = String::new();
                    if reader.read_line(&mut line).await.unwrap_or(0) == 0 {
                        return;
                    }
                    let req: Value = serde_json::from_str(&line).unwrap();
                    let _ = server_out.write_all(b"{\"protocol\":1}\n").await;
                    let path = req["path"].as_str().unwrap_or("");
                    let Some(payload) = files.get(path) else {
                        let err = json!({
                            "error": ErrorBody::new(ProtoCode::NotFound, "file not found")
                        });
                        let _ = server_out.write_all(format!("{err}\n").as_bytes()).await;
                        return;
                    };
                    let offset = req["offset"].as_u64().unwrap_or(0) as usize;
                    let req_len = req
                        .get("length")
                        .and_then(Value::as_u64)
                        .map(|n| n as usize);
                    if offset > payload.len() {
                        let err = json!({
                            "error": ErrorBody::new(ProtoCode::Conflict, "offset past end")
                                .with_details(json!({"actual": payload.len()}))
                        });
                        let _ = server_out.write_all(format!("{err}\n").as_bytes()).await;
                        return;
                    }
                    let avail = payload.len() - offset;
                    let len = req_len.map_or(avail, |n| n.min(avail));
                    let ready = json!({
                        "size": payload.len(),
                        "offset": offset,
                        "length": len,
                        "mtime": { "secs": 1_700_000_000i64, "nanos": 0u32 },
                        "file_id": 77u64,
                    });
                    let _ = server_out.write_all(format!("{ready}\n").as_bytes()).await;
                    let _ = server_out.write_all(&payload[offset..offset + len]).await;
                    let _ = server_out.shutdown().await;
                });
                Ok(RawStream {
                    reader: Box::new(client_in),
                    writer: Box::new(client_out),
                    stderr: None,
                    cancel,
                    guard: None,
                })
            })
        }
    }

    struct MockContentBackend {
        link: Link,
        live: Arc<AtomicBool>,
        holder: String,
    }

    impl MockContentBackend {
        fn new(files: HashMap<String, Vec<u8>>) -> Self {
            let opener_files = Arc::new(MemoryFilesOpener {
                files: Arc::new(files),
            });
            let session_opener: SessionOpener = Arc::new(move |entry, _, cancel| {
                let opener_files = Arc::clone(&opener_files);
                Box::pin(async move {
                    let (client_in, mut server_out) = tokio::io::duplex(16 * 1024);
                    let (server_in, client_out) = tokio::io::duplex(16 * 1024);
                    tokio::spawn(async move {
                        let mut reader = BufReader::new(server_in);
                        let mut line = String::new();
                        while reader.read_line(&mut line).await.unwrap_or(0) > 0 {
                            let req: legion_link::proto::Request =
                                serde_json::from_str(&line).unwrap();
                            let reply = ProtoResp::ok(
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
                            opener: opener_files,
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
            let cfg = LinkConfig {
                nodes: vec![NodeLink::ssh(
                    "tower",
                    SshTarget {
                        hosts: vec!["192.0.2.1".to_owned()],
                        port: 22,
                        user: "me".to_owned(),
                        host_keys: vec!["ssh-ed25519 AAAA".to_owned()],
                    },
                    Some("phone".to_owned()),
                )],
                ..LinkConfig::default()
            };
            Self {
                link: Link::with_opener(cfg, LinkOptions::default(), session_opener),
                live: Arc::new(AtomicBool::new(true)),
                holder: "pairing:tower:1".to_owned(),
            }
        }
    }

    impl Backend for MockContentBackend {
        fn state(&self) -> BoxFuture<'_, Result<AppState, AppError>> {
            Box::pin(async { Err(AppError::not_paired()) })
        }
        fn tools(&self, _on: Option<Value>) -> BoxFuture<'_, Result<Vec<Value>, AppError>> {
            Box::pin(async { Err(AppError::not_paired()) })
        }
        fn call(
            &self,
            _request: CallRequest,
            _on_event: Option<tauri::ipc::Channel<Value>>,
        ) -> BoxFuture<'_, Result<Value, AppError>> {
            Box::pin(async { Err(AppError::not_paired()) })
        }
        fn cancel(&self, _id: CallId) -> BoxFuture<'_, Result<CancelResult, AppError>> {
            Box::pin(async { Err(AppError::not_paired()) })
        }
        fn pair(
            &self,
            _invite: String,
            _name: String,
        ) -> BoxFuture<'_, Result<AppState, AppError>> {
            Box::pin(async { Err(AppError::not_paired()) })
        }
        fn unpair(&self, _node: String) -> BoxFuture<'_, Result<UnpairResult, AppError>> {
            Box::pin(async { Err(AppError::not_paired()) })
        }
        fn select_node(&self, _node: String) -> BoxFuture<'_, Result<AppState, AppError>> {
            Box::pin(async { Err(AppError::not_paired()) })
        }
        fn repair_store(&self) -> BoxFuture<'_, Result<RepairResult, AppError>> {
            Box::pin(async { Err(AppError::not_paired()) })
        }
        fn invalidate_sessions(&self) -> BoxFuture<'_, Result<(), AppError>> {
            Box::pin(async { Ok(()) })
        }
        fn learn(&self) -> BoxFuture<'_, Result<LearnResult, AppError>> {
            Box::pin(async { Err(AppError::not_paired()) })
        }
        fn accept_identity_change(
            &self,
            _node: String,
            _fingerprints: Vec<String>,
        ) -> BoxFuture<'_, Result<AppState, AppError>> {
            Box::pin(async { Err(AppError::not_paired()) })
        }
        fn dismiss_identity_change(
            &self,
            _node: String,
        ) -> BoxFuture<'_, Result<AppState, AppError>> {
            Box::pin(async { Err(AppError::not_paired()) })
        }
        fn content_target(&self, machine: &str) -> Result<ContentTarget, AppError> {
            if !self.live.load(Ordering::Acquire) || machine != "tower" {
                return Err(AppError::new(ErrorCode::NotFound, "Machine not paired."));
            }
            Ok(ContentTarget {
                link: self.link.clone(),
                link_node: "tower".to_owned(),
                machine: "tower".to_owned(),
                principal: "phone".to_owned(),
                holder: self.holder.clone(),
            })
        }
        fn is_pairing_holder_live(&self, holder: &str) -> bool {
            self.live.load(Ordering::Acquire) && holder == self.holder
        }
    }

    async fn raw_http(port: u16, request_text: &str) -> (u16, String, Vec<u8>) {
        let mut stream = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        stream.write_all(request_text.as_bytes()).await.unwrap();
        stream.flush().await.unwrap();
        let mut buf = Vec::new();
        stream.read_to_end(&mut buf).await.unwrap();
        let split = buf
            .windows(4)
            .position(|w| w == b"\r\n\r\n")
            .expect("HTTP response header separator");
        let head = String::from_utf8_lossy(&buf[..split]).into_owned();
        let status: u16 = head
            .lines()
            .next()
            .unwrap()
            .split_whitespace()
            .nth(1)
            .unwrap()
            .parse()
            .unwrap();
        (status, head, buf[split + 4..].to_vec())
    }

    #[tokio::test]
    async fn guard_rejects_wrong_host_wrong_port_dns_rebinding_wrong_origin_and_null_origin() {
        let mut files = HashMap::new();
        files.insert(
            "/home/me/photo.png".to_owned(),
            b"PNG-DATA-1234567890".to_vec(),
        );
        let backend = Arc::new(MockContentBackend::new(files));
        let server = Arc::new(AppContentServer::with_config(
            backend,
            ContentMountMode::Loopback,
            TicketConfig::default(),
            Duration::from_secs(30),
            vec!["tauri://localhost".to_owned()],
        ));
        assert!(
            server.loopback_port().is_none(),
            "listener must start lazily"
        );

        let minted = server
            .mint_url("tower", "/home/me/photo.png", false)
            .await
            .unwrap();
        let port = server
            .loopback_port()
            .expect("listener should now be bound");
        let path = minted
            .url
            .strip_prefix(&format!("http://127.0.0.1:{port}"))
            .unwrap();

        // 1) Valid Host, no Origin (typical <img>/<video>) -> 200 OK with CORP: cross-origin + sandbox CSP.
        let (status, head, body) = raw_http(
            port,
            &format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n"),
        )
        .await;
        assert_eq!(status, 200, "{head}");
        assert_eq!(body, b"PNG-DATA-1234567890");
        let lower = head.to_ascii_lowercase();
        assert!(
            lower.contains("cross-origin-resource-policy: cross-origin"),
            "{head}"
        );
        assert!(lower.contains("x-content-type-options: nosniff"), "{head}");
        assert!(lower.contains("frame-ancestors 'none'"), "{head}");

        // 2) Valid Host + allowed platform Origin -> 200 OK with CORS echo.
        let (status, head, _) = raw_http(
            port,
            &format!(
                "GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nOrigin: tauri://localhost\r\nConnection: close\r\n\r\n"
            ),
        )
        .await;
        assert_eq!(status, 200, "{head}");
        assert!(
            head.to_ascii_lowercase()
                .contains("access-control-allow-origin: tauri://localhost"),
            "{head}"
        );

        // 3) Wrong port -> 421 Misdirected Request.
        let wrong_port = if port == 65535 { port - 1 } else { port + 1 };
        let (status, _, _) = raw_http(
            port,
            &format!(
                "GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{wrong_port}\r\nConnection: close\r\n\r\n"
            ),
        )
        .await;
        assert_eq!(status, 421);

        // 4) DNS-rebinding style Host headers and 0.0.0.0 -> 421 Misdirected Request.
        for bad_host in [
            format!("evil.example:{port}"),
            format!("127.0.0.1.evil.example:{port}"),
            format!("0.0.0.0:{port}"),
            "evil.example".to_owned(),
        ] {
            let (status, _, _) = raw_http(
                port,
                &format!("GET {path} HTTP/1.1\r\nHost: {bad_host}\r\nConnection: close\r\n\r\n"),
            )
            .await;
            assert_eq!(status, 421, "expected 421 for Host: {bad_host}");
        }

        // 5) Wrong Origin and `Origin: null` -> 403 Forbidden.
        for bad_origin in [
            "https://evil.example",
            "http://evil.example",
            "null",
            "http://127.0.0.1:9999",
            &format!("http://127.0.0.1:{port}"),
        ] {
            let (status, _, _) = raw_http(
                port,
                &format!(
                    "GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nOrigin: {bad_origin}\r\nConnection: close\r\n\r\n"
                ),
            )
            .await;
            assert_eq!(status, 403, "expected 403 for Origin: {bad_origin}");
        }

        // 6) Range request -> 206 Partial Content.
        let (status, head, body) = raw_http(
            port,
            &format!(
                "GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nRange: bytes=4-8\r\nConnection: close\r\n\r\n"
            ),
        )
        .await;
        assert_eq!(status, 206, "{head}");
        assert_eq!(body, b"DATA-");

        server.shutdown();
    }

    #[tokio::test]
    async fn pairing_removal_kills_tickets_and_idle_listener_stops_and_restarts() {
        let mut files = HashMap::new();
        files.insert("/home/me/clip.webm".to_owned(), vec![0x42u8; 4096]);
        let backend = Arc::new(MockContentBackend::new(files));
        let live = Arc::clone(&backend.live);
        let server = Arc::new(AppContentServer::with_config(
            backend,
            ContentMountMode::Loopback,
            TicketConfig {
                sliding_ttl: Duration::from_millis(120),
                absolute_ttl: Duration::from_secs(10),
                ..TicketConfig::default()
            },
            Duration::from_millis(80),
            vec!["tauri://localhost".to_owned()],
        ));

        let minted = server
            .mint_url("tower", "/home/me/clip.webm", false)
            .await
            .unwrap();
        let port1 = server.loopback_port().unwrap();
        let path = minted
            .url
            .strip_prefix(&format!("http://127.0.0.1:{port1}"))
            .unwrap();

        // Removing the pairing immediately invalidates the ticket on the next request.
        live.store(false, Ordering::Release);
        server.revoke_stale_pairings();
        let (status, _, _) = raw_http(
            port1,
            &format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port1}\r\nConnection: close\r\n\r\n"),
        )
        .await;
        assert_eq!(status, 404);

        // Once tickets are gone and idle_shutdown elapses, the listener stops automatically.
        for _ in 0..30 {
            if server.loopback_port().is_none() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
        assert!(
            server.loopback_port().is_none(),
            "loopback listener should stop when idle with no live tickets"
        );

        // Re-enabling pairing and minting a new ticket lazily restarts the listener.
        live.store(true, Ordering::Release);
        let minted2 = server
            .mint_url("tower", "/home/me/clip.webm", true)
            .await
            .unwrap();
        assert!(minted2.url.ends_with("?download=1"));
        assert!(server.loopback_port().is_some());
        server.shutdown();
        assert!(server.loopback_port().is_none());
    }

    #[tokio::test]
    async fn custom_scheme_enforces_guard_and_clamps_open_ended_ranges() {
        let mut files = HashMap::new();
        let video: Vec<u8> = (0..(CUSTOM_SCHEME_MAX_RANGE_BYTES as usize + 50_000))
            .map(|i| (i % 251) as u8)
            .collect();
        files.insert("/home/me/video.webm".to_owned(), video.clone());
        let backend = Arc::new(MockContentBackend::new(files));
        let server = Arc::new(AppContentServer::with_config(
            backend,
            ContentMountMode::CustomScheme,
            TicketConfig::default(),
            Duration::from_secs(30),
            vec!["http://tauri.localhost".to_owned()],
        ));

        let minted = server
            .mint_url("tower", "/home/me/video.webm", false)
            .await
            .unwrap();
        assert!(
            minted
                .url
                .starts_with("http://legion-content.localhost/v1/content/"),
            "got {}",
            minted.url
        );
        assert!(server.loopback_port().is_none());

        // Open-ended range `bytes=0-` returns a bounded 2 MiB 206 Partial Content piece.
        let req = Request::builder()
            .method(Method::GET)
            .uri(&minted.url)
            .header(RANGE, "bytes=0-")
            .body(Vec::new())
            .unwrap();
        let resp = server.handle_custom_scheme(req).await;
        assert_eq!(resp.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(resp.body().len() as u64, CUSTOM_SCHEME_MAX_RANGE_BYTES);
        assert_eq!(
            resp.body(),
            &video[..CUSTOM_SCHEME_MAX_RANGE_BYTES as usize]
        );
        assert_eq!(
            resp.headers().get(CONTENT_RANGE).unwrap(),
            &format!(
                "bytes 0-{}/{}",
                CUSTOM_SCHEME_MAX_RANGE_BYTES - 1,
                video.len()
            )
        );

        // Android `wry` rewrites `http://legion-content.localhost/...` into
        // `legion-content://localhost/...` before calling the custom-scheme handler.
        let wry_rewritten = minted.url.replacen(
            "http://legion-content.localhost",
            "legion-content://localhost",
            1,
        );
        let req = Request::builder()
            .method(Method::GET)
            .uri(&wry_rewritten)
            .header(RANGE, "bytes=0-1023")
            .body(Vec::new())
            .unwrap();
        let resp = server.handle_custom_scheme(req).await;
        assert_eq!(resp.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(resp.body().len(), 1024);

        // Origin: null and evil origins are refused with 403.
        for bad_origin in ["null", "https://evil.example"] {
            let req = Request::builder()
                .method(Method::GET)
                .uri(&minted.url)
                .header("origin", bad_origin)
                .body(Vec::new())
                .unwrap();
            let resp = server.handle_custom_scheme(req).await;
            assert_eq!(resp.status(), StatusCode::FORBIDDEN);
        }

        // Wrong URI authority / Host spoofing is refused with 421.
        for bad_uri in [
            "http://evil.example/v1/content/abc/video.webm",
            "legion-content://evil.example/v1/content/abc/video.webm",
            "legion-content://localhost:8080/v1/content/abc/video.webm",
        ] {
            let spoof = Request::builder()
                .method(Method::GET)
                .uri(bad_uri)
                .body(Vec::new())
                .unwrap();
            let resp = server.handle_custom_scheme(spoof).await;
            assert_eq!(
                resp.status(),
                StatusCode::MISDIRECTED_REQUEST,
                "uri={bad_uri}"
            );
        }
    }
}
