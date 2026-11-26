//! How the HTTP door answers: JSON bodies, and the status that fits each error code.

use axum::body::Body;
use axum::http::header::{CACHE_CONTROL, CONTENT_TYPE, HeaderValue};
use axum::http::{Response, StatusCode};
use legion_core::RouteOutcome;
use legion_core::proto::{ErrorBody, ErrorCode};
use serde::Serialize;

/// The status a call that failed with `code` is answered with. The body says the code itself;
/// the status is for clients and proxies that look no further.
pub(crate) fn status_for(code: ErrorCode) -> StatusCode {
    match code {
        ErrorCode::InvalidArgs => StatusCode::BAD_REQUEST,
        ErrorCode::Denied => StatusCode::FORBIDDEN,
        ErrorCode::NotFound | ErrorCode::UnknownTool => StatusCode::NOT_FOUND,
        ErrorCode::Exists | ErrorCode::Conflict => StatusCode::CONFLICT,
        ErrorCode::Busy => StatusCode::LOCKED,
        ErrorCode::Unsupported => StatusCode::NOT_IMPLEMENTED,
        ErrorCode::Unreachable => StatusCode::BAD_GATEWAY,
        ErrorCode::Timeout => StatusCode::GATEWAY_TIMEOUT,
        // What nginx answers when the client went away first; there is no standard status for it.
        ErrorCode::Cancelled => StatusCode::from_u16(499).unwrap_or(StatusCode::BAD_REQUEST),
        ErrorCode::Failed | ErrorCode::Internal => StatusCode::INTERNAL_SERVER_ERROR,
    }
}

/// A JSON response.
pub(crate) fn json<T: Serialize>(status: StatusCode, value: &T) -> Response<Body> {
    let body = serde_json::to_vec(value).unwrap_or_else(|error| {
        let error = ErrorBody::new(
            ErrorCode::Internal,
            format!("The reply cannot be written as JSON: {error}."),
        );
        serde_json::to_vec(&error).unwrap_or_default()
    });
    let mut response = Response::new(Body::from(body));
    *response.status_mut() = status;
    let headers = response.headers_mut();
    headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    headers.insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

/// An error, with the status its code calls for.
pub(crate) fn error(error: &ErrorBody) -> Response<Body> {
    json(status_for(error.code), error)
}

/// An error the door answers with before any call reaches the node, with a status of its own.
pub(crate) fn refusal(status: StatusCode, code: ErrorCode, message: &str) -> Response<Body> {
    json(status, &ErrorBody::new(code, message))
}

/// A routed call's response: the tool's own object with 200, its error with the status that fits
/// the code, or a fan-out's `{"results": …}` with 200, or 502 when every machine failed.
pub(crate) fn outcome(outcome: RouteOutcome) -> Response<Body> {
    let outcome = for_http(outcome);
    let failed = outcome.all_failed();
    let fanned = matches!(outcome, RouteOutcome::Many(_));
    match outcome.into_wire() {
        Ok(value) if fanned && failed => json(StatusCode::BAD_GATEWAY, &value),
        Ok(value) => json(StatusCode::OK, &value),
        Err(failure) => error(&failure),
    }
}

/// Every error in an outcome with hints written for the command line pointing at what an HTTP
/// client has instead. An unknown tool's hint lists its family's tools, including ones the caller
/// may not use, so it points at the caller's own listing instead.
pub(crate) fn for_http(outcome: RouteOutcome) -> RouteOutcome {
    let rewrite = |error: ErrorBody| {
        let hint = error.hint.as_deref().unwrap_or_default();
        if error.code == ErrorCode::UnknownTool || hint.contains("`legion tools`") {
            error.with_hint("GET /v1/tools lists the tools you may use here.")
        } else if hint.contains("`legion help ") {
            error.with_hint("GET /v1/tools gives each tool's input schema.")
        } else {
            error
        }
    };
    match outcome {
        RouteOutcome::One(result) => RouteOutcome::One(result.map_err(rewrite)),
        RouteOutcome::Many(results) => RouteOutcome::Many(
            results
                .into_iter()
                .map(|(machine, result)| (machine, result.map_err(rewrite)))
                .collect(),
        ),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use serde_json::json;

    use super::*;

    #[test]
    fn every_code_has_the_status_that_fits_it() {
        for (code, status) in [
            (ErrorCode::InvalidArgs, 400),
            (ErrorCode::Denied, 403),
            (ErrorCode::NotFound, 404),
            (ErrorCode::UnknownTool, 404),
            (ErrorCode::Exists, 409),
            (ErrorCode::Conflict, 409),
            (ErrorCode::Busy, 423),
            (ErrorCode::Unsupported, 501),
            (ErrorCode::Unreachable, 502),
            (ErrorCode::Timeout, 504),
            (ErrorCode::Cancelled, 499),
            (ErrorCode::Failed, 500),
            (ErrorCode::Internal, 500),
        ] {
            assert_eq!(status_for(code).as_u16(), status, "{code}");
        }
    }

    #[test]
    fn a_fan_out_fails_only_when_every_machine_did() {
        let down = || Err(ErrorBody::new(ErrorCode::Unreachable, "Down."));
        let mut partial = BTreeMap::new();
        partial.insert("pi".to_owned(), Ok(json!({})));
        partial.insert("tower".to_owned(), down());
        assert_eq!(outcome(RouteOutcome::Many(partial)).status(), 200);
        let mut failed = BTreeMap::new();
        failed.insert("tower".to_owned(), down());
        assert_eq!(outcome(RouteOutcome::Many(failed)).status(), 502);
        assert_eq!(
            outcome(RouteOutcome::One(Err(ErrorBody::new(
                ErrorCode::Busy,
                "Busy."
            ))))
            .status(),
            423
        );
    }

    #[test]
    fn command_line_hints_point_at_the_listing() {
        let error = ErrorBody::new(ErrorCode::InvalidArgs, "Wrong.")
            .with_hint("`legion help fs.read` shows its arguments.");
        let RouteOutcome::One(Err(error)) = for_http(RouteOutcome::One(Err(error))) else {
            panic!("the error went missing");
        };
        assert_eq!(
            error.hint.as_deref(),
            Some("GET /v1/tools gives each tool's input schema.")
        );
    }
}
