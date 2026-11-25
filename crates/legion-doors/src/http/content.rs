//! Content endpoints mounted on the HTTP door:
//! - `POST /v1/content`: bearer-authenticated ticket creation (`{on?, path}` -> `{url, expires_in}`).
//! - `GET`/`HEAD /v1/content/{ticket}/{name}`: ticket-authenticated single-range byte streaming
//!   powered by `legion-content`.
//!
//! # Origin policy for `/v1/content/{ticket}/{name}`
//!
//! The HTTP door's `Host` and `Origin` guards run on `/v1/content/{ticket}/{name}` just as they do
//! on every other route:
//! - Browser media elements (`<img>`, `<video>`, `<audio>`), `<a download>`, and top-level
//!   navigations send **no `Origin` header**; `gate::origin_allowed` admits requests with no
//!   `Origin`. Because every content reply carries `Cross-Origin-Resource-Policy: same-origin`
//!   (and `Referrer-Policy: no-referrer`), an unlisted third-party page cannot embed a content URL
//!   even if it somehow learned a 256-bit ticket.
//! - Same-origin `fetch()` / `<video crossorigin>` calls from the served UI (when `[serve] ui =
//!   true`) and cross-origin calls from origins listed in `[serve] origins` are allowed (listed
//!   origins receive `Access-Control-Allow-Origin` and exposed range/ETag headers).
//! - Any request carrying an unlisted `Origin` header (including `Origin: null` from a sandboxed
//!   iframe or `data:` page) is refused with `403 Forbidden`.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use axum::body::Body;
use axum::extract::{Path as AxumPath, Request, State};
use axum::http::header::{CACHE_CONTROL, HeaderValue};
use axum::http::{Response, StatusCode};
use legion_content::{
    ContentSource, ContentTime, CorpPolicy, OpenRangeError, OpenedRange, ServeOptions, Ticket,
    TicketRequest, encode_path_segment, extract_file_name, handle_request,
};
use legion_core::proto::{ErrorBody, ErrorCode};
use legion_core::tokens::Tokens;
use legion_core::{ContentError, ContentRequest, Door};
use serde::Deserialize;
use serde_json::{Value, json};

use super::{Shared, read_body, reply};
use crate::Authenticated;

/// The JSON body of `POST /v1/content`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CreateContentBody {
    #[serde(default)]
    on: Option<Value>,
    path: String,
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
/// principal, machine, and path.
pub(crate) async fn create(State(shared): State<Arc<Shared>>, request: Request) -> Response<Body> {
    let Some(who) = request.extensions().get::<Authenticated>().cloned() else {
        return reply::error(&ErrorBody::new(
            ErrorCode::Internal,
            "The request reached POST /v1/content without a principal.",
        ));
    };
    let bytes = match read_body(request.into_body()).await {
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

    // Check `fs.read` permission, path confinement (roots, config/state, secrets, symlinks),
    // regular-file type, and remote reachability right now by probing zero bytes.
    let probe_req = ContentRequest {
        machine: Some(target_machine.clone()),
        path: body.path.clone(),
        offset: 0,
        length: Some(0),
        principal: who.principal.clone(),
        door: Door::Http,
        cancel: shared.link.shutdown.child_token(),
    };
    match shared.router.open_content(&shared.node, probe_req).await {
        Ok(_opened) => {}
        Err(ContentError::OutOfRange { .. }) => {
            // offset 0 cannot be out of range on any file.
            return reply::error(&ErrorBody::new(
                ErrorCode::Internal,
                "Unexpected out-of-range error while probing file.",
            ));
        }
        Err(ContentError::Error(error)) => return reply::error(&error),
    }

    // Ensure the path yields a valid filename segment before minting the ticket.
    if let Err(error) = extract_file_name(&body.path) {
        return reply::error(&error);
    }

    let issued = match shared.content_tickets.issue(TicketRequest {
        holder: token_holder(&who),
        principal: who.principal,
        machine: target_machine,
        path: body.path,
    }) {
        Ok(issued) => issued,
        Err(error) => {
            let status = if error.code == ErrorCode::Busy {
                StatusCode::SERVICE_UNAVAILABLE
            } else {
                reply::status_for(error.code)
            };
            return reply::json(status, &error);
        }
    };

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

/// `GET` / `HEAD /v1/content/{ticket}/{name}`: streams file bytes with single-range support.
pub(crate) async fn serve(
    State(shared): State<Arc<Shared>>,
    AxumPath((ticket_id, url_name)): AxumPath<(String, String)>,
    request: Request,
) -> Response<Body> {
    let source = DoorContentSource {
        shared: Arc::clone(&shared),
    };
    handle_request(
        &shared.content_tickets,
        &source,
        request.method(),
        &ticket_id,
        &url_name,
        request.uri().query(),
        request.headers(),
        ServeOptions {
            corp: CorpPolicy::SameOrigin,
        },
    )
    .await
    .map(Body::new)
}

struct DoorContentSource {
    shared: Arc<Shared>,
}

impl ContentSource for DoorContentSource {
    fn open_range<'a>(
        &'a self,
        ticket: &'a Ticket,
        offset: u64,
        length: Option<u64>,
    ) -> Pin<Box<dyn Future<Output = Result<OpenedRange, OpenRangeError>> + Send + 'a>> {
        Box::pin(async move {
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
            let req = ContentRequest {
                machine: Some(ticket.machine.clone()),
                path: ticket.path.clone(),
                offset,
                length,
                principal: ticket.principal.clone(),
                door: Door::Http,
                cancel: self.shared.link.shutdown.child_token(),
            };
            match self
                .shared
                .router
                .open_content(&self.shared.node, req)
                .await
            {
                Ok(opened) => Ok(OpenedRange {
                    size: opened.size,
                    offset: opened.offset,
                    length: opened.length,
                    mtime: ContentTime::new(opened.mtime_secs, opened.mtime_nanos),
                    reader: opened.reader,
                }),
                Err(ContentError::OutOfRange { size }) => Err(OpenRangeError::OutOfRange { size }),
                Err(ContentError::Error(body)) => Err(OpenRangeError::Error(body)),
            }
        })
    }
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

/// Whether `path` targets the ticket-authenticated content read endpoint
/// (`/v1/content/<ticket>/<name>`), strictly two non-empty segments under `/v1/content/`.
pub(crate) fn is_content_read_path(path: &str) -> bool {
    let Some(rest) = path.strip_prefix("/v1/content/") else {
        return false;
    };
    let Some((ticket, name)) = rest.split_once('/') else {
        return false;
    };
    !ticket.is_empty() && !name.is_empty() && !name.contains('/')
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
