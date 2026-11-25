//! Reusable `Host` and `Origin` guard shared by content-serving listeners (the HTTP door and
//! native app web views).

use std::net::IpAddr;

use http::header::{HOST, ORIGIN};
use http::{HeaderMap, HeaderValue, Uri};

/// Why [`HostOriginGuard::check`] refused a request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GuardRejection {
    /// Required headers were missing, repeated, non-ASCII, or contradicted the request URI.
    Malformed(&'static str),
    /// The `Host` header did not match this listener's loopback port or any configured host.
    MisdirectedHost,
    /// The `Origin` header was neither listed nor allowed as same-origin.
    ForbiddenOrigin,
}

/// Validates `Host` and `Origin` headers on every request before a ticket or token is touched.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostOriginGuard {
    /// Loopback port this listener is bound to (`127.0.0.1:<port>`, `localhost:<port>`, `[::1]:<port>`).
    pub port: u16,
    /// Extra allowed host names (with optional `:port`), such as a Tailscale Serve name.
    pub allowed_hosts: Vec<String>,
    /// Allowed browser page origins (such as `https://app.example.net` or `tauri://localhost`).
    pub allowed_origins: Vec<String>,
    /// Whether an `Origin` matching the already-validated `Host` header is accepted without being
    /// listed in [`Self::allowed_origins`] (for example when the server also serves its own UI).
    pub allow_same_origin: bool,
}

impl HostOriginGuard {
    /// Creates a loopback-only guard bound to `port` with no extra hosts or origins.
    pub fn loopback(port: u16) -> Self {
        Self {
            port,
            allowed_hosts: Vec::new(),
            allowed_origins: Vec::new(),
            allow_same_origin: false,
        }
    }

    /// Checks header well-formedness, `Host`, and `Origin` in order.
    ///
    /// Returns `Ok(Some(origin))` when `Origin` matched [`Self::allowed_origins`] (so callers can
    /// emit CORS headers), or `Ok(None)` when `Origin` was absent or admitted via
    /// [`Self::allow_same_origin`].
    pub fn check(
        &self,
        headers: &HeaderMap,
        uri: &Uri,
    ) -> Result<Option<HeaderValue>, GuardRejection> {
        if let Some(reason) = Self::malformed(headers, uri) {
            return Err(GuardRejection::Malformed(reason));
        }
        if !self.host_allowed(headers) {
            return Err(GuardRejection::MisdirectedHost);
        }
        if !self.origin_allowed(headers) {
            return Err(GuardRejection::ForbiddenOrigin);
        }
        Ok(self.listed_origin(headers))
    }

    /// Verifies that `Host` is present once, valid text, consistent with `uri`'s authority, and
    /// that `Origin` appears at most once.
    pub fn malformed(headers: &HeaderMap, uri: &Uri) -> Option<&'static str> {
        if headers.get_all(HOST).iter().nth(1).is_some() {
            return Some("The request names its host more than once.");
        }
        if headers.get_all(ORIGIN).iter().nth(1).is_some() {
            return Some("The request gives more than one Origin.");
        }
        let Some(host) = headers.get(HOST) else {
            return Some("The request has no Host header.");
        };
        let Ok(host) = host.to_str() else {
            return Some("The request's Host header is not text.");
        };
        if let Some(authority) = uri.authority()
            && !authority.as_str().eq_ignore_ascii_case(host.trim())
        {
            return Some("The request names one host in its target and another in Host.");
        }
        None
    }

    /// Whether `Host` names this listener on loopback with [`Self::port`], or an entry in
    /// [`Self::allowed_hosts`].
    pub fn host_allowed(&self, headers: &HeaderMap) -> bool {
        let named = headers.get(HOST).and_then(|value| value.to_str().ok());
        let Some((host, given_port)) = named.and_then(split_authority) else {
            return false;
        };
        let loopback = host == "localhost"
            || host
                .parse::<IpAddr>()
                .is_ok_and(|address| address.is_loopback());
        if loopback && given_port == Some(self.port) {
            return true;
        }
        self.allowed_hosts
            .iter()
            .any(|allowed| match split_authority(allowed) {
                Some((name, None)) => name == host,
                Some((name, Some(allowed_port))) => {
                    name == host && given_port == Some(allowed_port)
                }
                None => false,
            })
    }

    /// Whether `Origin`, if present, is allowed (listed in [`Self::allowed_origins`] or, when
    /// [`Self::allow_same_origin`] is true, same-origin with `Host`). Requests without `Origin`
    /// are admitted.
    pub fn origin_allowed(&self, headers: &HeaderMap) -> bool {
        let Some(origin) = headers.get(ORIGIN) else {
            return true;
        };
        let Ok(origin) = origin.to_str() else {
            return false;
        };
        if self.is_listed_origin(origin) {
            return true;
        }
        self.allow_same_origin && is_same_origin(headers, origin)
    }

    /// Returns the request's `Origin` header value when it explicitly appears in
    /// [`Self::allowed_origins`].
    pub fn listed_origin(&self, headers: &HeaderMap) -> Option<HeaderValue> {
        let origin = headers.get(ORIGIN)?;
        let text = origin.to_str().ok()?;
        self.is_listed_origin(text).then(|| origin.clone())
    }

    fn is_listed_origin(&self, origin: &str) -> bool {
        let origin = normalise_origin(origin);
        self.allowed_origins
            .iter()
            .any(|allowed| normalise_origin(allowed) == origin)
    }
}

fn is_same_origin(headers: &HeaderMap, origin: &str) -> bool {
    let Some(host_header) = headers.get(HOST).and_then(|v| v.to_str().ok()) else {
        return false;
    };
    let Some((req_host, req_port)) = split_authority(host_header) else {
        return false;
    };
    let trimmed = origin.trim();
    let (default_port, rest) = if let Some(rest) = trimmed.strip_prefix("https://") {
        (443u16, rest)
    } else if let Some(rest) = trimmed.strip_prefix("http://") {
        (80u16, rest)
    } else {
        return false;
    };
    if rest.contains(['/', '?', '#', '@']) {
        return false;
    }
    let Some((orig_host, orig_port)) = split_authority(rest) else {
        return false;
    };
    if orig_host != req_host {
        return false;
    }
    match req_port {
        Some(port) => orig_port.unwrap_or(default_port) == port,
        None => orig_port.is_none() || orig_port == Some(default_port),
    }
}

/// Splits `host[:port]` or `[v6][:port]` into lower-cased host (without trailing dot) and optional
/// port.
fn split_authority(text: &str) -> Option<(String, Option<u16>)> {
    let text = text.trim();
    let (host, port) = if let Some(rest) = text.strip_prefix('[') {
        let (host, after) = rest.split_once(']')?;
        match after {
            "" => (host, None),
            after => (host, Some(after.strip_prefix(':')?)),
        }
    } else {
        match text.rsplit_once(':') {
            // Two colons without brackets is a bare IPv6 address, invalid in Host.
            Some((host, _)) if host.contains(':') => return None,
            Some((host, port)) => (host, Some(port)),
            None => (text, None),
        }
    };
    let port = match port {
        Some(port) => Some(port.parse::<u16>().ok()?),
        None => None,
    };
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    if host.is_empty() {
        return None;
    }
    Some((host, port))
}

fn normalise_origin(origin: &str) -> String {
    origin.trim().trim_end_matches('/').to_ascii_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(pairs: &[(&'static str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (k, v) in pairs {
            map.insert(*k, HeaderValue::from_str(v).unwrap());
        }
        map
    }

    #[test]
    fn guards_loopback_hosts_and_tauri_or_same_origin() {
        let guard = HostOriginGuard {
            port: 7117,
            allowed_hosts: vec!["tower.example.ts.net".to_owned()],
            allowed_origins: vec![
                "tauri://localhost".to_owned(),
                "https://tauri.localhost".to_owned(),
            ],
            allow_same_origin: true,
        };
        let uri = Uri::from_static("/v1/content/abc/photo.jpg");

        // No Origin (media element) on loopback: admitted without CORS echo.
        assert_eq!(
            guard.check(&headers(&[("host", "127.0.0.1:7117")]), &uri),
            Ok(None)
        );
        // Same-origin fetch from UI: admitted without CORS echo.
        assert_eq!(
            guard.check(
                &headers(&[
                    ("host", "tower.example.ts.net"),
                    ("origin", "https://tower.example.ts.net"),
                ]),
                &uri
            ),
            Ok(None)
        );
        // Listed Tauri web view origin: admitted with CORS echo.
        assert_eq!(
            guard.check(
                &headers(&[("host", "127.0.0.1:7117"), ("origin", "tauri://localhost"),]),
                &uri
            ),
            Ok(Some(HeaderValue::from_static("tauri://localhost")))
        );
        // Unlisted third-party origin or null origin: rejected 403.
        assert_eq!(
            guard.check(
                &headers(&[
                    ("host", "127.0.0.1:7117"),
                    ("origin", "https://evil.example")
                ]),
                &uri
            ),
            Err(GuardRejection::ForbiddenOrigin)
        );
        assert_eq!(
            guard.check(
                &headers(&[("host", "127.0.0.1:7117"), ("origin", "null")]),
                &uri
            ),
            Err(GuardRejection::ForbiddenOrigin)
        );
        // DNS rebinding host: rejected 421.
        assert_eq!(
            guard.check(&headers(&[("host", "evil.example:7117")]), &uri),
            Err(GuardRejection::MisdirectedHost)
        );
        // Missing Host or duplicate Origin: malformed 400.
        assert!(matches!(
            guard.check(&HeaderMap::new(), &uri),
            Err(GuardRejection::Malformed(_))
        ));
    }
}
