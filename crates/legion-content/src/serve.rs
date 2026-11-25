//! HTTP `GET`/`HEAD` single-range serving over [`ContentSource`].

use std::any::Any;
use std::future::Future;
use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::time::Duration;

use http::header::{
    ACCEPT_RANGES, ALLOW, CACHE_CONTROL, CONNECTION, CONTENT_DISPOSITION, CONTENT_LENGTH,
    CONTENT_RANGE, CONTENT_SECURITY_POLICY, CONTENT_TYPE, ETAG, HeaderName, HeaderValue, IF_RANGE,
    RANGE, REFERRER_POLICY, RETRY_AFTER, X_CONTENT_TYPE_OPTIONS,
};
use http::{HeaderMap, Method, Response, StatusCode};
use legion_proto::{ErrorBody, ErrorCode};

use crate::body::{
    BoxAsyncRead, ContentBody, LIVENESS_CHECK_BYTES, LIVENESS_CHECK_INTERVAL, LiveCheck,
};
use crate::limit::{ContentLimiter, LimitExceeded, LimitScope, StreamPermit};
use crate::media::{CONTENT_SANDBOX_CSP, classify_filename, content_disposition_attachment};
use crate::range::{
    ContentTime, ParsedRange, RangeDecision, ResolvedRange, etag, if_range_matches, parse_range,
    resolve_range,
};
use crate::tickets::{Ticket, TicketTable, decode_path_segment};

const CROSS_ORIGIN_RESOURCE_POLICY: HeaderName =
    HeaderName::from_static("cross-origin-resource-policy");

/// Value emitted in `Cross-Origin-Resource-Policy` on every content reply.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CorpPolicy {
    /// `same-origin`: used by the HTTP door where the interface and content share an origin.
    #[default]
    SameOrigin,
    /// `cross-origin`: used when a native app serves content from loopback to a web view on
    /// another origin.
    CrossOrigin,
}

impl CorpPolicy {
    fn as_header_value(self) -> HeaderValue {
        match self {
            CorpPolicy::SameOrigin => HeaderValue::from_static("same-origin"),
            CorpPolicy::CrossOrigin => HeaderValue::from_static("cross-origin"),
        }
    }
}

/// Options controlling response headers and in-flight liveness checks emitted by
/// [`handle_request`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ServeOptions {
    /// Policy emitted in `Cross-Origin-Resource-Policy`.
    pub corp: CorpPolicy,
    /// Byte interval between in-flight revocation and hard-TTL checks while streaming.
    pub liveness_check_bytes: u64,
    /// Time interval between in-flight revocation and hard-TTL checks while streaming.
    pub liveness_check_interval: Duration,
}

impl Default for ServeOptions {
    fn default() -> Self {
        Self {
            corp: CorpPolicy::SameOrigin,
            liveness_check_bytes: LIVENESS_CHECK_BYTES,
            liveness_check_interval: LIVENESS_CHECK_INTERVAL,
        }
    }
}

/// Per-request hooks for concurrency limits, byte accounting, and connection-level RAII guards.
#[derive(Default)]
pub struct StreamHooks {
    /// Optional concurrency limiter enforcing per-ticket, per-holder, and global active stream
    /// caps (`429 Too Many Requests` + `Retry-After` + `Connection: close` when exceeded).
    pub limiter: Option<ContentLimiter>,
    /// Optional counter incremented as response body bytes are yielded to the HTTP transport.
    pub bytes_served: Option<Arc<AtomicU64>>,
    /// Optional caller-supplied RAII guard kept alive for as long as a streaming body is active.
    pub active_guard: Option<Box<dyn Any + Send + Sync>>,
}

/// Metadata and byte stream returned by [`ContentSource::open_range`].
pub struct OpenedRange {
    /// Total file size in bytes when opened.
    pub size: u64,
    /// Starting byte offset of `reader`.
    pub offset: u64,
    /// Exact number of bytes `reader` will yield.
    pub length: u64,
    /// File modification timestamp used to build the strong `ETag`.
    pub mtime: ContentTime,
    /// Optional opaque file identity (such as a hash of `(dev, ino)` on Unix) folded into the
    /// strong `ETag`.
    pub file_id: Option<u64>,
    /// Async byte reader yielding `length` bytes with backpressure.
    pub reader: BoxAsyncRead,
}

/// Failure returned by [`ContentSource::open_range`] or [`ContentSource::check_live`].
#[derive(Debug)]
pub enum OpenRangeError {
    /// Requested `offset` exceeds the file's current `size`.
    OutOfRange {
        /// Current file size in bytes (used for `Content-Range: bytes */<size>`).
        size: u64,
    },
    /// The ticket's backing credential (token or pairing) is no longer valid; the ticket table
    /// revokes the holder and answers `404 Not Found`.
    TicketRevoked,
    /// Any other structured Legion error (for example `denied`, `not_found`, `unreachable`).
    Error(ErrorBody),
}

/// Backing byte-range opener mounted by [`handle_request`].
///
/// Implementations are responsible for checking that the ticket's credential is still valid,
/// re-checking `fs.read` permission on every use, recording audit entries, and opening the
/// requested byte range with backpressure.
pub trait ContentSource: Send + Sync + 'static {
    /// Opens `[offset, offset + length)` (or `[offset, size)` when `length` is `None`) for
    /// `ticket`.
    ///
    /// A `length` of `Some(0)` with `offset == 0` is used to inspect `size` and `mtime` without
    /// transferring file bytes (for `HEAD`, suffix ranges, and `If-Range` probes).
    fn open_range<'a>(
        &'a self,
        ticket: &'a Ticket,
        offset: u64,
        length: Option<u64>,
    ) -> Pin<Box<dyn Future<Output = Result<OpenedRange, OpenRangeError>> + Send + 'a>>;

    /// Fast synchronous liveness check run periodically while a body is actively streaming.
    ///
    /// Returning `Err(_)` immediately aborts the in-progress HTTP response. Implementations should
    /// re-verify that the holder's credential is still active and `fs.read` policy still permits
    /// the read so revoking a token or policy cuts off active downloads.
    fn check_live(&self, _ticket: &Ticket) -> Result<(), OpenRangeError> {
        Ok(())
    }
}

/// Maps a [`legion_proto::ErrorCode`] to its HTTP status code.
pub fn status_for_error(code: ErrorCode) -> StatusCode {
    match code {
        ErrorCode::InvalidArgs => StatusCode::BAD_REQUEST,
        ErrorCode::Denied => StatusCode::FORBIDDEN,
        ErrorCode::NotFound | ErrorCode::UnknownTool => StatusCode::NOT_FOUND,
        ErrorCode::Exists | ErrorCode::Conflict => StatusCode::CONFLICT,
        ErrorCode::Busy => StatusCode::LOCKED,
        ErrorCode::Unsupported => StatusCode::NOT_IMPLEMENTED,
        ErrorCode::Unreachable => StatusCode::BAD_GATEWAY,
        ErrorCode::Timeout => StatusCode::GATEWAY_TIMEOUT,
        ErrorCode::Cancelled => StatusCode::from_u16(499).unwrap_or(StatusCode::BAD_REQUEST),
        ErrorCode::Failed | ErrorCode::Internal => StatusCode::INTERNAL_SERVER_ERROR,
    }
}

/// Answers one `GET` or `HEAD` request for `/v1/content/<ticket>/<file name>`.
#[allow(clippy::too_many_arguments)]
pub async fn handle_request<S: ContentSource>(
    tickets: &Arc<TicketTable>,
    source: &Arc<S>,
    method: &Method,
    ticket_id: &str,
    url_file_name: &str,
    query: Option<&str>,
    headers: &HeaderMap,
    options: ServeOptions,
    hooks: StreamHooks,
) -> Response<ContentBody> {
    let head_only = match *method {
        Method::GET => false,
        Method::HEAD => true,
        _ => {
            let mut resp = json_error(
                StatusCode::METHOD_NOT_ALLOWED,
                &ErrorBody::new(
                    ErrorCode::Unsupported,
                    "Content addresses accept only GET and HEAD.",
                ),
                options,
            );
            resp.headers_mut()
                .insert(ALLOW, HeaderValue::from_static("GET, HEAD"));
            return resp;
        }
    };

    let Some(ticket) = tickets.lookup(ticket_id) else {
        return ticket_not_found(options);
    };

    let Some(decoded_name) = decode_path_segment(url_file_name) else {
        return ticket_not_found(options);
    };
    if decoded_name != ticket.file_name {
        return ticket_not_found(options);
    }

    let download = wants_download(query);
    let parsed_range = match headers.get(RANGE) {
        None => ParsedRange::None,
        Some(val) => match val.to_str() {
            Ok(text) => parse_range(text),
            Err(_) => ParsedRange::Invalid,
        },
    };
    let if_range = match headers.get(IF_RANGE) {
        None => None,
        Some(val) => Some(val.to_str().unwrap_or("")),
    };

    if head_only {
        return serve_head(
            tickets,
            source.as_ref(),
            &ticket,
            parsed_range,
            if_range,
            download,
            options,
        )
        .await;
    }

    let permit = match hooks
        .limiter
        .as_ref()
        .map(|limiter| limiter.try_acquire(ticket.key_hash, &ticket.holder))
    {
        None => None,
        Some(Ok(permit)) => Some(permit),
        Some(Err(exceeded)) => return too_many_requests(exceeded, options),
    };

    let stream_ctx = StreamContext {
        tickets: Arc::clone(tickets),
        source: Arc::clone(source),
        ticket: ticket.clone(),
        options,
        bytes_served: hooks.bytes_served,
        permit,
        active_guard: hooks.active_guard,
    };

    serve_get(
        tickets,
        source.as_ref(),
        &ticket,
        parsed_range,
        if_range,
        download,
        options,
        stream_ctx,
    )
    .await
}

struct StreamContext<S: ContentSource> {
    tickets: Arc<TicketTable>,
    source: Arc<S>,
    ticket: Ticket,
    options: ServeOptions,
    bytes_served: Option<Arc<AtomicU64>>,
    permit: Option<StreamPermit>,
    active_guard: Option<Box<dyn Any + Send + Sync>>,
}

impl<S: ContentSource> StreamContext<S> {
    fn into_body(self, reader: BoxAsyncRead, length: u64) -> ContentBody {
        if length == 0 {
            return ContentBody::empty();
        }
        let tickets = self.tickets;
        let source = self.source;
        let ticket = self.ticket;
        let live_check: LiveCheck = Arc::new(move || {
            if !tickets.is_live(&ticket.key_hash) {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "content ticket expired or was revoked",
                ));
            }
            if let Err(err) = source.check_live(&ticket) {
                if matches!(err, OpenRangeError::TicketRevoked) {
                    tickets.revoke_holder(&ticket.holder);
                }
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "content access was revoked while streaming",
                ));
            }
            Ok(())
        });
        let mut guards: Vec<Box<dyn Any + Send + Sync>> = Vec::with_capacity(2);
        if let Some(permit) = self.permit {
            guards.push(Box::new(permit));
        }
        if let Some(active) = self.active_guard {
            guards.push(active);
        }
        ContentBody::stream_with_controls(
            reader,
            length,
            Some(live_check),
            self.options.liveness_check_bytes,
            self.options.liveness_check_interval,
            self.bytes_served,
            guards,
        )
    }
}

/// Validates that an [`OpenedRange`] returned by a [`ContentSource`] matches the requested offset
/// and clamped length without integer overflow.
fn validate_opened_range(
    opened: &OpenedRange,
    requested_offset: u64,
    requested_length: Option<u64>,
) -> Result<(), ErrorBody> {
    if opened.offset != requested_offset || opened.offset > opened.size {
        return Err(ErrorBody::new(
            ErrorCode::Failed,
            "Content source returned an invalid byte offset.",
        ));
    }
    let available = opened.size - opened.offset;
    let expected = requested_length.map_or(available, |n| n.min(available));
    if opened.length != expected {
        return Err(ErrorBody::new(
            ErrorCode::Failed,
            "Content source returned an unexpected byte length.",
        ));
    }
    if opened.length > 0 {
        let Some(end) = opened
            .offset
            .checked_add(opened.length)
            .and_then(|s| s.checked_sub(1))
        else {
            return Err(ErrorBody::new(
                ErrorCode::Failed,
                "Content source returned an overflowing byte range.",
            ));
        };
        if end >= opened.size {
            return Err(ErrorBody::new(
                ErrorCode::Failed,
                "Content source returned a byte range past end of file.",
            ));
        }
    }
    Ok(())
}

fn resolved_range_from_opened(opened: &OpenedRange) -> Result<ResolvedRange, ErrorBody> {
    let end = opened
        .offset
        .checked_add(opened.length)
        .and_then(|s| s.checked_sub(1))
        .filter(|&end| end < opened.size)
        .ok_or_else(|| {
            ErrorBody::new(
                ErrorCode::Failed,
                "Content source returned an invalid byte range.",
            )
        })?;
    Ok(ResolvedRange {
        start: opened.offset,
        end,
        length: opened.length,
    })
}

async fn serve_head<S: ContentSource>(
    tickets: &TicketTable,
    source: &S,
    ticket: &Ticket,
    parsed_range: ParsedRange,
    if_range: Option<&str>,
    download: bool,
    options: ServeOptions,
) -> Response<ContentBody> {
    let opened = match source.open_range(ticket, 0, Some(0)).await {
        Ok(opened) => opened,
        Err(err) => return map_open_error(tickets, ticket, err, options),
    };
    if let Err(err) = validate_opened_range(&opened, 0, Some(0)) {
        return json_error(StatusCode::INTERNAL_SERVER_ERROR, &err, options);
    }
    let tag = etag(opened.size, opened.mtime, opened.file_id);
    let effective_range = apply_if_range(parsed_range, if_range, tag.to_str().unwrap_or(""));
    match resolve_range(effective_range, opened.size) {
        RangeDecision::Full => build_content_response(
            StatusCode::OK,
            ticket,
            opened.size,
            None,
            tag,
            download,
            options,
            ContentBody::empty(),
        ),
        RangeDecision::Partial(range) => build_content_response(
            StatusCode::PARTIAL_CONTENT,
            ticket,
            opened.size,
            Some(range),
            tag,
            download,
            options,
            ContentBody::empty(),
        ),
        RangeDecision::Unsatisfiable => unsatisfiable(opened.size, tag, options),
    }
}

#[allow(clippy::too_many_arguments)]
async fn serve_get<S: ContentSource>(
    tickets: &TicketTable,
    source: &S,
    ticket: &Ticket,
    parsed_range: ParsedRange,
    if_range: Option<&str>,
    download: bool,
    options: ServeOptions,
    stream_ctx: StreamContext<S>,
) -> Response<ContentBody> {
    match parsed_range {
        ParsedRange::None | ParsedRange::Ignore => {
            open_and_stream_full(tickets, source, ticket, download, options, stream_ctx).await
        }
        ParsedRange::Invalid => {
            // Even on invalid syntax, check If-Range (which overrides Range when mismatched) and
            // obtain current file size for `Content-Range: bytes */<size>`.
            let probe = match source.open_range(ticket, 0, Some(0)).await {
                Ok(opened) => opened,
                Err(err) => return map_open_error(tickets, ticket, err, options),
            };
            if let Err(err) = validate_opened_range(&probe, 0, Some(0)) {
                return json_error(StatusCode::INTERNAL_SERVER_ERROR, &err, options);
            }
            let tag = etag(probe.size, probe.mtime, probe.file_id);
            if if_range.is_some_and(|ir| !if_range_matches(ir, tag.to_str().unwrap_or(""))) {
                return open_and_stream_full(
                    tickets, source, ticket, download, options, stream_ctx,
                )
                .await;
            }
            unsatisfiable(probe.size, tag, options)
        }
        ParsedRange::Suffix(suffix) => {
            let probe = match source.open_range(ticket, 0, Some(0)).await {
                Ok(opened) => opened,
                Err(err) => return map_open_error(tickets, ticket, err, options),
            };
            if let Err(err) = validate_opened_range(&probe, 0, Some(0)) {
                return json_error(StatusCode::INTERNAL_SERVER_ERROR, &err, options);
            }
            let tag = etag(probe.size, probe.mtime, probe.file_id);
            if if_range.is_some_and(|ir| !if_range_matches(ir, tag.to_str().unwrap_or(""))) {
                return open_and_stream_full(
                    tickets, source, ticket, download, options, stream_ctx,
                )
                .await;
            }
            let RangeDecision::Partial(resolved) =
                resolve_range(ParsedRange::Suffix(suffix), probe.size)
            else {
                return unsatisfiable(probe.size, tag, options);
            };
            let opened = match source
                .open_range(ticket, resolved.start, Some(resolved.length))
                .await
            {
                Ok(opened) => opened,
                Err(OpenRangeError::OutOfRange { size }) => {
                    return unsatisfiable_without_etag(size, options);
                }
                Err(err) => return map_open_error(tickets, ticket, err, options),
            };
            if let Err(err) = validate_opened_range(&opened, resolved.start, Some(resolved.length))
            {
                return json_error(StatusCode::INTERNAL_SERVER_ERROR, &err, options);
            }
            let actual_tag = etag(opened.size, opened.mtime, opened.file_id);
            if opened.length == 0 || opened.offset >= opened.size {
                return unsatisfiable(opened.size, actual_tag, options);
            }
            let actual_range = match resolved_range_from_opened(&opened) {
                Ok(range) => range,
                Err(err) => return json_error(StatusCode::INTERNAL_SERVER_ERROR, &err, options),
            };
            let body = stream_ctx.into_body(opened.reader, opened.length);
            build_content_response(
                StatusCode::PARTIAL_CONTENT,
                ticket,
                opened.size,
                Some(actual_range),
                actual_tag,
                download,
                options,
                body,
            )
        }
        ParsedRange::FromStart { start, end } => {
            // Fast path: open the requested range directly in a single round-trip.
            let requested_len =
                end.and_then(|e| e.checked_sub(start).and_then(|d| d.checked_add(1)));
            match source.open_range(ticket, start, requested_len).await {
                Ok(opened) => {
                    if let Err(err) = validate_opened_range(&opened, start, requested_len) {
                        return json_error(StatusCode::INTERNAL_SERVER_ERROR, &err, options);
                    }
                    let tag = etag(opened.size, opened.mtime, opened.file_id);
                    if if_range.is_some_and(|ir| !if_range_matches(ir, tag.to_str().unwrap_or("")))
                    {
                        drop(opened);
                        return open_and_stream_full(
                            tickets, source, ticket, download, options, stream_ctx,
                        )
                        .await;
                    }
                    if opened.size == 0 || start >= opened.size || opened.length == 0 {
                        let size = opened.size;
                        drop(opened);
                        return unsatisfiable(size, tag, options);
                    }
                    let actual_range = match resolved_range_from_opened(&opened) {
                        Ok(range) => range,
                        Err(err) => {
                            return json_error(StatusCode::INTERNAL_SERVER_ERROR, &err, options);
                        }
                    };
                    let body = stream_ctx.into_body(opened.reader, opened.length);
                    build_content_response(
                        StatusCode::PARTIAL_CONTENT,
                        ticket,
                        opened.size,
                        Some(actual_range),
                        tag,
                        download,
                        options,
                        body,
                    )
                }
                Err(OpenRangeError::OutOfRange { size }) => {
                    if let Some(ir) = if_range {
                        // RFC 9110 §13.1.5: if If-Range fails to match, ignore Range and send 200.
                        let probe = match source.open_range(ticket, 0, Some(0)).await {
                            Ok(opened) => opened,
                            Err(err) => return map_open_error(tickets, ticket, err, options),
                        };
                        if let Err(err) = validate_opened_range(&probe, 0, Some(0)) {
                            return json_error(StatusCode::INTERNAL_SERVER_ERROR, &err, options);
                        }
                        let tag = etag(probe.size, probe.mtime, probe.file_id);
                        if !if_range_matches(ir, tag.to_str().unwrap_or("")) {
                            return open_and_stream_full(
                                tickets, source, ticket, download, options, stream_ctx,
                            )
                            .await;
                        }
                        return unsatisfiable(probe.size, tag, options);
                    }
                    unsatisfiable_without_etag(size, options)
                }
                Err(err) => map_open_error(tickets, ticket, err, options),
            }
        }
    }
}

async fn open_and_stream_full<S: ContentSource>(
    tickets: &TicketTable,
    source: &S,
    ticket: &Ticket,
    download: bool,
    options: ServeOptions,
    stream_ctx: StreamContext<S>,
) -> Response<ContentBody> {
    let opened = match source.open_range(ticket, 0, None).await {
        Ok(opened) => opened,
        Err(err) => return map_open_error(tickets, ticket, err, options),
    };
    if let Err(err) = validate_opened_range(&opened, 0, None) {
        return json_error(StatusCode::INTERNAL_SERVER_ERROR, &err, options);
    }
    let tag = etag(opened.size, opened.mtime, opened.file_id);
    let body = stream_ctx.into_body(opened.reader, opened.length);
    build_content_response(
        StatusCode::OK,
        ticket,
        opened.size,
        None,
        tag,
        download,
        options,
        body,
    )
}

fn apply_if_range(parsed: ParsedRange, if_range: Option<&str>, current_etag: &str) -> ParsedRange {
    match (parsed, if_range) {
        (ParsedRange::None | ParsedRange::Ignore, _) => parsed,
        (_, Some(ir)) if !if_range_matches(ir, current_etag) => ParsedRange::Ignore,
        (other, _) => other,
    }
}

#[allow(clippy::too_many_arguments)]
fn build_content_response(
    status: StatusCode,
    ticket: &Ticket,
    total_size: u64,
    range: Option<ResolvedRange>,
    tag: HeaderValue,
    download: bool,
    options: ServeOptions,
    body: ContentBody,
) -> Response<ContentBody> {
    // Always classify from the ticket's bound file name, never from the URL segment.
    let policy = classify_filename(&ticket.file_name);
    let content_length = range.map_or(total_size, |r| r.length);

    let mut response = Response::new(body);
    *response.status_mut() = status;
    let headers = response.headers_mut();
    apply_fixed_headers(headers, options);
    headers.insert(
        CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(CONTENT_SANDBOX_CSP),
    );
    headers.insert(CONTENT_TYPE, HeaderValue::from_static(policy.content_type));
    headers.insert(ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    headers.insert(ETAG, tag);
    headers.insert(CONTENT_LENGTH, HeaderValue::from(content_length));
    if let Some(r) = range
        && let Ok(val) = HeaderValue::from_str(&format!("bytes {}-{}/{total_size}", r.start, r.end))
    {
        headers.insert(CONTENT_RANGE, val);
    }
    if download || policy.attachment {
        headers.insert(
            CONTENT_DISPOSITION,
            content_disposition_attachment(&ticket.file_name),
        );
    }
    response
}

fn too_many_requests(exceeded: LimitExceeded, options: ServeOptions) -> Response<ContentBody> {
    let message = match exceeded.scope {
        LimitScope::Ticket => "Too many concurrent reads are already open for this content link.",
        LimitScope::Holder => "Too many concurrent content reads are already open for this token.",
        LimitScope::Global => {
            "The server is serving as many concurrent content reads as it allows."
        }
    };
    let mut response = json_error(
        StatusCode::TOO_MANY_REQUESTS,
        &ErrorBody::new(ErrorCode::Busy, message)
            .with_hint("Wait for an active download or media stream to finish and try again."),
        options,
    );
    let headers = response.headers_mut();
    headers.insert(RETRY_AFTER, HeaderValue::from(exceeded.retry_after_secs));
    headers.insert(CONNECTION, HeaderValue::from_static("close"));
    response
}

fn unsatisfiable(size: u64, tag: HeaderValue, options: ServeOptions) -> Response<ContentBody> {
    let mut response = unsatisfiable_without_etag(size, options);
    response.headers_mut().insert(ETAG, tag);
    response
}

fn unsatisfiable_without_etag(size: u64, options: ServeOptions) -> Response<ContentBody> {
    let mut response = Response::new(ContentBody::empty());
    *response.status_mut() = StatusCode::RANGE_NOT_SATISFIABLE;
    let headers = response.headers_mut();
    apply_fixed_headers(headers, options);
    headers.insert(
        CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(CONTENT_SANDBOX_CSP),
    );
    headers.insert(ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    headers.insert(CONTENT_LENGTH, HeaderValue::from(0u64));
    if let Ok(val) = HeaderValue::from_str(&format!("bytes */{size}")) {
        headers.insert(CONTENT_RANGE, val);
    }
    response
}

fn map_open_error(
    tickets: &TicketTable,
    ticket: &Ticket,
    err: OpenRangeError,
    options: ServeOptions,
) -> Response<ContentBody> {
    match err {
        OpenRangeError::TicketRevoked => {
            tickets.revoke_holder(&ticket.holder);
            ticket_not_found(options)
        }
        OpenRangeError::OutOfRange { size } => unsatisfiable_without_etag(size, options),
        OpenRangeError::Error(body) => json_error(status_for_error(body.code), &body, options),
    }
}

fn ticket_not_found(options: ServeOptions) -> Response<ContentBody> {
    json_error(
        StatusCode::NOT_FOUND,
        &ErrorBody::new(
            ErrorCode::NotFound,
            "This content link is invalid or has expired.",
        )
        .with_hint("Request a fresh content link from POST /v1/content."),
        options,
    )
}

fn json_error(
    status: StatusCode,
    error: &ErrorBody,
    options: ServeOptions,
) -> Response<ContentBody> {
    let bytes = serde_json::to_vec(error).unwrap_or_default();
    let len = bytes.len() as u64;
    let mut response = Response::new(ContentBody::from_bytes(bytes));
    *response.status_mut() = status;
    let headers = response.headers_mut();
    apply_fixed_headers(headers, options);
    headers.insert(
        CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(CONTENT_SANDBOX_CSP),
    );
    headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    headers.insert(CONTENT_LENGTH, HeaderValue::from(len));
    response
}

fn apply_fixed_headers(headers: &mut HeaderMap, options: ServeOptions) {
    headers.insert(X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    headers.insert(CROSS_ORIGIN_RESOURCE_POLICY, options.corp.as_header_value());
    headers.insert(REFERRER_POLICY, HeaderValue::from_static("no-referrer"));
    headers.insert(CACHE_CONTROL, HeaderValue::from_static("private, no-store"));
}

fn wants_download(query: Option<&str>) -> bool {
    query.is_some_and(|q| {
        q.split('&').any(|pair| match pair.split_once('=') {
            Some((name, value)) => name == "download" && matches!(value.trim(), "1" | "true"),
            None => pair == "download",
        })
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::io::SeekFrom;
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::task::{Context, Poll};

    use http_body::Body;
    use tokio::io::{AsyncRead, AsyncSeekExt, ReadBuf};

    use super::*;
    use crate::limit::ConcurrencyLimits;
    use crate::tickets::{TicketConfig, TicketRequest};

    async fn collect_body(mut body: ContentBody) -> Result<Vec<u8>, std::io::Error> {
        let mut out = Vec::new();
        let mut pinned = Pin::new(&mut body);
        loop {
            match std::future::poll_fn(|cx| pinned.as_mut().poll_frame(cx)).await {
                None => return Ok(out),
                Some(Ok(frame)) => {
                    if let Ok(data) = frame.into_data() {
                        out.extend_from_slice(&data);
                    }
                }
                Some(Err(err)) => return Err(err),
            }
        }
    }

    struct DiskSource {
        root: PathBuf,
        revoked: Arc<AtomicBool>,
        denied: Arc<AtomicBool>,
    }

    impl ContentSource for DiskSource {
        fn open_range<'a>(
            &'a self,
            ticket: &'a Ticket,
            offset: u64,
            length: Option<u64>,
        ) -> Pin<Box<dyn Future<Output = Result<OpenedRange, OpenRangeError>> + Send + 'a>>
        {
            Box::pin(async move {
                self.check_live(ticket)?;
                let path = self.root.join(&ticket.path);
                let mut file = tokio::fs::File::open(&path).await.map_err(|err| {
                    OpenRangeError::Error(ErrorBody::new(
                        ErrorCode::NotFound,
                        format!("open failed: {err}"),
                    ))
                })?;
                let meta = file.metadata().await.map_err(|err| {
                    OpenRangeError::Error(ErrorBody::new(
                        ErrorCode::Failed,
                        format!("stat failed: {err}"),
                    ))
                })?;
                let size = meta.len();
                let mtime = meta
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map_or(ContentTime::new(0, 0), |d| {
                        ContentTime::new(d.as_secs() as i64, d.subsec_nanos())
                    });
                if offset > size {
                    return Err(OpenRangeError::OutOfRange { size });
                }
                let available = size - offset;
                let actual = length.map_or(available, |n| n.min(available));
                if offset > 0 {
                    file.seek(SeekFrom::Start(offset)).await.map_err(|err| {
                        OpenRangeError::Error(ErrorBody::new(
                            ErrorCode::Failed,
                            format!("seek failed: {err}"),
                        ))
                    })?;
                }
                Ok(OpenedRange {
                    size,
                    offset,
                    length: actual,
                    mtime,
                    file_id: Some(1),
                    reader: Box::new(tokio::io::AsyncReadExt::take(file, actual)),
                })
            })
        }

        fn check_live(&self, _ticket: &Ticket) -> Result<(), OpenRangeError> {
            if self.revoked.load(Ordering::Acquire) {
                return Err(OpenRangeError::TicketRevoked);
            }
            if self.denied.load(Ordering::Acquire) {
                return Err(OpenRangeError::Error(ErrorBody::new(
                    ErrorCode::Denied,
                    "Role no longer allows fs.read.",
                )));
            }
            Ok(())
        }
    }

    #[tokio::test]
    async fn range_conformance_head_if_range_and_download() {
        let dir = tempfile::tempdir().unwrap();
        let data: Vec<u8> = (0u8..100).collect();
        std::fs::write(dir.path().join("sample.mp4"), &data).unwrap();

        let tickets = Arc::new(TicketTable::new());
        let source = Arc::new(DiskSource {
            root: dir.path().to_path_buf(),
            revoked: Arc::new(AtomicBool::new(false)),
            denied: Arc::new(AtomicBool::new(false)),
        });
        let issued = tickets
            .issue(TicketRequest {
                holder: "tok1".to_owned(),
                principal: "phone".to_owned(),
                machine: "tower".to_owned(),
                path: "sample.mp4".to_owned(),
            })
            .unwrap();

        // Full GET
        let resp = handle_request(
            &tickets,
            &source,
            &Method::GET,
            &issued.id,
            &issued.file_name,
            None,
            &HeaderMap::new(),
            ServeOptions::default(),
            StreamHooks::default(),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(resp.headers().get(CONTENT_TYPE).unwrap(), "video/mp4");
        assert_eq!(resp.headers().get(ACCEPT_RANGES).unwrap(), "bytes");
        assert_eq!(
            resp.headers().get(X_CONTENT_TYPE_OPTIONS).unwrap(),
            "nosniff"
        );
        assert_eq!(
            resp.headers().get(CROSS_ORIGIN_RESOURCE_POLICY).unwrap(),
            "same-origin"
        );
        assert_eq!(resp.headers().get(REFERRER_POLICY).unwrap(), "no-referrer");
        assert_eq!(
            resp.headers().get(CACHE_CONTROL).unwrap(),
            "private, no-store"
        );
        assert_eq!(
            resp.headers().get(CONTENT_SECURITY_POLICY).unwrap(),
            CONTENT_SANDBOX_CSP
        );
        assert!(resp.headers().get(CONTENT_DISPOSITION).is_none());
        let etag_val = resp
            .headers()
            .get(ETAG)
            .unwrap()
            .to_str()
            .unwrap()
            .to_owned();
        assert_eq!(collect_body(resp.into_body()).await.unwrap(), data);

        // First byte
        let mut h = HeaderMap::new();
        h.insert(RANGE, HeaderValue::from_static("bytes=0-0"));
        let resp = handle_request(
            &tickets,
            &source,
            &Method::GET,
            &issued.id,
            &issued.file_name,
            None,
            &h,
            ServeOptions::default(),
            StreamHooks::default(),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(resp.headers().get(CONTENT_RANGE).unwrap(), "bytes 0-0/100");
        assert_eq!(resp.headers().get(CONTENT_LENGTH).unwrap(), "1");
        assert_eq!(collect_body(resp.into_body()).await.unwrap(), vec![0u8]);

        // Last byte
        h.insert(RANGE, HeaderValue::from_static("bytes=99-99"));
        let resp = handle_request(
            &tickets,
            &source,
            &Method::GET,
            &issued.id,
            &issued.file_name,
            None,
            &h,
            ServeOptions::default(),
            StreamHooks::default(),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(
            resp.headers().get(CONTENT_RANGE).unwrap(),
            "bytes 99-99/100"
        );
        assert_eq!(collect_body(resp.into_body()).await.unwrap(), vec![99u8]);

        // Suffix range
        h.insert(RANGE, HeaderValue::from_static("bytes=-10"));
        let resp = handle_request(
            &tickets,
            &source,
            &Method::GET,
            &issued.id,
            &issued.file_name,
            None,
            &h,
            ServeOptions::default(),
            StreamHooks::default(),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(
            resp.headers().get(CONTENT_RANGE).unwrap(),
            "bytes 90-99/100"
        );
        assert_eq!(collect_body(resp.into_body()).await.unwrap(), data[90..]);

        // Open-ended range
        h.insert(RANGE, HeaderValue::from_static("bytes=85-"));
        let resp = handle_request(
            &tickets,
            &source,
            &Method::GET,
            &issued.id,
            &issued.file_name,
            None,
            &h,
            ServeOptions::default(),
            StreamHooks::default(),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(
            resp.headers().get(CONTENT_RANGE).unwrap(),
            "bytes 85-99/100"
        );
        assert_eq!(collect_body(resp.into_body()).await.unwrap(), data[85..]);

        // Unsatisfiable range
        h.insert(RANGE, HeaderValue::from_static("bytes=100-120"));
        let resp = handle_request(
            &tickets,
            &source,
            &Method::GET,
            &issued.id,
            &issued.file_name,
            None,
            &h,
            ServeOptions::default(),
            StreamHooks::default(),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::RANGE_NOT_SATISFIABLE);
        assert_eq!(resp.headers().get(CONTENT_RANGE).unwrap(), "bytes */100");
        assert!(collect_body(resp.into_body()).await.unwrap().is_empty());

        // If-Range matching ETag serves 206
        h.insert(RANGE, HeaderValue::from_static("bytes=10-19"));
        h.insert(IF_RANGE, HeaderValue::from_str(&etag_val).unwrap());
        let resp = handle_request(
            &tickets,
            &source,
            &Method::GET,
            &issued.id,
            &issued.file_name,
            None,
            &h,
            ServeOptions::default(),
            StreamHooks::default(),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(collect_body(resp.into_body()).await.unwrap(), data[10..20]);

        // If-Range mismatch serves full 200 OK (even when Range would be out-of-range!)
        h.insert(RANGE, HeaderValue::from_static("bytes=500-600"));
        h.insert(IF_RANGE, HeaderValue::from_static("\"stale-etag\""));
        let resp = handle_request(
            &tickets,
            &source,
            &Method::GET,
            &issued.id,
            &issued.file_name,
            None,
            &h,
            ServeOptions::default(),
            StreamHooks::default(),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(resp.headers().get(CONTENT_LENGTH).unwrap(), "100");
        assert_eq!(collect_body(resp.into_body()).await.unwrap(), data);

        // HEAD returns headers and empty body
        let resp = handle_request(
            &tickets,
            &source,
            &Method::HEAD,
            &issued.id,
            &issued.file_name,
            Some("download=1"),
            &HeaderMap::new(),
            ServeOptions::default(),
            StreamHooks::default(),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(resp.headers().get(CONTENT_LENGTH).unwrap(), "100");
        assert_eq!(
            resp.headers().get(CONTENT_DISPOSITION).unwrap(),
            "attachment; filename=\"sample.mp4\""
        );
        assert!(collect_body(resp.into_body()).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn pdf_is_attachment_and_every_response_carries_frame_ancestors_sandbox_csp() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("doc.pdf"), b"%PDF-1.4\n").unwrap();
        std::fs::write(dir.path().join("page.html"), b"<script>alert(1)</script>").unwrap();
        std::fs::write(dir.path().join("pic.svg"), b"<svg onload='alert(1)'/>").unwrap();
        std::fs::write(
            dir.path().join("sub.vtt"),
            b"WEBVTT\n\n00:00.000 --> 00:01.000\nHi\n",
        )
        .unwrap();
        std::fs::write(dir.path().join("raw.bin"), b"\x00\x01\x02").unwrap();

        let tickets = Arc::new(TicketTable::new());
        let source = Arc::new(DiskSource {
            root: dir.path().to_path_buf(),
            revoked: Arc::new(AtomicBool::new(false)),
            denied: Arc::new(AtomicBool::new(false)),
        });

        let issue = |name: &str| {
            tickets
                .issue(TicketRequest {
                    holder: "h".to_owned(),
                    principal: "p".to_owned(),
                    machine: "tower".to_owned(),
                    path: name.to_owned(),
                })
                .unwrap()
        };

        // PDF: forced attachment as application/octet-stream with full sandbox CSP.
        let pdf = issue("doc.pdf");
        let resp = handle_request(
            &tickets,
            &source,
            &Method::GET,
            &pdf.id,
            &pdf.file_name,
            None,
            &HeaderMap::new(),
            ServeOptions::default(),
            StreamHooks::default(),
        )
        .await;
        assert_eq!(
            resp.headers().get(CONTENT_TYPE).unwrap(),
            "application/octet-stream"
        );
        assert_eq!(
            resp.headers().get(CONTENT_DISPOSITION).unwrap(),
            "attachment; filename=\"doc.pdf\""
        );
        assert_eq!(
            resp.headers().get(CONTENT_SECURITY_POLICY).unwrap(),
            CONTENT_SANDBOX_CSP
        );
        assert!(CONTENT_SANDBOX_CSP.contains("frame-ancestors 'none'"));

        // WebVTT serves as text/vtt; charset=utf-8.
        let vtt = issue("sub.vtt");
        let resp = handle_request(
            &tickets,
            &source,
            &Method::GET,
            &vtt.id,
            &vtt.file_name,
            None,
            &HeaderMap::new(),
            ServeOptions::default(),
            StreamHooks::default(),
        )
        .await;
        assert_eq!(
            resp.headers().get(CONTENT_TYPE).unwrap(),
            "text/vtt; charset=utf-8"
        );
        assert_eq!(
            resp.headers().get(CONTENT_SECURITY_POLICY).unwrap(),
            CONTENT_SANDBOX_CSP
        );

        // HTML is text/plain + sandbox CSP, and URL segment spoofing is refused (404).
        let html = issue("page.html");
        let spoof = handle_request(
            &tickets,
            &source,
            &Method::GET,
            &html.id,
            "doc.pdf",
            None,
            &HeaderMap::new(),
            ServeOptions::default(),
            StreamHooks::default(),
        )
        .await;
        assert_eq!(spoof.status(), StatusCode::NOT_FOUND);

        let resp = handle_request(
            &tickets,
            &source,
            &Method::GET,
            &html.id,
            &html.file_name,
            None,
            &HeaderMap::new(),
            ServeOptions::default(),
            StreamHooks::default(),
        )
        .await;
        assert_eq!(
            resp.headers().get(CONTENT_TYPE).unwrap(),
            "text/plain; charset=utf-8"
        );
        assert_eq!(
            resp.headers().get(CONTENT_SECURITY_POLICY).unwrap(),
            CONTENT_SANDBOX_CSP
        );

        // SVG is image/svg+xml + sandbox CSP.
        let svg = issue("pic.svg");
        let resp = handle_request(
            &tickets,
            &source,
            &Method::GET,
            &svg.id,
            &svg.file_name,
            None,
            &HeaderMap::new(),
            ServeOptions::default(),
            StreamHooks::default(),
        )
        .await;
        assert_eq!(resp.headers().get(CONTENT_TYPE).unwrap(), "image/svg+xml");
        assert_eq!(
            resp.headers().get(CONTENT_SECURITY_POLICY).unwrap(),
            CONTENT_SANDBOX_CSP
        );

        // Unknown extension is octet-stream + attachment + sandbox CSP.
        let bin = issue("raw.bin");
        let resp = handle_request(
            &tickets,
            &source,
            &Method::GET,
            &bin.id,
            &bin.file_name,
            None,
            &HeaderMap::new(),
            ServeOptions::default(),
            StreamHooks::default(),
        )
        .await;
        assert_eq!(
            resp.headers().get(CONTENT_TYPE).unwrap(),
            "application/octet-stream"
        );
        assert_eq!(
            resp.headers().get(CONTENT_DISPOSITION).unwrap(),
            "attachment; filename=\"raw.bin\""
        );
        assert_eq!(
            resp.headers().get(CONTENT_SECURITY_POLICY).unwrap(),
            CONTENT_SANDBOX_CSP
        );
    }

    #[tokio::test]
    async fn three_gib_sparse_file_reads_tail_without_memory_growth() {
        use std::io::Write;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sparse.bin");
        let tail_bytes = b"LEGION-SPARSE-TAIL-END-MARKER-0123456789";
        let total_size: u64 = 3 * 1024 * 1024 * 1024; // 3 GiB
        let tail_offset = total_size - tail_bytes.len() as u64;
        {
            let mut f = std::fs::File::create(&path).unwrap();
            f.set_len(total_size).unwrap();
            std::io::Seek::seek(&mut f, SeekFrom::Start(tail_offset)).unwrap();
            f.write_all(tail_bytes).unwrap();
        }
        assert_eq!(std::fs::metadata(&path).unwrap().len(), total_size);

        let tickets = Arc::new(TicketTable::new());
        let source = Arc::new(DiskSource {
            root: dir.path().to_path_buf(),
            revoked: Arc::new(AtomicBool::new(false)),
            denied: Arc::new(AtomicBool::new(false)),
        });
        let issued = tickets
            .issue(TicketRequest {
                holder: "h".to_owned(),
                principal: "p".to_owned(),
                machine: "tower".to_owned(),
                path: "sparse.bin".to_owned(),
            })
            .unwrap();

        // Open-ended read at the end of 3 GiB sparse file.
        let mut h = HeaderMap::new();
        h.insert(
            RANGE,
            HeaderValue::from_str(&format!("bytes={tail_offset}-")).unwrap(),
        );
        let resp = handle_request(
            &tickets,
            &source,
            &Method::GET,
            &issued.id,
            &issued.file_name,
            None,
            &h,
            ServeOptions::default(),
            StreamHooks::default(),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(
            resp.headers().get(CONTENT_RANGE).unwrap(),
            &format!("bytes {tail_offset}-{}/{total_size}", total_size - 1)
        );
        assert_eq!(collect_body(resp.into_body()).await.unwrap(), tail_bytes);

        // Suffix read of last 40 bytes of 3 GiB sparse file.
        h.insert(
            RANGE,
            HeaderValue::from_str(&format!("bytes=-{}", tail_bytes.len())).unwrap(),
        );
        let resp = handle_request(
            &tickets,
            &source,
            &Method::GET,
            &issued.id,
            &issued.file_name,
            None,
            &h,
            ServeOptions::default(),
            StreamHooks::default(),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(collect_body(resp.into_body()).await.unwrap(), tail_bytes);
    }

    #[tokio::test]
    async fn client_disconnect_mid_stream_drops_reader_and_releases_permit() {
        struct CountingReader {
            reads: Arc<AtomicUsize>,
            dropped: Arc<AtomicBool>,
        }

        impl AsyncRead for CountingReader {
            fn poll_read(
                self: Pin<&mut Self>,
                _cx: &mut Context<'_>,
                buf: &mut ReadBuf<'_>,
            ) -> Poll<std::io::Result<()>> {
                self.reads.fetch_add(1, Ordering::SeqCst);
                let fill = buf.remaining().min(16 * 1024);
                buf.put_slice(&vec![b'A'; fill]);
                Poll::Ready(Ok(()))
            }
        }

        impl Drop for CountingReader {
            fn drop(&mut self) {
                self.dropped.store(true, Ordering::SeqCst);
            }
        }

        struct HugeSource {
            reads: Arc<AtomicUsize>,
            dropped: Arc<AtomicBool>,
        }

        impl ContentSource for HugeSource {
            fn open_range<'a>(
                &'a self,
                _ticket: &'a Ticket,
                offset: u64,
                length: Option<u64>,
            ) -> Pin<Box<dyn Future<Output = Result<OpenedRange, OpenRangeError>> + Send + 'a>>
            {
                let reads = Arc::clone(&self.reads);
                let dropped = Arc::clone(&self.dropped);
                Box::pin(async move {
                    let size = 100 * 1024 * 1024u64;
                    let actual = length.map_or(size - offset, |n| n.min(size - offset));
                    Ok(OpenedRange {
                        size,
                        offset,
                        length: actual,
                        mtime: ContentTime::new(1_700_000_000, 0),
                        file_id: None,
                        reader: Box::new(CountingReader { reads, dropped }),
                    })
                })
            }
        }

        let reads = Arc::new(AtomicUsize::new(0));
        let dropped = Arc::new(AtomicBool::new(false));
        let source = Arc::new(HugeSource {
            reads: Arc::clone(&reads),
            dropped: Arc::clone(&dropped),
        });
        let tickets = Arc::new(TicketTable::new());
        let limiter = ContentLimiter::with_limits(ConcurrencyLimits {
            max_per_ticket: 1,
            max_per_holder: 2,
            max_global: 4,
            retry_after_secs: 5,
        });
        let issued = tickets
            .issue(TicketRequest {
                holder: "h".to_owned(),
                principal: "p".to_owned(),
                machine: "tower".to_owned(),
                path: "/huge.mp4".to_owned(),
            })
            .unwrap();

        let resp = handle_request(
            &tickets,
            &source,
            &Method::GET,
            &issued.id,
            &issued.file_name,
            None,
            &HeaderMap::new(),
            ServeOptions::default(),
            StreamHooks {
                limiter: Some(limiter.clone()),
                ..StreamHooks::default()
            },
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(limiter.active_global(), 1);

        // Second concurrent stream on the same ticket gets 429 + Retry-After + Connection: close.
        let blocked = handle_request(
            &tickets,
            &source,
            &Method::GET,
            &issued.id,
            &issued.file_name,
            None,
            &HeaderMap::new(),
            ServeOptions::default(),
            StreamHooks {
                limiter: Some(limiter.clone()),
                ..StreamHooks::default()
            },
        )
        .await;
        assert_eq!(blocked.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(blocked.headers().get(RETRY_AFTER).unwrap(), "5");
        assert_eq!(blocked.headers().get(CONNECTION).unwrap(), "close");

        let mut body = resp.into_body();
        for _ in 0..2 {
            let frame = std::future::poll_fn(|cx| Pin::new(&mut body).poll_frame(cx)).await;
            assert!(frame.unwrap().is_ok());
        }
        assert_eq!(reads.load(Ordering::SeqCst), 2);
        assert!(!dropped.load(Ordering::SeqCst));
        drop(body);
        assert!(dropped.load(Ordering::SeqCst));
        assert_eq!(limiter.active_global(), 0);
        assert_eq!(reads.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn in_flight_revocation_policy_loss_and_hard_ttl_abort_active_stream() {
        let dir = tempfile::tempdir().unwrap();
        let data = vec![b'X'; 5 * 1024 * 1024]; // 5 MiB
        std::fs::write(dir.path().join("big.bin"), &data).unwrap();

        let revoked = Arc::new(AtomicBool::new(false));
        let denied = Arc::new(AtomicBool::new(false));
        let source = Arc::new(DiskSource {
            root: dir.path().to_path_buf(),
            revoked: Arc::clone(&revoked),
            denied: Arc::clone(&denied),
        });
        let tickets = Arc::new(TicketTable::new());
        let opts = ServeOptions {
            corp: CorpPolicy::SameOrigin,
            liveness_check_bytes: 256 * 1024,
            liveness_check_interval: Duration::from_secs(60),
        };

        // 1) Token revoked mid-stream -> aborts after the first check interval and revokes holder.
        let t1 = tickets
            .issue(TicketRequest {
                holder: "h-revoke".to_owned(),
                principal: "p".to_owned(),
                machine: "tower".to_owned(),
                path: "big.bin".to_owned(),
            })
            .unwrap();
        let resp = handle_request(
            &tickets,
            &source,
            &Method::GET,
            &t1.id,
            &t1.file_name,
            None,
            &HeaderMap::new(),
            opts,
            StreamHooks::default(),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        let mut body = resp.into_body();
        // Read one frame, then revoke credential.
        let first = std::future::poll_fn(|cx| Pin::new(&mut body).poll_frame(cx)).await;
        assert!(first.unwrap().is_ok());
        revoked.store(true, Ordering::Release);
        let err = collect_body(body).await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
        assert_eq!(tickets.len_for_holder("h-revoke"), 0);
        revoked.store(false, Ordering::Release);

        // 2) Policy fs.read denied mid-stream -> aborts stream.
        let t2 = tickets
            .issue(TicketRequest {
                holder: "h-deny".to_owned(),
                principal: "p".to_owned(),
                machine: "tower".to_owned(),
                path: "big.bin".to_owned(),
            })
            .unwrap();
        let resp = handle_request(
            &tickets,
            &source,
            &Method::GET,
            &t2.id,
            &t2.file_name,
            None,
            &HeaderMap::new(),
            opts,
            StreamHooks::default(),
        )
        .await;
        let mut body = resp.into_body();
        let first = std::future::poll_fn(|cx| Pin::new(&mut body).poll_frame(cx)).await;
        assert!(first.unwrap().is_ok());
        denied.store(true, Ordering::Release);
        let err = collect_body(body).await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
        denied.store(false, Ordering::Release);

        // 3) Hard absolute TTL expiring mid-stream -> aborts stream.
        let short_tickets = Arc::new(TicketTable::with_config(TicketConfig {
            sliding_ttl: Duration::from_millis(200),
            absolute_ttl: Duration::from_millis(80),
            ..TicketConfig::default()
        }));
        let t3 = short_tickets
            .issue(TicketRequest {
                holder: "h-ttl".to_owned(),
                principal: "p".to_owned(),
                machine: "tower".to_owned(),
                path: "big.bin".to_owned(),
            })
            .unwrap();
        let resp = handle_request(
            &short_tickets,
            &source,
            &Method::GET,
            &t3.id,
            &t3.file_name,
            None,
            &HeaderMap::new(),
            ServeOptions {
                corp: CorpPolicy::SameOrigin,
                liveness_check_bytes: 1024 * 1024,
                liveness_check_interval: Duration::from_millis(20),
            },
            StreamHooks::default(),
        )
        .await;
        let mut body = resp.into_body();
        let first = std::future::poll_fn(|cx| Pin::new(&mut body).poll_frame(cx)).await;
        assert!(first.unwrap().is_ok());
        tokio::time::sleep(Duration::from_millis(120)).await;
        let err = collect_body(body).await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
    }

    #[tokio::test]
    async fn invalid_opened_range_from_misbehaving_source_is_rejected_without_overflow() {
        struct LyingSource {
            size: u64,
            offset: u64,
            length: u64,
        }

        impl ContentSource for LyingSource {
            fn open_range<'a>(
                &'a self,
                _ticket: &'a Ticket,
                _offset: u64,
                _length: Option<u64>,
            ) -> Pin<Box<dyn Future<Output = Result<OpenedRange, OpenRangeError>> + Send + 'a>>
            {
                let (size, offset, length) = (self.size, self.offset, self.length);
                Box::pin(async move {
                    Ok(OpenedRange {
                        size,
                        offset,
                        length,
                        mtime: ContentTime::new(1_700_000_000, 0),
                        file_id: None,
                        reader: Box::new(tokio::io::empty()),
                    })
                })
            }
        }

        let tickets = Arc::new(TicketTable::new());
        let issued = tickets
            .issue(TicketRequest {
                holder: "h".to_owned(),
                principal: "p".to_owned(),
                machine: "tower".to_owned(),
                path: "/data.bin".to_owned(),
            })
            .unwrap();

        let cases = [
            // Overflowing offset + length
            (u64::MAX, 10u64, u64::MAX),
            // Wrong offset
            (100, 5, 10),
            // Length exceeds size - offset
            (100, 10, 95),
        ];

        for (size, offset, length) in cases {
            let source = Arc::new(LyingSource {
                size,
                offset,
                length,
            });
            let mut h = HeaderMap::new();
            h.insert(RANGE, HeaderValue::from_static("bytes=10-19"));
            let resp = handle_request(
                &tickets,
                &source,
                &Method::GET,
                &issued.id,
                &issued.file_name,
                None,
                &h,
                ServeOptions::default(),
                StreamHooks::default(),
            )
            .await;
            assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
        }
    }
}
