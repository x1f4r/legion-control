//! Integration test against a real OpenSSH `sshd` forcing `legion gate --as <principal>`.
//!
//! Ignored by default; run via `scripts/link-sshd-test.sh` or by setting:
//! - `LEGION_LINK_SSHD=host:port`
//! - `LEGION_LINK_USER=<ssh account>`
//! - `LEGION_LINK_HOST_KEY=<OpenSSH public host key line>`
//! - `LEGION_LINK_KEY_FILE=<path to OpenSSH private key file>`

#![allow(clippy::unwrap_used)]

use std::env;
use std::fs;

use legion_link::proto::{ErrorCode, PROTOCOL};
use legion_link::{
    CancellationToken, DeviceKey, Link, LinkConfig, NodeLink, REASON_AUTH_FAILED,
    REASON_HOST_KEY_MISMATCH, SshTarget, SshTimeouts, Subcommand, ssh,
};
use serde_json::json;

fn env_target() -> Option<(SshTarget, DeviceKey, String)> {
    let addr = env::var("LEGION_LINK_SSHD").ok()?;
    let (host, port_str) = addr.rsplit_once(':')?;
    let port: u16 = port_str.parse().ok()?;
    let user = env::var("LEGION_LINK_USER").ok()?;
    let host_key = env::var("LEGION_LINK_HOST_KEY").ok()?;
    let key_file = env::var("LEGION_LINK_KEY_FILE").ok()?;
    let key_pem = fs::read_to_string(&key_file).unwrap();
    let key = DeviceKey::from_openssh(&key_pem).unwrap();
    let node = env::var("LEGION_LINK_NODE").unwrap_or_else(|_| "tower".to_owned());
    let target = SshTarget {
        hosts: vec![host.to_owned()],
        port,
        user,
        host_keys: vec![host_key],
    };
    Some((target, key, node))
}

#[tokio::test]
#[ignore = "requires LEGION_LINK_SSHD, LEGION_LINK_USER, LEGION_LINK_HOST_KEY, LEGION_LINK_KEY_FILE"]
async fn real_sshd_session_and_security_checks() {
    let Some((target, key, node)) = env_target() else {
        panic!(
            "Set LEGION_LINK_SSHD, LEGION_LINK_USER, LEGION_LINK_HOST_KEY, and LEGION_LINK_KEY_FILE (or run scripts/link-sshd-test.sh)."
        );
    };

    // 1. Host key mismatch is rejected cleanly and names the offered SHA-256 fingerprint.
    let wrong_host_key = DeviceKey::from_seed([99u8; 32])
        .public_key_line("wrong")
        .unwrap();
    let bad_pin = SshTarget {
        host_keys: vec![wrong_host_key],
        ..target.clone()
    };
    let mismatch_err = ssh::open(
        &bad_pin,
        &key,
        Subcommand::Rpc,
        SshTimeouts::default(),
        CancellationToken::new(),
    )
    .await
    .err()
    .unwrap();
    assert_eq!(mismatch_err.code, ErrorCode::Denied, "{mismatch_err}");
    assert!(
        mismatch_err.message.contains("SHA256:"),
        "{}",
        mismatch_err.message
    );
    assert_eq!(
        mismatch_err.details.as_ref().unwrap()["reason"],
        json!(REASON_HOST_KEY_MISMATCH)
    );

    // 2. Unauthorized client key is rejected with auth_failed.
    let rogue_key = DeviceKey::generate().unwrap();
    let auth_err = ssh::open(
        &target,
        &rogue_key,
        Subcommand::Rpc,
        SshTimeouts::default(),
        CancellationToken::new(),
    )
    .await
    .err()
    .unwrap();
    assert_eq!(auth_err.code, ErrorCode::Denied, "{auth_err}");
    assert_eq!(
        auth_err.details.as_ref().unwrap()["reason"],
        json!(REASON_AUTH_FAILED)
    );

    // 3. Valid key + pinned host key opens a Link session and executes tools through `legion gate`.
    let config = LinkConfig {
        device_key: Some(key.to_openssh().unwrap()),
        nodes: vec![NodeLink::ssh(&node, target, Some("phone".to_owned()))],
    };
    let link = Link::new(config);
    let hello = link.hello(&node).await.unwrap();
    assert_eq!(hello["protocol"], json!(PROTOCOL));
    assert_eq!(hello["node"], json!(node));

    let tools = link.tools(&node, None).await.unwrap();
    let list = tools["tools"].as_array().unwrap();
    assert!(
        list.iter().any(|t| t["name"] == json!("sys.info")),
        "sys.info missing from tools list"
    );

    let info = link
        .call(&node, "sys.info", json!({}), None, |_| {})
        .await
        .unwrap();
    assert!(
        info.get("os").and_then(|o| o.get("family")).is_some(),
        "{info}"
    );
    assert!(
        info.get("os").and_then(|o| o.get("arch")).is_some(),
        "{info}"
    );

    link.close().await;
}
