//! Native in-app content server built on [`legion_content`].
//!
//! Serves media and file bytes from paired machines through [`legion_link::Link::open_content`]
//! behind single-file 256-bit tickets so `<img>`, `<video>`, and `<audio>` elements in the web
//! view play and seek straight from the machine without copying files into memory or exposing the
//! interface origin.
//!
//! All platforms (Linux, macOS, iOS/iPadOS, Windows, and Android) mount `legion-content` on a lazy
//! HTTP/1.1 listener bound strictly to `127.0.0.1:0` (system-chosen ephemeral port), stopping
//! automatically once no live tickets or active streams remain for [`DEFAULT_IDLE_SHUTDOWN`]. On
//! Android, `res/xml/network_security_config.xml` denies cleartext traffic everywhere except the
//! loopback literal `127.0.0.1`, allowing WebView to stream and seek arbitrarily large media with
//! flat memory while keeping cleartext blocked for every other host.

use std::collections::VecDeque;
use std::convert::Infallible;
use std::future::Future;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
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
use hyper_util::rt::{TokioIo, TokioTimer};
use legion_content::{
    CONTENT_SANDBOX_CSP, ContentBody, ContentLimiter, ContentSource, ContentTime, CorpPolicy,
    GuardRejection, HostOriginGuard, MAX_PATH_BYTES, OpenRangeError, OpenedRange, ServeOptions,
    StreamHooks, Ticket, TicketConfig, TicketRequest, TicketTable, encode_path_segment,
    extract_file_name, handle_request,
};
use legion_link::proto::{ErrorBody as ProtoErrorBody, ErrorCode as ProtoErrorCode};
use serde::{Deserialize, Serialize};
use tokio::net::TcpListener;
use tokio::sync::{Mutex as AsyncMutex, Semaphore};
use tokio_util::sync::CancellationToken;

use crate::backend::{AppError, Backend, ErrorCode};

/// Custom URI scheme name registered with Tauri on Android.
pub const CUSTOM_SCHEME_NAME: &str = "legion-content";

/// Authority host `wry` uses on Android for [`CUSTOM_SCHEME_NAME`].
pub const CUSTOM_SCHEME_HOST: &str = "legion-content.localhost";

/// Maximum byte piece answered for one `Range` request over the buffered custom URI scheme (2 MiB).
pub const CUSTOM_SCHEME_MAX_RANGE_BYTES: u64 = 2 * 1024 * 1024;

/// Maximum unranged response body served over the buffered custom URI scheme (16 MiB). Larger
/// unranged requests are rejected with `413 Payload Too Large` before reading a single byte.
pub const CUSTOM_SCHEME_MAX_FULL_BYTES: u64 = 16 * 1024 * 1024;

/// Maximum total in-flight buffered memory across concurrent custom-scheme responses (24 MiB).
pub const CUSTOM_SCHEME_TOTAL_MEMORY_BUDGET_BYTES: usize = 24 * 1024 * 1024;

/// Accounting unit for the custom-scheme memory budget semaphore (64 KiB).
const CUSTOM_SCHEME_BUDGET_UNIT_BYTES: usize = 64 * 1024;

/// Maximum simultaneous TCP connections accepted by the loopback content listener.
pub const MAX_LOOPBACK_CONNECTIONS: usize = 32;

/// Maximum time a loopback client may take to send HTTP request headers (5 seconds).
pub const LOOPBACK_HEADER_READ_TIMEOUT: Duration = Duration::from_secs(5);

/// Initial exponential backoff delay after a transient `accept()` error.
const ACCEPT_BACKOFF_INITIAL: Duration = Duration::from_millis(25);

/// Maximum exponential backoff delay between `accept()` retries.
const ACCEPT_BACKOFF_MAX: Duration = Duration::from_millis(500);

/// Consecutive `accept()` errors before tearing down a dead loopback listener so it can rebind.
const MAX_CONSECUTIVE_ACCEPT_ERRORS: u32 = 8;

/// Idle duration after all tickets expire or are revoked before the loopback listener shuts down.
pub const DEFAULT_IDLE_SHUTDOWN: Duration = Duration::from_secs(60);

/// Maximum recent content requests retained for diagnostics (never stores ticket secrets).
const MAX_REQUEST_LOG: usize = 64;

const CROSS_ORIGIN_RESOURCE_POLICY: HeaderName =
    HeaderName::from_static("cross-origin-resource-policy");

/// Transport mount used to reach [`AppContentServer`] from the web view.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ContentMountMode {
    /// Streaming loopback HTTP/1.1 listener on `127.0.0.1:0` (all platforms).
    Loopback,
    /// Legacy buffered Tauri custom URI scheme `legion-content` (retained for tests).
    CustomScheme,
}

impl ContentMountMode {
    /// Returns the content transport mount selected for the current target platform.
    pub const fn current_platform() -> Self {
        Self::Loopback
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

/// Diagnostic status of the most recent content request(s) for one ticket URL.
///
/// Queried by the interface via `content_status` when an `<img>`, `<video>`, or `<audio>` element
/// errors so the viewer distinguishes HTTP/transport/size/revocation failures from genuine media
/// decode failures.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContentUrlStatus {
    /// Number of HTTP requests observed for this ticket URL so far.
    pub request_count: u32,
    /// HTTP status code of the most recent request, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<u16>,
    /// Response `Content-Type` header of the most recent request, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_type: Option<String>,
    /// True when any streaming body served for this ticket aborted mid-stream.
    pub stream_failed: bool,
    /// Structured Legion error code from an error response body, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_code: Option<String>,
    /// Human-readable error message from an error response body, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_message: Option<String>,
    /// True when the ticket referenced by the URL is still live in the ticket table.
    pub ticket_live: bool,
}

struct TicketStatusEntry {
    key_hash: [u8; 32],
    request_count: u32,
    status: Option<u16>,
    content_type: Option<String>,
    stream_failed: Arc<AtomicBool>,
    error_code: Option<String>,
    error_message: Option<String>,
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
    accept_alive: Arc<AtomicBool>,
    accept_task: tokio::task::JoinHandle<()>,
}

impl ListenerHandle {
    fn is_alive(&self) -> bool {
        !self.shutdown.is_cancelled()
            && self.accept_alive.load(Ordering::Acquire)
            && !self.accept_task.is_finished()
    }
}

struct AcceptAliveGuard {
    alive: Arc<AtomicBool>,
}

impl Drop for AcceptAliveGuard {
    fn drop(&mut self) {
        self.alive.store(false, Ordering::Release);
    }
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
    if err.code == ProtoErrorCode::Busy {
        let retry_after_secs = err
            .details
            .as_ref()
            .and_then(|d| d.get("retry_after_secs"))
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(2)
            .max(1);
        return OpenRangeError::Busy {
            retry_after_secs,
            error: err,
        };
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
    header_read_timeout: Duration,
    loopback_conn_limit: Arc<Semaphore>,
    custom_scheme_max_full_bytes: u64,
    custom_scheme_memory_budget: Arc<Semaphore>,
    custom_scheme_buffered_bytes: Arc<AtomicUsize>,
    custom_scheme_peak_buffered_bytes: Arc<AtomicUsize>,
    listener_lock: AsyncMutex<()>,
    listener: Mutex<Option<ListenerHandle>>,
    active_streams: Arc<AtomicUsize>,
    activity: Arc<Mutex<ActivityState>>,
    request_log: Mutex<VecDeque<ContentRequestLog>>,
    ticket_statuses: Mutex<VecDeque<TicketStatusEntry>>,
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
        Self::with_limits(
            backend,
            mode,
            ticket_config,
            idle_shutdown,
            allowed_origins,
            MAX_LOOPBACK_CONNECTIONS,
            LOOPBACK_HEADER_READ_TIMEOUT,
            CUSTOM_SCHEME_MAX_FULL_BYTES,
            CUSTOM_SCHEME_TOTAL_MEMORY_BUDGET_BYTES,
        )
    }

    /// Creates a content server with explicit connection, header-timeout, and custom-scheme memory
    /// limits (used with tighter budgets in tests).
    #[allow(clippy::too_many_arguments)]
    pub fn with_limits(
        backend: Arc<dyn Backend>,
        mode: ContentMountMode,
        ticket_config: TicketConfig,
        idle_shutdown: Duration,
        allowed_origins: Vec<String>,
        max_loopback_connections: usize,
        header_read_timeout: Duration,
        custom_scheme_max_full_bytes: u64,
        custom_scheme_total_memory_budget_bytes: usize,
    ) -> Self {
        let source = Arc::new(LinkContentSource {
            backend: Arc::clone(&backend),
        });
        let conn_cap = max_loopback_connections.max(1);
        let budget_units = custom_scheme_total_memory_budget_bytes
            .div_ceil(CUSTOM_SCHEME_BUDGET_UNIT_BYTES)
            .max(1);
        Self {
            backend,
            mode,
            tickets: Arc::new(TicketTable::with_config(ticket_config)),
            limiter: ContentLimiter::default(),
            source,
            allowed_origins,
            idle_shutdown,
            header_read_timeout,
            loopback_conn_limit: Arc::new(Semaphore::new(conn_cap)),
            custom_scheme_max_full_bytes,
            custom_scheme_memory_budget: Arc::new(Semaphore::new(budget_units)),
            custom_scheme_buffered_bytes: Arc::new(AtomicUsize::new(0)),
            custom_scheme_peak_buffered_bytes: Arc::new(AtomicUsize::new(0)),
            listener_lock: AsyncMutex::new(()),
            listener: Mutex::new(None),
            active_streams: Arc::new(AtomicUsize::new(0)),
            activity: Arc::new(Mutex::new(ActivityState {
                last_activity: Instant::now(),
            })),
            request_log: Mutex::new(VecDeque::with_capacity(MAX_REQUEST_LOG)),
            ticket_statuses: Mutex::new(VecDeque::with_capacity(MAX_REQUEST_LOG)),
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

    /// Returns the observed HTTP status, content type, mid-stream failure flag, and ticket
    /// liveness for `raw_url` without exposing any secret ticket identifier.
    pub fn content_status_for_url(&self, raw_url: &str) -> ContentUrlStatus {
        let Some(ticket_id) = extract_ticket_id_from_url(raw_url) else {
            return ContentUrlStatus::default();
        };
        let key_hash = ticket_hash(ticket_id);
        let ticket_live = self.tickets.is_live(&key_hash);
        let statuses = self
            .ticket_statuses
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let Some(entry) = statuses.iter().find(|entry| entry.key_hash == key_hash) else {
            return ContentUrlStatus {
                ticket_live,
                ..ContentUrlStatus::default()
            };
        };
        ContentUrlStatus {
            request_count: entry.request_count,
            status: entry.status,
            content_type: entry.content_type.clone(),
            stream_failed: entry.stream_failed.load(Ordering::Acquire),
            error_code: entry.error_code.clone(),
            error_message: entry.error_message.clone(),
            ticket_live,
        }
    }

    fn ensure_ticket_stream_flag(&self, key_hash: [u8; 32]) -> Arc<AtomicBool> {
        let mut statuses = self
            .ticket_statuses
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(entry) = statuses.iter().find(|entry| entry.key_hash == key_hash) {
            return Arc::clone(&entry.stream_failed);
        }
        if statuses.len() >= MAX_REQUEST_LOG {
            statuses.pop_front();
        }
        let flag = Arc::new(AtomicBool::new(false));
        statuses.push_back(TicketStatusEntry {
            key_hash,
            request_count: 0,
            status: None,
            content_type: None,
            stream_failed: Arc::clone(&flag),
            error_code: None,
            error_message: None,
        });
        flag
    }

    fn record_ticket_status(
        &self,
        key_hash: [u8; 32],
        status: StatusCode,
        headers: &HeaderMap,
        error_body: Option<&[u8]>,
        increment_count: bool,
    ) {
        let content_type = headers
            .get(CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        let (error_code, error_message) = if status.is_client_error() || status.is_server_error() {
            parse_proto_error_summary(error_body)
        } else {
            (None, None)
        };
        let mut statuses = self
            .ticket_statuses
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(entry) = statuses.iter_mut().find(|entry| entry.key_hash == key_hash) {
            if increment_count {
                entry.request_count = entry.request_count.saturating_add(1);
            }
            entry.status = Some(status.as_u16());
            entry.content_type = content_type;
            entry.error_code = error_code;
            entry.error_message = error_message;
            return;
        }
        if statuses.len() >= MAX_REQUEST_LOG {
            statuses.pop_front();
        }
        statuses.push_back(TicketStatusEntry {
            key_hash,
            request_count: u32::from(increment_count),
            status: Some(status.as_u16()),
            content_type,
            stream_failed: Arc::new(AtomicBool::new(false)),
            error_code,
            error_message,
        });
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
                let port = self.ensure_live_loopback_listener().await?;
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

    /// Probes the loopback listener after an app resume (e.g. iOS foregrounding after background
    /// suspension) and tears down + rebinds it if the OS reclaimed the listening socket.
    pub fn on_app_resumed(self: &Arc<Self>) {
        if self.mode != ContentMountMode::Loopback {
            return;
        }
        let server = Arc::clone(self);
        crate::transfers::spawn_bg(async move {
            let _ = server.probe_or_recover_loopback().await;
        });
    }

    /// Verifies from our own listener state that an active loopback accept loop is still running;
    /// if it has stopped or been cancelled while live tickets remain, tears down the stale handle
    /// and binds a fresh OS-chosen port on `127.0.0.1:0` (never probing via TCP connect and never
    /// reusing the previous port).
    pub async fn probe_or_recover_loopback(self: &Arc<Self>) -> Result<Option<u16>, AppError> {
        if self.mode != ContentMountMode::Loopback {
            return Ok(None);
        }
        let _guard = self.listener_lock.lock().await;
        let state = {
            let slot = self
                .listener
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            slot.as_ref().map(|h| (h.port, h.is_alive()))
        };
        let Some((port, alive)) = state else {
            return Ok(None);
        };
        if alive {
            return Ok(Some(port));
        }

        // Accept task is no longer live: tear down the stale handle and bind a fresh OS-chosen
        // port if live tickets remain.
        self.teardown_listener_if_port(port);
        if self.tickets.is_empty() {
            return Ok(None);
        }
        let new_port = self.bind_loopback_listener_locked().await?;
        Ok(Some(new_port))
    }

    /// Returns the peak concurrent custom-scheme response bytes buffered so far.
    #[cfg(test)]
    pub fn peak_custom_scheme_buffered_bytes(&self) -> usize {
        self.custom_scheme_peak_buffered_bytes
            .load(Ordering::Acquire)
    }

    /// Handles one Tauri custom URI scheme request (`http://legion-content.localhost/v1/content/...`)
    /// with bounded range pieces, pre-read size capping, global in-flight memory budgeting, and
    /// full [`HostOriginGuard`] + `legion-content` security headers.
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
        let max_bytes = usize::try_from(self.custom_scheme_max_full_bytes).unwrap_or(usize::MAX);
        let Some(expected_host) = canonical_host else {
            return collect_bounded_response(
                guard_error_response(GuardRejection::MisdirectedHost, CorpPolicy::CrossOrigin),
                max_bytes,
                &self.custom_scheme_memory_budget,
                &self.custom_scheme_buffered_bytes,
                &self.custom_scheme_peak_buffered_bytes,
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
            max_full_bytes: Some(self.custom_scheme_max_full_bytes),
            ..ServeOptions::default()
        };

        let response = self
            .handle_checked_request(&guard, &method, &uri, &headers, options)
            .await;
        let ticket_key_hash = parse_content_path(uri.path()).map(|(id, _)| ticket_hash(id));
        let initial_status = response.status();
        let collected = collect_bounded_response(
            response,
            max_bytes,
            &self.custom_scheme_memory_budget,
            &self.custom_scheme_buffered_bytes,
            &self.custom_scheme_peak_buffered_bytes,
        )
        .await;
        if let Some(key_hash) = ticket_key_hash
            && collected.status() != initial_status
        {
            self.record_ticket_status(
                key_hash,
                collected.status(),
                collected.headers(),
                Some(collected.body().as_slice()),
                false,
            );
        }
        collected
    }

    fn touch_activity(&self) {
        self.activity
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .last_activity = Instant::now();
    }

    fn teardown_listener_if_port(&self, port: u16) {
        let mut slot = self
            .listener
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if slot.as_ref().is_some_and(|h| h.port == port)
            && let Some(handle) = slot.take()
        {
            handle.shutdown.cancel();
        }
    }

    /// Ensures a loopback listener owned by this server is alive before returning a freshly minted
    /// URL. Decides liveness strictly from Legion's own accept-task state (never via a TCP connect
    /// to the old port) and always binds a fresh OS-chosen port on recovery.
    async fn ensure_live_loopback_listener(self: &Arc<Self>) -> Result<u16, AppError> {
        let _guard = self.listener_lock.lock().await;
        let state = {
            let slot = self
                .listener
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            slot.as_ref().map(|h| (h.port, h.is_alive()))
        };
        if let Some((port, alive)) = state {
            if alive {
                return Ok(port);
            }
            self.teardown_listener_if_port(port);
        }
        self.bind_loopback_listener_locked().await
    }

    async fn bind_loopback_listener_locked(self: &Arc<Self>) -> Result<u16, AppError> {
        if let Some(port) = {
            let slot = self
                .listener
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            slot.as_ref().filter(|h| h.is_alive()).map(|h| h.port)
        } {
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
        let accept_alive = Arc::new(AtomicBool::new(true));
        let server = Arc::clone(self);
        let accept_shutdown = shutdown.clone();
        let accept_flag = Arc::clone(&accept_alive);
        let accept_task = tokio::spawn(async move {
            server
                .run_accept_loop(tcp, port, accept_shutdown, accept_flag)
                .await;
        });
        {
            let mut slot = self
                .listener
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            *slot = Some(ListenerHandle {
                port,
                shutdown: shutdown.clone(),
                accept_alive,
                accept_task,
            });
        }
        self.touch_activity();

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
        accept_alive: Arc<AtomicBool>,
    ) {
        let _alive_guard = AcceptAliveGuard {
            alive: accept_alive,
        };
        let guard = Arc::new(HostOriginGuard {
            port,
            allowed_hosts: Vec::new(),
            allowed_origins: self.allowed_origins.clone(),
            allow_same_origin: false,
        });
        let options = ServeOptions {
            corp: CorpPolicy::CrossOrigin,
            max_range_bytes: None,
            max_full_bytes: None,
            ..ServeOptions::default()
        };
        let header_read_timeout = self.header_read_timeout;
        let mut consecutive_errors: u32 = 0;
        let mut backoff = ACCEPT_BACKOFF_INITIAL;

        loop {
            let accepted = tokio::select! {
                biased;
                () = shutdown.cancelled() => break,
                res = tcp.accept() => res,
            };
            let (stream, _peer) = match accepted {
                Ok(pair) => {
                    consecutive_errors = 0;
                    backoff = ACCEPT_BACKOFF_INITIAL;
                    pair
                }
                Err(err) => {
                    consecutive_errors = consecutive_errors.saturating_add(1);
                    if is_fatal_accept_error(&err)
                        || consecutive_errors >= MAX_CONSECUTIVE_ACCEPT_ERRORS
                    {
                        self.teardown_listener_if_port(port);
                        break;
                    }
                    tokio::select! {
                        biased;
                        () = shutdown.cancelled() => break,
                        () = tokio::time::sleep(backoff) => {}
                    }
                    backoff = (backoff * 2).min(ACCEPT_BACKOFF_MAX);
                    continue;
                }
            };
            let Ok(conn_permit) = Arc::clone(&self.loopback_conn_limit).try_acquire_owned() else {
                // Drop excess connections immediately instead of spawning unbounded tasks.
                drop(stream);
                continue;
            };
            let _ = stream.set_nodelay(true);
            let server = Arc::clone(&self);
            let conn_guard = Arc::clone(&guard);
            let conn_shutdown = shutdown.clone();
            tokio::spawn(async move {
                let _permit = conn_permit;
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
                    .timer(TokioTimer::new())
                    .header_read_timeout(header_read_timeout)
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
            Err(rejection) => {
                let resp = guard_error_response(rejection, options.corp);
                if let Some((ticket_id, _)) = parse_content_path(uri.path()) {
                    self.record_ticket_status(
                        ticket_hash(ticket_id),
                        resp.status(),
                        resp.headers(),
                        resp.body().fixed_bytes(),
                        true,
                    );
                }
                return resp;
            }
        };

        if *method == Method::OPTIONS {
            return preflight_response(cors_origin, options.corp);
        }

        let Some((ticket_id, file_name)) = parse_content_path(uri.path()) else {
            return not_found_response(options.corp, cors_origin);
        };
        let key_hash = ticket_hash(ticket_id);
        let stream_failed = self.ensure_ticket_stream_flag(key_hash);

        self.active_streams.fetch_add(1, Ordering::AcqRel);
        let active_guard = ActiveStreamGuard {
            active_streams: Arc::clone(&self.active_streams),
            activity: Arc::clone(&self.activity),
        };
        let hooks = StreamHooks {
            limiter: Some(self.limiter.clone()),
            bytes_served: None,
            stream_failed: Some(stream_failed),
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

        self.record_ticket_status(
            key_hash,
            response.status(),
            response.headers(),
            response.body().fixed_bytes(),
            true,
        );
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

        #[cfg(all(debug_assertions, target_os = "android"))]
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
        #[cfg(all(debug_assertions, not(target_os = "android")))]
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

fn ticket_hash(ticket_id: &str) -> [u8; 32] {
    *blake3::hash(ticket_id.as_bytes()).as_bytes()
}

fn extract_ticket_id_from_url(raw_url: &str) -> Option<&str> {
    let trimmed = raw_url.trim();
    if trimmed.is_empty() {
        return None;
    }
    let without_fragment = trimmed.split('#').next().unwrap_or(trimmed);
    let without_query = without_fragment
        .split('?')
        .next()
        .unwrap_or(without_fragment);
    let path = if let Some(after_scheme) = without_query.split_once("://").map(|(_, rest)| rest) {
        let slash = after_scheme.find('/')?;
        &after_scheme[slash..]
    } else {
        without_query
    };
    parse_content_path(path).map(|(ticket_id, _)| ticket_id)
}

fn parse_proto_error_summary(raw: Option<&[u8]>) -> (Option<String>, Option<String>) {
    let Some(bytes) = raw else {
        return (None, None);
    };
    let Ok(err) = serde_json::from_slice::<ProtoErrorBody>(bytes) else {
        return (None, None);
    };
    let code = serde_json::to_value(err.code)
        .ok()
        .and_then(|v| v.as_str().map(str::to_owned));
    (code, Some(err.message))
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

fn is_fatal_accept_error(err: &std::io::Error) -> bool {
    matches!(
        err.kind(),
        std::io::ErrorKind::InvalidInput
            | std::io::ErrorKind::NotConnected
            | std::io::ErrorKind::BrokenPipe
    )
}

fn payload_too_large_response() -> Response<Vec<u8>> {
    let msg = serde_json::to_vec(&ProtoErrorBody::new(
        ProtoErrorCode::Unsupported,
        "Unranged custom-scheme response exceeds the in-memory size limit.",
    ))
    .unwrap_or_default();
    let len = msg.len() as u64;
    let mut err_resp = Response::new(msg);
    *err_resp.status_mut() = StatusCode::PAYLOAD_TOO_LARGE;
    apply_base_security_headers(err_resp.headers_mut(), CorpPolicy::CrossOrigin);
    err_resp
        .headers_mut()
        .insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    err_resp
        .headers_mut()
        .insert(CONTENT_LENGTH, HeaderValue::from(len));
    err_resp
}

struct MemoryUsageGuard<'a> {
    current: &'a AtomicUsize,
    bytes: usize,
}

impl<'a> MemoryUsageGuard<'a> {
    fn acquire(current: &'a AtomicUsize, peak: &AtomicUsize, bytes: usize) -> Self {
        if bytes > 0 {
            let now = current.fetch_add(bytes, Ordering::AcqRel) + bytes;
            peak.fetch_max(now, Ordering::AcqRel);
        }
        Self { current, bytes }
    }

    fn grow(&mut self, peak: &AtomicUsize, extra: usize) {
        if extra > 0 {
            let now = self.current.fetch_add(extra, Ordering::AcqRel) + extra;
            peak.fetch_max(now, Ordering::AcqRel);
            self.bytes = self.bytes.saturating_add(extra);
        }
    }
}

impl Drop for MemoryUsageGuard<'_> {
    fn drop(&mut self) {
        if self.bytes > 0 {
            self.current.fetch_sub(self.bytes, Ordering::AcqRel);
        }
    }
}

async fn collect_bounded_response(
    response: Response<ContentBody>,
    max_bytes: usize,
    budget: &Semaphore,
    current_buffered: &AtomicUsize,
    peak_buffered: &AtomicUsize,
) -> Response<Vec<u8>> {
    let (parts, mut body) = response.into_parts();
    let declared_len = parts
        .headers
        .get(CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<usize>().ok());

    if let Some(len) = declared_len
        && len > max_bytes
    {
        return payload_too_large_response();
    }

    let initial_units = declared_len
        .filter(|&len| len > 0)
        .map(|len| len.div_ceil(CUSTOM_SCHEME_BUDGET_UNIT_BYTES))
        .unwrap_or(0);

    let mut held_permits = Vec::new();
    let mut reserved_bytes = 0usize;
    if initial_units > 0 {
        let Ok(permits) = budget
            .acquire_many(u32::try_from(initial_units).unwrap_or(u32::MAX))
            .await
        else {
            return payload_too_large_response();
        };
        held_permits.push(permits);
        reserved_bytes = initial_units.saturating_mul(CUSTOM_SCHEME_BUDGET_UNIT_BYTES);
    }
    let mut usage_guard =
        MemoryUsageGuard::acquire(current_buffered, peak_buffered, declared_len.unwrap_or(0));
    let mut out = match declared_len {
        Some(len) if len > 0 => Vec::with_capacity(len),
        _ => Vec::new(),
    };

    let mut pinned = Pin::new(&mut body);
    while let Some(frame) = std::future::poll_fn(|cx| pinned.as_mut().poll_frame(cx)).await {
        match frame {
            Ok(frame) => {
                if let Ok(data) = frame.into_data() {
                    let next_len = out.len().saturating_add(data.len());
                    if next_len > max_bytes {
                        return payload_too_large_response();
                    }
                    if next_len > reserved_bytes {
                        let extra_bytes = next_len - reserved_bytes;
                        let extra_units = extra_bytes.div_ceil(CUSTOM_SCHEME_BUDGET_UNIT_BYTES);
                        let Ok(permits) = budget
                            .acquire_many(u32::try_from(extra_units).unwrap_or(u32::MAX))
                            .await
                        else {
                            return payload_too_large_response();
                        };
                        held_permits.push(permits);
                        reserved_bytes = reserved_bytes.saturating_add(
                            extra_units.saturating_mul(CUSTOM_SCHEME_BUDGET_UNIT_BYTES),
                        );
                    }
                    if declared_len.is_none() {
                        usage_guard.grow(peak_buffered, data.len());
                    }
                    out.extend_from_slice(&data);
                }
            }
            Err(err) => {
                let (status, code, message) = if err.kind() == std::io::ErrorKind::PermissionDenied
                {
                    (
                        StatusCode::GONE,
                        ProtoErrorCode::NotFound,
                        "Content stream expired or was evicted.",
                    )
                } else {
                    (
                        StatusCode::BAD_GATEWAY,
                        ProtoErrorCode::Failed,
                        "Content stream ended before completing.",
                    )
                };
                let msg =
                    serde_json::to_vec(&ProtoErrorBody::new(code, message)).unwrap_or_default();
                let len = msg.len() as u64;
                let mut err_resp = Response::new(msg);
                *err_resp.status_mut() = status;
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
    drop(usage_guard);
    drop(held_permits);
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

    #[derive(Clone, Default)]
    enum MockStreamFault {
        #[default]
        None,
        ErrorOnGet {
            code: ProtoCode,
            message: &'static str,
        },
        TruncateAfterBytes(usize),
    }

    struct MemoryFilesOpener {
        files: Arc<HashMap<String, Vec<u8>>>,
        window_policy: Option<legion_link::content::WindowPolicy>,
        fault: Arc<Mutex<MockStreamFault>>,
    }

    impl StreamOpener for MemoryFilesOpener {
        fn window_policy(&self) -> Option<legion_link::content::WindowPolicy> {
            self.window_policy
        }

        fn open_stream(
            &self,
            _cancel: CancellationToken,
        ) -> Pin<Box<dyn Future<Output = Result<RawStream, StreamOpenError>> + Send + '_>> {
            let files = Arc::clone(&self.files);
            let supports_keep_open = self.window_policy.is_some();
            let fault = Arc::clone(&self.fault);
            Box::pin(async move {
                let (client_in, mut server_out) = tokio::io::duplex(64 * 1024);
                let (server_in, client_out) = tokio::io::duplex(64 * 1024);
                let cancel = CancellationToken::new();
                let park = CancellationToken::new();
                let watch_cancel = cancel.clone();
                let watch_park = park.clone();
                tokio::spawn(async move {
                    let mut reader = BufReader::new(server_in);
                    let mut sent_protocol = false;
                    loop {
                        let mut line = String::new();
                        let read_res = tokio::select! {
                            biased;
                            () = watch_cancel.cancelled() => return,
                            () = watch_park.cancelled() => return,
                            res = reader.read_line(&mut line) => res,
                        };
                        if read_res.unwrap_or(0) == 0 {
                            return;
                        }
                        let req: Value = serde_json::from_str(&line).unwrap();
                        if !sent_protocol {
                            let _ = server_out.write_all(b"{\"protocol\":1}\n").await;
                            sent_protocol = true;
                        }
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
                        let active_fault = fault.lock().unwrap().clone();
                        if let MockStreamFault::ErrorOnGet { code, message } = active_fault {
                            let err = json!({ "error": ErrorBody::new(code, message) });
                            let _ = server_out.write_all(format!("{err}\n").as_bytes()).await;
                            return;
                        }
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
                        let keep_open =
                            supports_keep_open && req["keep_open"].as_bool() == Some(true);
                        let mut ready = json!({
                            "size": payload.len(),
                            "offset": offset,
                            "length": len,
                            "mtime": { "secs": 1_700_000_000i64, "nanos": 0u32 },
                            "file_id": 77u64,
                        });
                        if keep_open {
                            ready["keep_open"] = Value::Bool(true);
                        }
                        if server_out
                            .write_all(format!("{ready}\n").as_bytes())
                            .await
                            .is_err()
                        {
                            return;
                        }
                        if let MockStreamFault::TruncateAfterBytes(cut) = active_fault
                            && len > cut
                        {
                            let _ = server_out.write_all(&payload[offset..offset + cut]).await;
                            let _ = server_out.shutdown().await;
                            return;
                        }
                        let write_ok = tokio::select! {
                            biased;
                            () = watch_cancel.cancelled() => false,
                            () = watch_park.cancelled() => false,
                            res = server_out.write_all(&payload[offset..offset + len]) => res.is_ok(),
                        };
                        if !write_ok {
                            return;
                        }
                        if keep_open {
                            if server_out.write_all(b"{\"done\":true}\n").await.is_err() {
                                return;
                            }
                            continue;
                        }
                        let _ = server_out.shutdown().await;
                        return;
                    }
                });
                Ok(RawStream {
                    reader: Box::new(client_in),
                    writer: Box::new(client_out),
                    stderr: None,
                    cancel,
                    park: Some(park),
                    guard: None,
                })
            })
        }
    }

    struct MockContentBackend {
        link: Link,
        live: Arc<AtomicBool>,
        holder: String,
        fault: Arc<Mutex<MockStreamFault>>,
    }

    impl MockContentBackend {
        fn new(files: HashMap<String, Vec<u8>>) -> Self {
            Self::with_options(files, LinkOptions::default(), None)
        }

        fn with_options(
            files: HashMap<String, Vec<u8>>,
            link_options: LinkOptions,
            window_policy: Option<legion_link::content::WindowPolicy>,
        ) -> Self {
            let fault = Arc::new(Mutex::new(MockStreamFault::None));
            let opener_files = Arc::new(MemoryFilesOpener {
                files: Arc::new(files),
                window_policy,
                fault: Arc::clone(&fault),
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
                link: Link::with_opener(cfg, link_options, session_opener),
                live: Arc::new(AtomicBool::new(true)),
                holder: "pairing:tower:1".to_owned(),
                fault,
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

    #[tokio::test]
    async fn loopback_enforces_connection_cap_header_timeout_and_recovers_after_death() {
        let mut files = HashMap::new();
        files.insert("/home/me/hello.txt".to_owned(), b"hello-loopback".to_vec());
        let backend = Arc::new(MockContentBackend::new(files));
        let server = Arc::new(AppContentServer::with_limits(
            backend,
            ContentMountMode::Loopback,
            TicketConfig::default(),
            Duration::from_secs(30),
            vec!["tauri://localhost".to_owned()],
            2,
            Duration::from_millis(200),
            CUSTOM_SCHEME_MAX_FULL_BYTES,
            CUSTOM_SCHEME_TOTAL_MEMORY_BUDGET_BYTES,
        ));

        let minted = server
            .mint_url("tower", "/home/me/hello.txt", false)
            .await
            .unwrap();
        let port = server.loopback_port().unwrap();
        let path = minted
            .url
            .strip_prefix(&format!("http://127.0.0.1:{port}"))
            .unwrap();

        // Slowloris header-read timeout: send incomplete HTTP headers and verify server closes
        // promptly after 200ms rather than holding the slot open forever.
        let mut slow1 = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        slow1.write_all(b"GET /v1/content/").await.unwrap();
        let mut slow2 = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        slow2.write_all(b"GET /v1/content/").await.unwrap();
        // Give accept loop a moment to register both permits.
        tokio::time::sleep(Duration::from_millis(40)).await;

        // Third simultaneous connection exceeds the cap of 2 and is closed immediately.
        let mut overflow = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        let mut byte = [0u8; 1];
        let overflow_read =
            tokio::time::timeout(Duration::from_millis(120), overflow.read(&mut byte)).await;
        assert!(
            matches!(overflow_read, Ok(Ok(0)) | Ok(Err(_))),
            "excess connection above cap should be dropped immediately, got {overflow_read:?}"
        );

        // After the 200ms header read timeout expires, slowloris sockets close and normal
        // requests succeed again.
        let mut sink = Vec::new();
        let slow_closed =
            tokio::time::timeout(Duration::from_secs(2), slow1.read_to_end(&mut sink)).await;
        assert!(
            slow_closed.is_ok(),
            "slowloris socket must time out and close"
        );
        drop(slow2);

        let (status, _, body) = raw_http(
            port,
            &format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n"),
        )
        .await;
        assert_eq!(status, 200);
        assert_eq!(body, b"hello-loopback");

        // Simulate OS reclaiming the listening socket during iOS background suspend: cancel the
        // current listener token while keeping the port recorded, then call
        // `probe_or_recover_loopback()` and confirm it recovers cleanly.
        {
            let guard = server.listener.lock().unwrap();
            if let Some(h) = guard.as_ref() {
                h.shutdown.cancel();
            }
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
        let recovered_port = server
            .probe_or_recover_loopback()
            .await
            .unwrap()
            .expect("listener should recover while live ticket exists");
        let (status, _, body) = raw_http(
            recovered_port,
            &format!(
                "GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{recovered_port}\r\nConnection: close\r\n\r\n"
            ),
        )
        .await;
        assert_eq!(status, 200);
        assert_eq!(body, b"hello-loopback");
        server.shutdown();
    }

    #[tokio::test]
    async fn custom_scheme_rejects_oversize_unranged_pre_read_and_bounds_parallel_memory() {
        let mut files = HashMap::new();
        // 1.2 MiB file against a tight 1 MiB max_full_bytes cap and 2 MiB total budget.
        let oversize = vec![0x5Au8; 1_200_000];
        let chunk_file: Vec<u8> = (0..512 * 1024usize).map(|i| (i % 251) as u8).collect();
        files.insert("/home/me/huge.png".to_owned(), oversize);
        files.insert("/home/me/clip.webm".to_owned(), chunk_file.clone());

        let backend = Arc::new(MockContentBackend::new(files));
        let budget_bytes = 2 * 1024 * 1024; // 2 MiB budget across 20 parallel 512 KiB requests
        let server = Arc::new(AppContentServer::with_limits(
            backend,
            ContentMountMode::CustomScheme,
            TicketConfig::default(),
            Duration::from_secs(30),
            vec!["http://tauri.localhost".to_owned()],
            MAX_LOOPBACK_CONNECTIONS,
            LOOPBACK_HEADER_READ_TIMEOUT,
            1024 * 1024, // 1 MiB max_full_bytes
            budget_bytes,
        ));

        // 1) Unranged GET for 1.2 MiB file fails immediately with 413 Payload Too Large.
        let huge = server
            .mint_url("tower", "/home/me/huge.png", false)
            .await
            .unwrap();
        let req = Request::builder()
            .method(Method::GET)
            .uri(&huge.url)
            .body(Vec::new())
            .unwrap();
        let resp = server.handle_custom_scheme(req).await;
        assert_eq!(resp.status(), StatusCode::PAYLOAD_TOO_LARGE);
        // Ranged GET on the same file still succeeds with 206 Partial Content!
        let range_req = Request::builder()
            .method(Method::GET)
            .uri(&huge.url)
            .header(RANGE, "bytes=0-1023")
            .body(Vec::new())
            .unwrap();
        let range_resp = server.handle_custom_scheme(range_req).await;
        assert_eq!(range_resp.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(range_resp.body().len(), 1024);

        // 2) Fire 12 parallel 512 KiB requests across 4 tickets (6 MiB total, within
        // ContentLimiter's per-ticket=8 / per-holder=16 caps) against a 2 MiB global memory budget
        // and verify every single one succeeds with exact bytes while peak buffered memory stays
        // strictly <= budget.
        let mut urls = Vec::new();
        for _ in 0..4 {
            urls.push(
                server
                    .mint_url("tower", "/home/me/clip.webm", false)
                    .await
                    .unwrap()
                    .url,
            );
        }
        let mut tasks = Vec::new();
        for i in 0..12 {
            let srv = Arc::clone(&server);
            let url = urls[i % urls.len()].clone();
            tasks.push(tokio::spawn(async move {
                let req = Request::builder()
                    .method(Method::GET)
                    .uri(&url)
                    .header(RANGE, "bytes=0-524287")
                    .body(Vec::new())
                    .unwrap();
                srv.handle_custom_scheme(req).await
            }));
        }
        for t in tasks {
            let resp = t.await.unwrap();
            assert_eq!(resp.status(), StatusCode::PARTIAL_CONTENT);
            assert_eq!(resp.body().len(), chunk_file.len());
            assert_eq!(resp.body(), &chunk_file);
        }

        // Also fire a storm of 24 parallel requests across 6 tickets: excess streams cleanly
        // receive 429 while accepted ones return 206, and peak buffered memory still stays <= budget.
        let mut storm = Vec::new();
        for i in 0..24 {
            let srv = Arc::clone(&server);
            let url = urls[i % urls.len()].clone();
            storm.push(tokio::spawn(async move {
                let req = Request::builder()
                    .method(Method::GET)
                    .uri(&url)
                    .header(RANGE, "bytes=0-524287")
                    .body(Vec::new())
                    .unwrap();
                srv.handle_custom_scheme(req).await
            }));
        }
        let mut ok_count = 0usize;
        for t in storm {
            let resp = t.await.unwrap();
            assert!(
                matches!(
                    resp.status(),
                    StatusCode::PARTIAL_CONTENT | StatusCode::TOO_MANY_REQUESTS
                ),
                "unexpected status {}",
                resp.status()
            );
            if resp.status() == StatusCode::PARTIAL_CONTENT {
                assert_eq!(resp.body(), &chunk_file);
                ok_count += 1;
            }
        }
        assert!(ok_count >= 1);
        let peak = server.peak_custom_scheme_buffered_bytes();
        assert!(
            peak <= budget_bytes,
            "peak in-flight buffered memory ({peak}) exceeded budget ({budget_bytes})"
        );
    }

    #[tokio::test]
    async fn html_and_svg_carry_sandbox_csp_nosniff_and_safe_content_types_on_both_mounts() {
        fn assert_raw_security_headers(head: &str, expected_ct: Option<&str>) {
            let lower = head.to_ascii_lowercase();
            assert!(
                lower.contains("x-content-type-options: nosniff"),
                "missing nosniff: {head}"
            );
            assert!(
                lower.contains(
                    "content-security-policy: sandbox; default-src 'none'; frame-ancestors 'none'"
                ),
                "missing sandbox CSP: {head}"
            );
            assert!(
                lower.contains("cross-origin-resource-policy: cross-origin"),
                "missing CORP: {head}"
            );
            assert!(
                lower.contains("cache-control: private, no-store"),
                "missing private, no-store: {head}"
            );
            assert!(
                !lower.contains("content-type: text/html"),
                "content response must never emit text/html: {head}"
            );
            if let Some(ct) = expected_ct {
                assert!(
                    lower.contains(&format!("content-type: {ct}")),
                    "expected content-type {ct}: {head}"
                );
            }
        }

        fn assert_response_security_headers<B>(resp: &Response<B>, expected_ct: Option<&str>) {
            assert_eq!(
                resp.headers()
                    .get(X_CONTENT_TYPE_OPTIONS)
                    .and_then(|v| v.to_str().ok()),
                Some("nosniff")
            );
            assert_eq!(
                resp.headers()
                    .get(CONTENT_SECURITY_POLICY)
                    .and_then(|v| v.to_str().ok()),
                Some(CONTENT_SANDBOX_CSP)
            );
            assert_eq!(
                resp.headers()
                    .get(CROSS_ORIGIN_RESOURCE_POLICY)
                    .and_then(|v| v.to_str().ok()),
                Some("cross-origin")
            );
            assert_eq!(
                resp.headers()
                    .get(CACHE_CONTROL)
                    .and_then(|v| v.to_str().ok()),
                Some("private, no-store")
            );
            let actual_ct = resp
                .headers()
                .get(CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .map(str::to_ascii_lowercase);
            assert!(
                !actual_ct
                    .as_deref()
                    .is_some_and(|ct| ct.contains("text/html")),
                "content response must never emit text/html: {actual_ct:?}"
            );
            if let Some(ct) = expected_ct {
                assert_eq!(actual_ct.as_deref(), Some(ct));
            }
        }

        let html_bytes =
            b"<html><script>window.__TAURI__.core.invoke('state')</script></html>".to_vec();
        let mut large_html = Vec::with_capacity(128 * 1024);
        while large_html.len() < 128 * 1024 {
            large_html.extend_from_slice(&html_bytes);
        }
        large_html.truncate(128 * 1024);
        let svg_bytes =
            b"<svg xmlns='http://www.w3.org/2000/svg'><script>alert(1)</script></svg>".to_vec();
        let mut files = HashMap::new();
        files.insert("/home/me/evil.html".to_owned(), html_bytes.clone());
        files.insert("/home/me/evil-large.html".to_owned(), large_html.clone());
        files.insert("/home/me/evil.svg".to_owned(), svg_bytes.clone());

        // 1) Loopback mount (desktop & iOS).
        let lb_backend = Arc::new(MockContentBackend::new(files.clone()));
        let lb = Arc::new(AppContentServer::with_config(
            Arc::clone(&lb_backend) as Arc<dyn Backend>,
            ContentMountMode::Loopback,
            TicketConfig::default(),
            Duration::from_secs(30),
            vec!["tauri://localhost".to_owned()],
        ));
        let lb_html = lb
            .mint_url("tower", "/home/me/evil.html", false)
            .await
            .unwrap();
        let lb_svg = lb
            .mint_url("tower", "/home/me/evil.svg", false)
            .await
            .unwrap();
        let port = lb.loopback_port().unwrap();
        let lb_html_path = lb_html
            .url
            .strip_prefix(&format!("http://127.0.0.1:{port}"))
            .unwrap()
            .to_owned();
        let lb_svg_path = lb_svg
            .url
            .strip_prefix(&format!("http://127.0.0.1:{port}"))
            .unwrap()
            .to_owned();

        // 200 OK & 206 Partial Content for both HTML and SVG.
        let mut html_etag = String::new();
        for (path, expected_ct) in [
            (lb_html_path.as_str(), "text/plain; charset=utf-8"),
            (lb_svg_path.as_str(), "image/svg+xml"),
        ] {
            let (status, head, _) = raw_http(
                port,
                &format!(
                    "GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n"
                ),
            )
            .await;
            assert_eq!(status, 200, "{head}");
            assert_raw_security_headers(&head, Some(expected_ct));
            if path == lb_html_path {
                for line in head.lines() {
                    if let Some((k, v)) = line.split_once(':')
                        && k.trim().eq_ignore_ascii_case("etag")
                    {
                        html_etag = v.trim().to_owned();
                    }
                }
            }

            let (status206, head206, _) = raw_http(
                port,
                &format!(
                    "GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nRange: bytes=0-7\r\nConnection: close\r\n\r\n"
                ),
            )
            .await;
            assert_eq!(status206, 206, "{head206}");
            assert_raw_security_headers(&head206, Some(expected_ct));
        }

        // Conditional If-None-Match: responses are `Cache-Control: private, no-store`, so no
        // unguarded 304 is returned — returns 200 with full sandbox headers.
        assert!(!html_etag.is_empty());
        let (status_cond, head_cond, _) = raw_http(
            port,
            &format!(
                "GET {lb_html_path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nIf-None-Match: {html_etag}\r\nConnection: close\r\n\r\n"
            ),
        )
        .await;
        assert_eq!(status_cond, 200, "{head_cond}");
        assert_raw_security_headers(&head_cond, Some("text/plain; charset=utf-8"));

        // 403 Forbidden (bad Origin), 421 Misdirected Request (bad Host), 405 Method Not Allowed.
        let (status403, head403, _) = raw_http(
            port,
            &format!(
                "GET {lb_html_path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nOrigin: https://evil.example\r\nConnection: close\r\n\r\n"
            ),
        )
        .await;
        assert_eq!(status403, 403, "{head403}");
        assert_raw_security_headers(&head403, Some("application/json"));

        let (status421, head421, _) = raw_http(
            port,
            &format!("GET {lb_html_path} HTTP/1.1\r\nHost: evil.example:{port}\r\nConnection: close\r\n\r\n"),
        )
        .await;
        assert_eq!(status421, 421, "{head421}");
        assert_raw_security_headers(&head421, Some("application/json"));

        let (status405, head405, _) = raw_http(
            port,
            &format!("POST {lb_html_path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"),
        )
        .await;
        assert_eq!(status405, 405, "{head405}");
        assert_raw_security_headers(&head405, Some("application/json"));

        // 404 Not Found (unknown ticket and unknown route).
        for bad_path in [
            "/v1/content/doesnotexist/evil.html",
            "/not-content/evil.html",
        ] {
            let (status404, head404, _) = raw_http(
                port,
                &format!(
                    "GET {bad_path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n"
                ),
            )
            .await;
            assert_eq!(status404, 404, "{head404}");
            assert_raw_security_headers(&head404, Some("application/json"));
        }

        // 413 Payload Too Large (via handle_checked_request with max_full_bytes).
        let lb_guard = HostOriginGuard {
            port,
            allowed_hosts: Vec::new(),
            allowed_origins: vec!["tauri://localhost".to_owned()],
            allow_same_origin: false,
        };
        let mut lb_headers = HeaderMap::new();
        lb_headers.insert(
            HOST,
            HeaderValue::from_str(&format!("127.0.0.1:{port}")).unwrap(),
        );
        let resp413 = lb
            .handle_checked_request(
                &lb_guard,
                &Method::GET,
                &lb_html_path.parse::<Uri>().unwrap(),
                &lb_headers,
                ServeOptions {
                    corp: CorpPolicy::CrossOrigin,
                    max_full_bytes: Some(8),
                    ..ServeOptions::default()
                },
            )
            .await;
        assert_eq!(resp413.status(), StatusCode::PAYLOAD_TOO_LARGE);
        assert_response_security_headers(&resp413, Some("application/json"));

        // 416 Range Not Satisfiable.
        let (status416, head416, _) = raw_http(
            port,
            &format!(
                "GET {lb_html_path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nRange: bytes=99999-\r\nConnection: close\r\n\r\n"
            ),
        )
        .await;
        assert_eq!(status416, 416, "{head416}");
        assert_raw_security_headers(&head416, None);

        // 429 Too Many Requests (saturate holder permits using distinct ticket hashes).
        let mut held = Vec::new();
        for idx in 0u8..64 {
            let mut key_hash = [0xAAu8; 32];
            key_hash[0] = idx;
            match lb.limiter.try_acquire(key_hash, "pairing:tower:1") {
                Ok(permit) => held.push(permit),
                Err(_) => break,
            }
        }
        let (status429, head429, _) = raw_http(
            port,
            &format!(
                "GET {lb_html_path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n"
            ),
        )
        .await;
        drop(held);
        assert_eq!(status429, 429, "{head429}");
        assert_raw_security_headers(&head429, Some("application/json"));

        // 502 Bad Gateway (upstream unreachable) & 503 Service Unavailable (upstream busy).
        *lb_backend.fault.lock().unwrap() = MockStreamFault::ErrorOnGet {
            code: ProtoCode::Unreachable,
            message: "machine unreachable",
        };
        let (status502, head502, _) = raw_http(
            port,
            &format!(
                "GET {lb_html_path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n"
            ),
        )
        .await;
        assert_eq!(status502, 502, "{head502}");
        assert_raw_security_headers(&head502, Some("application/json"));

        *lb_backend.fault.lock().unwrap() = MockStreamFault::ErrorOnGet {
            code: ProtoCode::Busy,
            message: "all stream slots busy",
        };
        let (status503, head503, _) = raw_http(
            port,
            &format!(
                "GET {lb_html_path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n"
            ),
        )
        .await;
        assert_eq!(status503, 503, "{head503}");
        assert_raw_security_headers(&head503, Some("application/json"));

        // Aborted body mid-stream on loopback: pre-abort headers carry sandbox CSP, nosniff, and
        // text/plain, and the connection aborts strictly before Content-Length with no error HTML.
        *lb_backend.fault.lock().unwrap() = MockStreamFault::None;
        let lb_large_html = lb
            .mint_url("tower", "/home/me/evil-large.html", false)
            .await
            .unwrap();
        let lb_large_path = lb_large_html
            .url
            .strip_prefix(&format!("http://127.0.0.1:{port}"))
            .unwrap()
            .to_owned();
        *lb_backend.fault.lock().unwrap() = MockStreamFault::TruncateAfterBytes(70 * 1024);
        let (status_abort, head_abort, body_abort) = raw_http(
            port,
            &format!(
                "GET {lb_large_path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n"
            ),
        )
        .await;
        *lb_backend.fault.lock().unwrap() = MockStreamFault::None;
        assert_eq!(status_abort, 200, "{head_abort}");
        assert_raw_security_headers(&head_abort, Some("text/plain; charset=utf-8"));
        assert!(
            body_abort.len() < large_html.len(),
            "aborted loopback body must terminate early ({} vs {})",
            body_abort.len(),
            large_html.len()
        );
        lb.shutdown();

        // 2) Custom-scheme mount (Android).
        let cs_backend = Arc::new(MockContentBackend::new(files));
        let cs = Arc::new(AppContentServer::with_limits(
            Arc::clone(&cs_backend) as Arc<dyn Backend>,
            ContentMountMode::CustomScheme,
            TicketConfig::default(),
            Duration::from_secs(30),
            vec!["http://tauri.localhost".to_owned()],
            MAX_LOOPBACK_CONNECTIONS,
            LOOPBACK_HEADER_READ_TIMEOUT,
            1024,
            64 * 1024,
        ));
        let cs_html = cs
            .mint_url("tower", "/home/me/evil.html", false)
            .await
            .unwrap();
        let cs_svg = cs
            .mint_url("tower", "/home/me/evil.svg", false)
            .await
            .unwrap();

        // 200 OK & 206 Partial Content for both HTML and SVG.
        let mut cs_html_etag = HeaderValue::from_static("");
        for (url, expected_ct) in [
            (&cs_html.url, "text/plain; charset=utf-8"),
            (&cs_svg.url, "image/svg+xml"),
        ] {
            let req = Request::builder()
                .method(Method::GET)
                .uri(url)
                .body(Vec::new())
                .unwrap();
            let resp = cs.handle_custom_scheme(req).await;
            assert_eq!(resp.status(), StatusCode::OK);
            assert_response_security_headers(&resp, Some(expected_ct));
            if url == &cs_html.url {
                cs_html_etag = resp.headers().get(http::header::ETAG).unwrap().clone();
            }

            let req206 = Request::builder()
                .method(Method::GET)
                .uri(url)
                .header(RANGE, "bytes=0-7")
                .body(Vec::new())
                .unwrap();
            let resp206 = cs.handle_custom_scheme(req206).await;
            assert_eq!(resp206.status(), StatusCode::PARTIAL_CONTENT);
            assert_response_security_headers(&resp206, Some(expected_ct));
        }

        // Conditional If-None-Match on custom scheme -> 200 OK (no 304, no-store preserved).
        let req_cond = Request::builder()
            .method(Method::GET)
            .uri(&cs_html.url)
            .header("if-none-match", cs_html_etag)
            .body(Vec::new())
            .unwrap();
        let resp_cond = cs.handle_custom_scheme(req_cond).await;
        assert_eq!(resp_cond.status(), StatusCode::OK);
        assert_response_security_headers(&resp_cond, Some("text/plain; charset=utf-8"));

        // 403, 421, 405, 404 on custom scheme.
        let req403 = Request::builder()
            .method(Method::GET)
            .uri(&cs_html.url)
            .header("origin", "https://evil.example")
            .body(Vec::new())
            .unwrap();
        let resp403 = cs.handle_custom_scheme(req403).await;
        assert_eq!(resp403.status(), StatusCode::FORBIDDEN);
        assert_response_security_headers(&resp403, Some("application/json"));

        let req421 = Request::builder()
            .method(Method::GET)
            .uri("http://evil.example/v1/content/tok/evil.html")
            .body(Vec::new())
            .unwrap();
        let resp421 = cs.handle_custom_scheme(req421).await;
        assert_eq!(resp421.status(), StatusCode::MISDIRECTED_REQUEST);
        assert_response_security_headers(&resp421, Some("application/json"));

        let req405 = Request::builder()
            .method(Method::POST)
            .uri(&cs_html.url)
            .body(Vec::new())
            .unwrap();
        let resp405 = cs.handle_custom_scheme(req405).await;
        assert_eq!(resp405.status(), StatusCode::METHOD_NOT_ALLOWED);
        assert_response_security_headers(&resp405, Some("application/json"));

        for bad_uri in [
            "http://legion-content.localhost/v1/content/doesnotexist/evil.html",
            "http://legion-content.localhost/not-content/evil.html",
        ] {
            let req404 = Request::builder()
                .method(Method::GET)
                .uri(bad_uri)
                .body(Vec::new())
                .unwrap();
            let resp404 = cs.handle_custom_scheme(req404).await;
            assert_eq!(resp404.status(), StatusCode::NOT_FOUND);
            assert_response_security_headers(&resp404, Some("application/json"));
        }

        // 413 Payload Too Large on custom scheme (max_full_bytes = 8 < html_bytes.len()).
        let cs_small = Arc::new(AppContentServer::with_limits(
            Arc::clone(&cs_backend) as Arc<dyn Backend>,
            ContentMountMode::CustomScheme,
            TicketConfig::default(),
            Duration::from_secs(30),
            vec!["http://tauri.localhost".to_owned()],
            MAX_LOOPBACK_CONNECTIONS,
            LOOPBACK_HEADER_READ_TIMEOUT,
            8,
            64 * 1024,
        ));
        let cs_small_html = cs_small
            .mint_url("tower", "/home/me/evil.html", false)
            .await
            .unwrap();
        let req413 = Request::builder()
            .method(Method::GET)
            .uri(&cs_small_html.url)
            .body(Vec::new())
            .unwrap();
        let resp413 = cs_small.handle_custom_scheme(req413).await;
        assert_eq!(resp413.status(), StatusCode::PAYLOAD_TOO_LARGE);
        assert_response_security_headers(&resp413, Some("application/json"));

        // 416 Range Not Satisfiable on custom scheme.
        let req416 = Request::builder()
            .method(Method::GET)
            .uri(&cs_html.url)
            .header(RANGE, "bytes=99999-")
            .body(Vec::new())
            .unwrap();
        let resp416 = cs.handle_custom_scheme(req416).await;
        assert_eq!(resp416.status(), StatusCode::RANGE_NOT_SATISFIABLE);
        assert_response_security_headers(&resp416, None);

        // 429 Too Many Requests on custom scheme (saturate holder permits).
        let mut cs_held = Vec::new();
        for idx in 0u8..64 {
            let mut key_hash = [0xBBu8; 32];
            key_hash[0] = idx;
            match cs.limiter.try_acquire(key_hash, "pairing:tower:1") {
                Ok(permit) => cs_held.push(permit),
                Err(_) => break,
            }
        }
        let req429 = Request::builder()
            .method(Method::GET)
            .uri(&cs_html.url)
            .body(Vec::new())
            .unwrap();
        let resp429 = cs.handle_custom_scheme(req429).await;
        drop(cs_held);
        assert_eq!(resp429.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_response_security_headers(&resp429, Some("application/json"));

        // 502 Bad Gateway (upstream unreachable) & 503 Service Unavailable (upstream busy).
        *cs_backend.fault.lock().unwrap() = MockStreamFault::ErrorOnGet {
            code: ProtoCode::Unreachable,
            message: "machine unreachable",
        };
        let req502 = Request::builder()
            .method(Method::GET)
            .uri(&cs_html.url)
            .body(Vec::new())
            .unwrap();
        let resp502 = cs.handle_custom_scheme(req502).await;
        assert_eq!(resp502.status(), StatusCode::BAD_GATEWAY);
        assert_response_security_headers(&resp502, Some("application/json"));

        *cs_backend.fault.lock().unwrap() = MockStreamFault::ErrorOnGet {
            code: ProtoCode::Busy,
            message: "all stream slots busy",
        };
        let req503 = Request::builder()
            .method(Method::GET)
            .uri(&cs_html.url)
            .body(Vec::new())
            .unwrap();
        let resp503 = cs.handle_custom_scheme(req503).await;
        assert_eq!(resp503.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_response_security_headers(&resp503, Some("application/json"));

        // Aborted body on custom scheme -> 502 Bad Gateway (EOF/I/O error) and 410 Gone
        // (PermissionDenied / evicted parked stream), both carrying full security headers.
        *cs_backend.fault.lock().unwrap() = MockStreamFault::TruncateAfterBytes(10);
        let req_abort = Request::builder()
            .method(Method::GET)
            .uri(&cs_html.url)
            .body(Vec::new())
            .unwrap();
        let resp_abort = cs.handle_custom_scheme(req_abort).await;
        *cs_backend.fault.lock().unwrap() = MockStreamFault::None;
        assert_eq!(resp_abort.status(), StatusCode::BAD_GATEWAY);
        assert_response_security_headers(&resp_abort, Some("application/json"));

        // 410 Gone on custom scheme when a parked stream is evicted mid-body.
        struct EvictedReader;
        impl tokio::io::AsyncRead for EvictedReader {
            fn poll_read(
                self: Pin<&mut Self>,
                _cx: &mut std::task::Context<'_>,
                _buf: &mut tokio::io::ReadBuf<'_>,
            ) -> std::task::Poll<std::io::Result<()>> {
                std::task::Poll::Ready(Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "Content stream was closed while idle.",
                )))
            }
        }
        let budget = Arc::new(tokio::sync::Semaphore::new(16));
        let buffered = AtomicUsize::new(0);
        let peak = AtomicUsize::new(0);
        let resp410 = collect_bounded_response(
            Response::new(ContentBody::stream(Box::new(EvictedReader), 64)),
            1024,
            &budget,
            &buffered,
            &peak,
        )
        .await;
        assert_eq!(resp410.status(), StatusCode::GONE);
        assert_response_security_headers(&resp410, Some("application/json"));
    }

    async fn read_http_head_and_prefix(
        stream: &mut TcpStream,
        min_body_bytes: usize,
    ) -> (u16, String, Vec<u8>) {
        let mut buf = Vec::new();
        let mut chunk = [0u8; 4096];
        loop {
            let n = stream.read(&mut chunk).await.unwrap();
            assert!(n > 0, "unexpected EOF before response headers");
            buf.extend_from_slice(&chunk[..n]);
            if let Some(split) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                let body_start = split + 4;
                if buf.len() - body_start >= min_body_bytes {
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
                    return (status, head, buf[body_start..].to_vec());
                }
            }
        }
    }

    #[tokio::test]
    async fn evicted_parked_stream_aborts_loopback_cleanly_and_fresh_ticket_recovers() {
        // Large enough to exceed loopback kernel TCP send/recv buffers so a paused client pauses
        // hyper body polling and triggers ContentStream's 2-second idle park.
        let payload: Vec<u8> = (0..(12 * 1024 * 1024))
            .map(|i| ((i * 29 + 13) % 251) as u8)
            .collect();
        let mut files = HashMap::new();
        files.insert("/home/me/movie.bin".to_owned(), payload.clone());

        let backend = Arc::new(MockContentBackend::with_options(
            files,
            LinkOptions {
                max_parked_streams: 1,
                ..LinkOptions::default()
            },
            Some(legion_link::content::WindowPolicy {
                initial_bytes: 256 * 1024,
                max_bytes: 256 * 1024,
            }),
        ));
        let server = Arc::new(AppContentServer::with_config(
            backend,
            ContentMountMode::Loopback,
            TicketConfig::default(),
            Duration::from_secs(30),
            vec!["tauri://localhost".to_owned()],
        ));

        let t1 = server
            .mint_url("tower", "/home/me/movie.bin", false)
            .await
            .unwrap();
        let t2 = server
            .mint_url("tower", "/home/me/movie.bin", false)
            .await
            .unwrap();
        let port = server.loopback_port().unwrap();
        let path1 = t1
            .url
            .strip_prefix(&format!("http://127.0.0.1:{port}"))
            .unwrap();
        let path2 = t2
            .url
            .strip_prefix(&format!("http://127.0.0.1:{port}"))
            .unwrap();

        // Start stream 1, read response headers + a small prefix, then pause past the 2s park timer.
        let mut s1 = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        s1.write_all(
            format!("GET {path1} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n")
                .as_bytes(),
        )
        .await
        .unwrap();
        let (status1, head1, mut body1) = read_http_head_and_prefix(&mut s1, 4096).await;
        assert_eq!(status1, 200, "{head1}");
        assert!(
            head1
                .to_ascii_lowercase()
                .contains(&format!("content-length: {}", payload.len())),
            "{head1}"
        );
        tokio::time::sleep(Duration::from_millis(2400)).await;

        // Start stream 2, pause past the 2s park timer so it parks and evicts stream 1 from the
        // cap-1 ParkedStreamPool.
        let mut s2 = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        s2.write_all(
            format!("GET {path2} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n")
                .as_bytes(),
        )
        .await
        .unwrap();
        let (status2, head2, _body2) = read_http_head_and_prefix(&mut s2, 4096).await;
        assert_eq!(status2, 200, "{head2}");
        tokio::time::sleep(Duration::from_millis(2400)).await;

        // Resuming stream 1 must abort cleanly strictly before Content-Length rather than hanging
        // or delivering corrupted bytes.
        let mut chunk = vec![0u8; 64 * 1024];
        loop {
            match s1.read(&mut chunk).await {
                Ok(0) | Err(_) => break,
                Ok(n) => body1.extend_from_slice(&chunk[..n]),
            }
        }
        assert!(
            body1.len() < payload.len(),
            "evicted parked stream must abort before full Content-Length ({} vs {})",
            body1.len(),
            payload.len()
        );
        assert_eq!(body1, &payload[..body1.len()]);

        // Minting a fresh ticket (what FilesViewer does on media error) recovers the exact tail.
        let t3 = server
            .mint_url("tower", "/home/me/movie.bin", false)
            .await
            .unwrap();
        let path3 = t3
            .url
            .strip_prefix(&format!("http://127.0.0.1:{port}"))
            .unwrap();
        let (status3, head3, rest) = raw_http(
            port,
            &format!(
                "GET {path3} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nRange: bytes={}-\r\nConnection: close\r\n\r\n",
                body1.len()
            ),
        )
        .await;
        assert_eq!(status3, 206, "{head3}");
        body1.extend_from_slice(&rest);
        assert_eq!(body1.len(), payload.len());
        assert_eq!(body1, payload);

        drop(s2);
        server.shutdown();
    }

    #[tokio::test]
    async fn custom_scheme_maps_evicted_parked_stream_to_410_gone() {
        struct FailAfterPrefix {
            prefix: Option<Vec<u8>>,
            kind: std::io::ErrorKind,
        }

        impl tokio::io::AsyncRead for FailAfterPrefix {
            fn poll_read(
                mut self: Pin<&mut Self>,
                _cx: &mut std::task::Context<'_>,
                buf: &mut tokio::io::ReadBuf<'_>,
            ) -> std::task::Poll<std::io::Result<()>> {
                if let Some(prefix) = self.prefix.take() {
                    buf.put_slice(&prefix);
                    return std::task::Poll::Ready(Ok(()));
                }
                std::task::Poll::Ready(Err(std::io::Error::new(
                    self.kind,
                    "Content stream was closed while idle.",
                )))
            }
        }

        let budget = Arc::new(tokio::sync::Semaphore::new(64));
        let buffered = AtomicUsize::new(0);
        let peak = AtomicUsize::new(0);

        // 1) Evicted parked stream (PermissionDenied) -> 410 Gone with full security headers.
        let mut ok_resp = Response::new(ContentBody::stream(
            Box::new(FailAfterPrefix {
                prefix: Some(b"partial".to_vec()),
                kind: std::io::ErrorKind::PermissionDenied,
            }),
            1024,
        ));
        *ok_resp.status_mut() = StatusCode::PARTIAL_CONTENT;
        ok_resp
            .headers_mut()
            .insert(CONTENT_LENGTH, HeaderValue::from_static("1024"));
        let evicted =
            collect_bounded_response(ok_resp, 1024 * 1024, &budget, &buffered, &peak).await;
        assert_eq!(evicted.status(), StatusCode::GONE);
        assert_eq!(
            evicted.headers().get(X_CONTENT_TYPE_OPTIONS).unwrap(),
            "nosniff"
        );
        assert_eq!(
            evicted.headers().get(CONTENT_SECURITY_POLICY).unwrap(),
            CONTENT_SANDBOX_CSP
        );
        assert_eq!(
            evicted.headers().get(CROSS_ORIGIN_RESOURCE_POLICY).unwrap(),
            "cross-origin"
        );

        // 2) Non-eviction I/O error (BrokenPipe) -> 502 Bad Gateway.
        let mut err_resp = Response::new(ContentBody::stream(
            Box::new(FailAfterPrefix {
                prefix: Some(b"partial".to_vec()),
                kind: std::io::ErrorKind::BrokenPipe,
            }),
            1024,
        ));
        *err_resp.status_mut() = StatusCode::PARTIAL_CONTENT;
        let broken =
            collect_bounded_response(err_resp, 1024 * 1024, &budget, &buffered, &peak).await;
        assert_eq!(broken.status(), StatusCode::BAD_GATEWAY);
    }

    #[tokio::test]
    async fn suspend_drops_loopback_listener_and_evicts_parked_streams_then_resume_recovers_cleanly()
     {
        let image_bytes = b"\x89PNG\r\n\x1a\nresume-image-payload".to_vec();
        // Large enough to exceed loopback TCP buffers so a paused client triggers idle parking.
        let movie_bytes: Vec<u8> = (0..(12 * 1024 * 1024))
            .map(|i| ((i * 31 + 7) % 251) as u8)
            .collect();
        let mut files = HashMap::new();
        files.insert("/home/me/photo.png".to_owned(), image_bytes.clone());
        files.insert("/home/me/movie.bin".to_owned(), movie_bytes.clone());

        let backend = Arc::new(MockContentBackend::with_options(
            files,
            LinkOptions {
                max_parked_streams: 1,
                ..LinkOptions::default()
            },
            Some(legion_link::content::WindowPolicy {
                initial_bytes: 256 * 1024,
                max_bytes: 256 * 1024,
            }),
        ));
        let server = Arc::new(AppContentServer::with_config(
            backend,
            ContentMountMode::Loopback,
            TicketConfig::default(),
            Duration::from_secs(30),
            vec!["tauri://localhost".to_owned()],
        ));

        // 1) Before suspension: mint and open two streams on the cap-1 pool so stream 1 parks
        // and is evicted when stream 2 parks.
        let pre1 = server
            .mint_url("tower", "/home/me/movie.bin", false)
            .await
            .unwrap();
        let pre2 = server
            .mint_url("tower", "/home/me/movie.bin", false)
            .await
            .unwrap();
        let port_before = server.loopback_port().unwrap();
        let path1 = pre1
            .url
            .strip_prefix(&format!("http://127.0.0.1:{port_before}"))
            .unwrap();
        let path2 = pre2
            .url
            .strip_prefix(&format!("http://127.0.0.1:{port_before}"))
            .unwrap();

        let mut s1 = TcpStream::connect(("127.0.0.1", port_before))
            .await
            .unwrap();
        s1.write_all(
            format!(
                "GET {path1} HTTP/1.1\r\nHost: 127.0.0.1:{port_before}\r\nConnection: close\r\n\r\n"
            )
            .as_bytes(),
        )
        .await
        .unwrap();
        let (st1, head1, mut body1) = read_http_head_and_prefix(&mut s1, 4096).await;
        assert_eq!(st1, 200, "{head1}");
        tokio::time::sleep(Duration::from_millis(2400)).await;

        let mut s2 = TcpStream::connect(("127.0.0.1", port_before))
            .await
            .unwrap();
        s2.write_all(
            format!(
                "GET {path2} HTTP/1.1\r\nHost: 127.0.0.1:{port_before}\r\nConnection: close\r\n\r\n"
            )
            .as_bytes(),
        )
        .await
        .unwrap();
        let (st2, head2, _body2) = read_http_head_and_prefix(&mut s2, 4096).await;
        assert_eq!(st2, 200, "{head2}");
        tokio::time::sleep(Duration::from_millis(2400)).await;

        // 2) Simulate OS background suspension reclaiming the listening socket while the stale
        // ListenerHandle is still recorded in `server.listener`.
        {
            let guard = server.listener.lock().unwrap();
            if let Some(h) = guard.as_ref() {
                h.shutdown.cancel();
            }
        }
        tokio::time::sleep(Duration::from_millis(50)).await;

        // Pre-suspend evicted stream aborts cleanly strictly before Content-Length.
        let mut chunk = vec![0u8; 64 * 1024];
        loop {
            match s1.read(&mut chunk).await {
                Ok(0) | Err(_) => break,
                Ok(n) => body1.extend_from_slice(&chunk[..n]),
            }
        }
        assert!(body1.len() < movie_bytes.len());
        assert_eq!(body1, &movie_bytes[..body1.len()]);
        drop(s2);

        // 3) Resume: race `probe_or_recover_loopback()` against concurrent `mint_url()` calls
        // (matching iOS `WindowEvent::Resumed` racing with immediate post-resume UI requests).
        let (probe_res, img_res, vid_res) = tokio::join!(
            server.probe_or_recover_loopback(),
            server.mint_url("tower", "/home/me/photo.png", false),
            server.mint_url("tower", "/home/me/movie.bin", false),
        );
        let recovered_port = probe_res
            .unwrap()
            .expect("probe must see or recover a live loopback listener");
        let img_ticket = img_res.unwrap();
        let vid_ticket = vid_res.unwrap();
        assert_eq!(server.loopback_port(), Some(recovered_port));

        let expected_origin = format!("http://127.0.0.1:{recovered_port}");
        let img_path = img_ticket
            .url
            .strip_prefix(&expected_origin)
            .expect("image URL must point to recovered live port");
        let vid_path = vid_ticket
            .url
            .strip_prefix(&expected_origin)
            .expect("video URL must point to recovered live port");

        // First requests after resume must succeed byte-exact without error.
        let (img_status, img_head, img_body) = raw_http(
            recovered_port,
            &format!(
                "GET {img_path} HTTP/1.1\r\nHost: 127.0.0.1:{recovered_port}\r\nConnection: close\r\n\r\n"
            ),
        )
        .await;
        assert_eq!(img_status, 200, "{img_head}");
        assert_eq!(img_body, image_bytes);

        let (vid_status, vid_head, vid_tail) = raw_http(
            recovered_port,
            &format!(
                "GET {vid_path} HTTP/1.1\r\nHost: 127.0.0.1:{recovered_port}\r\nRange: bytes={}-\r\nConnection: close\r\n\r\n",
                body1.len()
            ),
        )
        .await;
        assert_eq!(vid_status, 206, "{vid_head}");
        body1.extend_from_slice(&vid_tail);
        assert_eq!(body1, movie_bytes);

        // 4) Simulate a long suspension where the listener was completely torn down and all
        // tickets were cleared: the very first post-resume `mint_url` recreates the listener.
        server.shutdown();
        assert_eq!(server.loopback_port(), None);
        let after_long = server
            .mint_url("tower", "/home/me/photo.png", false)
            .await
            .unwrap();
        let long_port = server
            .loopback_port()
            .expect("mint_url must recreate loopback listener after full teardown");
        let long_path = after_long
            .url
            .strip_prefix(&format!("http://127.0.0.1:{long_port}"))
            .unwrap();
        let (long_status, long_head, long_body) = raw_http(
            long_port,
            &format!(
                "GET {long_path} HTTP/1.1\r\nHost: 127.0.0.1:{long_port}\r\nConnection: close\r\n\r\n"
            ),
        )
        .await;
        assert_eq!(long_status, 200, "{long_head}");
        assert_eq!(long_body, image_bytes);

        server.shutdown();
    }

    #[test]
    fn current_platform_uses_loopback_everywhere() {
        assert_eq!(
            ContentMountMode::current_platform(),
            ContentMountMode::Loopback
        );
    }

    #[tokio::test]
    async fn loopback_handles_range_edge_cases_parallel_small_and_duplicate_ranges() {
        let payload: Vec<u8> = (0..(2 * 1024 * 1024usize))
            .map(|i| ((i * 37 + 11) % 251) as u8)
            .collect();
        let mut files = HashMap::new();
        files.insert("/home/me/master.flac".to_owned(), payload.clone());

        let backend = Arc::new(MockContentBackend::new(files));
        let server = Arc::new(AppContentServer::with_config(
            backend,
            ContentMountMode::Loopback,
            TicketConfig::default(),
            Duration::from_secs(30),
            vec!["http://tauri.localhost".to_owned()],
        ));

        let t1 = server
            .mint_url("tower", "/home/me/master.flac", false)
            .await
            .unwrap();
        let t2 = server
            .mint_url("tower", "/home/me/master.flac", false)
            .await
            .unwrap();
        let port = server.loopback_port().unwrap();
        let path1 = t1
            .url
            .strip_prefix(&format!("http://127.0.0.1:{port}"))
            .unwrap()
            .to_owned();
        let path2 = t2
            .url
            .strip_prefix(&format!("http://127.0.0.1:{port}"))
            .unwrap()
            .to_owned();

        // 1) Open-ended range `bytes=0-` streams the full 2 MiB file over loopback.
        let (st, head, body) = raw_http(
            port,
            &format!(
                "GET {path1} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nRange: bytes=0-\r\nConnection: close\r\n\r\n"
            ),
        )
        .await;
        assert_eq!(st, 206, "{head}");
        assert_eq!(body.len(), payload.len());
        assert_eq!(body, payload);

        // 2) Huge range past end (`bytes=1048576-999999999`) clamps cleanly to EOF.
        let (st, head, body) = raw_http(
            port,
            &format!(
                "GET {path1} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nRange: bytes=1048576-999999999\r\nConnection: close\r\n\r\n"
            ),
        )
        .await;
        assert_eq!(st, 206, "{head}");
        assert_eq!(body, &payload[1048576..]);

        // 3) Past-the-end range -> 416 Range Not Satisfiable with `bytes */<size>`.
        let (st, head, _) = raw_http(
            port,
            &format!(
                "GET {path1} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nRange: bytes={}-\r\nConnection: close\r\n\r\n",
                payload.len()
            ),
        )
        .await;
        assert_eq!(st, 416, "{head}");
        assert!(
            head.to_ascii_lowercase()
                .contains(&format!("content-range: bytes */{}", payload.len())),
            "{head}"
        );

        // 4) 16 small parallel ranges split across two tickets (8 per ticket, within per-ticket
        // and per-holder caps) all succeed byte-exact.
        let mut parallel_tasks = Vec::new();
        for i in 0..16usize {
            let p = if i < 8 { path1.clone() } else { path2.clone() };
            let start = i * 65_536;
            let end = start + 8191;
            parallel_tasks.push(tokio::spawn(async move {
                let (s, h, b) = raw_http(
                    port,
                    &format!(
                        "GET {p} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nRange: bytes={start}-{end}\r\nConnection: close\r\n\r\n"
                    ),
                )
                .await;
                (start, end, s, h, b)
            }));
        }
        for task in parallel_tasks {
            let (start, end, s, h, b) = task.await.unwrap();
            assert_eq!(s, 206, "{h}");
            assert_eq!(b, &payload[start..=end]);
        }

        // 5) 8 duplicate identical ranges fired simultaneously on the same ticket all return 206
        // with identical bytes.
        let mut dup_tasks = Vec::new();
        for _ in 0..8usize {
            let p = path1.clone();
            dup_tasks.push(tokio::spawn(async move {
                raw_http(
                    port,
                    &format!(
                        "GET {p} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nRange: bytes=4096-20479\r\nConnection: close\r\n\r\n"
                    ),
                )
                .await
            }));
        }
        for task in dup_tasks {
            let (s, h, b) = task.await.unwrap();
            assert_eq!(s, 206, "{h}");
            assert_eq!(b, &payload[4096..20480]);
        }

        server.shutdown();
    }

    #[tokio::test]
    async fn content_status_reports_healthy_media_truncation_http_errors_and_unrequested_urls() {
        let flac_bytes: Vec<u8> = (0..(128 * 1024)).map(|i| (i % 251) as u8).collect();
        let mut files = HashMap::new();
        files.insert("/home/me/track.flac".to_owned(), flac_bytes.clone());

        let backend = Arc::new(MockContentBackend::new(files));
        let server = Arc::new(AppContentServer::with_config(
            Arc::clone(&backend) as Arc<dyn Backend>,
            ContentMountMode::Loopback,
            TicketConfig::default(),
            Duration::from_secs(30),
            vec!["http://tauri.localhost".to_owned()],
        ));

        // 1) Freshly minted URL before any request: ticket_live=true, request_count=0.
        let healthy = server
            .mint_url("tower", "/home/me/track.flac", false)
            .await
            .unwrap();
        let port = server.loopback_port().unwrap();
        let initial = server.content_status_for_url(&healthy.url);
        assert!(initial.ticket_live);
        assert_eq!(initial.request_count, 0);
        assert_eq!(initial.status, None);
        assert!(!initial.stream_failed);

        // 2) Healthy 206 audio/flac delivery: status=206, content_type=audio/flac, stream_failed=false.
        let healthy_path = healthy
            .url
            .strip_prefix(&format!("http://127.0.0.1:{port}"))
            .unwrap();
        let (st, head, body) = raw_http(
            port,
            &format!(
                "GET {healthy_path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nRange: bytes=0-4095\r\nConnection: close\r\n\r\n"
            ),
        )
        .await;
        assert_eq!(st, 206, "{head}");
        assert_eq!(body.len(), 4096);
        let after_ok = server.content_status_for_url(&healthy.url);
        assert_eq!(after_ok.request_count, 1);
        assert_eq!(after_ok.status, Some(206));
        assert_eq!(after_ok.content_type.as_deref(), Some("audio/flac"));
        assert!(!after_ok.stream_failed);
        assert!(after_ok.ticket_live);

        // 3) Mid-stream truncation on loopback: status=200 but stream_failed=true so the viewer
        // knows NOT to blame the device's codec.
        let truncated = server
            .mint_url("tower", "/home/me/track.flac", false)
            .await
            .unwrap();
        let truncated_path = truncated
            .url
            .strip_prefix(&format!("http://127.0.0.1:{port}"))
            .unwrap();
        *backend.fault.lock().unwrap() = MockStreamFault::TruncateAfterBytes(70 * 1024);
        let (st_trunc, _, body_trunc) = raw_http(
            port,
            &format!(
                "GET {truncated_path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n"
            ),
        )
        .await;
        *backend.fault.lock().unwrap() = MockStreamFault::None;
        assert_eq!(st_trunc, 200);
        assert!(body_trunc.len() < flac_bytes.len());
        let after_trunc = server.content_status_for_url(&truncated.url);
        assert_eq!(after_trunc.status, Some(200));
        assert_eq!(after_trunc.content_type.as_deref(), Some("audio/flac"));
        assert!(
            after_trunc.stream_failed,
            "mid-stream truncation must flip stream_failed=true"
        );

        // 4) Upstream 502 error records status=502 and structured error code/message.
        let failing = server
            .mint_url("tower", "/home/me/track.flac", false)
            .await
            .unwrap();
        let failing_path = failing
            .url
            .strip_prefix(&format!("http://127.0.0.1:{port}"))
            .unwrap();
        *backend.fault.lock().unwrap() = MockStreamFault::ErrorOnGet {
            code: ProtoCode::Unreachable,
            message: "machine offline",
        };
        let (st_502, _, _) = raw_http(
            port,
            &format!(
                "GET {failing_path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n"
            ),
        )
        .await;
        *backend.fault.lock().unwrap() = MockStreamFault::None;
        assert_eq!(st_502, 502);
        let after_502 = server.content_status_for_url(&failing.url);
        assert_eq!(after_502.status, Some(502));
        assert_eq!(after_502.error_code.as_deref(), Some("unreachable"));
        assert_eq!(after_502.error_message.as_deref(), Some("machine offline"));

        // 5) Revoking tickets marks ticket_live=false.
        server.revoke_all();
        assert!(!server.content_status_for_url(&healthy.url).ticket_live);

        // 6) Malformed URL returns empty default status cleanly.
        assert_eq!(
            server.content_status_for_url("not-a-content-url"),
            ContentUrlStatus::default()
        );

        server.shutdown();
    }

    #[tokio::test]
    async fn loopback_recovery_ignores_port_squatter_and_binds_new_os_port() {
        let secret = b"private-audio-bytes-never-for-squatter".to_vec();
        let mut files = HashMap::new();
        files.insert("/home/me/secret.flac".to_owned(), secret.clone());

        let backend = Arc::new(MockContentBackend::new(files));
        let server = Arc::new(AppContentServer::with_config(
            backend,
            ContentMountMode::Loopback,
            TicketConfig::default(),
            Duration::from_secs(30),
            vec!["http://tauri.localhost".to_owned()],
        ));

        let _initial = server
            .mint_url("tower", "/home/me/secret.flac", false)
            .await
            .unwrap();
        let old_port = server.loopback_port().unwrap();

        // Simulate Legion's listening socket dying while its stale ListenerHandle still names
        // `old_port`.
        {
            let guard = server.listener.lock().unwrap();
            if let Some(h) = guard.as_ref() {
                h.shutdown.cancel();
            }
        }
        // Wait until Legion's accept task exits and releases `old_port`.
        let squatter = {
            let mut bound = None;
            for _ in 0..40 {
                if let Ok(l) = TcpListener::bind(("127.0.0.1", old_port)).await {
                    bound = Some(l);
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            bound.expect("squatter should bind the released old port")
        };

        let squatter_hits = Arc::new(AtomicUsize::new(0));
        let squatter_stop = CancellationToken::new();
        let hits_clone = Arc::clone(&squatter_hits);
        let stop_clone = squatter_stop.clone();
        let squatter_task = tokio::spawn(async move {
            loop {
                tokio::select! {
                    biased;
                    () = stop_clone.cancelled() => break,
                    res = squatter.accept() => {
                        if let Ok((_sock, _)) = res {
                            hits_clone.fetch_add(1, Ordering::SeqCst);
                        }
                    }
                }
            }
        });

        // Minting a new URL must detect from Legion's own state that the old listener is dead,
        // never probe the squatter port, and bind a fresh OS-chosen port.
        let fresh = server
            .mint_url("tower", "/home/me/secret.flac", false)
            .await
            .unwrap();
        let new_port = server.loopback_port().unwrap();
        assert_ne!(
            new_port, old_port,
            "recovery must bind a new OS-chosen port rather than trusting a squatter on {old_port}"
        );
        assert!(
            fresh
                .url
                .starts_with(&format!("http://127.0.0.1:{new_port}/v1/content/")),
            "minted URL must point at new port {new_port}, got {}",
            fresh.url
        );

        let path = fresh
            .url
            .strip_prefix(&format!("http://127.0.0.1:{new_port}"))
            .unwrap();
        let (status, head, body) = raw_http(
            new_port,
            &format!(
                "GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{new_port}\r\nConnection: close\r\n\r\n"
            ),
        )
        .await;
        assert_eq!(status, 200, "{head}");
        assert_eq!(body, secret);

        // Give the squatter task a moment in case anything tried to connect asynchronously.
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert_eq!(
            squatter_hits.load(Ordering::SeqCst),
            0,
            "nothing may connect to a squatter listening on the old port"
        );

        squatter_stop.cancel();
        let _ = squatter_task.await;
        server.shutdown();
    }
}
