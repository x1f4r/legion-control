//! Integration test against a real OpenSSH `sshd` forcing `legion gate --as <principal>`.
//!
//! Ignored by default; run via `scripts/link-sshd-test.sh` or by setting:
//! - `LEGION_LINK_SSHD=host:port`
//! - `LEGION_LINK_USER=<ssh account>`
//! - `LEGION_LINK_HOST_KEY=<OpenSSH public host key line (ed25519)>`
//! - `LEGION_LINK_HOST_KEY_ECDSA=<optional OpenSSH ecdsa host key line on the same server>`
//! - `LEGION_LINK_MISMATCH_HOST=<optional loopback host on the same port with a different host key>`
//! - `LEGION_LINK_KEY_FILE=<path to OpenSSH private key file>`

#![allow(clippy::unwrap_used)]

use std::env;
use std::fs;
use std::process::Command;
use std::time::Duration;

use legion_link::proto::{ErrorCode, PROTOCOL};
use legion_link::{
    CancellationToken, DeviceKey, Invite, Link, LinkConfig, NodeLink, REASON_AUTH_FAILED,
    REASON_DEVICE_REMOVED_OR_UNKNOWN, REASON_HOST_KEY_MISMATCH, SshTarget, SshTimeouts, Subcommand,
    ssh,
};
use serde_json::json;

struct TestEnv {
    host: String,
    port: u16,
    user: String,
    ed25519_host_key: String,
    ecdsa_host_key: Option<String>,
    mismatch_host: Option<String>,
    auth_keys_path: Option<String>,
    enrol_script: Option<String>,
    key: DeviceKey,
    node: String,
}

fn env_target() -> Option<TestEnv> {
    let addr = env::var("LEGION_LINK_SSHD").ok()?;
    let (host, port_str) = addr.rsplit_once(':')?;
    let port: u16 = port_str.parse().ok()?;
    let user = env::var("LEGION_LINK_USER").ok()?;
    let ed25519_host_key = env::var("LEGION_LINK_HOST_KEY").ok()?;
    let ecdsa_host_key = env::var("LEGION_LINK_HOST_KEY_ECDSA")
        .ok()
        .filter(|s| !s.trim().is_empty());
    let mismatch_host = env::var("LEGION_LINK_MISMATCH_HOST")
        .ok()
        .filter(|s| !s.trim().is_empty());
    let auth_keys_path = env::var("LEGION_LINK_AUTH_KEYS")
        .ok()
        .filter(|s| !s.trim().is_empty());
    let enrol_script = env::var("LEGION_LINK_ENROL_SCRIPT")
        .ok()
        .filter(|s| !s.trim().is_empty());
    let key_file = env::var("LEGION_LINK_KEY_FILE").ok()?;
    let key_pem = fs::read_to_string(&key_file).unwrap();
    let key = DeviceKey::from_openssh(&key_pem).unwrap();
    let node = env::var("LEGION_LINK_NODE").unwrap_or_else(|_| "tower".to_owned());
    Some(TestEnv {
        host: host.to_owned(),
        port,
        user,
        ed25519_host_key,
        ecdsa_host_key,
        mismatch_host,
        auth_keys_path,
        enrol_script,
        key,
        node,
    })
}

fn stale_ecdsa_key() -> String {
    // Valid NIST P-256 generator public key (never matches the server's generated host key).
    let sec1 = [
        0x04u8, 0x6b, 0x17, 0xd1, 0xf2, 0xe1, 0x2c, 0x42, 0x47, 0xf8, 0xbc, 0xe6, 0xe5, 0x63, 0xa4,
        0x40, 0xf2, 0x77, 0x03, 0x7d, 0x81, 0x2d, 0xeb, 0x33, 0xa0, 0xf4, 0xa1, 0x39, 0x45, 0xd8,
        0x98, 0xc2, 0x96, 0x4f, 0xe3, 0x42, 0xe2, 0xfe, 0x1a, 0x7f, 0x9b, 0x8e, 0xe7, 0xeb, 0x4a,
        0x7c, 0x0f, 0x9e, 0x16, 0x2b, 0xce, 0x33, 0x57, 0x6b, 0x31, 0x5e, 0xce, 0xcb, 0xb6, 0x40,
        0x68, 0x37, 0xbf, 0x51, 0xf5,
    ];
    // Encode RFC 4253 ecdsa-sha2-nistp256 key blob cleanly without reaching into russh internals.
    use base64::Engine;
    use base64::engine::general_purpose::STANDARD;
    let mut blob = Vec::new();
    for part in [
        b"ecdsa-sha2-nistp256".as_slice(),
        b"nistp256".as_slice(),
        &sec1,
    ] {
        blob.extend_from_slice(&(part.len() as u32).to_be_bytes());
        blob.extend_from_slice(part);
    }
    format!("ecdsa-sha2-nistp256 {}", STANDARD.encode(blob))
}

#[tokio::test]
#[ignore = "requires LEGION_LINK_SSHD, LEGION_LINK_USER, LEGION_LINK_HOST_KEY, LEGION_LINK_KEY_FILE"]
async fn real_sshd_session_and_security_checks() {
    let Some(env) = env_target() else {
        panic!(
            "Set LEGION_LINK_SSHD, LEGION_LINK_USER, LEGION_LINK_HOST_KEY, and LEGION_LINK_KEY_FILE (or run scripts/link-sshd-test.sh)."
        );
    };
    let target = SshTarget {
        hosts: vec![env.host.clone()],
        port: env.port,
        user: env.user.clone(),
        host_keys: vec![env.ed25519_host_key.clone()],
    };

    // 1. Host key mismatch is rejected cleanly, names the node, and reports SHA-256 fingerprint.
    let stale_ed25519 = DeviceKey::from_seed([99u8; 32])
        .public_key_line("wrong")
        .unwrap();
    let bad_pin = SshTarget {
        host_keys: vec![stale_ed25519.clone()],
        ..target.clone()
    };
    let mismatch_err = ssh::open(
        &env.node,
        &bad_pin,
        &env.key,
        Subcommand::Rpc,
        SshTimeouts::default(),
        CancellationToken::new(),
    )
    .await
    .err()
    .unwrap();
    assert_eq!(mismatch_err.code, ErrorCode::Denied, "{mismatch_err}");
    assert_eq!(mismatch_err.machine.as_deref(), Some(env.node.as_str()));
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
        &env.node,
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
    assert_eq!(auth_err.machine.as_deref(), Some(env.node.as_str()));
    assert_eq!(
        auth_err.details.as_ref().unwrap()["reason"],
        json!(REASON_AUTH_FAILED)
    );

    // 3. Multi-key server (ed25519 + ecdsa): single-key pin for each type, plus mixed stale+valid
    //    pins in both orders (canonical ed25519 preference AND retry-once after ed25519 mismatch).
    if let Some(ecdsa_key) = &env.ecdsa_host_key {
        let stale_ecdsa = stale_ecdsa_key();
        let pin_sets = [
            ("only-ed25519", vec![env.ed25519_host_key.clone()]),
            ("only-ecdsa", vec![ecdsa_key.clone()]),
            (
                "stale-ecdsa-then-valid-ed25519",
                vec![stale_ecdsa.clone(), env.ed25519_host_key.clone()],
            ),
            (
                "stale-ed25519-then-valid-ecdsa",
                vec![stale_ed25519.clone(), ecdsa_key.clone()],
            ),
            (
                "valid-ecdsa-then-stale-ed25519",
                vec![ecdsa_key.clone(), stale_ed25519.clone()],
            ),
        ];
        for (label, pins) in pin_sets {
            let multi_target = SshTarget {
                host_keys: pins,
                ..target.clone()
            };
            let session = ssh::open(
                &env.node,
                &multi_target,
                &env.key,
                Subcommand::Rpc,
                SshTimeouts::default(),
                CancellationToken::new(),
            )
            .await
            .unwrap_or_else(|err| panic!("pin set {label} failed: {err}"));
            assert_eq!(session.info().hello.node, env.node, "{label}");
            session.close().await;
        }
    }

    // 4. Host list whose first entry is dead falls through to the live host.
    let dead_first = SshTarget {
        hosts: vec!["127.0.0.2".to_owned(), env.host.clone()],
        ..target.clone()
    };
    let session = ssh::open(
        &env.node,
        &dead_first,
        &env.key,
        Subcommand::Rpc,
        SshTimeouts::default(),
        CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(session.info().hello.node, env.node);
    session.close().await;

    // 5. Host list whose first entry has a mismatching host key still tries the next host; when
    //    no host succeeds, host_key_mismatch takes priority over unreachable.
    if let Some(wrong_host) = &env.mismatch_host {
        let mismatch_then_good = SshTarget {
            hosts: vec![wrong_host.clone(), env.host.clone()],
            ..target.clone()
        };
        let session = ssh::open(
            &env.node,
            &mismatch_then_good,
            &env.key,
            Subcommand::Rpc,
            SshTimeouts::default(),
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(session.info().hello.node, env.node);
        session.close().await;

        let mismatch_then_dead = SshTarget {
            hosts: vec![wrong_host.clone(), "127.0.0.2".to_owned()],
            ..target.clone()
        };
        let err = ssh::open(
            &env.node,
            &mismatch_then_dead,
            &env.key,
            Subcommand::Rpc,
            SshTimeouts::default(),
            CancellationToken::new(),
        )
        .await
        .err()
        .unwrap();
        assert_eq!(err.code, ErrorCode::Denied, "{err}");
        assert_eq!(
            err.details.as_ref().unwrap()["reason"],
            json!(REASON_HOST_KEY_MISMATCH)
        );
    }

    // 6. Valid key + pinned host key opens a Link session, executes tools, and reconnects cleanly
    //    after the server-side `legion gate` session process is killed.
    let config = LinkConfig {
        device_key: Some(env.key.to_openssh().unwrap()),
        nodes: vec![NodeLink::ssh(&env.node, target, Some("phone".to_owned()))],
        ..LinkConfig::default()
    };
    let link = Link::new(config);
    let hello = link.hello(&env.node).await.unwrap();
    assert_eq!(hello["protocol"], json!(PROTOCOL));
    assert_eq!(hello["node"], json!(env.node));

    let tools = link.tools(&env.node, None).await.unwrap();
    let list = tools["tools"].as_array().unwrap();
    assert!(
        list.iter().any(|t| t["name"] == json!("sys.info")),
        "sys.info missing from tools list"
    );

    let info = link
        .call(&env.node, "sys.info", json!({}), None, |_| {})
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

    // Find and kill the live `legion gate --as phone` process, then verify Link reconnects.
    let procs = link
        .call(
            &env.node,
            "sys.procs",
            json!({"filter": "legion", "limit": 50}),
            None,
            |_| {},
        )
        .await
        .unwrap();
    let gate_pids: Vec<u64> = procs["procs"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|p| {
            p.get("cmd")
                .and_then(|c| c.as_str())
                .is_some_and(|c| c.contains("gate") && c.contains("phone"))
        })
        .filter_map(|p| p.get("pid").and_then(|id| id.as_u64()))
        .collect();
    assert!(
        !gate_pids.is_empty(),
        "could not find live `legion gate --as phone` process in {procs}"
    );
    for pid in &gate_pids {
        let _ = Command::new("kill").args(["-9", &pid.to_string()]).status();
    }
    tokio::time::sleep(Duration::from_millis(200)).await;

    let info_after = link
        .call(&env.node, "sys.info", json!({}), None, |_| {})
        .await
        .unwrap();
    assert!(
        info_after.get("os").and_then(|o| o.get("family")).is_some(),
        "{info_after}"
    );

    link.close().await;

    // 7. In-place `Link::pair` starting from an empty `LinkConfig`, live tool call without
    //    rebuilding `Link`, key removal detection (`device_removed_or_unknown`) on reconnect, and
    //    `Link::forget`.
    if let (Some(auth_keys_path), Some(enrol_script)) = (&env.auth_keys_path, &env.enrol_script) {
        use base64::Engine;
        use base64::engine::general_purpose::STANDARD;

        let original_auth_keys = fs::read_to_string(auth_keys_path).unwrap();
        let invite_seed = [42u8; 32];
        let invite_pub = DeviceKey::from_seed(invite_seed)
            .public_key_line("invite")
            .unwrap();
        let with_invite =
            format!("{original_auth_keys}command=\"{enrol_script}\",restrict {invite_pub}\n");
        fs::write(auth_keys_path, &with_invite).unwrap();

        let invite_text = Invite {
            v: 1,
            id: "inv-sshd-1".to_owned(),
            node: env.node.clone(),
            hosts: vec![env.host.clone()],
            port: env.port,
            user: env.user.clone(),
            host_keys: vec![env.ed25519_host_key.clone()],
            key: STANDARD.encode(invite_seed),
            principal: "phone".to_owned(),
            expires: "2099-01-01T00:00:00Z".to_owned(),
        }
        .encode()
        .unwrap();

        let pair_link = Link::new(LinkConfig::default());
        assert!(pair_link.config().device_key.is_none());

        let enrolled = pair_link.pair(&invite_text, "tablet").await.unwrap();
        assert_eq!(enrolled.node, env.node);
        assert_eq!(enrolled.principal, "phone");
        assert_eq!(enrolled.role, "observer");
        assert_eq!(enrolled.device, "tablet");
        assert!(pair_link.config().device_key.is_some());
        assert!(pair_link.config().node(&env.node).is_some());

        // The live Link immediately uses its newly generated device key without rebuilding.
        let paired_info = pair_link
            .call(&env.node, "sys.info", json!({}), None, |_| {})
            .await
            .unwrap();
        assert!(
            paired_info
                .get("os")
                .and_then(|o| o.get("family"))
                .is_some(),
            "{paired_info}"
        );

        // Locate the live `legion gate --as phone` process, remove the newly paired device key by
        // restoring `authorized_keys`, and kill the gate process so the next call must reconnect.
        let paired_procs = pair_link
            .call(
                &env.node,
                "sys.procs",
                json!({"filter": "legion", "limit": 50}),
                None,
                |_| {},
            )
            .await
            .unwrap();
        let paired_gate_pids: Vec<u64> = paired_procs["procs"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|p| {
                p.get("cmd")
                    .and_then(|c| c.as_str())
                    .is_some_and(|c| c.contains("gate") && c.contains("phone"))
            })
            .filter_map(|p| p.get("pid").and_then(|id| id.as_u64()))
            .collect();
        assert!(
            !paired_gate_pids.is_empty(),
            "could not find live `legion gate --as phone` process in {paired_procs}"
        );

        fs::write(auth_keys_path, &original_auth_keys).unwrap();
        for pid in &paired_gate_pids {
            let _ = Command::new("kill").args(["-9", &pid.to_string()]).status();
        }
        tokio::time::sleep(Duration::from_millis(200)).await;

        let removed_err = pair_link
            .call(&env.node, "sys.info", json!({}), None, |_| {})
            .await
            .unwrap_err();
        assert_eq!(removed_err.code, ErrorCode::Denied, "{removed_err}");
        assert_eq!(removed_err.machine.as_deref(), Some(env.node.as_str()));
        assert_eq!(
            removed_err.details.as_ref().unwrap()["reason"],
            json!(REASON_DEVICE_REMOVED_OR_UNKNOWN),
            "{removed_err}"
        );
        let hint = removed_err.hint.as_deref().unwrap().to_ascii_lowercase();
        assert!(hint.contains("pair"), "expected pair hint, got: {hint}");

        // `forget` drops the node from LinkConfig while preserving the device key.
        assert!(pair_link.forget(&env.node).await);
        assert!(!pair_link.forget(&env.node).await);
        assert!(pair_link.config().node(&env.node).is_none());
        assert!(pair_link.config().device_key.is_some());
        let after_forget = pair_link.hello(&env.node).await.unwrap_err();
        assert_eq!(after_forget.code, ErrorCode::NotFound);

        pair_link.close().await;
    }
}
