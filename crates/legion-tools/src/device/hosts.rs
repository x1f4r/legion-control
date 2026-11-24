//! Host detection for pairing invites: explicit overrides, Tailscale self addresses, or hostname.

use std::time::Duration;

use legion_core::CancellationToken;
use legion_core::proto::{ErrorBody, ErrorCode};
use legion_os::run::{RunOptions, run};
use legion_os::shell::CommandSpec;
use serde::Deserialize;

/// Timeout for querying `tailscale status --json` during `device.invite`.
const TAILSCALE_TIMEOUT: Duration = Duration::from_secs(5);

/// Maximum length of a single host name or address entry.
const MAX_HOST_LEN: usize = 255;

/// Optional `host` argument of `device.invite`: either one host string or a list of host strings.
#[derive(Clone, Debug, Deserialize)]
#[serde(untagged)]
pub(crate) enum HostArg {
    /// A single host name or address.
    One(String),
    /// Multiple host names or addresses, in preferred order.
    Many(Vec<String>),
}

impl HostArg {
    fn into_vec(self) -> Vec<String> {
        match self {
            HostArg::One(host) => vec![host],
            HostArg::Many(hosts) => hosts,
        }
    }
}

/// Resolves the list of host names / addresses placed in an invite.
///
/// When `explicit` is provided, validates and uses those hosts verbatim.
/// Otherwise, if `tailscale` is on `PATH` and `tailscale status --json` succeeds while running,
/// returns the node's own MagicDNS name (trailing dot stripped) followed by its Tailscale IPs.
/// Failing that, falls back to the system hostname and never enumerates other local interfaces.
pub(crate) async fn resolve_hosts(
    explicit: Option<HostArg>,
    cancel: CancellationToken,
) -> Result<Vec<String>, ErrorBody> {
    if let Some(arg) = explicit {
        return validate_explicit_hosts(arg.into_vec());
    }

    let options = RunOptions::new(CommandSpec::argv("tailscale", ["status", "--json"]))
        .c_locale()
        .timeout(TAILSCALE_TIMEOUT)
        .cancel(cancel);

    if let Ok(output) = run(options).await
        && !output.cancelled
        && !output.timed_out
        && output.success()
    {
        let tailscale_hosts = parse_tailscale_self_hosts(&output.stdout.text);
        if !tailscale_hosts.is_empty() {
            return Ok(tailscale_hosts);
        }
    }

    Ok(vec![system_hostname()])
}

/// Validates explicitly provided `host` entries and deduplicates them in order.
pub(crate) fn validate_explicit_hosts(raw_hosts: Vec<String>) -> Result<Vec<String>, ErrorBody> {
    let mut out = Vec::new();
    for raw in raw_hosts {
        let host = raw.trim();
        if host.is_empty() {
            return Err(
                ErrorBody::new(ErrorCode::InvalidArgs, "host entries must not be empty.")
                    .with_hint(
                        "Give a host name or IP address such as `tower.example` or `100.64.0.1`.",
                    ),
            );
        }
        if host.len() > MAX_HOST_LEN || host.chars().any(|c| c.is_control() || c.is_whitespace()) {
            return Err(ErrorBody::new(
                ErrorCode::InvalidArgs,
                format!("{host:?} is not a valid host name or address."),
            )
            .with_hint("Use a host name or IP address with no spaces or control characters."));
        }
        let owned = host.to_owned();
        if !out.contains(&owned) {
            out.push(owned);
        }
    }
    if out.is_empty() {
        return Err(ErrorBody::new(
            ErrorCode::InvalidArgs,
            "At least one host is required when `host` is given.",
        )
        .with_hint(
            "Omit `host` to detect it automatically, or pass a host name or list of hosts.",
        ));
    }
    Ok(out)
}

/// Extracts this node's own MagicDNS name (trailing dot removed) and Tailscale IP addresses from
/// `tailscale status --json` output. Returns an empty list if Tailscale is not running or has no
/// usable self addresses.
pub fn parse_tailscale_self_hosts(json: &str) -> Vec<String> {
    #[derive(Deserialize)]
    struct Status {
        #[serde(rename = "BackendState", default)]
        backend_state: Option<String>,
        #[serde(rename = "Self", default)]
        self_node: Option<SelfNode>,
    }

    #[derive(Deserialize)]
    struct SelfNode {
        #[serde(rename = "DNSName", default)]
        dns_name: String,
        #[serde(rename = "TailscaleIPs", default)]
        tailscale_ips: Vec<String>,
    }

    let Ok(status) = serde_json::from_str::<Status>(json) else {
        return Vec::new();
    };
    if let Some(state) = status.backend_state.as_deref()
        && !state.eq_ignore_ascii_case("Running")
    {
        return Vec::new();
    }
    let Some(self_node) = status.self_node else {
        return Vec::new();
    };

    let mut hosts = Vec::new();
    let dns = self_node.dns_name.trim().trim_end_matches('.');
    if !dns.is_empty() && !dns.chars().any(|c| c.is_control() || c.is_whitespace()) {
        hosts.push(dns.to_owned());
    }
    for raw_ip in self_node.tailscale_ips {
        let ip = raw_ip.trim();
        if !ip.is_empty()
            && !ip.chars().any(|c| c.is_control() || c.is_whitespace())
            && !hosts.iter().any(|existing| existing == ip)
        {
            hosts.push(ip.to_owned());
        }
    }
    hosts
}

fn system_hostname() -> String {
    let raw = gethostname::gethostname().to_string_lossy().into_owned();
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed.chars().any(|c| c.is_control() || c.is_whitespace()) {
        "localhost".to_owned()
    } else {
        trimmed.to_owned()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn parses_tailscale_self_magicdns_and_ips_and_ignores_stopped_state() {
        let running = r#"{
            "BackendState": "Running",
            "Self": {
                "DNSName": "tower.tailnet.example.",
                "TailscaleIPs": ["100.64.0.10", "fd7a:115c:a1e0::10"]
            }
        }"#;
        assert_eq!(
            parse_tailscale_self_hosts(running),
            vec!["tower.tailnet.example", "100.64.0.10", "fd7a:115c:a1e0::10",]
        );

        let stopped = r#"{
            "BackendState": "Stopped",
            "Self": {
                "DNSName": "tower.tailnet.example.",
                "TailscaleIPs": ["100.64.0.10"]
            }
        }"#;
        assert!(parse_tailscale_self_hosts(stopped).is_empty());

        let ips_only = r#"{
            "BackendState": "Running",
            "Self": {
                "DNSName": "",
                "TailscaleIPs": ["100.64.0.10", "100.64.0.10"]
            }
        }"#;
        assert_eq!(parse_tailscale_self_hosts(ips_only), vec!["100.64.0.10"]);
    }

    #[test]
    fn explicit_host_validation_deduplicates_and_rejects_bad_values() {
        assert_eq!(
            validate_explicit_hosts(vec![
                "tower.example".to_owned(),
                "192.0.2.10".to_owned(),
                "tower.example".to_owned(),
            ])
            .unwrap(),
            vec!["tower.example", "192.0.2.10"]
        );
        assert!(validate_explicit_hosts(vec![]).is_err());
        assert!(validate_explicit_hosts(vec!["  ".to_owned()]).is_err());
        assert!(validate_explicit_hosts(vec!["bad host".to_owned()]).is_err());
        assert!(validate_explicit_hosts(vec!["bad\nhost".to_owned()]).is_err());
    }
}
