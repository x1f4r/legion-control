//! What a request must show before anything behind the listener sees it: a `Host` this server
//! answers to, an `Origin` it trusts if it names one, and a token.

use std::net::IpAddr;

use axum::http::header::{AUTHORIZATION, HOST, ORIGIN};
use axum::http::{HeaderMap, Uri};
use legion_core::config::ServeConfig;
use legion_core::tokens::Tokens;

use crate::Authenticated;

/// Why a request's token will not do.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Refusal {
    /// No token.
    NoToken,
    /// A token that is not one, or no longer one. Which of the two is not said.
    BadToken,
}

/// Why a request is not well formed enough to judge, when it is not: every header the gate
/// decides by is given once at most, so what is checked is what is used, and the host is named
/// once, as text, in `Host` and in nothing that disagrees with it.
pub(crate) fn malformed(headers: &HeaderMap, uri: &Uri) -> Option<&'static str> {
    for (name, message) in [
        (HOST, "The request names its host more than once."),
        (ORIGIN, "The request gives more than one Origin."),
        (
            AUTHORIZATION,
            "The request gives more than one Authorization.",
        ),
    ] {
        if headers.get_all(name).iter().nth(1).is_some() {
            return Some(message);
        }
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

/// Whether the request's `Host` is this server: a loopback name or address with the port it
/// listens on, or one of the names `[serve] hosts` gives. A request naming no host is refused,
/// since a client that names none cannot be told apart from one that names the wrong one.
/// [`malformed`] has already made sure there is one `Host` and nothing disagreeing with it.
pub(crate) fn host_allowed(headers: &HeaderMap, port: u16, config: &ServeConfig) -> bool {
    let named = headers.get(HOST).and_then(|value| value.to_str().ok());
    let Some((host, given_port)) = named.and_then(split_authority) else {
        return false;
    };
    let loopback = host == "localhost"
        || host
            .parse::<IpAddr>()
            .is_ok_and(|address| address.is_loopback());
    if loopback && given_port == Some(port) {
        return true;
    }
    config
        .hosts
        .iter()
        .any(|allowed| match split_authority(allowed) {
            Some((name, None)) => name == host,
            Some((name, Some(allowed_port))) => name == host && given_port == Some(allowed_port),
            None => false,
        })
}

/// Whether a request's `Origin`, if it has one, is listed in `[serve] origins`. A request
/// without one did not come from a browser page, and the token is what guards it.
pub(crate) fn origin_allowed(headers: &HeaderMap, config: &ServeConfig) -> bool {
    let Some(origin) = headers.get(ORIGIN) else {
        return true;
    };
    let Ok(origin) = origin.to_str() else {
        return false;
    };
    let origin = normalise_origin(origin);
    config
        .origins
        .iter()
        .any(|allowed| normalise_origin(allowed) == origin)
}

/// Who the request's bearer token says it is.
pub(crate) fn principal(headers: &HeaderMap, tokens: &Tokens) -> Result<Authenticated, Refusal> {
    let Some(value) = headers.get(AUTHORIZATION) else {
        return Err(Refusal::NoToken);
    };
    let value = value.to_str().map_err(|_| Refusal::BadToken)?;
    let (scheme, token) = value.trim().split_once(' ').ok_or(Refusal::BadToken)?;
    if !scheme.eq_ignore_ascii_case("bearer") {
        return Err(Refusal::BadToken);
    }
    tokens
        .verify_entry(token.trim(), jiff::Timestamp::now())
        .map(|(name, entry)| Authenticated {
            principal: entry.principal.as_str().to_owned(),
            token: name.to_owned(),
            hash: entry.hash.clone(),
            expires: entry
                .expires
                .as_deref()
                .and_then(|expires| legion_core::tokens::parse_time(expires).ok()),
        })
        .ok_or(Refusal::BadToken)
}

/// `host` and `port` from `host[:port]` or `[v6][:port]`, the host in lower case and without a
/// trailing dot. `None` when it is not one.
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
            // Two colons and no brackets is a bare IPv6 address, which a Host header never is.
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
    use axum::http::HeaderValue;

    use super::*;

    fn headers(pairs: &[(&'static str, &str)]) -> HeaderMap {
        let mut headers = HeaderMap::new();
        for (name, value) in pairs {
            headers.insert(*name, HeaderValue::from_str(value).unwrap());
        }
        headers
    }

    fn config(hosts: &[&str], origins: &[&str]) -> ServeConfig {
        ServeConfig {
            hosts: hosts.iter().map(|host| (*host).to_owned()).collect(),
            origins: origins.iter().map(|origin| (*origin).to_owned()).collect(),
        }
    }

    fn host_ok(host: &str, config: &ServeConfig) -> bool {
        host_allowed(&headers(&[("host", host)]), 7117, config)
    }

    #[test]
    fn loopback_with_the_port_is_this_server() {
        let none = config(&[], &[]);
        for host in [
            "127.0.0.1:7117",
            "localhost:7117",
            "LOCALHOST:7117",
            "[::1]:7117",
            "127.1.2.3:7117",
        ] {
            assert!(host_ok(host, &none), "{host}");
        }
        for host in [
            "127.0.0.1",
            "127.0.0.1:80",
            "localhost:7118",
            "evil.example:7117",
            "evil.example",
            "",
            "::1",
            "[::1]",
            "127.0.0.1:x",
        ] {
            assert!(!host_ok(host, &none), "{host}");
        }
        assert!(!host_allowed(&HeaderMap::new(), 7117, &none));
    }

    #[test]
    fn a_request_names_its_host_once_and_each_deciding_header_once() {
        let target = Uri::from_static("/v1/tools");
        let host = || headers(&[("host", "127.0.0.1:7117")]);
        assert_eq!(malformed(&host(), &target), None);
        assert!(malformed(&HeaderMap::new(), &target).is_some());
        for name in [HOST, ORIGIN, AUTHORIZATION] {
            let mut twice = host();
            twice.append(name.clone(), HeaderValue::from_static("a"));
            twice.append(name.clone(), HeaderValue::from_static("b"));
            assert!(malformed(&twice, &target).is_some(), "{name}");
        }
        let mut binary = HeaderMap::new();
        binary.insert(
            HOST,
            HeaderValue::from_bytes(b"127.0.0.1:7117\xff").unwrap(),
        );
        assert!(malformed(&binary, &target).is_some());
        let absolute = Uri::from_static("http://evil.example:7117/v1/tools");
        assert!(malformed(&host(), &absolute).is_some());
        let same = Uri::from_static("http://127.0.0.1:7117/v1/tools");
        assert_eq!(malformed(&host(), &same), None);
    }

    #[test]
    fn listed_hosts_match_by_name_or_by_name_and_port() {
        let listed = config(&["tower.example.ts.net", "pi.example.net:8443"], &[]);
        assert!(host_ok("tower.example.ts.net", &listed));
        assert!(host_ok("Tower.Example.ts.net.:8443", &listed));
        assert!(host_ok("pi.example.net:8443", &listed));
        assert!(!host_ok("pi.example.net", &listed));
        assert!(!host_ok("pi.example.net:443", &listed));
        assert!(!host_ok("ts.net", &listed));
    }

    #[test]
    fn an_origin_is_refused_unless_listed() {
        let none = config(&[], &[]);
        let listed = config(&[], &["https://app.example.net"]);
        assert!(origin_allowed(&HeaderMap::new(), &none));
        let from = |origin: &str| headers(&[("origin", origin)]);
        assert!(!origin_allowed(&from("https://evil.example"), &none));
        assert!(!origin_allowed(&from("null"), &none));
        assert!(origin_allowed(&from("https://app.example.net"), &listed));
        assert!(origin_allowed(&from("HTTPS://APP.example.net/"), &listed));
        assert!(!origin_allowed(
            &from("https://app.example.net:8443"),
            &listed
        ));
    }

    #[test]
    fn only_a_bearer_token_is_read() {
        let tokens = Tokens::default();
        assert_eq!(principal(&HeaderMap::new(), &tokens), Err(Refusal::NoToken));
        for value in ["Basic abc", "Bearer", "lgn_abc", "Bearer lgn_abc"] {
            assert_eq!(
                principal(&headers(&[("authorization", value)]), &tokens),
                Err(Refusal::BadToken),
                "{value}"
            );
        }
    }
}
