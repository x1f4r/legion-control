//! Embedded static assets of the Legion web interface (`apps/ui`), served at `/`.

use axum::body::Body;
use axum::http::header::{
    CACHE_CONTROL, CONTENT_TYPE, ETAG as ETAG_HEADER, HeaderValue, IF_NONE_MATCH,
};
use axum::http::{HeaderMap, Response, StatusCode};

include!(concat!(env!("OUT_DIR"), "/ui_assets.rs"));

/// Strict content security policy applied to every HTTP door response.
pub(crate) const CONTENT_SECURITY_POLICY: &str = "default-src 'none'; script-src 'self'; style-src 'self'; img-src 'self' data:; media-src 'self'; connect-src 'self'; manifest-src 'self'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'";

/// Serves one embedded asset by exact path, answering `304 Not Modified` when `If-None-Match`
/// matches the build's [`ETAG`]. Returns `None` for any path outside the embedded set.
pub(crate) fn serve(path: &str, headers: &HeaderMap, head_only: bool) -> Option<Response<Body>> {
    let (bytes, content_type) = get(path)?;
    let etag = HeaderValue::from_static(ETAG);
    if matches_etag(headers) {
        let mut response = Response::new(Body::empty());
        *response.status_mut() = StatusCode::NOT_MODIFIED;
        let out = response.headers_mut();
        out.insert(ETAG_HEADER, etag);
        out.insert(CACHE_CONTROL, HeaderValue::from_static("no-cache"));
        return Some(response);
    }
    let body = if head_only {
        Body::empty()
    } else {
        Body::from(bytes)
    };
    let mut response = Response::new(body);
    *response.status_mut() = StatusCode::OK;
    let out = response.headers_mut();
    out.insert(CONTENT_TYPE, HeaderValue::from_static(content_type));
    out.insert(ETAG_HEADER, etag);
    out.insert(CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    Some(response)
}

fn matches_etag(headers: &HeaderMap) -> bool {
    headers
        .get_all(IF_NONE_MATCH)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .map(str::trim)
        .any(|tag| tag == "*" || tag == ETAG || tag.strip_prefix("W/") == Some(ETAG))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn embedded_set_contains_ui_and_excludes_design_and_tests() {
        assert!(get("/").is_some());
        assert!(get("/index.html").is_some());
        assert!(get("/styles.css").is_some());
        assert!(get("/app.js").is_some());
        assert!(get("/manifest.webmanifest").is_some());
        assert!(get("/icons/legion.svg").is_some());
        assert!(get("/design/prototype.html").is_none());
        assert!(get("/lib/test/format.test.mjs").is_none());
        assert!(get("/vendor/README.md").is_none());
        assert!(get("/../node.toml").is_none());
        assert!(get("/lib/").is_none());
    }

    #[test]
    fn conditional_get_returns_not_modified_for_matching_etag() {
        let mut headers = HeaderMap::new();
        headers.insert(IF_NONE_MATCH, HeaderValue::from_static(ETAG));
        let reply = serve("/", &headers, false).unwrap();
        assert_eq!(reply.status(), StatusCode::NOT_MODIFIED);
        assert_eq!(reply.headers().get(ETAG_HEADER).unwrap(), ETAG);
    }
}
