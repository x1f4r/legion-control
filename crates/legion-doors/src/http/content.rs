//! Content endpoints mounted on the HTTP door:
//! - `POST /v1/content`: bearer-authenticated ticket creation (`{on?, path}` -> `{url, expires_in}`).
//! - `GET`/`HEAD /v1/content/{ticket}/{name}`: ticket-authenticated single-range byte streaming
//!   powered by `legion-content`.
//!
//! # Origin and resource policy for `/v1/content/{ticket}/{name}`
//!
//! The HTTP door's `Host` and `Origin` guards run on `/v1/content/{ticket}/{name}` just as they do
//! on every other route:
//! - Browser media elements (`<img>`, `<video>`, `<audio>`), `<a download>`, and top-level
//!   navigations send **no `Origin` header**; `gate::origin_allowed` admits requests with no
//!   `Origin`. Every content reply carries `Cross-Origin-Resource-Policy: same-origin`,
//!   `Referrer-Policy: no-referrer`, and
//!   `Content-Security-Policy: sandbox; default-src 'none'; frame-ancestors 'none'`.
//! - Same-origin `fetch()` / `<video crossorigin>` calls from the served UI (when `[serve] ui =
//!   true`) and cross-origin CORS requests from origins listed in `[serve] origins` are allowed
//!   (listed origins receive `Access-Control-Allow-Origin` and exposed range/ETag headers; because
//!   CORP is `same-origin`, listed origins use CORS `fetch()` or `crossorigin` rather than no-cors
//!   subresource embedding).
//! - Any request carrying an unlisted `Origin` header (including `Origin: null` from a sandboxed
//!   iframe or `data:` page) is refused with `403 Forbidden`.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::header::{CACHE_CONTROL, HeaderValue, RETRY_AFTER};
use axum::http::{Response, StatusCode};
use http_body::Body as _;
use legion_content::{
    ContentSource, ContentTime, CorpPolicy, MAX_PATH_BYTES, OpenRangeError, OpenedRange,
    ServeOptions, StreamHooks, Ticket, TicketRequest, encode_path_segment, extract_file_name,
    handle_request,
};
use legion_core::policy::{Access, Policy};
use legion_core::proto::{Effect, ErrorBody, ErrorCode};
use legion_core::tokens::Tokens;
use legion_core::{ContentError, ContentRequest, Door, Node};
use serde::Deserialize;
use serde_json::{Value, json};

use super::{ConnectionControl, Shared, read_body_limited, reply};
use crate::Authenticated;
use tokio_util::sync::CancellationToken;

/// Maximum JSON request body accepted by `POST /v1/content` (16 KiB).
pub(crate) const MAX_CREATE_BODY: usize = 16 * 1024;

/// Coalescing window for successful `content.read` audit lines per ticket.
pub(crate) const DEFAULT_AUDIT_COALESCE_WINDOW: Duration = Duration::from_secs(60);

/// The JSON body of `POST /v1/content`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CreateContentBody {
    #[serde(default)]
    on: Option<Value>,
    path: String,
}

struct TicketAuditBucket {
    machine: String,
    principal: String,
    path: String,
    requests: u64,
    bytes_served: Arc<AtomicU64>,
    flushed_requests: u64,
    flushed_bytes: u64,
    last_flushed: Instant,
}

struct AuditSnapshot {
    machine: String,
    principal: String,
    path: String,
    requests: u64,
    bytes_served: u64,
    started: Instant,
}

/// Coalesces successful `content.read` audit records per ticket to at most one entry per ticket
/// per coalescing window, flushing remaining deltas when tickets expire, are evicted, are revoked,
/// or when the door shuts down.
pub(crate) struct ContentAuditCoalescer {
    window: Duration,
    buckets: Mutex<HashMap<[u8; 32], TicketAuditBucket>>,
}

impl Default for ContentAuditCoalescer {
    fn default() -> Self {
        Self::with_window(DEFAULT_AUDIT_COALESCE_WINDOW)
    }
}

impl ContentAuditCoalescer {
    pub(crate) fn with_window(window: Duration) -> Self {
        Self {
            window,
            buckets: Mutex::new(HashMap::new()),
        }
    }

    /// Registers one successful range open for `ticket` and returns `(bytes_counter, is_first_use)`.
    fn record_open(&self, ticket: &Ticket) -> (Arc<AtomicU64>, bool) {
        let mut buckets = self.buckets.lock().unwrap_or_else(PoisonError::into_inner);
        let is_first_use = !buckets.contains_key(&ticket.key_hash);
        let bucket = buckets
            .entry(ticket.key_hash)
            .or_insert_with(|| TicketAuditBucket {
                machine: ticket.machine.clone(),
                principal: ticket.principal.clone(),
                path: ticket.path.clone(),
                requests: 0,
                bytes_served: Arc::new(AtomicU64::new(0)),
                flushed_requests: 0,
                flushed_bytes: 0,
                last_flushed: Instant::now(),
            });
        bucket.requests = bucket.requests.saturating_add(1);
        (Arc::clone(&bucket.bytes_served), is_first_use)
    }

    /// Flushes any bucket whose coalescing window has elapsed.
    pub(crate) async fn flush_due(&self, node: &Node) {
        let snapshots = {
            let mut buckets = self.buckets.lock().unwrap_or_else(PoisonError::into_inner);
            let now = Instant::now();
            let mut out = Vec::new();
            for bucket in buckets.values_mut() {
                if now.saturating_duration_since(bucket.last_flushed) < self.window {
                    continue;
                }
                if let Some(snap) = take_delta(bucket, now) {
                    out.push(snap);
                }
            }
            out
        };
        write_snapshots(node, snapshots).await;
    }

    /// Removes and flushes the buckets for specific evicted/revoked ticket hashes.
    pub(crate) async fn flush_removed(&self, node: &Node, hashes: &[[u8; 32]]) {
        if hashes.is_empty() {
            return;
        }
        let snapshots = {
            let mut buckets = self.buckets.lock().unwrap_or_else(PoisonError::into_inner);
            let now = Instant::now();
            let mut out = Vec::new();
            for hash in hashes {
                if let Some(mut bucket) = buckets.remove(hash)
                    && let Some(snap) = take_delta(&mut bucket, now)
                {
                    out.push(snap);
                }
            }
            out
        };
        write_snapshots(node, snapshots).await;
    }

    /// Removes and flushes any bucket whose ticket is no longer present in the ticket table (for
    /// example tickets evicted by the per-holder LRU cap or revoked during an in-flight stream).
    pub(crate) async fn flush_missing(&self, node: &Node, is_present: impl Fn(&[u8; 32]) -> bool) {
        let snapshots = {
            let mut buckets = self.buckets.lock().unwrap_or_else(PoisonError::into_inner);
            let now = Instant::now();
            let missing: Vec<[u8; 32]> = buckets
                .keys()
                .copied()
                .filter(|hash| !is_present(hash))
                .collect();
            let mut out = Vec::new();
            for hash in missing {
                if let Some(mut bucket) = buckets.remove(&hash)
                    && let Some(snap) = take_delta(&mut bucket, now)
                {
                    out.push(snap);
                }
            }
            out
        };
        write_snapshots(node, snapshots).await;
    }

    /// Flushes every remaining bucket (called on door shutdown).
    pub(crate) async fn flush_all(&self, node: &Node) {
        let snapshots = {
            let mut buckets = self.buckets.lock().unwrap_or_else(PoisonError::into_inner);
            let now = Instant::now();
            let mut out = Vec::new();
            for (_, mut bucket) in buckets.drain() {
                if let Some(snap) = take_delta(&mut bucket, now) {
                    out.push(snap);
                }
            }
            out
        };
        write_snapshots(node, snapshots).await;
    }
}

fn take_delta(bucket: &mut TicketAuditBucket, now: Instant) -> Option<AuditSnapshot> {
    let total_bytes = bucket.bytes_served.load(Ordering::Relaxed);
    let delta_requests = bucket.requests.saturating_sub(bucket.flushed_requests);
    let delta_bytes = total_bytes.saturating_sub(bucket.flushed_bytes);
    if delta_requests == 0 && delta_bytes == 0 {
        return None;
    }
    let started = bucket.last_flushed;
    bucket.flushed_requests = bucket.requests;
    bucket.flushed_bytes = total_bytes;
    bucket.last_flushed = now;
    Some(AuditSnapshot {
        machine: bucket.machine.clone(),
        principal: bucket.principal.clone(),
        path: bucket.path.clone(),
        requests: delta_requests,
        bytes_served: delta_bytes,
        started,
    })
}

async fn write_snapshots(node: &Node, snapshots: Vec<AuditSnapshot>) {
    for snap in snapshots {
        let args = json!({
            "path": snap.path,
            "requests": snap.requests,
            "bytes_served": snap.bytes_served,
        });
        node.record_audit(
            snap.machine,
            Some(snap.principal),
            Door::Http,
            "content.read".to_owned(),
            &args,
            snap.started,
            &Ok(Value::Null),
        )
        .await;
    }
}

/// Builds the opaque ticket holder identifier from an authenticated bearer token.
///
/// Includes both the token's name and its stored hash so removing a token OR replacing a token
/// under the same name immediately invalidates every ticket issued under the previous token.
pub(crate) fn token_holder(who: &Authenticated) -> String {
    format!("{}:{}", who.token, who.hash)
}

/// Returns `true` when `holder` still corresponds to an active, non-expired token with the exact
/// same name and hash in `tokens`. When `tokens` is `None` (unreadable `tokens.toml`), returns
/// `false` so every ticket fails closed.
pub(crate) fn holder_is_live(
    holder: &str,
    principal: Option<&str>,
    tokens: Option<&Tokens>,
    now: jiff::Timestamp,
) -> bool {
    let Some((token_name, token_hash)) = holder.split_once(':') else {
        return false;
    };
    tokens
        .and_then(|t| t.tokens.get(token_name))
        .is_some_and(|entry| {
            entry.hash == token_hash
                && principal.is_none_or(|p| entry.principal.as_str() == p)
                && !entry.expired(now)
        })
}

/// `POST /v1/content`: verifies `fs.read` permission and file accessibility for the target
/// machine and path, then issues a 256-bit random content ticket bound to the caller's token,
/// principal, machine, and path, recording one `content.ticket` audit line (never logging the
/// ticket secret).
pub(crate) async fn create(State(shared): State<Arc<Shared>>, request: Request) -> Response<Body> {
    let started = Instant::now();
    let Some(who) = request.extensions().get::<Authenticated>().cloned() else {
        return reply::error(&ErrorBody::new(
            ErrorCode::Internal,
            "The request reached POST /v1/content without a principal.",
        ));
    };
    let bytes = match read_body_limited(request.into_body(), MAX_CREATE_BODY).await {
        Ok(bytes) => bytes,
        Err(response) => return *response,
    };
    let body: CreateContentBody = match serde_json::from_slice(&bytes) {
        Ok(body) => body,
        Err(error) => {
            return reply::error(
                &ErrorBody::new(
                    ErrorCode::InvalidArgs,
                    format!("The request body is not a content request: {error}."),
                )
                .with_hint(
                    "Send a JSON object such as {\"path\": \"~/photos/cat.jpg\", \"on\": \"tower\"}.",
                ),
            );
        }
    };

    let target_machine = match parse_single_machine(body.on, &shared.node.name()) {
        Ok(machine) => machine,
        Err(error) => return reply::error(&error),
    };
    if body.path.is_empty() {
        return reply::error(
            &ErrorBody::new(ErrorCode::InvalidArgs, "path cannot be empty.")
                .with_hint("Pass the full path to a file, absolute or starting with ~."),
        );
    }
    if body.path.len() > MAX_PATH_BYTES {
        return reply::error(
            &ErrorBody::new(
                ErrorCode::InvalidArgs,
                format!("path is longer than the {MAX_PATH_BYTES}-byte limit."),
            )
            .with_hint("Pass a shorter file path."),
        );
    }
    // Validate the basename before probing any filesystem or remote machine.
    if let Err(error) = extract_file_name(&body.path) {
        return reply::error(&error);
    }

    let audit_args = json!({ "path": body.path });

    let holder = token_holder(&who);

    // Check `fs.read` permission, path confinement (roots, config/state, secrets, symlinks),
    // regular-file type, and remote reachability right now by probing zero bytes.
    let probe_req = ContentRequest {
        machine: Some(target_machine.clone()),
        path: body.path.clone(),
        offset: 0,
        length: Some(0),
        principal: who.principal.clone(),
        holder: Some(holder.clone()),
        door: Door::Http,
        audit: false,
        cancel: shared.link.shutdown.child_token(),
    };
    let probe_result = match shared.router.open_content(&shared.node, probe_req).await {
        Ok(_opened) => Ok(()),
        Err(ContentError::OutOfRange { .. }) => Err((
            None,
            ErrorBody::new(
                ErrorCode::Internal,
                "Unexpected out-of-range error while probing file.",
            ),
        )),
        Err(ContentError::Busy {
            retry_after_secs,
            error,
        }) => Err((Some(retry_after_secs), error)),
        Err(ContentError::Error(error)) => Err((None, error)),
    };
    if let Err((retry_after, error)) = probe_result {
        shared
            .node
            .record_audit(
                target_machine,
                Some(who.principal),
                Door::Http,
                "content.ticket".to_owned(),
                &audit_args,
                started,
                &Err(error.clone()),
            )
            .await;
        if let Some(secs) = retry_after {
            let mut response = reply::json(StatusCode::SERVICE_UNAVAILABLE, &error);
            response
                .headers_mut()
                .insert(RETRY_AFTER, HeaderValue::from(secs.max(1)));
            return response;
        }
        return reply::error(&error);
    }

    let issued = match shared.content_tickets.issue(TicketRequest {
        holder,
        principal: who.principal.clone(),
        machine: target_machine.clone(),
        path: body.path,
    }) {
        Ok(issued) => issued,
        Err(error) => {
            shared
                .node
                .record_audit(
                    target_machine,
                    Some(who.principal),
                    Door::Http,
                    "content.ticket".to_owned(),
                    &audit_args,
                    started,
                    &Err(error.clone()),
                )
                .await;
            let status = if error.code == ErrorCode::Busy {
                StatusCode::SERVICE_UNAVAILABLE
            } else {
                reply::status_for(error.code)
            };
            return reply::json(status, &error);
        }
    };
    // Flush any coalesced read bucket whose ticket was evicted by the per-holder LRU cap.
    shared
        .content_audit
        .flush_missing(&shared.node, |hash| {
            shared.content_tickets.contains_hash(hash)
        })
        .await;

    shared
        .node
        .record_audit(
            target_machine,
            Some(who.principal),
            Door::Http,
            "content.ticket".to_owned(),
            &audit_args,
            started,
            &Ok(Value::Null),
        )
        .await;

    let url = format!(
        "/v1/content/{}/{}",
        issued.id,
        encode_path_segment(&issued.file_name)
    );
    let mut response = reply::json(
        StatusCode::OK,
        &json!({
            "url": url,
            "expires_in": issued.expires_in,
        }),
    );
    response
        .headers_mut()
        .insert(CACHE_CONTROL, HeaderValue::from_static("private, no-store"));
    response
}

/// RAII guard that activates per-connection write-stall, minimum-rate, and send-buffer limits
/// while a content response body is streaming, and stops the independent revocation watcher when
/// the response ends.
struct ActiveContentStreamGuard {
    conn_active: Option<Arc<AtomicUsize>>,
    stop_watcher: CancellationToken,
}

impl ActiveContentStreamGuard {
    fn new(flag: Option<Arc<AtomicUsize>>, stop_watcher: CancellationToken) -> Self {
        if let Some(ref counter) = flag {
            counter.fetch_add(1, Ordering::Release);
        }
        Self {
            conn_active: flag,
            stop_watcher,
        }
    }
}

impl Drop for ActiveContentStreamGuard {
    fn drop(&mut self) {
        self.stop_watcher.cancel();
        if let Some(ref counter) = self.conn_active {
            counter.fetch_sub(1, Ordering::Release);
        }
    }
}

/// `GET` / `HEAD /v1/content/{ticket}/{name}`: streams file bytes with single-range support.
///
/// Reads the raw percent-encoded path segments directly from `request.uri().path()` so filenames
/// containing `%` (such as `100%.txt` or `a%41.txt`) are decoded exactly once by
/// `legion-content` instead of being double-decoded by Axum's `Path` extractor.
pub(crate) async fn serve(State(shared): State<Arc<Shared>>, request: Request) -> Response<Body> {
    let raw_path = request.uri().path().to_owned();
    let Some((ticket_id, raw_name)) = extract_raw_content_segments(&raw_path) else {
        return reply::json(
            StatusCode::NOT_FOUND,
            &ErrorBody::new(
                ErrorCode::NotFound,
                "This content link is invalid or has expired.",
            ),
        );
    };

    let open_slot: OpenAuditSlot = Arc::new(Mutex::new(None));
    let source = Arc::new(DoorContentSource {
        shared: Arc::clone(&shared),
        open_slot: Arc::clone(&open_slot),
    });

    let conn_ctrl = request.extensions().get::<ConnectionControl>().cloned();
    let stop_watcher = CancellationToken::new();

    let hooks = StreamHooks {
        limiter: Some(shared.content_limiter.clone()),
        bytes_served: None,
        active_guard: Some(Box::new(ActiveContentStreamGuard::new(
            conn_ctrl.as_ref().map(|c| Arc::clone(&c.active)),
            stop_watcher.clone(),
        ))),
    };

    let response = handle_request(
        &shared.content_tickets,
        &source,
        request.method(),
        ticket_id,
        raw_name,
        request.uri().query(),
        request.headers(),
        ServeOptions {
            corp: CorpPolicy::SameOrigin,
            liveness_check_interval: shared.content_liveness_interval,
            ..ServeOptions::default()
        },
        hooks,
    )
    .await;

    // Wrap the streaming body with the ticket's coalesced byte counter and start an independent
    // revocation watcher that aborts the TCP connection (TCP RST) even when a slow or stalled
    // reader prevents body polling.
    let opened = open_slot
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .take();
    if let Some((ticket, counter)) = opened {
        if let Some(ctrl) = conn_ctrl
            && response.status().is_success()
            && !response.body().is_end_stream()
        {
            let watch_shared = Arc::clone(&shared);
            let watch_source = Arc::clone(&source);
            let watch_interval = shared
                .content_liveness_interval
                .max(Duration::from_millis(50));
            tokio::spawn(async move {
                loop {
                    tokio::select! {
                        biased;
                        () = stop_watcher.cancelled() => break,
                        () = watch_shared.link.shutdown.cancelled() => break,
                        () = tokio::time::sleep(watch_interval) => {
                            let live = watch_shared.content_tickets.is_live(&ticket.key_hash)
                                && watch_source.check_live(&ticket).is_ok();
                            if !live {
                                let removed = watch_shared
                                    .content_tickets
                                    .revoke_holder_hashes(&ticket.holder);
                                watch_shared
                                    .content_audit
                                    .flush_removed(&watch_shared.node, &removed)
                                    .await;
                                ctrl.abort.cancel();
                                break;
                            }
                        }
                    }
                }
            });
        }
        response.map(|body| {
            Body::new(AuditCountingBody {
                inner: body,
                counter,
            })
        })
    } else {
        stop_watcher.cancel();
        response.map(Body::new)
    }
}

struct AuditCountingBody {
    inner: legion_content::ContentBody,
    counter: Arc<AtomicU64>,
}

impl http_body::Body for AuditCountingBody {
    type Data = axum::body::Bytes;
    type Error = std::io::Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Result<http_body::Frame<Self::Data>, Self::Error>>> {
        let poll = Pin::new(&mut self.inner).poll_frame(cx);
        if let std::task::Poll::Ready(Some(Ok(ref frame))) = poll
            && let Some(data) = frame.data_ref()
        {
            self.counter.fetch_add(data.len() as u64, Ordering::Relaxed);
        }
        poll
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> http_body::SizeHint {
        self.inner.size_hint()
    }
}

type OpenAuditSlot = Arc<Mutex<Option<(Ticket, Arc<AtomicU64>)>>>;

struct DoorContentSource {
    shared: Arc<Shared>,
    open_slot: OpenAuditSlot,
}

impl ContentSource for DoorContentSource {
    fn open_range<'a>(
        &'a self,
        ticket: &'a Ticket,
        offset: u64,
        length: Option<u64>,
    ) -> Pin<Box<dyn Future<Output = Result<OpenedRange, OpenRangeError>> + Send + 'a>> {
        Box::pin(async move {
            let started = Instant::now();
            let tokens = self.shared.node.config().get::<Tokens>().ok();
            let now = jiff::Timestamp::now();
            if !holder_is_live(
                &ticket.holder,
                Some(&ticket.principal),
                tokens.as_deref(),
                now,
            ) {
                // Revoke all tickets for this holder and flush their coalesced read summaries
                // immediately so revoked activity is never stranded in memory.
                let removed = self
                    .shared
                    .content_tickets
                    .revoke_holder_hashes(&ticket.holder);
                self.shared
                    .content_audit
                    .flush_removed(&self.shared.node, &removed)
                    .await;
                return Err(OpenRangeError::TicketRevoked);
            }
            let req = ContentRequest {
                machine: Some(ticket.machine.clone()),
                path: ticket.path.clone(),
                offset,
                length,
                principal: ticket.principal.clone(),
                holder: Some(ticket.holder.clone()),
                door: Door::Http,
                audit: false,
                cancel: self.shared.link.shutdown.child_token(),
            };
            match self
                .shared
                .router
                .open_content(&self.shared.node, req)
                .await
            {
                Ok(opened) => {
                    let (counter, first_use) = self.shared.content_audit.record_open(ticket);
                    if first_use {
                        // Write one durable audit line immediately when a ticket is first used so
                        // an access is always on record even if the door is killed before the
                        // coalesced summary flushes.
                        let args = json!({ "path": ticket.path });
                        self.shared
                            .node
                            .record_audit(
                                ticket.machine.clone(),
                                Some(ticket.principal.clone()),
                                Door::Http,
                                "content.open".to_owned(),
                                &args,
                                started,
                                &Ok(Value::Null),
                            )
                            .await;
                    }
                    *self
                        .open_slot
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner) = Some((ticket.clone(), counter));
                    Ok(OpenedRange {
                        size: opened.size,
                        offset: opened.offset,
                        length: opened.length,
                        mtime: ContentTime::new(opened.mtime_secs, opened.mtime_nanos),
                        file_id: opened.file_id,
                        reader: opened.reader,
                    })
                }
                Err(ContentError::OutOfRange { size }) => Err(OpenRangeError::OutOfRange { size }),
                Err(ContentError::Busy {
                    retry_after_secs,
                    error,
                }) => {
                    let args = json!({
                        "path": ticket.path,
                        "offset": offset,
                        "length": length,
                    });
                    self.shared
                        .node
                        .record_audit(
                            ticket.machine.clone(),
                            Some(ticket.principal.clone()),
                            Door::Http,
                            "content.read".to_owned(),
                            &args,
                            started,
                            &Err(error.clone()),
                        )
                        .await;
                    Err(OpenRangeError::Busy {
                        retry_after_secs,
                        error: sanitize_holder_error(&error),
                    })
                }
                Err(ContentError::Error(body)) => {
                    // Record denied/failed content reads immediately in the server audit log.
                    let args = json!({
                        "path": ticket.path,
                        "offset": offset,
                        "length": length,
                    });
                    self.shared
                        .node
                        .record_audit(
                            ticket.machine.clone(),
                            Some(ticket.principal.clone()),
                            Door::Http,
                            "content.read".to_owned(),
                            &args,
                            started,
                            &Err(body.clone()),
                        )
                        .await;
                    Err(OpenRangeError::Error(sanitize_holder_error(&body)))
                }
            }
        })
    }

    fn check_live(&self, ticket: &Ticket) -> Result<(), OpenRangeError> {
        let tokens = self.shared.node.config().get::<Tokens>().ok();
        let now = jiff::Timestamp::now();
        if !holder_is_live(
            &ticket.holder,
            Some(&ticket.principal),
            tokens.as_deref(),
            now,
        ) {
            return Err(OpenRangeError::TicketRevoked);
        }
        let policy = self.shared.node.config().get::<Policy>().map_err(|_| {
            OpenRangeError::Error(ErrorBody::new(
                ErrorCode::Denied,
                "Access to this file was denied.",
            ))
        })?;
        let grant = policy
            .authorize_name(
                &ticket.principal,
                "fs.read",
                Some(Effect::Read),
                self.shared.node.paths(),
            )
            .map_err(|_| {
                OpenRangeError::Error(ErrorBody::new(
                    ErrorCode::Denied,
                    "Access to this file was denied.",
                ))
            })?;
        if ticket.machine == self.shared.node.name()
            && grant.check_path(&ticket.path, Access::Read).is_err()
        {
            return Err(OpenRangeError::Error(ErrorBody::new(
                ErrorCode::Denied,
                "Access to this file was denied.",
            )));
        }
        Ok(())
    }
}

/// Strips resolved absolute paths, remote stderr snippets, and internal hints from error replies
/// returned to ticket holders on `GET`/`HEAD /v1/content/{ticket}/{name}`.
fn sanitize_holder_error(err: &ErrorBody) -> ErrorBody {
    let message = match err.code {
        ErrorCode::Denied => "Access to this file was denied.",
        ErrorCode::NotFound | ErrorCode::UnknownTool => "The requested file could not be found.",
        ErrorCode::Exists | ErrorCode::Conflict => {
            "The requested path is not a readable regular file."
        }
        ErrorCode::Unreachable => "The machine holding this file could not be reached.",
        ErrorCode::Timeout => "Reading this file timed out.",
        ErrorCode::Cancelled => "The file read was cancelled.",
        ErrorCode::InvalidArgs => "The requested file path is not valid.",
        ErrorCode::Unsupported => "This machine does not support content streaming.",
        ErrorCode::Busy => "The server is busy reading this file.",
        ErrorCode::Failed | ErrorCode::Internal => "The requested file could not be read.",
    };
    ErrorBody::new(err.code, message)
}

fn parse_single_machine(on: Option<Value>, local_name: &str) -> Result<String, ErrorBody> {
    match on {
        None | Some(Value::Null) => Ok(local_name.to_owned()),
        Some(Value::String(name)) => {
            let trimmed = name.trim();
            if trimmed.is_empty() {
                return Ok(local_name.to_owned());
            }
            if trimmed.eq_ignore_ascii_case("all") || trimmed.contains(',') {
                return Err(ErrorBody::new(
                    ErrorCode::InvalidArgs,
                    "on must name a single machine for a content link.",
                )
                .with_hint(
                    "Pass one machine name (such as \"tower\"), or omit on for this machine.",
                ));
            }
            Ok(trimmed.to_owned())
        }
        Some(_) => Err(ErrorBody::new(
            ErrorCode::InvalidArgs,
            "on must be a single machine name string.",
        )
        .with_hint("Pass one machine name (such as \"tower\"), or omit on for this machine.")),
    }
}

/// Extracts the raw (still percent-encoded) `(ticket, name)` segments from
/// `/v1/content/<ticket>/<name>`.
pub(crate) fn extract_raw_content_segments(path: &str) -> Option<(&str, &str)> {
    let rest = path.strip_prefix("/v1/content/")?;
    let (ticket, name) = rest.split_once('/')?;
    if ticket.is_empty() || name.is_empty() || name.contains('/') {
        return None;
    }
    Some((ticket, name))
}

/// Whether `path` targets the ticket-authenticated content read endpoint
/// (`/v1/content/<ticket>/<name>`), strictly two non-empty segments under `/v1/content/`.
pub(crate) fn is_content_read_path(path: &str) -> bool {
    extract_raw_content_segments(path).is_some()
}

/// Redacts the ticket segment from `/v1/content/<ticket>/...` before writing request logs to
/// stderr so bearer-equivalent content tickets never reach log files.
pub(crate) fn redact_log_path(path: &str) -> String {
    if let Some(rest) = path.strip_prefix("/v1/content/") {
        return match rest.split_once('/') {
            Some((_, name)) => format!("/v1/content/<redacted>/{name}"),
            None => "/v1/content/<redacted>".to_owned(),
        };
    }
    path.to_owned()
}
