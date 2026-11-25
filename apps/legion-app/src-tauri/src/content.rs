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

use std::collections::{HashMap, VecDeque};
use std::convert::Infallible;
use std::future::Future;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use http::header::{
    ACCESS_CONTROL_ALLOW_HEADERS, ACCESS_CONTROL_ALLOW_METHODS, ACCESS_CONTROL_ALLOW_ORIGIN,
    ACCESS_CONTROL_EXPOSE_HEADERS, CACHE_CONTROL, CONNECTION, CONTENT_LENGTH, CONTENT_RANGE,
    CONTENT_SECURITY_POLICY, CONTENT_TYPE, HOST, HeaderName, HeaderValue, ORIGIN, RANGE,
    REFERRER_POLICY, VARY, X_CONTENT_TYPE_OPTIONS,
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
use legion_link::{ContentRoutePolicy, Route};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpSocket, TcpStream};
use tokio::sync::{Mutex as AsyncMutex, Notify, Semaphore};
use tokio_util::sync::CancellationToken;

use crate::backend::{AppError, Backend, ErrorCode, NodeRouteIdentity};

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

/// Bounded TCP send buffer (`SO_SNDBUF`) for loopback content sockets (128 KiB), matching
/// `legion-doors` so paused players and revoked or torn-down streams backpressure promptly rather
/// than pulling megabytes into kernel loopback buffers.
const LOOPBACK_SOCKET_SNDBUF: u32 = 128 * 1024;

/// Maximum time an authenticated loopback client may take to send HTTP request headers (5 seconds).
pub const LOOPBACK_HEADER_READ_TIMEOUT: Duration = Duration::from_secs(5);

/// Maximum time an unauthenticated loopback connection (one that has not yet presented a valid
/// ticket) may take to send complete HTTP request headers before being closed (2 seconds).
pub const LOOPBACK_UNAUTH_HEADER_READ_TIMEOUT: Duration = Duration::from_secs(2);

/// Maximum total time allowed for a loopback proof-of-ownership liveness probe (400 ms).
pub const LOOPBACK_PROBE_TIMEOUT: Duration = Duration::from_millis(400);

/// Maximum age of a verified loopback proof before `mint_url` re-probes ownership (2 seconds).
pub const LOOPBACK_PROBE_FRESHNESS: Duration = Duration::from_secs(2);

/// Fixed loopback-only path answered by the proof-of-ownership liveness probe.
const LOOPBACK_PROBE_PATH: &str = "/v1/content/.probe";

/// Header carrying the fresh 256-bit base64url challenge sent by the liveness probe.
const LOOPBACK_PROBE_CHALLENGE_HEADER: HeaderName =
    HeaderName::from_static("x-legion-probe-challenge");

/// Header carrying the caller's keyed BLAKE3 authentication tag over the challenge.
const LOOPBACK_PROBE_AUTH_HEADER: HeaderName = HeaderName::from_static("x-legion-probe-auth");

/// Header carrying the listener's keyed BLAKE3 proof over the challenge.
const LOOPBACK_PROBE_PROOF_HEADER: HeaderName = HeaderName::from_static("x-legion-probe-proof");

/// Domain separation prefix for the probe request MAC.
const LOOPBACK_PROBE_REQ_DOMAIN: &[u8] = b"legion-loopback-probe-req-v1:";

/// Domain separation prefix for the probe response MAC.
const LOOPBACK_PROBE_RESP_DOMAIN: &[u8] = b"legion-loopback-probe-resp-v1:";

/// Maximum response bytes read from a loopback probe reply (4 KiB).
const LOOPBACK_PROBE_MAX_RESPONSE_BYTES: usize = 4096;

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
    probe_secret: [u8; 32],
    last_verified: Instant,
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

struct LoopbackConnSlot {
    id: u64,
    authenticated: Arc<AtomicBool>,
    evict: CancellationToken,
}

/// Bounded registry of active loopback TCP connections.
///
/// Tracks whether each connection has presented a valid content ticket so that when the connection
/// cap is reached, the oldest unauthenticated connection is evicted before refusing a new one.
struct LoopbackConnTable {
    max_connections: usize,
    next_id: u64,
    active: VecDeque<LoopbackConnSlot>,
}

impl LoopbackConnTable {
    fn new(max_connections: usize) -> Self {
        let cap = max_connections.max(1);
        Self {
            max_connections: cap,
            next_id: 1,
            active: VecDeque::with_capacity(cap),
        }
    }

    fn admit(
        table: &Arc<Mutex<Self>>,
    ) -> Option<(LoopbackConnGuard, Arc<AtomicBool>, CancellationToken)> {
        let mut guard = table
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if guard.active.len() >= guard.max_connections {
            let evict_idx = guard
                .active
                .iter()
                .position(|slot| !slot.authenticated.load(Ordering::Acquire))?;
            if let Some(evicted) = guard.active.remove(evict_idx) {
                evicted.evict.cancel();
            }
        }
        let id = guard.next_id;
        guard.next_id = guard.next_id.wrapping_add(1);
        let authenticated = Arc::new(AtomicBool::new(false));
        let evict = CancellationToken::new();
        guard.active.push_back(LoopbackConnSlot {
            id,
            authenticated: Arc::clone(&authenticated),
            evict: evict.clone(),
        });
        Some((
            LoopbackConnGuard {
                table: Arc::clone(table),
                id,
            },
            authenticated,
            evict,
        ))
    }

    fn release(&mut self, id: u64) {
        if let Some(idx) = self.active.iter().position(|slot| slot.id == id) {
            self.active.remove(idx);
        }
    }
}

struct LoopbackRequestCtx<'a> {
    conn_authenticated: &'a AtomicBool,
    probe_secret: &'a [u8; 32],
}

struct LoopbackConnGuard {
    table: Arc<Mutex<LoopbackConnTable>>,
    id: u64,
}

impl Drop for LoopbackConnGuard {
    fn drop(&mut self) {
        self.table
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .release(self.id);
    }
}

struct ActiveStreamGuard {
    active_streams: Arc<AtomicUsize>,
    active_streams_notify: Arc<Notify>,
    activity: Arc<Mutex<ActivityState>>,
}

impl Drop for ActiveStreamGuard {
    fn drop(&mut self) {
        if self.active_streams.fetch_sub(1, Ordering::AcqRel) == 1 {
            self.active_streams_notify.notify_waiters();
        }
        self.activity
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .last_activity = Instant::now();
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct FleetDirectBinding {
    holder: String,
    host_keys: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum TicketRouteBinding {
    /// Minted over `Route::Direct`: served only directly through this holder and pinned host-key
    /// identity. Never falls back to any relay.
    Direct {
        holder: String,
        direct_identity: Option<NodeRouteIdentity>,
        learned_host_keys: Option<Vec<String>>,
    },
    /// Minted over `Route::Relay { via }`: served only through that paired machine's identity, or
    /// upgraded at most once to a direct entry learned from that same machine during the ticket's
    /// fleet epoch, and after upgrade only through that direct entry's pinned identity.
    Relayed {
        relay_node: String,
        relay_identity: NodeRouteIdentity,
        upgraded_direct: Option<FleetDirectBinding>,
    },
}

fn parse_fleet_holder(holder: &str) -> Option<u64> {
    holder.strip_prefix("fleet:")?.parse::<u64>().ok()
}

struct LinkContentSource {
    backend: Arc<dyn Backend>,
    tickets: Arc<TicketTable>,
    ticket_cancels: Arc<Mutex<HashMap<[u8; 32], CancellationToken>>>,
    route_bindings: Arc<Mutex<HashMap<[u8; 32], TicketRouteBinding>>>,
}

impl LinkContentSource {
    fn authorize_target(
        &self,
        ticket: &Ticket,
        target: &crate::backend::ContentTarget,
    ) -> Result<(ContentRoutePolicy, Option<FleetDirectBinding>), OpenRangeError> {
        if !self.backend.is_pairing_holder_live(&ticket.holder) {
            return Err(OpenRangeError::TicketRevoked);
        }

        let binding = self
            .route_bindings
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&ticket.key_hash)
            .cloned();

        let Some(binding) = binding else {
            return if target.holder == ticket.holder {
                Ok((ContentRoutePolicy::DirectOnly, None))
            } else {
                Err(OpenRangeError::TicketRevoked)
            };
        };

        match binding {
            TicketRouteBinding::Direct {
                holder,
                direct_identity,
                learned_host_keys,
            } => {
                if target.holder != holder {
                    return Err(OpenRangeError::TicketRevoked);
                }
                if let Some(expected_identity) = direct_identity
                    && self.backend.node_route_identity(&ticket.machine).as_ref()
                        != Some(&expected_identity)
                {
                    return Err(OpenRangeError::TicketRevoked);
                }
                if let Some(expected_keys) = learned_host_keys {
                    let Some(identity) = target.learned_identity.as_ref() else {
                        return Err(OpenRangeError::TicketRevoked);
                    };
                    if identity.host_keys != expected_keys {
                        return Err(OpenRangeError::TicketRevoked);
                    }
                }
                Ok((ContentRoutePolicy::DirectOnly, None))
            }
            TicketRouteBinding::Relayed {
                relay_node,
                relay_identity,
                upgraded_direct,
            } => {
                let introducer_matches = |identity: &crate::backend::LearnedDirectIdentity| {
                    identity.from_paired_introducer
                        && relay_identity.is_paired
                        && identity.learned_from.as_deref() == Some(relay_node.as_str())
                        && identity.introducer_holder.as_deref()
                            == Some(relay_identity.holder.as_str())
                        && identity.introducer_host_keys == relay_identity.host_keys
                };

                if let Some(bound) = upgraded_direct {
                    // Once upgraded to a learned direct entry, the ticket is permanently bound to
                    // that exact holder, introducer, and pinned host-key identity; it never
                    // falls back to a relay or another fleet.
                    let Some(identity) = target.learned_identity.as_ref() else {
                        return Err(OpenRangeError::TicketRevoked);
                    };
                    let epoch_ok = parse_fleet_holder(&ticket.holder)
                        .is_none_or(|ep| identity.learned_fleet_epoch == ep);
                    if target.holder == bound.holder
                        && epoch_ok
                        && introducer_matches(identity)
                        && identity.host_keys == bound.host_keys
                    {
                        return Ok((ContentRoutePolicy::DirectOnly, None));
                    }
                    return Err(OpenRangeError::TicketRevoked);
                }

                // Still routed through the minting relay: verify the relay's own pairing identity
                // is still live and unchanged before serving or upgrading.
                if !self.backend.is_pairing_holder_live(&relay_identity.holder) {
                    return Err(OpenRangeError::TicketRevoked);
                }
                let Some(current_relay) = self.backend.node_route_identity(&relay_node) else {
                    return Err(OpenRangeError::TicketRevoked);
                };
                if current_relay != relay_identity {
                    return Err(OpenRangeError::TicketRevoked);
                }

                if target.holder == ticket.holder {
                    if parse_fleet_holder(&ticket.holder).is_some() {
                        return Ok((ContentRoutePolicy::RelayOnly(relay_node), None));
                    }
                    // Machine already had a configured direct entry whose mint probe fell back to
                    // `relay_node`. If that entry was learned from `relay_node`'s exact paired
                    // identity, permit upgrading to it once reachable; otherwise stay pinned
                    // strictly to `relay_node` so a fallback mint never crosses into another fleet.
                    if let Some(identity) = target.learned_identity.as_ref()
                        && introducer_matches(identity)
                        && !identity.host_keys.is_empty()
                    {
                        return Ok((
                            ContentRoutePolicy::DirectOrRelay(relay_node),
                            Some(FleetDirectBinding {
                                holder: target.holder.clone(),
                                host_keys: identity.host_keys.clone(),
                            }),
                        ));
                    }
                    return Ok((ContentRoutePolicy::RelayOnly(relay_node), None));
                }

                // Move from `fleet:<epoch>` to a direct entry: allowed at most once, and ONLY to
                // an entry learned from THAT SAME paired introducer identity during this ticket's
                // fleet epoch. Never allows a second paired fleet's introducer or a fresh direct
                // pairing under the same machine name.
                let Some(ticket_fleet_epoch) = parse_fleet_holder(&ticket.holder) else {
                    return Err(OpenRangeError::TicketRevoked);
                };
                let Some(identity) = target.learned_identity.as_ref() else {
                    return Err(OpenRangeError::TicketRevoked);
                };
                if !introducer_matches(identity)
                    || identity.learned_fleet_epoch != ticket_fleet_epoch
                    || identity.host_keys.is_empty()
                {
                    return Err(OpenRangeError::TicketRevoked);
                }
                Ok((
                    ContentRoutePolicy::DirectOrRelay(relay_node),
                    Some(FleetDirectBinding {
                        holder: target.holder.clone(),
                        host_keys: identity.host_keys.clone(),
                    }),
                ))
            }
        }
    }
}

impl ContentSource for LinkContentSource {
    fn open_range<'a>(
        &'a self,
        ticket: &'a Ticket,
        offset: u64,
        length: Option<u64>,
    ) -> Pin<Box<dyn Future<Output = Result<OpenedRange, OpenRangeError>> + Send + 'a>> {
        Box::pin(async move {
            let target = match self.backend.content_target(&ticket.machine) {
                Ok(target) => target,
                Err(_) => return Err(OpenRangeError::TicketRevoked),
            };
            let (route_policy, pending_upgrade) = self.authorize_target(ticket, &target)?;

            let stream_cancel = {
                let mut map = self
                    .ticket_cancels
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                map.entry(ticket.key_hash).or_default().child_token()
            };
            if !self.tickets.is_live(&ticket.key_hash) {
                if let Some(token) = self
                    .ticket_cancels
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .remove(&ticket.key_hash)
                {
                    token.cancel();
                }
                self.route_bindings
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .remove(&ticket.key_hash);
                return Err(OpenRangeError::TicketRevoked);
            }

            let stream = target
                .link
                .open_content_with_policy(
                    &target.link_node,
                    &ticket.path,
                    offset,
                    length,
                    route_policy,
                    stream_cancel,
                )
                .await
                .map_err(map_link_open_error)?;

            if let Some(bound) = pending_upgrade
                && stream.route() == &Route::Direct
            {
                let mut bindings = self
                    .route_bindings
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                if let Some(TicketRouteBinding::Relayed {
                    upgraded_direct, ..
                }) = bindings.get_mut(&ticket.key_hash)
                    && upgraded_direct.is_none()
                {
                    *upgraded_direct = Some(bound);
                }
            }

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
        let target = match self.backend.content_target(&ticket.machine) {
            Ok(target) => target,
            Err(_) => return Err(OpenRangeError::TicketRevoked),
        };
        self.authorize_target(ticket, &target).map(|_| ())
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
    probe_timeout: Duration,
    probe_freshness: Duration,
    loopback_conns: Arc<Mutex<LoopbackConnTable>>,
    custom_scheme_max_full_bytes: u64,
    custom_scheme_memory_budget: Arc<Semaphore>,
    custom_scheme_buffered_bytes: Arc<AtomicUsize>,
    custom_scheme_peak_buffered_bytes: Arc<AtomicUsize>,
    listener_lock: AsyncMutex<()>,
    listener: Mutex<Option<ListenerHandle>>,
    active_streams: Arc<AtomicUsize>,
    active_streams_notify: Arc<Notify>,
    activity: Arc<Mutex<ActivityState>>,
    request_log: Mutex<VecDeque<ContentRequestLog>>,
    ticket_statuses: Mutex<VecDeque<TicketStatusEntry>>,
    ticket_cancels: Arc<Mutex<HashMap<[u8; 32], CancellationToken>>>,
    route_bindings: Arc<Mutex<HashMap<[u8; 32], TicketRouteBinding>>>,
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
        let tickets = Arc::new(TicketTable::with_config(ticket_config));
        let ticket_cancels = Arc::new(Mutex::new(HashMap::new()));
        let route_bindings = Arc::new(Mutex::new(HashMap::new()));
        let source = Arc::new(LinkContentSource {
            backend: Arc::clone(&backend),
            tickets: Arc::clone(&tickets),
            ticket_cancels: Arc::clone(&ticket_cancels),
            route_bindings: Arc::clone(&route_bindings),
        });
        let conn_cap = max_loopback_connections.max(1);
        let budget_units = custom_scheme_total_memory_budget_bytes
            .div_ceil(CUSTOM_SCHEME_BUDGET_UNIT_BYTES)
            .max(1);
        Self {
            backend,
            mode,
            tickets,
            limiter: ContentLimiter::default(),
            source,
            allowed_origins,
            idle_shutdown,
            header_read_timeout,
            probe_timeout: LOOPBACK_PROBE_TIMEOUT,
            probe_freshness: LOOPBACK_PROBE_FRESHNESS,
            loopback_conns: Arc::new(Mutex::new(LoopbackConnTable::new(conn_cap))),
            custom_scheme_max_full_bytes,
            custom_scheme_memory_budget: Arc::new(Semaphore::new(budget_units)),
            custom_scheme_buffered_bytes: Arc::new(AtomicUsize::new(0)),
            custom_scheme_peak_buffered_bytes: Arc::new(AtomicUsize::new(0)),
            listener_lock: AsyncMutex::new(()),
            listener: Mutex::new(None),
            active_streams: Arc::new(AtomicUsize::new(0)),
            active_streams_notify: Arc::new(Notify::new()),
            activity: Arc::new(Mutex::new(ActivityState {
                last_activity: Instant::now(),
            })),
            request_log: Mutex::new(VecDeque::with_capacity(MAX_REQUEST_LOG)),
            ticket_statuses: Mutex::new(VecDeque::with_capacity(MAX_REQUEST_LOG)),
            ticket_cancels,
            route_bindings,
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
        // machines return a structured AppError immediately, and capture the exact route that
        // answered so the minted ticket stays bound to that route/introducer.
        let probe = target
            .link
            .open_content(&target.link_node, path, 0, Some(0))
            .await
            .map_err(AppError::from)?;
        let mint_route = probe.route().clone();
        drop(probe);

        let binding = match mint_route {
            Route::Direct => TicketRouteBinding::Direct {
                holder: target.holder.clone(),
                direct_identity: self.backend.node_route_identity(machine_trimmed),
                learned_host_keys: target
                    .learned_identity
                    .as_ref()
                    .map(|id| id.host_keys.clone()),
            },
            Route::Relay { via } => {
                let Some(relay_identity) = self.backend.node_route_identity(&via) else {
                    return Err(AppError::new(
                        ErrorCode::Unreachable,
                        format!("Can't reach {machine_trimmed} right now."),
                    ));
                };
                TicketRouteBinding::Relayed {
                    relay_node: via,
                    relay_identity,
                    upgraded_direct: None,
                }
            }
        };

        let issued = self
            .tickets
            .issue(TicketRequest {
                holder: target.holder,
                principal: target.principal,
                machine: target.machine,
                path: path.to_owned(),
            })
            .map_err(AppError::from)?;
        self.route_bindings
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(ticket_hash(&issued.id), binding);
        self.prune_dead_ticket_cancels();

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

    fn cancel_ticket_streams(&self, key_hash: &[u8; 32]) {
        if let Some(token) = self
            .ticket_cancels
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(key_hash)
        {
            token.cancel();
        }
        self.route_bindings
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(key_hash);
    }

    fn prune_dead_ticket_cancels(&self) {
        let dead: Vec<CancellationToken> = {
            let mut map = self
                .ticket_cancels
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let dead_keys: Vec<[u8; 32]> = map
                .keys()
                .copied()
                .filter(|h| !self.tickets.contains_hash(h))
                .collect();
            dead_keys
                .into_iter()
                .filter_map(|h| map.remove(&h))
                .collect()
        };
        for token in dead {
            token.cancel();
        }
        self.route_bindings
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .retain(|h, _| self.tickets.contains_hash(h));
    }

    /// Revokes the single content ticket identified by `raw_url` immediately and cancels any
    /// in-flight or active streams serving it (used when the viewer closes, switches files, or
    /// tears down a media element). Returns `true` if a live ticket was removed.
    pub fn revoke_url(&self, raw_url: &str) -> bool {
        let Some(ticket_id) = extract_ticket_id_from_url(raw_url) else {
            return false;
        };
        let key_hash = ticket_hash(ticket_id);
        let removed = self.tickets.revoke(ticket_id);
        self.cancel_ticket_streams(&key_hash);
        if removed {
            let mut statuses = self
                .ticket_statuses
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            statuses.retain(|entry| entry.key_hash != key_hash);
        }
        removed
    }

    /// Revokes every ticket belonging to pairings that are no longer live in [`Backend`].
    pub fn revoke_stale_pairings(&self) {
        let _ = self
            .tickets
            .retain_holders(|holder| self.backend.is_pairing_holder_live(holder));
        self.prune_dead_ticket_cancels();
    }

    /// Returns the number of content response streams currently being served.
    pub fn active_streams(&self) -> usize {
        self.active_streams.load(Ordering::Acquire)
    }

    /// Waits up to `timeout` for every active content response stream to finish and release its
    /// underlying direct or relayed [`legion_link::ContentStream`]. Returns `true` when zero
    /// streams remain active.
    pub async fn wait_for_active_streams_stopped(&self, timeout: Duration) -> bool {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let notified = self.active_streams_notify.notified();
            if self.active_streams.load(Ordering::Acquire) == 0 {
                return true;
            }
            let Some(remaining) = deadline.checked_duration_since(tokio::time::Instant::now())
            else {
                return self.active_streams.load(Ordering::Acquire) == 0;
            };
            if tokio::time::timeout(remaining, notified).await.is_err() {
                return self.active_streams.load(Ordering::Acquire) == 0;
            }
        }
    }

    /// Revokes every issued content ticket (used on store repair and app exit).
    pub fn revoke_all(&self) {
        let _ = self.tickets.retain_holders(|_| false);
        let tokens: Vec<CancellationToken> = self
            .ticket_cancels
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .drain()
            .map(|(_, tok)| tok)
            .collect();
        for tok in tokens {
            tok.cancel();
        }
        self.route_bindings
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clear();
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

    /// Verifies that an active loopback accept loop is still running AND answers a fresh
    /// proof-of-ownership probe with this listener's secret; if either check fails while live
    /// tickets remain, tears down the stale handle and binds a fresh OS-chosen port on
    /// `127.0.0.1:0`.
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
            slot.as_ref()
                .map(|h| (h.port, h.is_alive(), h.probe_secret))
        };
        let Some((port, alive, secret)) = state else {
            return Ok(None);
        };
        if alive && probe_loopback_ownership(port, &secret, self.probe_timeout).await {
            let still_live = {
                let mut slot = self
                    .listener
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                if let Some(h) = slot.as_mut().filter(|h| h.port == port && h.is_alive()) {
                    h.last_verified = Instant::now();
                    true
                } else {
                    false
                }
            };
            if still_live {
                return Ok(Some(port));
            }
        }

        // Accept task stopped or failed proof-of-ownership (e.g. reclaimed socket or squatter):
        // tear down the stale handle and bind a fresh OS-chosen port if live tickets remain.
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
            .handle_checked_request(&guard, &method, &uri, &headers, options, None)
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
            && self.tickets.contains_hash(&key_hash)
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
    /// URL. Requires both a running accept task and, whenever the last successful proof is older
    /// than [`LOOPBACK_PROBE_FRESHNESS`], a valid proof-of-ownership probe response; tears down and
    /// binds a fresh OS-chosen port on any failure.
    async fn ensure_live_loopback_listener(self: &Arc<Self>) -> Result<u16, AppError> {
        let _guard = self.listener_lock.lock().await;
        let state = {
            let slot = self
                .listener
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            slot.as_ref()
                .map(|h| (h.port, h.is_alive(), h.probe_secret, h.last_verified))
        };
        if let Some((port, alive, secret, last_verified)) = state {
            if alive {
                if last_verified.elapsed() < self.probe_freshness {
                    return Ok(port);
                }
                if probe_loopback_ownership(port, &secret, self.probe_timeout).await {
                    let still_live = {
                        let mut slot = self
                            .listener
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner());
                        if let Some(h) = slot.as_mut().filter(|h| h.port == port && h.is_alive()) {
                            h.last_verified = Instant::now();
                            true
                        } else {
                            false
                        }
                    };
                    if still_live {
                        return Ok(port);
                    }
                }
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

        let mut probe_secret = [0u8; 32];
        getrandom::fill(&mut probe_secret).map_err(|err| {
            AppError::new(
                ErrorCode::Internal,
                format!("Could not generate local content listener secret: {err}"),
            )
        })?;

        let bind_addr = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0));
        let socket = TcpSocket::new_v4().map_err(|err| {
            AppError::new(
                ErrorCode::Internal,
                format!("Could not create local content listener socket: {err}"),
            )
        })?;
        let _ = socket.set_send_buffer_size(LOOPBACK_SOCKET_SNDBUF);
        socket.bind(bind_addr).map_err(|err| {
            AppError::new(
                ErrorCode::Internal,
                format!("Could not bind local content listener on 127.0.0.1: {err}"),
            )
        })?;
        let tcp = socket.listen(1024).map_err(|err| {
            AppError::new(
                ErrorCode::Internal,
                format!("Could not listen on local content socket 127.0.0.1: {err}"),
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
                .run_accept_loop(tcp, port, probe_secret, accept_shutdown, accept_flag)
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
                probe_secret,
                last_verified: Instant::now(),
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
        probe_secret: [u8; 32],
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
            let Some((conn_guard_slot, conn_authenticated, conn_evict)) =
                LoopbackConnTable::admit(&self.loopback_conns)
            else {
                // Every active slot is held by an authenticated connection; drop excess immediately.
                drop(stream);
                continue;
            };
            let _ = stream.set_nodelay(true);
            let server = Arc::clone(&self);
            let conn_guard = Arc::clone(&guard);
            let conn_shutdown = shutdown.clone();
            let unauth_timeout = header_read_timeout.min(LOOPBACK_UNAUTH_HEADER_READ_TIMEOUT);
            tokio::spawn(async move {
                let _slot_guard = conn_guard_slot;
                let io = TokioIo::new(stream);
                let service_auth = Arc::clone(&conn_authenticated);
                let service = service_fn(move |req: Request<Incoming>| {
                    let server = Arc::clone(&server);
                    let guard = Arc::clone(&conn_guard);
                    let auth_flag = Arc::clone(&service_auth);
                    async move {
                        let method = req.method().clone();
                        let uri = req.uri().clone();
                        let headers = req.headers().clone();
                        let resp = server
                            .handle_checked_request(
                                &guard,
                                &method,
                                &uri,
                                &headers,
                                options,
                                Some(LoopbackRequestCtx {
                                    conn_authenticated: &auth_flag,
                                    probe_secret: &probe_secret,
                                }),
                            )
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
                let unauth_deadline = tokio::time::sleep(unauth_timeout);
                tokio::pin!(unauth_deadline);
                let mut check_unauth = true;
                loop {
                    tokio::select! {
                        biased;
                        () = conn_shutdown.cancelled() => {
                            // Drop the connection immediately on listener shutdown/teardown so
                            // in-flight streaming bodies stop reading from the remote source and
                            // release permits/guards without waiting for a paused client.
                            break;
                        }
                        () = conn_evict.cancelled() => {
                            // Immediately drop an evicted unauthenticated socket without waiting
                            // for its peer to finish headers.
                            break;
                        }
                        () = &mut unauth_deadline, if check_unauth => {
                            check_unauth = false;
                            if !conn_authenticated.load(Ordering::Acquire) {
                                break;
                            }
                        }
                        _ = &mut conn => break,
                    }
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
        loopback: Option<LoopbackRequestCtx<'_>>,
    ) -> Response<ContentBody> {
        self.touch_activity();

        let cors_origin = match guard.check(headers, uri) {
            Ok(origin) => origin,
            Err(rejection) => {
                return guard_error_response(rejection, options.corp);
            }
        };

        if *method == Method::OPTIONS {
            return preflight_response(cors_origin, options.corp);
        }

        // Loopback-only proof-of-ownership liveness probe. Must never flip `conn_authenticated`
        // (counts as unauthenticated for connection limits), never log secrets or populate ticket
        // diagnostics, and must fall through to the exact generic `404` when the secret is absent
        // or invalid.
        if *method == Method::GET
            && uri.path() == LOOPBACK_PROBE_PATH
            && uri.query().is_none()
            && let Some(ctx) = loopback.as_ref()
            && let Some(response) =
                verify_and_build_probe_response(ctx.probe_secret, headers, options.corp)
        {
            return response;
        }

        let Some((ticket_id, file_name)) = parse_content_path(uri.path()) else {
            return not_found_response(options.corp, cors_origin);
        };

        // Only track status, stream-failure state, and connection authentication for requests that
        // present a real, live ticket. Requests with unknown, expired, or invalid tickets must
        // never allocate status slots or keep a loopback socket alive past the unauthenticated
        // header timeout.
        let valid_ticket = self.tickets.lookup(ticket_id);
        if valid_ticket.is_some()
            && let Some(ctx) = loopback.as_ref()
        {
            ctx.conn_authenticated.store(true, Ordering::Release);
        }
        let key_hash = valid_ticket.as_ref().map(|t| t.key_hash);
        let stream_failed = key_hash.map(|h| self.ensure_ticket_stream_flag(h));

        self.active_streams.fetch_add(1, Ordering::AcqRel);
        let active_guard = ActiveStreamGuard {
            active_streams: Arc::clone(&self.active_streams),
            active_streams_notify: Arc::clone(&self.active_streams_notify),
            activity: Arc::clone(&self.activity),
        };
        let hooks = StreamHooks {
            limiter: Some(self.limiter.clone()),
            bytes_served: None,
            stream_failed,
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

        if response.status() == StatusCode::NOT_FOUND {
            response
                .headers_mut()
                .insert(CONNECTION, HeaderValue::from_static("close"));
        }

        if let Some(origin) = cors_origin {
            apply_cors_headers(response.headers_mut(), origin);
        }

        if let Some(key_hash) = key_hash {
            self.record_ticket_status(
                key_hash,
                response.status(),
                response.headers(),
                response.body().fixed_bytes(),
                true,
            );
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
        .headers_mut()
        .insert(CONNECTION, HeaderValue::from_static("close"));
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
    response
        .headers_mut()
        .insert(CONNECTION, HeaderValue::from_static("close"));
    if let Some(origin) = cors_origin {
        apply_cors_headers(response.headers_mut(), origin);
    }
    response
}

fn probe_mac(secret: &[u8; 32], domain: &[u8], challenge: &[u8; 32]) -> blake3::Hash {
    let mut hasher = blake3::Hasher::new_keyed(secret);
    hasher.update(domain);
    hasher.update(challenge);
    hasher.finalize()
}

fn decode_b64url_32(raw: &str) -> Option<[u8; 32]> {
    let decoded = URL_SAFE_NO_PAD.decode(raw.as_bytes()).ok()?;
    decoded.try_into().ok()
}

fn single_header_str<'a>(headers: &'a HeaderMap, name: &HeaderName) -> Option<&'a str> {
    let mut iter = headers.get_all(name).iter();
    let first = iter.next()?;
    if iter.next().is_some() {
        return None;
    }
    first.to_str().ok().map(str::trim)
}

fn verify_and_build_probe_response(
    secret: &[u8; 32],
    headers: &HeaderMap,
    corp: CorpPolicy,
) -> Option<Response<ContentBody>> {
    // The internal loopback probe never sends an Origin header.
    if headers.contains_key(ORIGIN) {
        return None;
    }
    let challenge = decode_b64url_32(single_header_str(
        headers,
        &LOOPBACK_PROBE_CHALLENGE_HEADER,
    )?)?;
    let auth = decode_b64url_32(single_header_str(headers, &LOOPBACK_PROBE_AUTH_HEADER)?)?;
    let expected_auth = probe_mac(secret, LOOPBACK_PROBE_REQ_DOMAIN, &challenge);
    if expected_auth != blake3::Hash::from_bytes(auth) {
        return None;
    }
    let proof = probe_mac(secret, LOOPBACK_PROBE_RESP_DOMAIN, &challenge);
    let proof_b64 = URL_SAFE_NO_PAD.encode(proof.as_bytes());
    let proof_val = HeaderValue::from_str(&proof_b64).ok()?;

    let mut response = Response::new(ContentBody::empty());
    *response.status_mut() = StatusCode::NO_CONTENT;
    apply_base_security_headers(response.headers_mut(), corp);
    response
        .headers_mut()
        .insert(LOOPBACK_PROBE_PROOF_HEADER, proof_val);
    response
        .headers_mut()
        .insert(CONNECTION, HeaderValue::from_static("close"));
    Some(response)
}

fn verify_probe_http_response(raw: &[u8], expected_proof: &blake3::Hash) -> bool {
    let Some(header_end) = raw.windows(4).position(|w| w == b"\r\n\r\n") else {
        return false;
    };
    let Ok(head) = std::str::from_utf8(&raw[..header_end]) else {
        return false;
    };
    let mut lines = head.split("\r\n");
    let Some(status_line) = lines.next() else {
        return false;
    };
    if status_line != "HTTP/1.1 204" && !status_line.starts_with("HTTP/1.1 204 ") {
        return false;
    }
    let mut proof_value: Option<&str> = None;
    for line in lines {
        let Some((name, val)) = line.split_once(':') else {
            return false;
        };
        if name
            .trim()
            .eq_ignore_ascii_case(LOOPBACK_PROBE_PROOF_HEADER.as_str())
        {
            if proof_value.is_some() {
                return false;
            }
            proof_value = Some(val.trim());
        }
    }
    let Some(proof_b64) = proof_value else {
        return false;
    };
    let Some(proof_bytes) = decode_b64url_32(proof_b64) else {
        return false;
    };
    *expected_proof == blake3::Hash::from_bytes(proof_bytes)
}

async fn probe_loopback_ownership(port: u16, secret: &[u8; 32], timeout: Duration) -> bool {
    let mut challenge = [0u8; 32];
    if getrandom::fill(&mut challenge).is_err() {
        return false;
    }
    let challenge_b64 = URL_SAFE_NO_PAD.encode(challenge);
    let req_tag = probe_mac(secret, LOOPBACK_PROBE_REQ_DOMAIN, &challenge);
    let req_tag_b64 = URL_SAFE_NO_PAD.encode(req_tag.as_bytes());
    let expected_proof = probe_mac(secret, LOOPBACK_PROBE_RESP_DOMAIN, &challenge);

    let request = format!(
        "GET {LOOPBACK_PROBE_PATH} HTTP/1.1\r\n\
         Host: 127.0.0.1:{port}\r\n\
         {LOOPBACK_PROBE_CHALLENGE_HEADER}: {challenge_b64}\r\n\
         {LOOPBACK_PROBE_AUTH_HEADER}: {req_tag_b64}\r\n\
         Connection: close\r\n\r\n"
    );

    let probe_fut = async {
        let mut stream = TcpStream::connect(SocketAddrV4::new(Ipv4Addr::LOCALHOST, port))
            .await
            .ok()?;
        let _ = stream.set_nodelay(true);
        stream.write_all(request.as_bytes()).await.ok()?;
        stream.flush().await.ok()?;

        let mut buf = Vec::with_capacity(512);
        let mut chunk = [0u8; 512];
        loop {
            let n = stream.read(&mut chunk).await.ok()?;
            if n == 0 {
                break;
            }
            let remaining = LOOPBACK_PROBE_MAX_RESPONSE_BYTES.saturating_sub(buf.len());
            buf.extend_from_slice(&chunk[..n.min(remaining)]);
            if buf.windows(4).any(|w| w == b"\r\n\r\n")
                || buf.len() >= LOOPBACK_PROBE_MAX_RESPONSE_BYTES
            {
                break;
            }
        }
        Some(buf)
    };

    let Ok(Some(raw)) = tokio::time::timeout(timeout, probe_fut).await else {
        return false;
    };
    verify_probe_http_response(&raw, &expected_proof)
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
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

    use legion_link::proto::{
        ErrorBody, ErrorCode as ProtoCode, HelloResult, PROTOCOL, Response as ProtoResp, VERSION,
    };
    use legion_link::{
        Link, LinkConfig, LinkOptions, NodeLink, RawStream, Session, SessionOpener, SshTarget,
        StreamOpenError, StreamOpener, TransportHandle,
    };
    use serde_json::{Value, json};
    use tokio::io::{AsyncBufReadExt, BufReader};
    use tokio::sync::Notify;

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

    #[derive(Clone)]
    enum MockFile {
        Bytes(Vec<u8>),
        /// Deterministic virtual pattern file of arbitrary length without allocating RAM.
        Pattern {
            len: u64,
            mul: u64,
            add: u64,
        },
    }

    impl MockFile {
        fn len(&self) -> u64 {
            match self {
                Self::Bytes(bytes) => bytes.len() as u64,
                Self::Pattern { len, .. } => *len,
            }
        }

        fn fill_range(&self, offset: u64, out: &mut [u8]) {
            match self {
                Self::Bytes(bytes) => {
                    let start = usize::try_from(offset).unwrap();
                    out.copy_from_slice(&bytes[start..start + out.len()]);
                }
                Self::Pattern { mul, add, .. } => {
                    for (idx, slot) in out.iter_mut().enumerate() {
                        let pos = offset.wrapping_add(idx as u64);
                        *slot = (pos.wrapping_mul(*mul).wrapping_add(*add) % 251) as u8;
                    }
                }
            }
        }

        fn assert_matches(&self, offset: u64, actual: &[u8]) {
            match self {
                Self::Bytes(bytes) => {
                    let start = usize::try_from(offset).unwrap();
                    assert_eq!(actual, &bytes[start..start + actual.len()]);
                }
                Self::Pattern { mul, add, .. } => {
                    for (idx, &byte) in actual.iter().enumerate() {
                        let pos = offset.wrapping_add(idx as u64);
                        let expected = (pos.wrapping_mul(*mul).wrapping_add(*add) % 251) as u8;
                        assert_eq!(byte, expected, "pattern mismatch at byte offset {pos}");
                    }
                }
            }
        }
    }

    #[derive(Default)]
    struct MockStreamStats {
        stream_opens: AtomicU64,
        read_windows: AtomicU64,
        bytes_produced: AtomicU64,
        parked_streams: AtomicU64,
        active_remote_tasks: AtomicUsize,
        notify: Notify,
    }

    struct ActiveTaskGuard {
        stats: Arc<MockStreamStats>,
    }

    impl Drop for ActiveTaskGuard {
        fn drop(&mut self) {
            self.stats
                .active_remote_tasks
                .fetch_sub(1, Ordering::AcqRel);
            self.stats.notify.notify_waiters();
        }
    }

    struct ParkGuard {
        park: CancellationToken,
        stats: Arc<MockStreamStats>,
    }

    impl Drop for ParkGuard {
        fn drop(&mut self) {
            if self.park.is_cancelled() {
                self.stats.parked_streams.fetch_add(1, Ordering::AcqRel);
                self.stats.notify.notify_waiters();
            }
        }
    }

    struct MemoryFilesOpener {
        files: Arc<HashMap<String, MockFile>>,
        window_policy: Option<legion_link::content::WindowPolicy>,
        fault: Arc<Mutex<MockStreamFault>>,
        stats: Arc<MockStreamStats>,
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
            let stats = Arc::clone(&self.stats);
            Box::pin(async move {
                stats.stream_opens.fetch_add(1, Ordering::AcqRel);
                let (client_in, mut server_out) = tokio::io::duplex(64 * 1024);
                let (server_in, client_out) = tokio::io::duplex(64 * 1024);
                let cancel = CancellationToken::new();
                let park = CancellationToken::new();
                let watch_cancel = cancel.clone();
                let watch_park = park.clone();
                stats.active_remote_tasks.fetch_add(1, Ordering::AcqRel);
                let task_stats = Arc::clone(&stats);
                tokio::spawn(async move {
                    let _task_guard = ActiveTaskGuard {
                        stats: Arc::clone(&task_stats),
                    };
                    let mut reader = BufReader::new(server_in);
                    let mut sent_protocol = false;
                    let mut scratch = vec![0u8; 64 * 1024];
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
                        let Some(file) = files.get(path) else {
                            let err = json!({
                                "error": ErrorBody::new(ProtoCode::NotFound, "file not found")
                            });
                            let _ = server_out.write_all(format!("{err}\n").as_bytes()).await;
                            return;
                        };
                        let file_len = file.len();
                        let offset = req["offset"].as_u64().unwrap_or(0);
                        let req_len = req.get("length").and_then(Value::as_u64);
                        let active_fault = fault.lock().unwrap().clone();
                        if let MockStreamFault::ErrorOnGet { code, message } = active_fault {
                            let err = json!({ "error": ErrorBody::new(code, message) });
                            let _ = server_out.write_all(format!("{err}\n").as_bytes()).await;
                            return;
                        }
                        if offset > file_len {
                            let err = json!({
                                "error": ErrorBody::new(ProtoCode::Conflict, "offset past end")
                                    .with_details(json!({"actual": file_len}))
                            });
                            let _ = server_out.write_all(format!("{err}\n").as_bytes()).await;
                            return;
                        }
                        let avail = file_len - offset;
                        let len = req_len.map_or(avail, |n| n.min(avail));
                        if len > 0 {
                            task_stats.read_windows.fetch_add(1, Ordering::AcqRel);
                        }
                        let keep_open =
                            supports_keep_open && req["keep_open"].as_bool() == Some(true);
                        let mut ready = json!({
                            "size": file_len,
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
                            && len > cut as u64
                        {
                            let cut_u64 = cut as u64;
                            let mut cut_written = 0u64;
                            while cut_written < cut_u64 {
                                let step =
                                    (cut_u64 - cut_written).min(scratch.len() as u64) as usize;
                                file.fill_range(offset + cut_written, &mut scratch[..step]);
                                if server_out.write_all(&scratch[..step]).await.is_err() {
                                    return;
                                }
                                cut_written += step as u64;
                                task_stats
                                    .bytes_produced
                                    .fetch_add(step as u64, Ordering::AcqRel);
                            }
                            let _ = server_out.shutdown().await;
                            return;
                        }
                        let mut written = 0u64;
                        while written < len {
                            let step = (len - written).min(scratch.len() as u64) as usize;
                            file.fill_range(offset + written, &mut scratch[..step]);
                            let write_ok = tokio::select! {
                                biased;
                                () = watch_cancel.cancelled() => false,
                                () = watch_park.cancelled() => false,
                                res = server_out.write_all(&scratch[..step]) => res.is_ok(),
                            };
                            if !write_ok {
                                return;
                            }
                            written += step as u64;
                            task_stats
                                .bytes_produced
                                .fetch_add(step as u64, Ordering::AcqRel);
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
                let park_guard = ParkGuard {
                    park: park.clone(),
                    stats,
                };
                Ok(RawStream {
                    reader: Box::new(client_in),
                    writer: Box::new(client_out),
                    stderr: None,
                    cancel,
                    park: Some(park),
                    guard: Some(Box::new(park_guard)),
                })
            })
        }
    }

    struct MockContentBackend {
        link: Link,
        live: Arc<AtomicBool>,
        holder: String,
        fault: Arc<Mutex<MockStreamFault>>,
        stats: Arc<MockStreamStats>,
    }

    impl MockContentBackend {
        fn new(files: HashMap<String, Vec<u8>>) -> Self {
            let mock_files = files
                .into_iter()
                .map(|(k, v)| (k, MockFile::Bytes(v)))
                .collect();
            Self::with_mock_files(mock_files, LinkOptions::default(), None)
        }

        fn with_mock_files(
            files: HashMap<String, MockFile>,
            link_options: LinkOptions,
            window_policy: Option<legion_link::content::WindowPolicy>,
        ) -> Self {
            let fault = Arc::new(Mutex::new(MockStreamFault::None));
            let stats = Arc::new(MockStreamStats::default());
            let opener_files = Arc::new(MemoryFilesOpener {
                files: Arc::new(files),
                window_policy,
                fault: Arc::clone(&fault),
                stats: Arc::clone(&stats),
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
                stats,
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
                learned_identity: None,
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
            Duration::from_millis(400),
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

        // 1) Slowloris header-read timeout: send incomplete HTTP headers and verify server closes
        // promptly after the configured timeout rather than holding the slot open forever.
        let mut slow = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        slow.write_all(b"GET /v1/content/").await.unwrap();
        let mut sink = Vec::new();
        let slow_closed =
            tokio::time::timeout(Duration::from_secs(2), slow.read_to_end(&mut sink)).await;
        assert!(
            slow_closed.is_ok(),
            "slowloris socket must time out and close"
        );

        // 2) Authenticated connection cap: when both slots are held by authenticated keep-alive
        // sockets, a third simultaneous connection exceeds the cap of 2 and is dropped immediately.
        let keep_alive_req = format!(
            "GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: keep-alive\r\n\r\n"
        );
        let mut auth1 = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        auth1.write_all(keep_alive_req.as_bytes()).await.unwrap();
        let mut auth2 = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        auth2.write_all(keep_alive_req.as_bytes()).await.unwrap();
        // Wait for both responses to finish so both slots are marked authenticated and idle on keep-alive.
        let mut buf1 = [0u8; 1024];
        let mut buf2 = [0u8; 1024];
        let n1 = tokio::time::timeout(Duration::from_millis(300), auth1.read(&mut buf1))
            .await
            .unwrap()
            .unwrap();
        let n2 = tokio::time::timeout(Duration::from_millis(300), auth2.read(&mut buf2))
            .await
            .unwrap()
            .unwrap();
        assert!(String::from_utf8_lossy(&buf1[..n1]).contains("200 OK"));
        assert!(String::from_utf8_lossy(&buf2[..n2]).contains("200 OK"));

        let mut overflow = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        let mut byte = [0u8; 1];
        let overflow_read =
            tokio::time::timeout(Duration::from_millis(150), overflow.read(&mut byte)).await;
        assert!(
            matches!(overflow_read, Ok(Ok(0)) | Ok(Err(_))),
            "excess connection above authenticated cap should be dropped immediately, got {overflow_read:?}"
        );
        drop(auth1);
        drop(auth2);
        tokio::time::sleep(Duration::from_millis(30)).await;

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
                None,
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

    /// Connects to the loopback listener with a small `SO_RCVBUF` so paused reads trigger TCP
    /// backpressure promptly across platforms (disabling Windows TCP receive-window autotuning).
    async fn connect_bounded_client(port: u16) -> TcpStream {
        let socket = TcpSocket::new_v4().unwrap();
        let _ = socket.set_recv_buffer_size(16 * 1024);
        socket
            .connect(SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, port)))
            .await
            .unwrap()
    }

    /// Waits deterministically until at least `expected_parked` mock streams have completed
    /// `ParkActivity` parking (including `ParkedStreamPool::register` eviction) AND every remote
    /// source task has exited.
    async fn wait_for_parked_and_idle(
        stats: &MockStreamStats,
        expected_parked: u64,
        timeout: Duration,
    ) {
        let res = tokio::time::timeout(timeout, async {
            loop {
                let notified = stats.notify.notified();
                if stats.parked_streams.load(Ordering::Acquire) >= expected_parked
                    && stats.active_remote_tasks.load(Ordering::Acquire) == 0
                {
                    return;
                }
                notified.await;
            }
        })
        .await;
        assert!(
            res.is_ok(),
            "timed out waiting for {expected_parked} parked streams and 0 active remote tasks \
             (parked={}, active={})",
            stats.parked_streams.load(Ordering::Acquire),
            stats.active_remote_tasks.load(Ordering::Acquire),
        );
    }

    async fn wait_for_zero_active_tasks(stats: &MockStreamStats, timeout: Duration) {
        let res = tokio::time::timeout(timeout, async {
            loop {
                let notified = stats.notify.notified();
                if stats.active_remote_tasks.load(Ordering::Acquire) == 0 {
                    return;
                }
                notified.await;
            }
        })
        .await;
        assert!(
            res.is_ok(),
            "timed out waiting for remote source tasks to stop (active={})",
            stats.active_remote_tasks.load(Ordering::Acquire),
        );
    }

    async fn wait_for_active_streams(
        server: &AppContentServer,
        expected: usize,
        timeout: Duration,
    ) {
        let res = tokio::time::timeout(timeout, async {
            loop {
                if server.active_streams() == expected {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await;
        assert!(
            res.is_ok(),
            "timed out waiting for active_streams == {expected} (actual={})",
            server.active_streams(),
        );
    }

    #[tokio::test]
    async fn relayed_picture_and_ranged_media_work_on_loopback_then_switch_to_direct() {
        use std::sync::atomic::AtomicUsize;

        let png_bytes = vec![
            0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 0, 0, 0, 0, 1, 2, 3, 4,
        ];
        let media_bytes: Vec<u8> = (0..96 * 1024usize).map(|i| (i % 251) as u8).collect();
        let mut studio_files = HashMap::new();
        studio_files.insert("/home/me/photo.png".to_owned(), png_bytes.clone());
        studio_files.insert("/home/me/video.mp4".to_owned(), media_bytes.clone());
        let studio_files = Arc::new(studio_files);

        let relay_hits = Arc::new(AtomicUsize::new(0));
        let direct_hits = Arc::new(AtomicUsize::new(0));
        let studio_key = legion_link::DeviceKey::from_seed([55u8; 32])
            .public_key_line("studio")
            .unwrap();
        let tower_key = legion_link::DeviceKey::from_seed([56u8; 32])
            .public_key_line("tower")
            .unwrap();

        struct FleetRelayStreamOpener {
            node: String,
            files: Arc<HashMap<String, Vec<u8>>>,
            relay_hits: Arc<AtomicUsize>,
            direct_hits: Arc<AtomicUsize>,
        }

        impl StreamOpener for FleetRelayStreamOpener {
            fn open_stream(
                &self,
                _cancel: CancellationToken,
            ) -> Pin<Box<dyn Future<Output = Result<RawStream, StreamOpenError>> + Send + '_>>
            {
                let node = self.node.clone();
                let files = Arc::clone(&self.files);
                let relay_hits = Arc::clone(&self.relay_hits);
                let direct_hits = Arc::clone(&self.direct_hits);
                Box::pin(async move {
                    let (client_in, mut server_out) = tokio::io::duplex(64 * 1024);
                    let (server_in, client_out) = tokio::io::duplex(64 * 1024);
                    let cancel = CancellationToken::new();
                    tokio::spawn(async move {
                        let _ = server_out
                            .write_all(
                                format!(
                                    "{}\n",
                                    json!({
                                        "protocol": 1,
                                        "capabilities": [legion_link::CAP_STREAM_RELAY]
                                    })
                                )
                                .as_bytes(),
                            )
                            .await;
                        let mut reader = BufReader::new(server_in);
                        let mut line = String::new();
                        if reader.read_line(&mut line).await.unwrap_or(0) == 0 {
                            return;
                        }
                        let req: Value = serde_json::from_str(&line).unwrap();
                        let inner = if node == "tower" && req["op"] == json!("relay") {
                            assert_eq!(req["on"], json!("studio"));
                            relay_hits.fetch_add(1, Ordering::SeqCst);
                            &req["request"]
                        } else {
                            assert_eq!(node, "studio");
                            direct_hits.fetch_add(1, Ordering::SeqCst);
                            &req
                        };
                        let path = inner["path"].as_str().unwrap_or("");
                        let Some(payload) = files.get(path) else {
                            let err = json!({
                                "error": ErrorBody::new(ProtoCode::NotFound, "not found")
                                    .with_machine("studio")
                            });
                            let _ = server_out.write_all(format!("{err}\n").as_bytes()).await;
                            return;
                        };
                        let offset = inner["offset"].as_u64().unwrap_or(0) as usize;
                        let avail = payload.len().saturating_sub(offset);
                        let len = inner
                            .get("length")
                            .and_then(Value::as_u64)
                            .map_or(avail, |n| (n as usize).min(avail));
                        let ready = json!({
                            "size": payload.len(),
                            "offset": offset,
                            "length": len,
                            "mtime": { "secs": 1_700_000_000i64, "nanos": 0u32 },
                            "file_id": 99u64,
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
                        park: None,
                        guard: None,
                    })
                })
            }
        }

        struct RelayedContentBackend {
            link: Link,
            live: Arc<AtomicBool>,
        }

        impl Backend for RelayedContentBackend {
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
                // Phone is paired ONLY with `tower`; `studio` routes via the link with fleet holder.
                let cfg = self.link.config();
                let (holder, learned_identity) = if let Some(entry) = cfg.node(machine) {
                    let learned_identity = if entry.origin == legion_link::NodeOrigin::Learned {
                        let paired_introducer = entry.learned_from.as_deref().and_then(|intro| {
                            cfg.node(intro)
                                .filter(|n| n.origin == legion_link::NodeOrigin::Paired)
                                .cloned()
                        });
                        let from_paired_introducer = paired_introducer.is_some();
                        let introducer_holder = paired_introducer
                            .as_ref()
                            .map(|intro| format!("pairing:{}:1", intro.node));
                        let introducer_host_keys = paired_introducer
                            .as_ref()
                            .and_then(|intro| intro.target.as_ref())
                            .map(|t| {
                                let mut set = std::collections::BTreeSet::new();
                                for line in &t.host_keys {
                                    let mut parts = line.split_whitespace();
                                    if let (Some(k), Some(d)) = (parts.next(), parts.next()) {
                                        set.insert(format!("{k} {d}"));
                                    }
                                }
                                set.into_iter().collect::<Vec<_>>()
                            })
                            .unwrap_or_default();
                        let learned_from = paired_introducer.map(|intro| intro.node);
                        let host_keys = entry
                            .target
                            .as_ref()
                            .map(|t| {
                                let mut set = std::collections::BTreeSet::new();
                                for line in &t.host_keys {
                                    let mut parts = line.split_whitespace();
                                    if let (Some(k), Some(d)) = (parts.next(), parts.next()) {
                                        set.insert(format!("{k} {d}"));
                                    }
                                }
                                set.into_iter().collect::<Vec<_>>()
                            })
                            .unwrap_or_default();
                        (!host_keys.is_empty()).then_some(crate::backend::LearnedDirectIdentity {
                            learned_fleet_epoch: 1,
                            from_paired_introducer,
                            learned_from,
                            introducer_holder,
                            introducer_host_keys,
                            host_keys,
                        })
                    } else {
                        None
                    };
                    (format!("pairing:{machine}:1"), learned_identity)
                } else {
                    ("fleet:1".to_owned(), None)
                };
                Ok(ContentTarget {
                    link: self.link.clone(),
                    link_node: machine.to_owned(),
                    machine: machine.to_owned(),
                    principal: "device:pixel".to_owned(),
                    holder,
                    learned_identity,
                })
            }
            fn is_pairing_holder_live(&self, _holder: &str) -> bool {
                self.live.load(Ordering::SeqCst)
            }
            fn node_route_identity(&self, node: &str) -> Option<NodeRouteIdentity> {
                let cfg = self.link.config();
                let entry = cfg.node(node)?;
                let host_keys = entry
                    .target
                    .as_ref()
                    .map(|t| {
                        let mut set = std::collections::BTreeSet::new();
                        for line in &t.host_keys {
                            let mut parts = line.split_whitespace();
                            if let (Some(k), Some(d)) = (parts.next(), parts.next()) {
                                set.insert(format!("{k} {d}"));
                            }
                        }
                        set.into_iter().collect::<Vec<_>>()
                    })
                    .unwrap_or_default();
                Some(NodeRouteIdentity {
                    node: entry.node.clone(),
                    is_paired: entry.origin == legion_link::NodeOrigin::Paired,
                    holder: format!("pairing:{}:1", entry.node),
                    host_keys,
                })
            }
        }

        let session_opener: SessionOpener = {
            let studio_files = Arc::clone(&studio_files);
            let relay_hits = Arc::clone(&relay_hits);
            let direct_hits = Arc::clone(&direct_hits);
            let studio_key = studio_key.clone();
            Arc::new(move |entry, _, cancel| {
                let studio_files = Arc::clone(&studio_files);
                let relay_hits = Arc::clone(&relay_hits);
                let direct_hits = Arc::clone(&direct_hits);
                let studio_key = studio_key.clone();
                Box::pin(async move {
                    let node_name = entry.node.clone();
                    let (client_in, mut server_out) = tokio::io::duplex(16 * 1024);
                    let (server_in, client_out) = tokio::io::duplex(16 * 1024);
                    let node_for_rpc = node_name.clone();
                    tokio::spawn(async move {
                        let mut reader = BufReader::new(server_in);
                        let mut line = String::new();
                        while reader.read_line(&mut line).await.unwrap_or(0) > 0 {
                            let req: legion_link::proto::Request =
                                serde_json::from_str(&line).unwrap();
                            let reply = if req.method == legion_link::proto::method::HELLO {
                                ProtoResp::ok(
                                    req.id,
                                    serde_json::to_value(HelloResult {
                                        node: node_for_rpc.clone(),
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
                                )
                            } else {
                                ProtoResp::ok(
                                    req.id,
                                    json!({
                                        "results": {
                                            "studio": {
                                                "ok": {
                                                    "node": "studio",
                                                    "hosts": ["studio.example.net"],
                                                    "port": 22,
                                                    "user": "me",
                                                    "host_keys": [studio_key],
                                                }
                                            }
                                        }
                                    }),
                                )
                            };
                            let _ = server_out
                                .write_all(
                                    format!("{}\n", serde_json::to_string(&reply).unwrap())
                                        .as_bytes(),
                                )
                                .await;
                            line.clear();
                        }
                    });
                    let stream_opener = Arc::new(FleetRelayStreamOpener {
                        node: node_name.clone(),
                        files: studio_files,
                        relay_hits,
                        direct_hits,
                    });
                    Session::open_transport(
                        client_in,
                        client_out,
                        Box::new(FakeTransport {
                            opener: stream_opener,
                        }),
                        &node_name,
                        None,
                        Duration::from_secs(5),
                        cancel,
                    )
                    .await
                    .map_err(|err| err.into_error())
                })
            })
        };

        let cfg = LinkConfig {
            device_key: Some("dummy".to_owned()),
            nodes: vec![NodeLink::ssh(
                "tower",
                SshTarget {
                    hosts: vec!["tower.example.net".to_owned()],
                    port: 22,
                    user: "me".to_owned(),
                    host_keys: vec![tower_key],
                },
                Some("device:pixel".to_owned()),
            )],
            ..LinkConfig::default()
        };
        let link = Link::with_opener(cfg, LinkOptions::default(), session_opener);
        let live = Arc::new(AtomicBool::new(true));
        let backend = Arc::new(RelayedContentBackend {
            link: link.clone(),
            live: Arc::clone(&live),
        });

        // 1) Universal loopback mount (desktop, iOS, and Android): picture GET + ranged media
        // reads on `studio` via relay `tower`, with both desktop and Android origins allowed.
        let lb = Arc::new(AppContentServer::with_config(
            backend.clone(),
            ContentMountMode::Loopback,
            TicketConfig::default(),
            Duration::from_secs(30),
            vec![
                "tauri://localhost".to_owned(),
                "http://tauri.localhost".to_owned(),
            ],
        ));
        let pic = lb
            .mint_url("studio", "/home/me/photo.png", false)
            .await
            .unwrap();
        let lb_port = lb.loopback_port().unwrap();
        let pic_path = pic
            .url
            .strip_prefix(&format!("http://127.0.0.1:{lb_port}"))
            .unwrap();
        let (status, head, body) = raw_http(
            lb_port,
            &format!(
                "GET {pic_path} HTTP/1.1\r\nHost: 127.0.0.1:{lb_port}\r\nOrigin: http://tauri.localhost\r\nConnection: close\r\n\r\n"
            ),
        )
        .await;
        assert_eq!(status, 200, "{head}");
        assert_eq!(body, png_bytes);

        let vid = lb
            .mint_url("studio", "/home/me/video.mp4", false)
            .await
            .unwrap();
        let vid_path = vid
            .url
            .strip_prefix(&format!("http://127.0.0.1:{lb_port}"))
            .unwrap();
        let (rstatus, rhead, rbody) = raw_http(
            lb_port,
            &format!(
                "GET {vid_path} HTTP/1.1\r\nHost: 127.0.0.1:{lb_port}\r\nOrigin: http://tauri.localhost\r\nRange: bytes=4096-12287\r\nConnection: close\r\n\r\n"
            ),
        )
        .await;
        assert_eq!(rstatus, 206, "{rhead}");
        assert_eq!(rbody, &media_bytes[4096..=12287]);

        let (rstatus2, rhead2, rbody2) = raw_http(
            lb_port,
            &format!(
                "GET {vid_path} HTTP/1.1\r\nHost: 127.0.0.1:{lb_port}\r\nOrigin: http://tauri.localhost\r\nRange: bytes=16384-32767\r\nConnection: close\r\n\r\n"
            ),
        )
        .await;
        assert_eq!(rstatus2, 206, "{rhead2}");
        assert_eq!(rbody2, &media_bytes[16384..=32767]);
        assert!(relay_hits.load(Ordering::SeqCst) >= 3);
        assert_eq!(direct_hits.load(Ordering::SeqCst), 0);

        // 2) Learn a direct route to `studio` -> subsequent reads on the active `fleet:` ticket
        // switch seamlessly to direct without reminting!
        let learned = link.learn().await.unwrap();
        assert_eq!(learned.added, vec!["studio".to_owned()]);
        let relay_before = relay_hits.load(Ordering::SeqCst);
        let (dstatus, dhead, dbody) = raw_http(
            lb_port,
            &format!(
                "GET {vid_path} HTTP/1.1\r\nHost: 127.0.0.1:{lb_port}\r\nOrigin: http://tauri.localhost\r\nRange: bytes=0-1023\r\nConnection: close\r\n\r\n"
            ),
        )
        .await;
        assert_eq!(dstatus, 206, "{dhead}");
        assert_eq!(dbody, &media_bytes[0..=1023]);
        assert_eq!(relay_hits.load(Ordering::SeqCst), relay_before);
        assert!(direct_hits.load(Ordering::SeqCst) >= 1);

        // 3) Unpairing revokes the holder -> relayed ticket is refused with 404 (TicketRevoked).
        live.store(false, Ordering::SeqCst);
        let (rev_status, rev_head, _) = raw_http(
            lb_port,
            &format!(
                "GET {pic_path} HTTP/1.1\r\nHost: 127.0.0.1:{lb_port}\r\nOrigin: http://tauri.localhost\r\nConnection: close\r\n\r\n"
            ),
        )
        .await;
        assert_eq!(rev_status, 404, "{rev_head}");

        lb.shutdown();
        link.close().await;
    }

    #[tokio::test]
    async fn evicted_parked_stream_aborts_loopback_cleanly_and_fresh_ticket_recovers() {
        // 2 GiB virtual pattern file: strictly larger than the RFC 7323 maximum TCP receive window
        // (~1 GiB) with zero RAM allocated, so paused streams always hit backpressure and park
        // regardless of OS loopback socket buffer sizing or autotuning.
        let movie_len: u64 = 2 * 1024 * 1024 * 1024;
        let movie = MockFile::Pattern {
            len: movie_len,
            mul: 29,
            add: 13,
        };
        let mut files = HashMap::new();
        files.insert("/home/me/movie.bin".to_owned(), movie.clone());

        let backend = Arc::new(MockContentBackend::with_mock_files(
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
        let stats = Arc::clone(&backend.stats);
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

        // Start stream 1, read response headers + a small prefix, then wait until ContentStream's
        // idle park watchdog closes the remote channel and parks stream 1.
        let mut s1 = connect_bounded_client(port).await;
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
                .contains(&format!("content-length: {movie_len}")),
            "{head1}"
        );
        wait_for_parked_and_idle(&stats, 1, Duration::from_secs(10)).await;

        // Start stream 2 and wait until it parks, which evicts stream 1 from the cap-1
        // ParkedStreamPool before stream 2's park guard drops.
        let mut s2 = connect_bounded_client(port).await;
        s2.write_all(
            format!("GET {path2} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n")
                .as_bytes(),
        )
        .await
        .unwrap();
        let (status2, head2, _body2) = read_http_head_and_prefix(&mut s2, 4096).await;
        assert_eq!(status2, 200, "{head2}");
        wait_for_parked_and_idle(&stats, 2, Duration::from_secs(10)).await;

        // Snapshot remote source counters right after eviction (both remote tasks are already
        // gone). Resuming stream 1 must abort cleanly without reading from the source again.
        let opens_at_eviction = stats.stream_opens.load(Ordering::Acquire);
        let windows_at_eviction = stats.read_windows.load(Ordering::Acquire);
        let bytes_at_eviction = stats.bytes_produced.load(Ordering::Acquire);

        let mut chunk = vec![0u8; 64 * 1024];
        loop {
            match s1.read(&mut chunk).await {
                Ok(0) | Err(_) => break,
                Ok(n) => body1.extend_from_slice(&chunk[..n]),
            }
        }
        let body1_len = body1.len() as u64;
        assert!(
            body1_len < movie_len,
            "evicted parked stream must abort before full Content-Length ({body1_len} vs {movie_len})",
        );
        movie.assert_matches(0, &body1);
        assert_eq!(
            stats.stream_opens.load(Ordering::Acquire),
            opens_at_eviction,
            "evicted parked stream must never reopen the remote source"
        );
        assert_eq!(
            stats.read_windows.load(Ordering::Acquire),
            windows_at_eviction,
            "evicted parked stream must never request another window from the source"
        );
        assert_eq!(
            stats.bytes_produced.load(Ordering::Acquire),
            bytes_at_eviction,
            "evicted parked stream must never read another byte from the source"
        );
        assert!(
            server.content_status_for_url(&t1.url).stream_failed,
            "evicted loopback stream must mark stream_failed for viewer recovery diagnostics"
        );

        // Minting a fresh ticket (what FilesViewer does on media error) recovers exact bytes
        // immediately after the cut point as well as the end of the file.
        let t3 = server
            .mint_url("tower", "/home/me/movie.bin", false)
            .await
            .unwrap();
        let path3 = t3
            .url
            .strip_prefix(&format!("http://127.0.0.1:{port}"))
            .unwrap();
        let continue_len = 256 * 1024u64;
        let continue_end = (body1_len + continue_len - 1).min(movie_len - 1);
        let (status3, head3, rest) = raw_http(
            port,
            &format!(
                "GET {path3} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nRange: bytes={body1_len}-{continue_end}\r\nConnection: close\r\n\r\n",
            ),
        )
        .await;
        assert_eq!(status3, 206, "{head3}");
        assert_eq!(rest.len() as u64, continue_end - body1_len + 1);
        movie.assert_matches(body1_len, &rest);

        let tail_start = movie_len - 4096;
        let tail_end = movie_len - 1;
        let (tail_status, tail_head, tail_bytes) = raw_http(
            port,
            &format!(
                "GET {path3} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nRange: bytes={tail_start}-{tail_end}\r\nConnection: close\r\n\r\n",
            ),
        )
        .await;
        assert_eq!(tail_status, 206, "{tail_head}");
        assert_eq!(tail_bytes.len(), 4096);
        movie.assert_matches(tail_start, &tail_bytes);

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
        // 2 GiB virtual pattern file: strictly larger than any OS loopback TCP buffer without
        // allocating RAM so paused streams always trigger idle parking deterministically.
        let movie_len: u64 = 2 * 1024 * 1024 * 1024;
        let movie = MockFile::Pattern {
            len: movie_len,
            mul: 31,
            add: 7,
        };
        let mut files = HashMap::new();
        files.insert(
            "/home/me/photo.png".to_owned(),
            MockFile::Bytes(image_bytes.clone()),
        );
        files.insert("/home/me/movie.bin".to_owned(), movie.clone());

        let backend = Arc::new(MockContentBackend::with_mock_files(
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
        let stats = Arc::clone(&backend.stats);
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

        let mut s1 = connect_bounded_client(port_before).await;
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
        wait_for_parked_and_idle(&stats, 1, Duration::from_secs(10)).await;

        let mut s2 = connect_bounded_client(port_before).await;
        s2.write_all(
            format!(
                "GET {path2} HTTP/1.1\r\nHost: 127.0.0.1:{port_before}\r\nConnection: close\r\n\r\n"
            )
            .as_bytes(),
        )
        .await
        .unwrap();
        let (st2, head2, mut body2) = read_http_head_and_prefix(&mut s2, 4096).await;
        assert_eq!(st2, 200, "{head2}");
        wait_for_parked_and_idle(&stats, 2, Duration::from_secs(10)).await;

        let opens_at_eviction = stats.stream_opens.load(Ordering::Acquire);
        let windows_at_eviction = stats.read_windows.load(Ordering::Acquire);
        let bytes_at_eviction = stats.bytes_produced.load(Ordering::Acquire);

        // 2) Simulate OS background suspension reclaiming the listening socket while the stale
        // ListenerHandle is still recorded in `server.listener`. Both streams on the old listener
        // must drop immediately without reading from the source again.
        {
            let guard = server.listener.lock().unwrap();
            if let Some(h) = guard.as_ref() {
                h.shutdown.cancel();
            }
        }
        wait_for_active_streams(&server, 0, Duration::from_secs(2)).await;

        // Pre-suspend evicted stream 1 and suspended stream 2 both abort cleanly strictly before
        // Content-Length and never touch the source again.
        let mut chunk = vec![0u8; 64 * 1024];
        loop {
            match s1.read(&mut chunk).await {
                Ok(0) | Err(_) => break,
                Ok(n) => body1.extend_from_slice(&chunk[..n]),
            }
        }
        let body1_len = body1.len() as u64;
        assert!(body1_len < movie_len);
        movie.assert_matches(0, &body1);

        loop {
            match s2.read(&mut chunk).await {
                Ok(0) | Err(_) => break,
                Ok(n) => body2.extend_from_slice(&chunk[..n]),
            }
        }
        assert!((body2.len() as u64) < movie_len);
        movie.assert_matches(0, &body2);
        assert_eq!(
            stats.stream_opens.load(Ordering::Acquire),
            opens_at_eviction,
            "evicted/suspended streams must never reopen the remote source"
        );
        assert_eq!(
            stats.read_windows.load(Ordering::Acquire),
            windows_at_eviction,
            "evicted/suspended streams must never request another window from the source"
        );
        assert_eq!(
            stats.bytes_produced.load(Ordering::Acquire),
            bytes_at_eviction,
            "evicted/suspended streams must never read another byte from the source"
        );
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

        let continue_len = 256 * 1024u64;
        let continue_end = (body1_len + continue_len - 1).min(movie_len - 1);
        let (vid_status, vid_head, vid_tail) = raw_http(
            recovered_port,
            &format!(
                "GET {vid_path} HTTP/1.1\r\nHost: 127.0.0.1:{recovered_port}\r\nRange: bytes={body1_len}-{continue_end}\r\nConnection: close\r\n\r\n",
            ),
        )
        .await;
        assert_eq!(vid_status, 206, "{vid_head}");
        assert_eq!(vid_tail.len() as u64, continue_end - body1_len + 1);
        movie.assert_matches(body1_len, &vid_tail);

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

    #[tokio::test]
    async fn shutdown_immediately_aborts_active_loopback_stream_and_stops_source_reads() {
        let movie_len: u64 = 2 * 1024 * 1024 * 1024;
        let movie = MockFile::Pattern {
            len: movie_len,
            mul: 37,
            add: 19,
        };
        let mut files = HashMap::new();
        files.insert("/home/me/movie.bin".to_owned(), movie.clone());

        let backend = Arc::new(MockContentBackend::with_mock_files(
            files,
            LinkOptions::default(),
            Some(legion_link::content::WindowPolicy {
                initial_bytes: 256 * 1024,
                max_bytes: 256 * 1024,
            }),
        ));
        let stats = Arc::clone(&backend.stats);
        let server = Arc::new(AppContentServer::with_config(
            backend,
            ContentMountMode::Loopback,
            TicketConfig::default(),
            Duration::from_secs(30),
            vec!["tauri://localhost".to_owned()],
        ));

        let minted = server
            .mint_url("tower", "/home/me/movie.bin", false)
            .await
            .unwrap();
        let port = server.loopback_port().unwrap();
        let path = minted
            .url
            .strip_prefix(&format!("http://127.0.0.1:{port}"))
            .unwrap();

        // Open an active (unparked) stream and read only 4 KiB so the socket is backpressured
        // well before the 2-second idle park timer.
        let mut sock = connect_bounded_client(port).await;
        sock.write_all(
            format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n")
                .as_bytes(),
        )
        .await
        .unwrap();
        let (status, head, mut body) = read_http_head_and_prefix(&mut sock, 4096).await;
        assert_eq!(status, 200, "{head}");
        assert_eq!(server.active_streams(), 1);

        // Shut down the server while the stream is still active and backpressured: the connection
        // task must abort immediately, releasing active_streams and stopping the remote source
        // task BEFORE the client reads another byte or closes its socket.
        server.shutdown();
        wait_for_active_streams(&server, 0, Duration::from_secs(2)).await;
        wait_for_zero_active_tasks(&stats, Duration::from_secs(2)).await;

        let opens_after_shutdown = stats.stream_opens.load(Ordering::Acquire);
        let windows_after_shutdown = stats.read_windows.load(Ordering::Acquire);
        let bytes_after_shutdown = stats.bytes_produced.load(Ordering::Acquire);

        let mut chunk = vec![0u8; 64 * 1024];
        loop {
            match sock.read(&mut chunk).await {
                Ok(0) | Err(_) => break,
                Ok(n) => body.extend_from_slice(&chunk[..n]),
            }
        }
        assert!((body.len() as u64) < movie_len);
        movie.assert_matches(0, &body);
        assert_eq!(
            stats.stream_opens.load(Ordering::Acquire),
            opens_after_shutdown
        );
        assert_eq!(
            stats.read_windows.load(Ordering::Acquire),
            windows_after_shutdown
        );
        assert_eq!(
            stats.bytes_produced.load(Ordering::Acquire),
            bytes_after_shutdown
        );
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

    #[tokio::test]
    async fn invalid_ticket_requests_cannot_evict_valid_ticket_status() {
        let flac_bytes: Vec<u8> = (0..(64 * 1024)).map(|i| (i % 251) as u8).collect();
        let mut files = HashMap::new();
        files.insert("/home/me/track.flac".to_owned(), flac_bytes);

        let backend = Arc::new(MockContentBackend::new(files));
        let server = Arc::new(AppContentServer::with_config(
            backend,
            ContentMountMode::Loopback,
            TicketConfig::default(),
            Duration::from_secs(30),
            vec!["http://tauri.localhost".to_owned()],
        ));

        let valid = server
            .mint_url("tower", "/home/me/track.flac", false)
            .await
            .unwrap();
        let port = server.loopback_port().unwrap();
        let valid_path = valid
            .url
            .strip_prefix(&format!("http://127.0.0.1:{port}"))
            .unwrap();

        let (st, head, _) = raw_http(
            port,
            &format!(
                "GET {valid_path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nRange: bytes=0-1023\r\nConnection: close\r\n\r\n"
            ),
        )
        .await;
        assert_eq!(st, 206, "{head}");

        // Flood more than MAX_REQUEST_LOG (64) junk requests — both invalid 43-char ticket IDs
        // (404) and guard-rejected Origin/Host requests (403/421). None may allocate status slots
        // or evict the real URL's diagnosis.
        let mut sample_junk_url = String::new();
        for i in 0..80usize {
            let fake_id = format!("junk{i:039}");
            assert_eq!(fake_id.len(), 43);
            let fake_path = format!("/v1/content/{fake_id}/track.flac");
            if i == 0 {
                sample_junk_url = format!("http://127.0.0.1:{port}{fake_path}");
            }
            let req = if i % 3 == 0 {
                format!(
                    "GET {fake_path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nOrigin: https://evil.example\r\nConnection: close\r\n\r\n"
                )
            } else if i % 3 == 1 {
                format!(
                    "GET {fake_path} HTTP/1.1\r\nHost: evil.example:{port}\r\nConnection: close\r\n\r\n"
                )
            } else {
                format!(
                    "GET {fake_path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n"
                )
            };
            let (junk_st, _, _) = raw_http(port, &req).await;
            assert!(matches!(junk_st, 403 | 404 | 421), "got {junk_st}");
        }

        let preserved = server.content_status_for_url(&valid.url);
        assert_eq!(preserved.request_count, 1);
        assert_eq!(preserved.status, Some(206));
        assert_eq!(preserved.content_type.as_deref(), Some("audio/flac"));
        assert!(!preserved.stream_failed);
        assert!(preserved.ticket_live);

        assert_eq!(
            server.content_status_for_url(&sample_junk_url),
            ContentUrlStatus::default(),
            "unknown tickets must never populate content_status"
        );

        server.shutdown();
    }

    #[tokio::test]
    async fn loopback_serves_viewer_while_32_partial_header_sockets_are_held() {
        let payload: Vec<u8> = (0..(64 * 1024)).map(|i| (i % 251) as u8).collect();
        let mut files = HashMap::new();
        files.insert("/home/me/track.flac".to_owned(), payload.clone());

        let backend = Arc::new(MockContentBackend::new(files));
        let server = Arc::new(AppContentServer::with_config(
            backend,
            ContentMountMode::Loopback,
            TicketConfig::default(),
            Duration::from_secs(30),
            vec!["http://tauri.localhost".to_owned()],
        ));

        let valid = server
            .mint_url("tower", "/home/me/track.flac", false)
            .await
            .unwrap();
        let port = server.loopback_port().unwrap();
        let valid_path = valid
            .url
            .strip_prefix(&format!("http://127.0.0.1:{port}"))
            .unwrap();

        // 1) Guard rejections (403, 421) and 404 responses emit `Connection: close` and actually
        // close the TCP connection even when the client asks for `Connection: keep-alive`.
        for (expected_status, req) in [
            (
                404u16,
                format!(
                    "GET /v1/content/nonexistent/track.flac HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: keep-alive\r\n\r\n"
                ),
            ),
            (
                404u16,
                format!(
                    "GET /other/path HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: keep-alive\r\n\r\n"
                ),
            ),
            (
                403u16,
                format!(
                    "GET {valid_path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nOrigin: https://evil.example\r\nConnection: keep-alive\r\n\r\n"
                ),
            ),
            (
                421u16,
                format!(
                    "GET {valid_path} HTTP/1.1\r\nHost: evil.example:{port}\r\nConnection: keep-alive\r\n\r\n"
                ),
            ),
        ] {
            let (st, head, _) = tokio::time::timeout(Duration::from_secs(2), raw_http(port, &req))
                .await
                .expect("server must close 404/guard-rejected keep-alive connections immediately");
            assert_eq!(st, expected_status, "{head}");
            assert!(
                head.to_ascii_lowercase().contains("connection: close"),
                "expected Connection: close on {expected_status}, got:\n{head}"
            );
        }

        // 2) Hold MAX_LOOPBACK_CONNECTIONS (32) partial-header sockets simultaneously open, then
        // issue a real viewer GET. The listener must evict the oldest unauthenticated socket and
        // serve the viewer's valid request immediately.
        let mut holders = Vec::with_capacity(MAX_LOOPBACK_CONNECTIONS);
        for _ in 0..MAX_LOOPBACK_CONNECTIONS {
            let mut sock = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
            sock.write_all(b"GET /v1/content/").await.unwrap();
            holders.push(sock);
        }
        tokio::time::sleep(Duration::from_millis(60)).await;

        let (viewer_status, viewer_head, viewer_body) = tokio::time::timeout(
            Duration::from_secs(2),
            raw_http(
                port,
                &format!(
                    "GET {valid_path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n"
                ),
            ),
        )
        .await
        .expect("viewer GET must succeed immediately even while 32 partial-header sockets are held");
        assert_eq!(viewer_status, 200, "{viewer_head}");
        assert_eq!(viewer_body, payload);

        // The oldest unauthenticated socket (holders[0]) was evicted and must see EOF immediately.
        let mut evicted_buf = [0u8; 16];
        let evicted_read = tokio::time::timeout(
            Duration::from_millis(300),
            holders[0].read(&mut evicted_buf),
        )
        .await;
        assert!(
            matches!(evicted_read, Ok(Ok(0)) | Ok(Err(_))),
            "oldest unauthenticated socket should be evicted, got {evicted_read:?}"
        );
        drop(holders);

        // 3) Under default limits, an unauthenticated partial-header socket closes after the short
        // ~2s unauthenticated header timeout (well below the 5s authenticated timeout).
        let started = Instant::now();
        let mut idle_unauth = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        idle_unauth.write_all(b"GET /v1/content/").await.unwrap();
        let mut sink = Vec::new();
        let closed = tokio::time::timeout(
            Duration::from_millis(3500),
            idle_unauth.read_to_end(&mut sink),
        )
        .await;
        assert!(
            closed.is_ok(),
            "unauthenticated partial-header socket must time out within ~2s (elapsed={:?})",
            started.elapsed()
        );
        assert!(
            started.elapsed() >= Duration::from_millis(1500),
            "unauthenticated timeout fired unexpectedly early: {:?}",
            started.elapsed()
        );

        server.shutdown();
    }

    #[tokio::test]
    async fn revoked_content_url_returns_404_on_replay() {
        let payload = b"sensitive-audio-track-bytes".to_vec();
        let mut files = HashMap::new();
        files.insert("/home/me/voice-note.flac".to_owned(), payload.clone());

        let backend = Arc::new(MockContentBackend::new(files));
        let server = Arc::new(AppContentServer::with_config(
            backend,
            ContentMountMode::Loopback,
            TicketConfig::default(),
            Duration::from_secs(30),
            vec!["http://tauri.localhost".to_owned()],
        ));

        let minted = server
            .mint_url("tower", "/home/me/voice-note.flac", false)
            .await
            .unwrap();
        let port = server.loopback_port().unwrap();
        let path = minted
            .url
            .strip_prefix(&format!("http://127.0.0.1:{port}"))
            .unwrap();

        let (status_before, head_before, body_before) = raw_http(
            port,
            &format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n"),
        )
        .await;
        assert_eq!(status_before, 200, "{head_before}");
        assert_eq!(body_before, payload);
        assert!(server.content_status_for_url(&minted.url).ticket_live);

        // Revoking the URL immediately invalidates the ticket; replaying it returns 404.
        assert!(server.revoke_url(&minted.url));
        assert!(!server.revoke_url(&minted.url));
        assert!(!server.revoke_url("not-a-content-url"));
        assert!(!server.content_status_for_url(&minted.url).ticket_live);

        let (status_after, head_after, _) = raw_http(
            port,
            &format!(
                "GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: keep-alive\r\n\r\n"
            ),
        )
        .await;
        assert_eq!(status_after, 404, "{head_after}");
        assert!(
            head_after
                .to_ascii_lowercase()
                .contains("connection: close"),
            "replayed revoked URL should emit Connection: close:\n{head_after}"
        );

        server.shutdown();
    }

    #[tokio::test]
    async fn relayed_mid_stream_failure_sets_stream_failed_on_loopback() {
        let flac_bytes: Vec<u8> = (0..(128 * 1024usize)).map(|i| (i % 251) as u8).collect();
        let flac_bytes_arc = Arc::new(flac_bytes.clone());
        let tower_key = legion_link::DeviceKey::from_seed([61u8; 32])
            .public_key_line("tower")
            .unwrap();
        let relay_hits = Arc::new(AtomicUsize::new(0));

        struct TruncatingRelayOpener {
            payload: Arc<Vec<u8>>,
            relay_hits: Arc<AtomicUsize>,
        }

        impl StreamOpener for TruncatingRelayOpener {
            fn open_stream(
                &self,
                _cancel: CancellationToken,
            ) -> Pin<Box<dyn Future<Output = Result<RawStream, StreamOpenError>> + Send + '_>>
            {
                let payload = Arc::clone(&self.payload);
                let relay_hits = Arc::clone(&self.relay_hits);
                Box::pin(async move {
                    let (client_in, mut server_out) = tokio::io::duplex(64 * 1024);
                    let (server_in, client_out) = tokio::io::duplex(64 * 1024);
                    let cancel = CancellationToken::new();
                    tokio::spawn(async move {
                        let _ = server_out
                            .write_all(
                                format!(
                                    "{}\n",
                                    json!({
                                        "protocol": 1,
                                        "capabilities": [legion_link::CAP_STREAM_RELAY]
                                    })
                                )
                                .as_bytes(),
                            )
                            .await;
                        let mut reader = BufReader::new(server_in);
                        let mut line = String::new();
                        if reader.read_line(&mut line).await.unwrap_or(0) == 0 {
                            return;
                        }
                        let req: Value = serde_json::from_str(&line).unwrap();
                        assert_eq!(req["op"], json!("relay"));
                        assert_eq!(req["on"], json!("studio"));
                        relay_hits.fetch_add(1, Ordering::SeqCst);
                        let inner = &req["request"];
                        let offset = inner["offset"].as_u64().unwrap_or(0) as usize;
                        let avail = payload.len().saturating_sub(offset);
                        let len = inner
                            .get("length")
                            .and_then(Value::as_u64)
                            .map_or(avail, |n| (n as usize).min(avail));
                        let ready = json!({
                            "size": payload.len(),
                            "offset": offset,
                            "length": len,
                            "mtime": { "secs": 1_700_000_000i64, "nanos": 0u32 },
                            "file_id": 77u64,
                        });
                        let _ = server_out.write_all(format!("{ready}\n").as_bytes()).await;
                        if len == 0 {
                            let _ = server_out.shutdown().await;
                            return;
                        }
                        // Deliver only the first 70 KiB (past ContentBody's 64 KiB first frame so
                        // HTTP 206 headers flush) then drop the relayed stream mid-transfer so
                        // loopback records `stream_failed: true`.
                        let cut = len.min(70 * 1024);
                        let _ = server_out.write_all(&payload[offset..offset + cut]).await;
                        let _ = server_out.shutdown().await;
                    });
                    Ok(RawStream {
                        reader: Box::new(client_in),
                        writer: Box::new(client_out),
                        stderr: None,
                        cancel,
                        park: None,
                        guard: None,
                    })
                })
            }
        }

        struct RelayedOnlyBackend {
            link: Link,
        }

        impl Backend for RelayedOnlyBackend {
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
                Ok(ContentTarget {
                    link: self.link.clone(),
                    link_node: machine.to_owned(),
                    machine: machine.to_owned(),
                    principal: "device:pixel".to_owned(),
                    holder: "fleet:1".to_owned(),
                    learned_identity: None,
                })
            }
            fn is_pairing_holder_live(&self, _holder: &str) -> bool {
                true
            }
            fn node_route_identity(&self, node: &str) -> Option<NodeRouteIdentity> {
                let entry = self.link.config().node(node).cloned()?;
                Some(NodeRouteIdentity {
                    node: entry.node,
                    is_paired: entry.origin == legion_link::NodeOrigin::Paired,
                    holder: format!("pairing:{node}:1"),
                    host_keys: entry.target.map(|t| t.host_keys).unwrap_or_default(),
                })
            }
        }

        let session_opener: SessionOpener = {
            let payload = Arc::clone(&flac_bytes_arc);
            let relay_hits = Arc::clone(&relay_hits);
            Arc::new(move |entry, _, cancel| {
                let payload = Arc::clone(&payload);
                let relay_hits = Arc::clone(&relay_hits);
                Box::pin(async move {
                    let node_name = entry.node.clone();
                    let (client_in, mut server_out) = tokio::io::duplex(16 * 1024);
                    let (server_in, client_out) = tokio::io::duplex(16 * 1024);
                    let node_for_rpc = node_name.clone();
                    tokio::spawn(async move {
                        let mut reader = BufReader::new(server_in);
                        let mut line = String::new();
                        while reader.read_line(&mut line).await.unwrap_or(0) > 0 {
                            let req: legion_link::proto::Request =
                                serde_json::from_str(&line).unwrap();
                            let reply = ProtoResp::ok(
                                req.id,
                                serde_json::to_value(HelloResult {
                                    node: node_for_rpc.clone(),
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
                                    format!("{}\n", serde_json::to_string(&reply).unwrap())
                                        .as_bytes(),
                                )
                                .await;
                            line.clear();
                        }
                    });
                    let stream_opener = Arc::new(TruncatingRelayOpener {
                        payload,
                        relay_hits,
                    });
                    Session::open_transport(
                        client_in,
                        client_out,
                        Box::new(FakeTransport {
                            opener: stream_opener,
                        }),
                        &node_name,
                        None,
                        Duration::from_secs(5),
                        cancel,
                    )
                    .await
                    .map_err(|err| err.into_error())
                })
            })
        };

        let link = Link::with_opener(
            LinkConfig {
                device_key: Some("dummy".to_owned()),
                nodes: vec![NodeLink::ssh(
                    "tower",
                    SshTarget {
                        hosts: vec!["tower.example.net".to_owned()],
                        port: 22,
                        user: "me".to_owned(),
                        host_keys: vec![tower_key],
                    },
                    Some("device:pixel".to_owned()),
                )],
                ..LinkConfig::default()
            },
            LinkOptions::default(),
            session_opener,
        );
        let backend: Arc<dyn Backend> = Arc::new(RelayedOnlyBackend { link: link.clone() });
        let server = Arc::new(AppContentServer::with_config(
            backend,
            ContentMountMode::Loopback,
            TicketConfig::default(),
            Duration::from_secs(30),
            vec!["http://tauri.localhost".to_owned()],
        ));

        let minted = server
            .mint_url("studio", "/home/me/track.flac", false)
            .await
            .unwrap();
        let port = server.loopback_port().unwrap();
        let path = minted
            .url
            .strip_prefix(&format!("http://127.0.0.1:{port}"))
            .unwrap();

        let (status, head, body) = raw_http(
            port,
            &format!(
                "GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nOrigin: http://tauri.localhost\r\nRange: bytes=0-\r\nConnection: close\r\n\r\n"
            ),
        )
        .await;
        assert_eq!(status, 206, "{head}");
        assert!(body.len() < flac_bytes.len());
        assert_eq!(relay_hits.load(Ordering::SeqCst), 2);

        let st = server.content_status_for_url(&minted.url);
        assert_eq!(st.request_count, 1);
        assert_eq!(st.status, Some(206));
        assert_eq!(st.content_type.as_deref(), Some("audio/flac"));
        assert!(
            st.stream_failed,
            "mid-stream relayed failure must set stream_failed=true so the viewer shows the transport sentence"
        );
        assert!(st.ticket_live);

        server.shutdown();
        link.close().await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn content_revoke_terminates_active_relayed_stream_on_relay_and_target() {
        use std::os::unix::fs::PermissionsExt;
        use std::path::{Path, PathBuf};
        use std::process::{Child, Command, Stdio};

        fn find_tool(name: &str) -> Option<PathBuf> {
            let path_var = std::env::var_os("PATH")?;
            for dir in std::env::split_paths(&path_var) {
                let candidate = dir.join(name);
                if candidate.is_file() {
                    return Some(candidate);
                }
            }
            None
        }

        fn find_sshd() -> Option<PathBuf> {
            find_tool("sshd").or_else(|| {
                ["/usr/sbin/sshd", "/sbin/sshd", "/usr/local/sbin/sshd"]
                    .into_iter()
                    .map(PathBuf::from)
                    .find(|p| p.is_file())
            })
        }

        fn find_legion_bin() -> Option<PathBuf> {
            if let Some(explicit) = std::env::var_os("LEGION_BIN") {
                let p = PathBuf::from(explicit);
                if p.is_file() {
                    return Some(p);
                }
            }
            let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
            let repo_root = manifest_dir.parent()?.parent()?.parent()?;
            for profile in ["debug", "release"] {
                let candidate = repo_root.join("target").join(profile).join("legion");
                if candidate.is_file() {
                    return Some(candidate);
                }
            }
            None
        }

        fn is_legion_pid_alive(pid: u32) -> bool {
            let proc_dir = PathBuf::from(format!("/proc/{pid}"));
            if proc_dir.is_dir() {
                let Ok(stat) = std::fs::read_to_string(proc_dir.join("stat")) else {
                    return false;
                };
                // Process state is the first token after the final ')'. Ignore zombies/dead.
                let state = stat
                    .rsplit_once(')')
                    .and_then(|(_, rest)| rest.split_whitespace().next())
                    .unwrap_or("");
                if matches!(state, "Z" | "X" | "x") {
                    return false;
                }
                let Ok(cmdline) = std::fs::read(proc_dir.join("cmdline")) else {
                    return false;
                };
                return cmdline.windows(b"legion".len()).any(|w| w == b"legion");
            }
            Command::new("kill")
                .args(["-0", &pid.to_string()])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .is_ok_and(|s| s.success())
        }

        fn count_live_pids(dir: &Path) -> usize {
            let Ok(entries) = std::fs::read_dir(dir) else {
                return 0;
            };
            entries
                .flatten()
                .filter_map(|e| std::fs::read_to_string(e.path()).ok())
                .filter_map(|s| s.trim().parse::<u32>().ok())
                .filter(|&pid| is_legion_pid_alive(pid))
                .count()
        }

        let (Some(sshd_bin), Some(keygen_bin)) = (find_sshd(), find_tool("ssh-keygen")) else {
            eprintln!("skipping 3-node content_revoke test: sshd/ssh-keygen unavailable");
            return;
        };
        let Some(legion_bin) = find_legion_bin() else {
            eprintln!(
                "skipping 3-node content_revoke test: built target/debug/legion not found (build workspace first)"
            );
            return;
        };
        let user_out = Command::new("id")
            .arg("-un")
            .stdin(Stdio::null())
            .output()
            .unwrap();
        assert!(user_out.status.success());
        let user = String::from_utf8(user_out.stdout)
            .unwrap()
            .trim()
            .to_owned();

        struct SshdGuard(Child);
        impl Drop for SshdGuard {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }

        let root = tempfile::tempdir().unwrap();
        let _ = std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700));
        let tower_home = root.path().join("tower-home");
        let studio_home = root.path().join("studio-home");
        let tower_legion = root.path().join("tower-legion");
        let studio_legion = root.path().join("studio-legion");
        let tower_pids = root.path().join("tower-pids");
        let studio_pids = root.path().join("studio-pids");
        let sshd_dir = root.path().join("sshd-tower");
        let auth_keys = tower_home.join(".ssh/authorized_keys");

        for d in [
            tower_home.join(".ssh"),
            studio_home.join("media"),
            tower_legion.join("config"),
            studio_legion.join("config"),
            tower_pids.clone(),
            studio_pids.clone(),
            sshd_dir.clone(),
        ] {
            std::fs::create_dir_all(&d).unwrap();
        }
        for d in [&tower_home, &tower_home.join(".ssh"), &sshd_dir] {
            let _ = std::fs::set_permissions(d, std::fs::Permissions::from_mode(0o700));
        }

        // Write a 16 MiB file on `studio` (larger than loopback TCP + SSH window buffers so a
        // paused loopback client leaves the relayed stream actively open across all 3 nodes).
        let file_size = 16 * 1024 * 1024usize;
        let media_path = studio_home.join("media/concert.flac");
        let media_bytes: Vec<u8> = (0..file_size).map(|i| ((i * 19 + 7) % 251) as u8).collect();
        std::fs::write(&media_path, &media_bytes).unwrap();

        // Wrapper for `studio` (target): records PID of every `legion stream` process before exec.
        let studio_wrapper = root.path().join("studio-exec.sh");
        std::fs::write(
            &studio_wrapper,
            format!(
                "#!/bin/sh\nexport LEGION_HOME='{}'\nexport HOME='{}'\nunset XDG_CONFIG_HOME XDG_STATE_HOME\nif [ \"$1\" = \"stream\" ]; then echo \"$$\" > '{}/stream.'\"$$\"; fi\nexec '{}' \"$@\"\n",
                studio_legion.display(),
                studio_home.display(),
                studio_pids.display(),
                legion_bin.display(),
            ),
        )
        .unwrap();
        let _ = std::fs::set_permissions(&studio_wrapper, std::fs::Permissions::from_mode(0o700));

        // Forced-command dispatch wrapper for `tower` (relay): records PID of `legion stream`.
        let tower_dispatch = root.path().join("tower-dispatch.sh");
        std::fs::write(
            &tower_dispatch,
            format!(
                "#!/bin/sh\nexport LEGION_HOME='{}'\nexport HOME='{}'\nunset XDG_CONFIG_HOME XDG_STATE_HOME\ncase \"$SSH_ORIGINAL_COMMAND\" in\n  *'legion stream'*)\n    echo \"$$\" > '{}/stream.'\"$$\"\n    exec '{}' stream\n    ;;\n  *'legion rpc'*)\n    exec '{}' rpc\n    ;;\n  *)\n    printf 'unexpected command: %s\\n' \"$SSH_ORIGINAL_COMMAND\" >&2\n    exit 2\n    ;;\nesac\n",
                tower_legion.display(),
                tower_home.display(),
                tower_pids.display(),
                legion_bin.display(),
                legion_bin.display(),
            ),
        )
        .unwrap();
        let _ = std::fs::set_permissions(&tower_dispatch, std::fs::Permissions::from_mode(0o700));

        let toml_lit = |p: &Path| format!("'{}'", p.display().to_string().replace('\'', "''"));
        let policy = "default = \"owner\"\n\n[principals.phone]\nrole = \"owner\"\n";
        std::fs::write(tower_legion.join("config/policy.toml"), policy).unwrap();
        std::fs::write(studio_legion.join("config/policy.toml"), policy).unwrap();
        std::fs::write(tower_legion.join("config/node.toml"), "name = \"tower\"\n").unwrap();
        std::fs::write(
            studio_legion.join("config/node.toml"),
            "name = \"studio\"\n",
        )
        .unwrap();
        std::fs::write(
            tower_legion.join("config/fleet.toml"),
            format!(
                "[machines.tower]\nssh = \"127.0.0.1\"\n\n[machines.studio]\ncommand = [{}, \"rpc\"]\n",
                toml_lit(&studio_wrapper),
            ),
        )
        .unwrap();
        std::fs::write(
            studio_legion.join("config/fleet.toml"),
            "[machines.studio]\nssh = \"127.0.0.1\"\n",
        )
        .unwrap();

        // Generate host key for `tower` sshd and a client device key for the app.
        let host_key = sshd_dir.join("host_ed25519");
        let host_key_pub = sshd_dir.join("host_ed25519.pub");
        let kg = Command::new(&keygen_bin)
            .args(["-q", "-t", "ed25519", "-N", "", "-C", "tower-host"])
            .arg("-f")
            .arg(&host_key)
            .stdin(Stdio::null())
            .output()
            .unwrap();
        assert!(kg.status.success());
        let _ = std::fs::set_permissions(&host_key, std::fs::Permissions::from_mode(0o600));
        let host_key_line = std::fs::read_to_string(&host_key_pub)
            .unwrap()
            .trim()
            .to_owned();

        let device_key = legion_link::DeviceKey::from_seed([91u8; 32]);
        let device_pub = device_key.public_key_line("phone").unwrap();
        std::fs::write(
            &auth_keys,
            format!(
                "command=\"{}\",restrict {device_pub}\n",
                tower_dispatch.display()
            ),
        )
        .unwrap();
        let _ = std::fs::set_permissions(&auth_keys, std::fs::Permissions::from_mode(0o600));

        let probe_sock = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let sshd_port = probe_sock.local_addr().unwrap().port();
        drop(probe_sock);

        let supports_no_penalties = Command::new(&sshd_bin)
            .args(["-T", "-f", "/dev/null", "-o", "PerSourcePenalties=no", "-h"])
            .arg(&host_key)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success());
        let sshd_cfg = sshd_dir.join("sshd_config");
        let sshd_log = sshd_dir.join("sshd.log");
        let mut cfg_text = format!(
            "ListenAddress 127.0.0.1\nPort {sshd_port}\nHostKey {}\nPidFile none\n\
             AuthorizedKeysFile {}\nAllowUsers {user}\nStrictModes yes\n\
             PubkeyAuthentication yes\nPasswordAuthentication no\n\
             KbdInteractiveAuthentication no\nChallengeResponseAuthentication no\n\
             PermitRootLogin no\nUsePAM no\nAllowAgentForwarding no\n\
             AllowTcpForwarding no\nX11Forwarding no\nPrintMotd no\n\
             SetEnv HOME={}\n",
            host_key.display(),
            auth_keys.display(),
            tower_home.display(),
        );
        if supports_no_penalties {
            cfg_text.push_str("PerSourcePenalties no\n");
        }
        std::fs::write(&sshd_cfg, &cfg_text).unwrap();

        let log_file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&sshd_log)
            .unwrap();
        let mut sshd_child = Command::new(&sshd_bin)
            .args(["-D", "-e", "-f"])
            .arg(&sshd_cfg)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::from(log_file))
            .spawn()
            .unwrap();
        let wait_deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < wait_deadline {
            assert!(
                sshd_child.try_wait().unwrap().is_none(),
                "sshd exited early:\n{}",
                std::fs::read_to_string(&sshd_log).unwrap_or_default()
            );
            if std::net::TcpStream::connect(("127.0.0.1", sshd_port)).is_ok() {
                break;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        let _sshd = SshdGuard(sshd_child);

        // Device is paired ONLY with `tower`; `studio` is reached strictly through relay `tower`.
        let link = Link::new(LinkConfig {
            device_key: Some(device_key.to_openssh().unwrap()),
            nodes: vec![NodeLink::ssh(
                "tower",
                SshTarget {
                    hosts: vec!["127.0.0.1".to_owned()],
                    port: sshd_port,
                    user,
                    host_keys: vec![host_key_line],
                },
                Some("phone".to_owned()),
            )],
            ..LinkConfig::default()
        });

        struct LiveRelayedBackend {
            link: Link,
        }
        impl Backend for LiveRelayedBackend {
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
                Ok(ContentTarget {
                    link: self.link.clone(),
                    link_node: machine.to_owned(),
                    machine: machine.to_owned(),
                    principal: "phone".to_owned(),
                    holder: "fleet:1".to_owned(),
                    learned_identity: None,
                })
            }
            fn is_pairing_holder_live(&self, _holder: &str) -> bool {
                true
            }
            fn node_route_identity(&self, node: &str) -> Option<NodeRouteIdentity> {
                let entry = self.link.config().node(node).cloned()?;
                Some(NodeRouteIdentity {
                    node: entry.node,
                    is_paired: entry.origin == legion_link::NodeOrigin::Paired,
                    holder: format!("pairing:{node}:1"),
                    host_keys: entry.target.map(|t| t.host_keys).unwrap_or_default(),
                })
            }
        }

        let server = Arc::new(AppContentServer::with_config(
            Arc::new(LiveRelayedBackend { link: link.clone() }),
            ContentMountMode::Loopback,
            TicketConfig::default(),
            Duration::from_secs(30),
            vec!["http://tauri.localhost".to_owned()],
        ));

        let minted = server
            .mint_url("studio", &media_path.to_string_lossy(), false)
            .await
            .unwrap();
        let lb_port = server.loopback_port().unwrap();
        let req_path = minted
            .url
            .strip_prefix(&format!("http://127.0.0.1:{lb_port}"))
            .unwrap();

        // Open a streaming GET and read headers + 64 KiB, then hold the TCP socket open so the
        // relayed stream stays active across device, relay `tower`, and target `studio`.
        let mut sock = TcpStream::connect(("127.0.0.1", lb_port)).await.unwrap();
        sock.write_all(
            format!(
                "GET {req_path} HTTP/1.1\r\nHost: 127.0.0.1:{lb_port}\r\nOrigin: http://tauri.localhost\r\nConnection: close\r\n\r\n"
            )
            .as_bytes(),
        )
        .await
        .unwrap();
        let (status, head, mut received) = read_http_head_and_prefix(&mut sock, 64 * 1024).await;
        assert_eq!(status, 200, "{head}");
        assert_eq!(received, &media_bytes[..received.len()]);

        // Confirm all three nodes have the active relayed stream running before revoke.
        assert_eq!(server.active_streams.load(Ordering::Acquire), 1);
        assert!(
            count_live_pids(&tower_pids) >= 1,
            "expected live relay `legion stream` on tower"
        );
        assert!(
            count_live_pids(&studio_pids) >= 1,
            "expected live target `legion stream` on studio"
        );

        // Revoke the URL (as viewer close does) BEFORE closing the TCP socket: both relay and
        // target `legion stream` processes must exit within bounded time.
        let revoke_started = Instant::now();
        assert!(server.revoke_url(&minted.url));

        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if count_live_pids(&tower_pids) == 0 && count_live_pids(&studio_pids) == 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert_eq!(
            count_live_pids(&tower_pids),
            0,
            "relay `legion stream` on tower lingered past {:?} after content_revoke",
            revoke_started.elapsed()
        );
        assert_eq!(
            count_live_pids(&studio_pids),
            0,
            "target `legion stream` on studio lingered past {:?} after content_revoke",
            revoke_started.elapsed()
        );

        // Draining the still-open loopback socket observes clean truncation before Content-Length
        // and drops device active_streams to 0.
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            let step = tokio::time::timeout(Duration::from_secs(2), sock.read(&mut buf)).await;
            assert!(
                step.is_ok(),
                "loopback stream did not abort after content_revoke"
            );
            match step.unwrap() {
                Ok(0) | Err(_) => break,
                Ok(n) => received.extend_from_slice(&buf[..n]),
            }
        }
        assert!(
            received.len() < media_bytes.len(),
            "revoked relayed stream must abort strictly before full 16 MiB ({} vs {})",
            received.len(),
            media_bytes.len()
        );
        drop(sock);

        let dev_deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < dev_deadline && server.active_streams.load(Ordering::Acquire) > 0 {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(
            server.active_streams.load(Ordering::Acquire),
            0,
            "device active_streams did not drop to 0 after content_revoke"
        );
        assert_eq!(count_live_pids(&tower_pids), 0);
        assert_eq!(count_live_pids(&studio_pids), 0);

        server.shutdown();
        link.close().await;
    }

    #[tokio::test]
    async fn fleet_ticket_binds_once_to_learned_identity_and_revokes_on_new_pairing_or_identity_change()
     {
        use crate::backend::LearnedDirectIdentity;

        let media_bytes: Vec<u8> = (0..64 * 1024usize).map(|i| (i % 251) as u8).collect();
        let media_arc = Arc::new(media_bytes.clone());
        let tower_key = legion_link::DeviceKey::from_seed([71u8; 32])
            .public_key_line("tower")
            .unwrap();
        let studio_key_a = legion_link::DeviceKey::from_seed([72u8; 32])
            .public_key_line("studio")
            .unwrap();
        let studio_key_b = legion_link::DeviceKey::from_seed([73u8; 32])
            .public_key_line("studio")
            .unwrap();
        let canonical = |k: &str| {
            let mut parts = k.split_whitespace();
            format!("{} {}", parts.next().unwrap(), parts.next().unwrap())
        };
        let key_a_canon = canonical(&studio_key_a);
        let key_b_canon = canonical(&studio_key_b);

        struct SimpleMediaOpener {
            payload: Arc<Vec<u8>>,
        }

        impl StreamOpener for SimpleMediaOpener {
            fn open_stream(
                &self,
                _cancel: CancellationToken,
            ) -> Pin<Box<dyn Future<Output = Result<RawStream, StreamOpenError>> + Send + '_>>
            {
                let payload = Arc::clone(&self.payload);
                Box::pin(async move {
                    let (client_in, mut server_out) = tokio::io::duplex(64 * 1024);
                    let (server_in, client_out) = tokio::io::duplex(64 * 1024);
                    let cancel = CancellationToken::new();
                    tokio::spawn(async move {
                        let _ = server_out
                            .write_all(
                                format!(
                                    "{}\n",
                                    json!({
                                        "protocol": 1,
                                        "capabilities": [legion_link::CAP_STREAM_RELAY]
                                    })
                                )
                                .as_bytes(),
                            )
                            .await;
                        let mut reader = BufReader::new(server_in);
                        let mut line = String::new();
                        if reader.read_line(&mut line).await.unwrap_or(0) == 0 {
                            return;
                        }
                        let req: Value = serde_json::from_str(&line).unwrap();
                        let inner = if req["op"] == json!("relay") {
                            &req["request"]
                        } else {
                            &req
                        };
                        let offset = inner["offset"].as_u64().unwrap_or(0) as usize;
                        let avail = payload.len().saturating_sub(offset);
                        let len = inner
                            .get("length")
                            .and_then(Value::as_u64)
                            .map_or(avail, |n| (n as usize).min(avail));
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
                        park: None,
                        guard: None,
                    })
                })
            }
        }

        let opener: SessionOpener = {
            let payload = Arc::clone(&media_arc);
            Arc::new(move |entry, _, cancel| {
                let payload = Arc::clone(&payload);
                let node_name = entry.node.clone();
                Box::pin(async move {
                    let (client_in, mut server_out) = tokio::io::duplex(8 * 1024);
                    let (server_in, client_out) = tokio::io::duplex(8 * 1024);
                    let node_for_rpc = node_name.clone();
                    tokio::spawn(async move {
                        let mut reader = BufReader::new(server_in);
                        let mut line = String::new();
                        while reader.read_line(&mut line).await.unwrap_or(0) > 0 {
                            let req: legion_link::proto::Request =
                                serde_json::from_str(&line).unwrap();
                            let reply = ProtoResp::ok(
                                req.id,
                                serde_json::to_value(HelloResult {
                                    node: node_for_rpc.clone(),
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
                            opener: Arc::new(SimpleMediaOpener { payload }),
                        }),
                        &node_name,
                        None,
                        Duration::from_secs(5),
                        cancel,
                    )
                    .await
                    .map_err(|err| err.into_error())
                })
            })
        };

        let tower_key_canon = {
            let mut parts = tower_key.split_whitespace();
            format!("{} {}", parts.next().unwrap(), parts.next().unwrap())
        };
        let link = Link::with_opener(
            LinkConfig {
                device_key: Some("dummy".to_owned()),
                nodes: vec![NodeLink::ssh(
                    "tower",
                    SshTarget {
                        hosts: vec!["tower.example.net".to_owned()],
                        port: 22,
                        user: "me".to_owned(),
                        host_keys: vec![tower_key.clone()],
                    },
                    Some("device:pixel".to_owned()),
                )],
                ..LinkConfig::default()
            },
            LinkOptions::default(),
            opener,
        );

        let attach_learned_studio = |link: &Link, host_key: &str| {
            let mut learned = NodeLink::ssh(
                "studio",
                SshTarget {
                    hosts: vec!["studio.example.net".to_owned()],
                    port: 22,
                    user: "me".to_owned(),
                    host_keys: vec![host_key.to_owned()],
                },
                Some("device:pixel".to_owned()),
            );
            learned.origin = legion_link::NodeOrigin::Learned;
            learned.learned_from = Some("tower".to_owned());
            link.upsert_node(learned);
        };

        #[derive(Clone)]
        struct MutableTargetState {
            live_fleet_epoch: u64,
            studio_holder: String,
            studio_learned: Option<LearnedDirectIdentity>,
            tower_key_canon: String,
        }

        struct MutableFleetBackend {
            link: Link,
            state: Arc<Mutex<MutableTargetState>>,
        }

        impl Backend for MutableFleetBackend {
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
                let st = self.state.lock().unwrap().clone();
                Ok(ContentTarget {
                    link: self.link.clone(),
                    link_node: machine.to_owned(),
                    machine: machine.to_owned(),
                    principal: "device:pixel".to_owned(),
                    holder: st.studio_holder,
                    learned_identity: st.studio_learned,
                })
            }
            fn is_pairing_holder_live(&self, holder: &str) -> bool {
                let st = self.state.lock().unwrap();
                holder == "pairing:tower:1"
                    || parse_fleet_holder(holder) == Some(st.live_fleet_epoch)
            }
            fn node_route_identity(&self, node: &str) -> Option<NodeRouteIdentity> {
                if node != "tower" {
                    return None;
                }
                let st = self.state.lock().unwrap();
                Some(NodeRouteIdentity {
                    node: "tower".to_owned(),
                    is_paired: true,
                    holder: "pairing:tower:1".to_owned(),
                    host_keys: vec![st.tower_key_canon.clone()],
                })
            }
        }

        let target_state = Arc::new(Mutex::new(MutableTargetState {
            live_fleet_epoch: 1,
            studio_holder: "fleet:1".to_owned(),
            studio_learned: None,
            tower_key_canon: tower_key_canon.clone(),
        }));
        let backend = Arc::new(MutableFleetBackend {
            link: link.clone(),
            state: Arc::clone(&target_state),
        });
        let server = Arc::new(AppContentServer::with_config(
            backend,
            ContentMountMode::Loopback,
            TicketConfig::default(),
            Duration::from_secs(30),
            vec!["http://tauri.localhost".to_owned()],
        ));

        let get_range = |port: u16, path: String, range: &'static str| async move {
            raw_http(
                port,
                &format!(
                    "GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nOrigin: http://tauri.localhost\r\nRange: {range}\r\nConnection: close\r\n\r\n"
                ),
            )
            .await
        };

        // 1) Fleet ticket upgrades once to a learned direct entry from a paired introducer during
        //    the ticket's epoch, keeps working on that bound identity, and is revoked immediately
        //    if the learned entry's pinned host keys later change (even if fleet epoch didn't bump).
        let t1 = server
            .mint_url("studio", "/home/me/video.mp4", false)
            .await
            .unwrap();
        let port = server.loopback_port().unwrap();
        let p1 = t1
            .url
            .strip_prefix(&format!("http://127.0.0.1:{port}"))
            .unwrap()
            .to_owned();
        let (st, head, body) = get_range(port, p1.clone(), "bytes=0-1023").await;
        assert_eq!(st, 206, "{head}");
        assert_eq!(body, &media_bytes[0..=1023]);

        // Learn `studio` with key A during epoch 1 -> first move succeeds and binds to key A.
        attach_learned_studio(&link, &key_a_canon);
        {
            let mut st = target_state.lock().unwrap();
            st.studio_holder = "pairing:studio:2".to_owned();
            st.studio_learned = Some(LearnedDirectIdentity {
                learned_fleet_epoch: 1,
                from_paired_introducer: true,
                learned_from: Some("tower".to_owned()),
                introducer_holder: Some("pairing:tower:1".to_owned()),
                introducer_host_keys: vec![tower_key_canon.clone()],
                host_keys: vec![key_a_canon.clone()],
            });
        }
        let (st, head, body) = get_range(port, p1.clone(), "bytes=1024-2047").await;
        assert_eq!(st, 206, "{head}");
        assert_eq!(body, &media_bytes[1024..=2047]);
        // Subsequent range read with identical bound identity still succeeds.
        let (st, head, _) = get_range(port, p1.clone(), "bytes=2048-3071").await;
        assert_eq!(st, 206, "{head}");

        // Now learned entry's identity changes to key B -> ticket MUST be refused with 404!
        {
            let mut st = target_state.lock().unwrap();
            st.studio_learned = Some(LearnedDirectIdentity {
                learned_fleet_epoch: 1,
                from_paired_introducer: true,
                learned_from: Some("tower".to_owned()),
                introducer_holder: Some("pairing:tower:1".to_owned()),
                introducer_host_keys: vec![tower_key_canon.clone()],
                host_keys: vec![key_b_canon.clone()],
            });
        }
        let (st, head, _) = get_range(port, p1.clone(), "bytes=3072-4095").await;
        assert_eq!(
            st, 404,
            "ticket bound to key A must be refused when learned host keys change to key B: {head}"
        );

        // 2) Once moved to a direct entry, a fleet ticket cannot bounce back to `fleet:` or move
        //    to a second direct entry.
        link.remove_node("studio").await;
        {
            let mut st = target_state.lock().unwrap();
            st.studio_holder = "fleet:1".to_owned();
            st.studio_learned = None;
        }
        let t2 = server
            .mint_url("studio", "/home/me/video.mp4", false)
            .await
            .unwrap();
        let p2 = t2
            .url
            .strip_prefix(&format!("http://127.0.0.1:{port}"))
            .unwrap()
            .to_owned();
        attach_learned_studio(&link, &key_a_canon);
        {
            let mut st = target_state.lock().unwrap();
            st.studio_holder = "pairing:studio:3".to_owned();
            st.studio_learned = Some(LearnedDirectIdentity {
                learned_fleet_epoch: 1,
                from_paired_introducer: true,
                learned_from: Some("tower".to_owned()),
                introducer_holder: Some("pairing:tower:1".to_owned()),
                introducer_host_keys: vec![tower_key_canon.clone()],
                host_keys: vec![key_a_canon.clone()],
            });
        }
        let (st, head, _) = get_range(port, p2.clone(), "bytes=0-511").await;
        assert_eq!(st, 206, "{head}");
        // Bouncing back to fleet or to a new pairing epoch holder ends the ticket.
        link.remove_node("studio").await;
        {
            let mut st = target_state.lock().unwrap();
            st.studio_holder = "fleet:1".to_owned();
            st.studio_learned = None;
        }
        let (st, head, _) = get_range(port, p2.clone(), "bytes=512-1023").await;
        assert_eq!(
            st, 404,
            "already-upgraded fleet ticket must not fall back to fleet: {head}"
        );

        // 3) A new direct `pair()` under the same name `studio` ends the open `fleet:` ticket
        //    (both because `learned_identity` is None for Paired entries AND because `pair()`
        //    bumps `fleet_pairing_epoch`).
        {
            let mut st = target_state.lock().unwrap();
            st.live_fleet_epoch = 2;
            st.studio_holder = "fleet:2".to_owned();
            st.studio_learned = None;
        }
        let t3 = server
            .mint_url("studio", "/home/me/video.mp4", false)
            .await
            .unwrap();
        let p3 = t3
            .url
            .strip_prefix(&format!("http://127.0.0.1:{port}"))
            .unwrap()
            .to_owned();
        let (st, head, _) = get_range(port, p3.clone(), "bytes=0-511").await;
        assert_eq!(st, 206, "{head}");
        // Even if the fleet epoch hadn't bumped yet, a Paired entry (`learned_identity: None`)
        // must refuse the fleet ticket.
        {
            let mut st = target_state.lock().unwrap();
            st.studio_holder = "pairing:studio:4".to_owned();
            st.studio_learned = None;
        }
        let (st, head, _) = get_range(port, p3.clone(), "bytes=512-1023").await;
        assert_eq!(
            st, 404,
            "fleet ticket must never move to a directly Paired entry: {head}"
        );
        // And bumping fleet epoch (as `LinkBackend::pair()` does) also ends it.
        {
            let mut st = target_state.lock().unwrap();
            st.live_fleet_epoch = 3;
        }
        let (st, head, _) = get_range(port, p3.clone(), "bytes=512-1023").await;
        assert_eq!(
            st, 404,
            "bumping fleet epoch on pair must revoke ticket: {head}"
        );

        // 4) A learned entry from a non-paired introducer or from another fleet epoch is refused.
        {
            let mut st = target_state.lock().unwrap();
            st.live_fleet_epoch = 4;
            st.studio_holder = "fleet:4".to_owned();
            st.studio_learned = None;
        }
        let t4 = server
            .mint_url("studio", "/home/me/video.mp4", false)
            .await
            .unwrap();
        let p4 = t4
            .url
            .strip_prefix(&format!("http://127.0.0.1:{port}"))
            .unwrap()
            .to_owned();
        {
            let mut st = target_state.lock().unwrap();
            st.studio_holder = "pairing:studio:5".to_owned();
            st.studio_learned = Some(LearnedDirectIdentity {
                learned_fleet_epoch: 4,
                from_paired_introducer: false,
                learned_from: Some("tower".to_owned()),
                introducer_holder: None,
                introducer_host_keys: Vec::new(),
                host_keys: vec![key_a_canon.clone()],
            });
        }
        let (st, head, _) = get_range(port, p4.clone(), "bytes=0-511").await;
        assert_eq!(
            st, 404,
            "fleet ticket must refuse learned entry from non-paired introducer: {head}"
        );

        {
            let mut st = target_state.lock().unwrap();
            st.studio_learned = Some(LearnedDirectIdentity {
                learned_fleet_epoch: 3,
                from_paired_introducer: true,
                learned_from: Some("tower".to_owned()),
                introducer_holder: Some("pairing:tower:1".to_owned()),
                introducer_host_keys: vec![tower_key_canon],
                host_keys: vec![key_a_canon],
            });
        }
        let (st, head, _) = get_range(port, p4, "bytes=0-511").await;
        assert_eq!(
            st, 404,
            "fleet ticket must refuse learned entry from another epoch: {head}"
        );

        server.shutdown();
        link.close().await;
    }

    #[tokio::test]
    async fn two_fleet_relayed_ticket_never_serves_other_fleet_studio_and_allows_same_introducer_upgrade()
     {
        use legion_link::proto::{HelloResult, PROTOCOL, Response as ProtoResp, VERSION};
        use legion_link::{
            CAP_STREAM_RELAY, DeviceKey, Link, LinkConfig, LinkOptions, NodeLink, NodeOrigin,
            RawStream, Session, SessionOpener, SshTarget, StreamOpenError, StreamOpener,
            TransportHandle,
        };
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

        use crate::backend::LearnedDirectIdentity;

        let fleet_a_bytes: Vec<u8> = (0..16_384usize)
            .map(|i| b'A'.wrapping_add((i % 13) as u8))
            .collect();
        let fleet_b_bytes: Vec<u8> = (0..16_384usize)
            .map(|i| b'a'.wrapping_add((i % 13) as u8))
            .collect();

        let tower_key_line = DeviceKey::from_seed([61u8; 32])
            .public_key_line("tower")
            .unwrap();
        let other_key_line = DeviceKey::from_seed([62u8; 32])
            .public_key_line("other-hub")
            .unwrap();
        let studio_a_key_line = DeviceKey::from_seed([63u8; 32])
            .public_key_line("studio-a")
            .unwrap();
        let studio_b_key_line = DeviceKey::from_seed([64u8; 32])
            .public_key_line("studio-b")
            .unwrap();

        let canon = |line: &str| {
            let mut parts = line.split_whitespace();
            format!("{} {}", parts.next().unwrap(), parts.next().unwrap())
        };
        let tower_key_canon = canon(&tower_key_line);
        let other_key_canon = canon(&other_key_line);
        let studio_a_key_canon = canon(&studio_a_key_line);
        let studio_b_key_canon = canon(&studio_b_key_line);

        let tower_down = Arc::new(AtomicBool::new(false));
        let studio_a_down = Arc::new(AtomicBool::new(false));
        let other_hub_hits = Arc::new(AtomicUsize::new(0));
        let studio_a_direct_hits = Arc::new(AtomicUsize::new(0));
        let studio_b_direct_hits = Arc::new(AtomicUsize::new(0));

        struct TaggedOpener {
            payload: Vec<u8>,
            file_id: u64,
            hits: Option<Arc<AtomicUsize>>,
        }

        impl StreamOpener for TaggedOpener {
            fn open_stream(
                &self,
                _cancel: CancellationToken,
            ) -> Pin<Box<dyn Future<Output = Result<RawStream, StreamOpenError>> + Send + '_>>
            {
                let payload = self.payload.clone();
                let file_id = self.file_id;
                let hits = self.hits.clone();
                Box::pin(async move {
                    if let Some(h) = hits {
                        h.fetch_add(1, Ordering::SeqCst);
                    }
                    let (client_in, mut server_out) = tokio::io::duplex(64 * 1024);
                    let (server_in, client_out) = tokio::io::duplex(64 * 1024);
                    tokio::spawn(async move {
                        let greeting = json!({
                            "protocol": 1,
                            "capabilities": [CAP_STREAM_RELAY],
                        });
                        let _ = server_out
                            .write_all(format!("{greeting}\n").as_bytes())
                            .await;
                        let mut reader = BufReader::new(server_in);
                        let mut line = String::new();
                        if reader.read_line(&mut line).await.unwrap_or(0) == 0 {
                            return;
                        }
                        let req: Value = serde_json::from_str(&line).unwrap();
                        let inner = if req["op"] == "relay" {
                            &req["request"]
                        } else {
                            &req
                        };
                        let offset = inner["offset"].as_u64().unwrap_or(0) as usize;
                        let length = inner
                            .get("length")
                            .and_then(Value::as_u64)
                            .map(|n| n as usize)
                            .unwrap_or(payload.len().saturating_sub(offset))
                            .min(payload.len().saturating_sub(offset));
                        let ready = json!({
                            "size": payload.len(),
                            "offset": offset,
                            "length": length,
                            "mtime": { "secs": 1_700_000_000i64, "nanos": 0u32 },
                            "file_id": file_id,
                        });
                        let _ = server_out.write_all(format!("{ready}\n").as_bytes()).await;
                        let _ = server_out
                            .write_all(&payload[offset..offset + length])
                            .await;
                        let _ = server_out.shutdown().await;
                    });
                    Ok(RawStream {
                        reader: Box::new(client_in),
                        writer: Box::new(client_out),
                        stderr: None,
                        cancel: CancellationToken::new(),
                        park: None,
                        guard: None,
                    })
                })
            }
        }

        struct TwoFleetTransport(Arc<dyn StreamOpener>);
        impl TransportHandle for TwoFleetTransport {
            fn collect_exit_info(
                &mut self,
                _: Duration,
            ) -> Pin<Box<dyn Future<Output = (Option<i32>, String)> + Send + '_>> {
                Box::pin(async { (Some(0), String::new()) })
            }
            fn close(self: Box<Self>, _: Duration) -> Pin<Box<dyn Future<Output = ()> + Send>> {
                Box::pin(async {})
            }
            fn stream_opener(&self) -> Option<Arc<dyn StreamOpener>> {
                Some(Arc::clone(&self.0))
            }
        }

        let opener: SessionOpener = {
            let tower_down = Arc::clone(&tower_down);
            let studio_a_down = Arc::clone(&studio_a_down);
            let other_hub_hits = Arc::clone(&other_hub_hits);
            let studio_a_direct_hits = Arc::clone(&studio_a_direct_hits);
            let studio_b_direct_hits = Arc::clone(&studio_b_direct_hits);
            let fleet_a_bytes = fleet_a_bytes.clone();
            let fleet_b_bytes = fleet_b_bytes.clone();
            let studio_a_key_canon = studio_a_key_canon.clone();
            Arc::new(move |entry, _, cancel| {
                let tower_down = Arc::clone(&tower_down);
                let studio_a_down = Arc::clone(&studio_a_down);
                let other_hub_hits = Arc::clone(&other_hub_hits);
                let studio_a_direct_hits = Arc::clone(&studio_a_direct_hits);
                let studio_b_direct_hits = Arc::clone(&studio_b_direct_hits);
                let fleet_a_bytes = fleet_a_bytes.clone();
                let fleet_b_bytes = fleet_b_bytes.clone();
                let studio_a_key_canon = studio_a_key_canon.clone();
                Box::pin(async move {
                    if entry.node == "tower" && tower_down.load(Ordering::SeqCst) {
                        return Err(ProtoErrorBody::new(
                            ProtoErrorCode::Unreachable,
                            "Can't reach tower right now.",
                        )
                        .with_machine("tower"));
                    }
                    let is_studio_a = entry.node == "studio"
                        && entry.target.as_ref().is_some_and(|t| {
                            t.host_keys
                                .iter()
                                .any(|k| k.starts_with(&studio_a_key_canon))
                        });
                    if is_studio_a && studio_a_down.load(Ordering::SeqCst) {
                        return Err(ProtoErrorBody::new(
                            ProtoErrorCode::Unreachable,
                            "Can't reach studio right now.",
                        )
                        .with_machine("studio"));
                    }

                    let (payload, file_id, hits) = match entry.node.as_str() {
                        "tower" => (fleet_a_bytes, 101u64, None),
                        "other-hub" => (fleet_b_bytes, 202u64, Some(other_hub_hits)),
                        "studio" if is_studio_a => {
                            (fleet_a_bytes, 101u64, Some(studio_a_direct_hits))
                        }
                        "studio" => (fleet_b_bytes, 202u64, Some(studio_b_direct_hits)),
                        _ => unreachable!(),
                    };

                    let (client_in, mut server_out) = tokio::io::duplex(16 * 1024);
                    let (server_in, client_out) = tokio::io::duplex(16 * 1024);
                    let node_name = entry.node.clone();
                    tokio::spawn(async move {
                        let mut reader = BufReader::new(server_in);
                        let mut line = String::new();
                        while reader.read_line(&mut line).await.unwrap_or(0) > 0 {
                            let req: legion_link::proto::Request =
                                serde_json::from_str(&line).unwrap();
                            let reply = ProtoResp::ok(
                                req.id,
                                serde_json::to_value(HelloResult {
                                    node: node_name.clone(),
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
                        Box::new(TwoFleetTransport(Arc::new(TaggedOpener {
                            payload,
                            file_id,
                            hits,
                        }))),
                        &entry.node,
                        None,
                        Duration::from_secs(5),
                        cancel,
                    )
                    .await
                    .map_err(|e| e.into_error())
                })
            })
        };

        let link = Link::with_opener(
            LinkConfig {
                device_key: Some("dummy".to_owned()),
                nodes: vec![
                    NodeLink::ssh(
                        "tower",
                        SshTarget {
                            hosts: vec!["tower.example.net".to_owned()],
                            port: 22,
                            user: "me".to_owned(),
                            host_keys: vec![tower_key_line.clone()],
                        },
                        Some("device:pixel".to_owned()),
                    ),
                    NodeLink::ssh(
                        "other-hub",
                        SshTarget {
                            hosts: vec!["other-hub.example.net".to_owned()],
                            port: 22,
                            user: "me".to_owned(),
                            host_keys: vec![other_key_line.clone()],
                        },
                        Some("device:pixel".to_owned()),
                    ),
                ],
                ..LinkConfig::default()
            },
            LinkOptions::default(),
            opener,
        );

        struct TwoFleetBackend {
            link: Link,
            tower_key_canon: String,
            other_key_canon: String,
        }

        impl Backend for TwoFleetBackend {
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
                let cfg = self.link.config();
                if let Some(entry) = cfg.node(machine) {
                    let learned_identity = if entry.origin == NodeOrigin::Learned {
                        let intro = entry.learned_from.as_deref().and_then(|n| cfg.node(n));
                        let intro_keys = intro
                            .and_then(|n| match n.node.as_str() {
                                "tower" => Some(vec![self.tower_key_canon.clone()]),
                                "other-hub" => Some(vec![self.other_key_canon.clone()]),
                                _ => None,
                            })
                            .unwrap_or_default();
                        let host_keys = entry
                            .target
                            .as_ref()
                            .map(|t| {
                                t.host_keys
                                    .iter()
                                    .map(|l| {
                                        let mut p = l.split_whitespace();
                                        format!("{} {}", p.next().unwrap(), p.next().unwrap())
                                    })
                                    .collect::<Vec<_>>()
                            })
                            .unwrap_or_default();
                        Some(LearnedDirectIdentity {
                            learned_fleet_epoch: 1,
                            from_paired_introducer: intro
                                .is_some_and(|n| n.origin == NodeOrigin::Paired),
                            learned_from: intro.map(|n| n.node.clone()),
                            introducer_holder: intro.map(|n| format!("pairing:{}:1", n.node)),
                            introducer_host_keys: intro_keys,
                            host_keys,
                        })
                    } else {
                        None
                    };
                    return Ok(ContentTarget {
                        link: self.link.clone(),
                        link_node: entry.node.clone(),
                        machine: entry.node.clone(),
                        principal: "device:pixel".to_owned(),
                        holder: format!("pairing:{}:1", entry.node),
                        learned_identity,
                    });
                }
                Ok(ContentTarget {
                    link: self.link.clone(),
                    link_node: machine.to_owned(),
                    machine: machine.to_owned(),
                    principal: "device:pixel".to_owned(),
                    holder: "fleet:1".to_owned(),
                    learned_identity: None,
                })
            }
            fn is_pairing_holder_live(&self, holder: &str) -> bool {
                matches!(
                    holder,
                    "fleet:1" | "pairing:tower:1" | "pairing:other-hub:1" | "pairing:studio:1"
                )
            }
            fn node_route_identity(&self, node: &str) -> Option<NodeRouteIdentity> {
                match node {
                    "tower" => Some(NodeRouteIdentity {
                        node: "tower".to_owned(),
                        is_paired: true,
                        holder: "pairing:tower:1".to_owned(),
                        host_keys: vec![self.tower_key_canon.clone()],
                    }),
                    "other-hub" => Some(NodeRouteIdentity {
                        node: "other-hub".to_owned(),
                        is_paired: true,
                        holder: "pairing:other-hub:1".to_owned(),
                        host_keys: vec![self.other_key_canon.clone()],
                    }),
                    _ => None,
                }
            }
        }

        let backend = Arc::new(TwoFleetBackend {
            link: link.clone(),
            tower_key_canon,
            other_key_canon,
        });
        let server = Arc::new(AppContentServer::with_config(
            backend,
            ContentMountMode::Loopback,
            TicketConfig::default(),
            Duration::from_secs(30),
            vec!["http://tauri.localhost".to_owned()],
        ));

        let get_range = |port: u16, path: String, range: &'static str| async move {
            raw_http(
                port,
                &format!(
                    "GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nOrigin: http://tauri.localhost\r\nRange: {range}\r\nConnection: close\r\n\r\n"
                ),
            )
            .await
        };

        // Sequence 1: Ticket minted for `studio` through `tower`. Then `other-hub` introduces its
        // own learned `studio` in the SAME fleet epoch -> ticket MUST NOT serve fleet B's `studio`.
        let t_seq1 = server
            .mint_url("studio", "/home/me/video.mp4", false)
            .await
            .unwrap();
        let port = server.loopback_port().unwrap();
        let p_seq1 = t_seq1
            .url
            .strip_prefix(&format!("http://127.0.0.1:{port}"))
            .unwrap()
            .to_owned();
        let (st, head, body) = get_range(port, p_seq1.clone(), "bytes=0-1023").await;
        assert_eq!(st, 206, "{head}");
        assert_eq!(body, &fleet_a_bytes[0..=1023]);
        assert_eq!(other_hub_hits.load(Ordering::SeqCst), 0);

        // `other-hub` introduces learned `studio` (fleet B) in epoch 1.
        {
            let mut learned_b = NodeLink::ssh(
                "studio",
                SshTarget {
                    hosts: vec!["studio-b.example.net".to_owned()],
                    port: 22,
                    user: "me".to_owned(),
                    host_keys: vec![studio_b_key_line.clone()],
                },
                Some("device:pixel".to_owned()),
            );
            learned_b.origin = NodeOrigin::Learned;
            learned_b.learned_from = Some("other-hub".to_owned());
            link.upsert_node(learned_b);
        }
        let (st, head, body) = get_range(port, p_seq1, "bytes=1024-2047").await;
        assert_eq!(
            st, 404,
            "ticket minted via tower must refuse learned studio introduced by other-hub: {head}"
        );
        assert_ne!(body, &fleet_b_bytes[1024..=2047]);
        assert_eq!(studio_b_direct_hits.load(Ordering::SeqCst), 0);
        assert_eq!(other_hub_hits.load(Ordering::SeqCst), 0);

        // Remove learned `studio-b` for Sequence 2.
        link.remove_node("studio").await;

        // Sequence 2: With NO learn/pair event, `tower` becomes unreachable during playback while
        // `other-hub` is online and knows a `studio` -> ticket MUST fail with the transport error
        // (502) and NEVER fall back to `other-hub`. Minting a fresh ticket then resolves afresh.
        let t_seq2 = server
            .mint_url("studio", "/home/me/video.mp4", false)
            .await
            .unwrap();
        let p_seq2 = t_seq2
            .url
            .strip_prefix(&format!("http://127.0.0.1:{port}"))
            .unwrap()
            .to_owned();
        let (st, head, body) = get_range(port, p_seq2.clone(), "bytes=0-1023").await;
        assert_eq!(st, 206, "{head}");
        assert_eq!(body, &fleet_a_bytes[0..=1023]);

        tower_down.store(true, Ordering::SeqCst);
        link.close().await;
        let (st, head, _) = get_range(port, p_seq2, "bytes=1024-2047").await;
        assert_eq!(
            st, 502,
            "ticket pinned to unreachable tower must fail with 502 transport error, never fall back to other-hub: {head}"
        );
        assert_eq!(
            other_hub_hits.load(Ordering::SeqCst),
            0,
            "other-hub must never be contacted for a ticket minted via tower"
        );
        let status = server.content_status_for_url(&t_seq2.url);
        assert_eq!(status.status, Some(502));
        assert_eq!(status.error_code.as_deref(), Some("unreachable"));

        // Viewer minting a NEW ticket resolves `studio` afresh (now via `other-hub`).
        let t_fresh = server
            .mint_url("studio", "/home/me/video.mp4", false)
            .await
            .unwrap();
        let p_fresh = t_fresh
            .url
            .strip_prefix(&format!("http://127.0.0.1:{port}"))
            .unwrap()
            .to_owned();
        let (st, head, body) = get_range(port, p_fresh, "bytes=0-511").await;
        assert_eq!(st, 206, "{head}");
        assert_eq!(body, &fleet_b_bytes[0..=511]);
        assert!(other_hub_hits.load(Ordering::SeqCst) >= 1);

        // Sequence 3: Route upgrade from the SAME introducer (`tower`) still works mid-playback,
        // and after upgrade the ticket is served ONLY through that direct entry's pinned identity.
        tower_down.store(false, Ordering::SeqCst);
        other_hub_hits.store(0, Ordering::SeqCst);
        studio_a_direct_hits.store(0, Ordering::SeqCst);
        link.close().await;

        let t_seq3 = server
            .mint_url("studio", "/home/me/video.mp4", false)
            .await
            .unwrap();
        let p_seq3 = t_seq3
            .url
            .strip_prefix(&format!("http://127.0.0.1:{port}"))
            .unwrap()
            .to_owned();
        let (st, head, body) = get_range(port, p_seq3.clone(), "bytes=0-1023").await;
        assert_eq!(st, 206, "{head}");
        assert_eq!(body, &fleet_a_bytes[0..=1023]);
        assert_eq!(studio_a_direct_hits.load(Ordering::SeqCst), 0);

        // Same introducer `tower` introduces learned `studio` (key A) mid-playback.
        {
            let mut learned_a = NodeLink::ssh(
                "studio",
                SshTarget {
                    hosts: vec!["studio-a.example.net".to_owned()],
                    port: 22,
                    user: "me".to_owned(),
                    host_keys: vec![studio_a_key_line.clone()],
                },
                Some("device:pixel".to_owned()),
            );
            learned_a.origin = NodeOrigin::Learned;
            learned_a.learned_from = Some("tower".to_owned());
            link.upsert_node(learned_a);
        }
        let (st, head, body) = get_range(port, p_seq3.clone(), "bytes=1024-2047").await;
        assert_eq!(st, 206, "upgrade from same introducer must work: {head}");
        assert_eq!(body, &fleet_a_bytes[1024..=2047]);
        assert!(
            studio_a_direct_hits.load(Ordering::SeqCst) >= 1,
            "range after same-introducer learn must use direct studio-A route"
        );
        assert_eq!(other_hub_hits.load(Ordering::SeqCst), 0);

        // Once upgraded, if direct `studio-A` goes unreachable, ticket fails with 502 and NEVER
        // falls back to `tower` or `other-hub`.
        studio_a_down.store(true, Ordering::SeqCst);
        link.close().await;
        let (st, head, _) = get_range(port, p_seq3.clone(), "bytes=2048-3071").await;
        assert_eq!(
            st, 502,
            "upgraded direct ticket must fail with 502 when direct studio is unreachable: {head}"
        );
        assert_eq!(other_hub_hits.load(Ordering::SeqCst), 0);

        // And if `studio`'s pinned host key changes to key B, ticket is refused with 404.
        studio_a_down.store(false, Ordering::SeqCst);
        {
            let mut updated = link
                .config()
                .node("studio")
                .cloned()
                .expect("learned studio entry present");
            updated.target.as_mut().unwrap().host_keys = vec![studio_b_key_line];
            link.upsert_node(updated);
        }
        let (st, head, _) = get_range(port, p_seq3, "bytes=2048-3071").await;
        assert_eq!(
            st, 404,
            "upgraded direct ticket must refuse changed host key: {head}"
        );
        assert_eq!(studio_b_direct_hits.load(Ordering::SeqCst), 0);
        let _ = studio_a_key_canon;
        let _ = studio_b_key_canon;

        server.shutdown();
        link.close().await;
    }

    #[tokio::test]
    async fn loopback_recovers_when_socket_is_reclaimed_under_running_accept_task() {
        let payload = b"reclaimed-socket-recovery-payload".to_vec();
        let mut files = HashMap::new();
        files.insert("/home/me/clip.bin".to_owned(), payload.clone());

        let backend = Arc::new(MockContentBackend::new(files));
        let server = Arc::new(AppContentServer::with_config(
            backend,
            ContentMountMode::Loopback,
            TicketConfig::default(),
            Duration::from_secs(30),
            vec!["http://tauri.localhost".to_owned()],
        ));

        let initial = server
            .mint_url("tower", "/home/me/clip.bin", false)
            .await
            .unwrap();
        let initial_port = server.loopback_port().unwrap();
        let initial_path = initial
            .url
            .strip_prefix(&format!("http://127.0.0.1:{initial_port}"))
            .unwrap()
            .to_owned();

        // 1) Simulate OS reclaiming the listening socket underneath an accept task that is STILL
        // running (shutdown NOT cancelled, accept_alive == true, accept_task NOT finished).
        let dead_port = {
            let probe_bind = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
            probe_bind.local_addr().unwrap().port()
        };
        {
            let mut slot = server.listener.lock().unwrap();
            if let Some(old) = slot.take() {
                old.shutdown.cancel();
            }
            let fake_shutdown = CancellationToken::new();
            let fake_shutdown_task = fake_shutdown.clone();
            let fake_accept_task = tokio::spawn(async move {
                fake_shutdown_task.cancelled().await;
            });
            *slot = Some(ListenerHandle {
                port: dead_port,
                shutdown: fake_shutdown,
                accept_alive: Arc::new(AtomicBool::new(true)),
                accept_task: fake_accept_task,
                probe_secret: [0x5Au8; 32],
                last_verified: Instant::now(),
            });
            assert!(
                slot.as_ref().unwrap().is_alive(),
                "simulated reclaimed-socket handle must report accept task still running"
            );
        }

        // Resume recovery ALWAYS forces a live proof-of-ownership probe even with fresh
        // `last_verified`, detects the dead port, and binds a new OS-chosen port.
        let recovered_port = server
            .probe_or_recover_loopback()
            .await
            .unwrap()
            .expect("resume probe must recover a new port while live tickets exist");
        assert_ne!(recovered_port, dead_port);
        assert_eq!(server.loopback_port(), Some(recovered_port));

        let (st1, head1, body1) = raw_http(
            recovered_port,
            &format!(
                "GET {initial_path} HTTP/1.1\r\nHost: 127.0.0.1:{recovered_port}\r\nConnection: close\r\n\r\n"
            ),
        )
        .await;
        assert_eq!(st1, 200, "{head1}");
        assert_eq!(body1, payload);

        // 2) Live listener with aged `last_verified`: pre-mint probe succeeds and keeps the same
        // port without unnecessary rebinding.
        {
            let mut slot = server.listener.lock().unwrap();
            let h = slot.as_mut().unwrap();
            h.last_verified = Instant::now() - (LOOPBACK_PROBE_FRESHNESS + Duration::from_secs(1));
        }
        let still_same = server
            .mint_url("tower", "/home/me/clip.bin", false)
            .await
            .unwrap();
        assert_eq!(server.loopback_port(), Some(recovered_port));
        assert!(
            still_same
                .url
                .starts_with(&format!("http://127.0.0.1:{recovered_port}/v1/content/"))
        );

        // 3) Reclaimed socket under a still-running accept task when `last_verified` has aged past
        // `LOOPBACK_PROBE_FRESHNESS`: `mint_url` detects it via probe and binds a fresh OS port.
        let dead_port_2 = {
            let probe_bind = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
            probe_bind.local_addr().unwrap().port()
        };
        {
            let mut slot = server.listener.lock().unwrap();
            if let Some(old) = slot.take() {
                old.shutdown.cancel();
            }
            let fake_shutdown = CancellationToken::new();
            let fake_shutdown_task = fake_shutdown.clone();
            let fake_accept_task = tokio::spawn(async move {
                fake_shutdown_task.cancelled().await;
            });
            *slot = Some(ListenerHandle {
                port: dead_port_2,
                shutdown: fake_shutdown,
                accept_alive: Arc::new(AtomicBool::new(true)),
                accept_task: fake_accept_task,
                probe_secret: [0xA5u8; 32],
                last_verified: Instant::now() - (LOOPBACK_PROBE_FRESHNESS + Duration::from_secs(1)),
            });
            assert!(slot.as_ref().unwrap().is_alive());
        }

        let after_mint = server
            .mint_url("tower", "/home/me/clip.bin", false)
            .await
            .unwrap();
        let new_port = server.loopback_port().unwrap();
        assert_ne!(new_port, dead_port_2);
        let new_path = after_mint
            .url
            .strip_prefix(&format!("http://127.0.0.1:{new_port}"))
            .unwrap();
        let (st2, head2, body2) = raw_http(
            new_port,
            &format!(
                "GET {new_path} HTTP/1.1\r\nHost: 127.0.0.1:{new_port}\r\nConnection: close\r\n\r\n"
            ),
        )
        .await;
        assert_eq!(st2, 200, "{head2}");
        assert_eq!(body2, payload);

        server.shutdown();
    }

    #[tokio::test]
    async fn loopback_probe_rejects_fake_204_and_hanging_squatters_without_leaking_tickets() {
        let payload = b"never-leak-ticket-to-squatter".to_vec();
        let mut files = HashMap::new();
        files.insert("/home/me/private.flac".to_owned(), payload.clone());

        let backend = Arc::new(MockContentBackend::new(files));
        let server = Arc::new(AppContentServer::with_config(
            backend,
            ContentMountMode::Loopback,
            TicketConfig::default(),
            Duration::from_secs(30),
            vec!["http://tauri.localhost".to_owned()],
        ));

        // Ensure at least one ticket exists.
        let _ = server
            .mint_url("tower", "/home/me/private.flac", false)
            .await
            .unwrap();

        // 1) Hostile squatter answering every request with a forged `204 No Content` echoing back
        // `X-Legion-Probe-Auth` as `X-Legion-Probe-Proof`.
        let fake_listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let fake_port = fake_listener.local_addr().unwrap().port();
        let seen_requests = Arc::new(Mutex::new(Vec::<String>::new()));
        let seen_clone = Arc::clone(&seen_requests);
        let fake_stop = CancellationToken::new();
        let fake_stop_task = fake_stop.clone();
        let fake_task = tokio::spawn(async move {
            loop {
                tokio::select! {
                    biased;
                    () = fake_stop_task.cancelled() => break,
                    res = fake_listener.accept() => {
                        let Ok((mut sock, _)) = res else { break };
                        let mut buf = vec![0u8; 4096];
                        if let Ok(n) = sock.read(&mut buf).await {
                            let req_text = String::from_utf8_lossy(&buf[..n]).into_owned();
                            let echoed_auth = req_text
                                .lines()
                                .find_map(|l| {
                                    let (k, v) = l.split_once(':')?;
                                    k.trim()
                                        .eq_ignore_ascii_case("x-legion-probe-auth")
                                        .then(|| v.trim().to_owned())
                                })
                                .unwrap_or_default();
                            seen_clone.lock().unwrap().push(req_text);
                            let resp = format!(
                                "HTTP/1.1 204 No Content\r\nX-Legion-Probe-Proof: {echoed_auth}\r\nConnection: close\r\n\r\n"
                            );
                            let _ = sock.write_all(resp.as_bytes()).await;
                        }
                    }
                }
            }
        });

        {
            let mut slot = server.listener.lock().unwrap();
            if let Some(old) = slot.take() {
                old.shutdown.cancel();
            }
            let fake_shutdown = CancellationToken::new();
            let fake_shutdown_task = fake_shutdown.clone();
            let fake_accept_task = tokio::spawn(async move {
                fake_shutdown_task.cancelled().await;
            });
            *slot = Some(ListenerHandle {
                port: fake_port,
                shutdown: fake_shutdown,
                accept_alive: Arc::new(AtomicBool::new(true)),
                accept_task: fake_accept_task,
                probe_secret: [0x33u8; 32],
                last_verified: Instant::now() - (LOOPBACK_PROBE_FRESHNESS + Duration::from_secs(1)),
            });
        }

        let minted = server
            .mint_url("tower", "/home/me/private.flac", false)
            .await
            .unwrap();
        let new_port = server.loopback_port().unwrap();
        assert_ne!(
            new_port, fake_port,
            "forged 204 squatter must fail proof-of-ownership and force a fresh OS port"
        );

        let ticket_id = extract_ticket_id_from_url(&minted.url).unwrap().to_owned();
        let reqs = seen_requests.lock().unwrap().clone();
        assert_eq!(reqs.len(), 1, "squatter should see only the single probe");
        assert!(
            reqs[0].starts_with(&format!("GET {LOOPBACK_PROBE_PATH} HTTP/1.1\r\n")),
            "unexpected request sent to squatter: {}",
            reqs[0]
        );
        assert!(
            !reqs[0].contains(&ticket_id),
            "squatter must never see any file ticket"
        );

        let new_path = minted
            .url
            .strip_prefix(&format!("http://127.0.0.1:{new_port}"))
            .unwrap();
        let (st, head, body) = raw_http(
            new_port,
            &format!(
                "GET {new_path} HTTP/1.1\r\nHost: 127.0.0.1:{new_port}\r\nConnection: close\r\n\r\n"
            ),
        )
        .await;
        assert_eq!(st, 200, "{head}");
        assert_eq!(body, payload);

        fake_stop.cancel();
        let _ = fake_task.await;

        // 2) Hanging/tarpit squatter that accepts TCP and sends nothing: probe times out quickly
        // and recovers onto a fresh OS-chosen port.
        let tarpit_listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let tarpit_port = tarpit_listener.local_addr().unwrap().port();
        let tarpit_stop = CancellationToken::new();
        let tarpit_stop_task = tarpit_stop.clone();
        let tarpit_task = tokio::spawn(async move {
            let mut held = Vec::new();
            loop {
                tokio::select! {
                    biased;
                    () = tarpit_stop_task.cancelled() => break,
                    res = tarpit_listener.accept() => {
                        if let Ok((sock, _)) = res {
                            held.push(sock);
                        }
                    }
                }
            }
        });

        {
            let mut slot = server.listener.lock().unwrap();
            if let Some(old) = slot.take() {
                old.shutdown.cancel();
            }
            let fake_shutdown = CancellationToken::new();
            let fake_shutdown_task = fake_shutdown.clone();
            let fake_accept_task = tokio::spawn(async move {
                fake_shutdown_task.cancelled().await;
            });
            *slot = Some(ListenerHandle {
                port: tarpit_port,
                shutdown: fake_shutdown,
                accept_alive: Arc::new(AtomicBool::new(true)),
                accept_task: fake_accept_task,
                probe_secret: [0x77u8; 32],
                last_verified: Instant::now(),
            });
        }

        let started = Instant::now();
        let recovered = server
            .probe_or_recover_loopback()
            .await
            .unwrap()
            .expect("tarpit timeout must trigger rebind");
        assert_ne!(recovered, tarpit_port);
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "tarpit probe should time out promptly (elapsed={:?})",
            started.elapsed()
        );

        let (st2, head2, body2) = raw_http(
            recovered,
            &format!(
                "GET {new_path} HTTP/1.1\r\nHost: 127.0.0.1:{recovered}\r\nConnection: close\r\n\r\n"
            ),
        )
        .await;
        assert_eq!(st2, 200, "{head2}");
        assert_eq!(body2, payload);

        tarpit_stop.cancel();
        let _ = tarpit_task.await;
        server.shutdown();
    }

    #[tokio::test]
    async fn loopback_probe_hides_endpoint_from_unauthenticated_callers_and_counts_as_unauthenticated()
     {
        let payload = b"probe-privacy-payload".to_vec();
        let mut files = HashMap::new();
        files.insert("/home/me/song.flac".to_owned(), payload.clone());

        let backend = Arc::new(MockContentBackend::new(files));
        let server = Arc::new(AppContentServer::with_config(
            backend,
            ContentMountMode::Loopback,
            TicketConfig::default(),
            Duration::from_secs(30),
            vec!["http://tauri.localhost".to_owned()],
        ));

        let _ = server
            .mint_url("tower", "/home/me/song.flac", false)
            .await
            .unwrap();
        let (port, secret) = {
            let slot = server.listener.lock().unwrap();
            let h = slot.as_ref().unwrap();
            (h.port, h.probe_secret)
        };

        // Baseline unknown-path 404.
        let (base_st, base_head, base_body) = raw_http(
            port,
            &format!(
                "GET /v1/content/nonexistent HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: keep-alive\r\n\r\n"
            ),
        )
        .await;
        assert_eq!(base_st, 404, "{base_head}");

        let challenge = [0x42u8; 32];
        let challenge_b64 = URL_SAFE_NO_PAD.encode(challenge);
        let valid_auth_b64 = URL_SAFE_NO_PAD
            .encode(probe_mac(&secret, LOOPBACK_PROBE_REQ_DOMAIN, &challenge).as_bytes());
        let wrong_auth_b64 = URL_SAFE_NO_PAD.encode([0x11u8; 32]);

        // Unauthenticated, malformed, wrong-secret, query-bearing, wrong-method, or Origin-bearing
        // calls to the probe path must return the exact same generic 404 + Connection: close.
        for req in [
            format!(
                "GET {LOOPBACK_PROBE_PATH} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: keep-alive\r\n\r\n"
            ),
            format!(
                "GET {LOOPBACK_PROBE_PATH} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n{LOOPBACK_PROBE_CHALLENGE_HEADER}: {challenge_b64}\r\nConnection: keep-alive\r\n\r\n"
            ),
            format!(
                "GET {LOOPBACK_PROBE_PATH} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n{LOOPBACK_PROBE_CHALLENGE_HEADER}: {challenge_b64}\r\n{LOOPBACK_PROBE_AUTH_HEADER}: {wrong_auth_b64}\r\nConnection: keep-alive\r\n\r\n"
            ),
            format!(
                "GET {LOOPBACK_PROBE_PATH}?x=1 HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n{LOOPBACK_PROBE_CHALLENGE_HEADER}: {challenge_b64}\r\n{LOOPBACK_PROBE_AUTH_HEADER}: {valid_auth_b64}\r\nConnection: keep-alive\r\n\r\n"
            ),
            format!(
                "POST {LOOPBACK_PROBE_PATH} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n{LOOPBACK_PROBE_CHALLENGE_HEADER}: {challenge_b64}\r\n{LOOPBACK_PROBE_AUTH_HEADER}: {valid_auth_b64}\r\nConnection: keep-alive\r\n\r\n"
            ),
            format!(
                "GET {LOOPBACK_PROBE_PATH} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nOrigin: http://tauri.localhost\r\n{LOOPBACK_PROBE_CHALLENGE_HEADER}: {challenge_b64}\r\n{LOOPBACK_PROBE_AUTH_HEADER}: {valid_auth_b64}\r\nConnection: keep-alive\r\n\r\n"
            ),
        ] {
            let (st, head, body) =
                tokio::time::timeout(Duration::from_secs(2), raw_http(port, &req))
                    .await
                    .expect("invalid probe request must close immediately");
            assert_eq!(st, 404, "expected 404 for {req}\ngot:\n{head}");
            assert_eq!(body, base_body, "probe 404 body must match generic 404");
            assert!(
                head.to_ascii_lowercase().contains("connection: close"),
                "probe 404 must emit Connection: close:\n{head}"
            );
            assert!(
                !head
                    .to_ascii_lowercase()
                    .contains(LOOPBACK_PROBE_PROOF_HEADER.as_str()),
                "404 must never leak probe proof header"
            );
        }

        // Valid probe request returns 204 with valid proof, Connection: close (closes even when
        // caller asks for keep-alive), does not log into `recent_requests`, and does NOT mark the
        // connection authenticated.
        let valid_req = format!(
            "GET {LOOPBACK_PROBE_PATH} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n{LOOPBACK_PROBE_CHALLENGE_HEADER}: {challenge_b64}\r\n{LOOPBACK_PROBE_AUTH_HEADER}: {valid_auth_b64}\r\nConnection: keep-alive\r\n\r\n"
        );
        let (ok_st, ok_head, ok_body) =
            tokio::time::timeout(Duration::from_secs(2), raw_http(port, &valid_req))
                .await
                .expect("valid probe must close immediately");
        assert_eq!(ok_st, 204, "{ok_head}");
        assert!(ok_body.is_empty());
        assert!(
            ok_head.to_ascii_lowercase().contains("connection: close"),
            "valid probe must emit Connection: close:\n{ok_head}"
        );
        assert!(
            server.recent_requests().is_empty(),
            "probe requests must not populate request_log"
        );

        // Directly confirm `handle_checked_request` leaves `conn_authenticated == false`.
        let guard = HostOriginGuard {
            port,
            allowed_hosts: Vec::new(),
            allowed_origins: vec!["http://tauri.localhost".to_owned()],
            allow_same_origin: false,
        };
        let mut headers = HeaderMap::new();
        headers.insert(
            HOST,
            HeaderValue::from_str(&format!("127.0.0.1:{port}")).unwrap(),
        );
        headers.insert(
            LOOPBACK_PROBE_CHALLENGE_HEADER,
            HeaderValue::from_str(&challenge_b64).unwrap(),
        );
        headers.insert(
            LOOPBACK_PROBE_AUTH_HEADER,
            HeaderValue::from_str(&valid_auth_b64).unwrap(),
        );
        let uri: Uri = LOOPBACK_PROBE_PATH.parse().unwrap();
        let conn_auth = AtomicBool::new(false);
        let resp = server
            .handle_checked_request(
                &guard,
                &Method::GET,
                &uri,
                &headers,
                ServeOptions {
                    corp: CorpPolicy::CrossOrigin,
                    ..ServeOptions::default()
                },
                Some(LoopbackRequestCtx {
                    conn_authenticated: &conn_auth,
                    probe_secret: &secret,
                }),
            )
            .await;
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);
        assert!(
            !conn_auth.load(Ordering::Acquire),
            "probe must count as unauthenticated for connection limits"
        );

        // Even while 32 unauthenticated partial-header sockets are held open, the probe succeeds
        // by evicting the oldest unauthenticated socket.
        let mut holders = Vec::with_capacity(MAX_LOOPBACK_CONNECTIONS);
        for _ in 0..MAX_LOOPBACK_CONNECTIONS {
            let mut sock = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
            sock.write_all(b"GET /v1/content/").await.unwrap();
            holders.push(sock);
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(
            probe_loopback_ownership(port, &secret, LOOPBACK_PROBE_TIMEOUT).await,
            "ownership probe must succeed even when 32 unauthenticated sockets are held"
        );
        drop(holders);

        server.shutdown();
    }
}
