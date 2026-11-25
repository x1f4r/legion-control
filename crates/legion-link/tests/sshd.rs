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
    CancellationToken, DeviceKey, Invite, Link, LinkConfig, NodeLink, PutRequest,
    REASON_AUTH_FAILED, REASON_DEVICE_REMOVED_OR_UNKNOWN, REASON_HOST_KEY_MISMATCH, SshTarget,
    SshTimeouts, Subcommand, ssh,
};
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

struct TestEnv {
    host: String,
    port: u16,
    user: String,
    ed25519_host_key: String,
    ecdsa_host_key: Option<String>,
    mismatch_host: Option<String>,
    auth_keys_path: Option<String>,
    enrol_script: Option<String>,
    sample_file: Option<String>,
    upload_key: Option<DeviceKey>,
    upload_dir: Option<String>,
    legion_bin: Option<String>,
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
    let sample_file = env::var("LEGION_LINK_SAMPLE_FILE")
        .ok()
        .filter(|s| !s.trim().is_empty());
    let upload_key = env::var("LEGION_LINK_UPLOAD_KEY_FILE")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .map(|p| DeviceKey::from_openssh(&fs::read_to_string(p).unwrap()).unwrap());
    let upload_dir = env::var("LEGION_LINK_UPLOAD_DIR")
        .ok()
        .filter(|s| !s.trim().is_empty());
    let legion_bin = env::var("LEGION_LINK_BIN")
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
        sample_file,
        upload_key,
        upload_dir,
        legion_bin,
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

    // 6b. `Link::open_content` over the live SSH session: full read, ranged read, concurrent
    //     channels on one connection, mid-stream cancellation without harming the parent session,
    //     and routing/error checks.
    if let Some(sample_file) = &env.sample_file {
        let expected = fs::read(sample_file).unwrap();
        assert!(
            expected.len() >= 100_000,
            "sample file should be >= 100 KiB"
        );

        // Concurrent ranges on the same SSH connection.
        let mut s1 = link
            .open_content(&env.node, sample_file, 1024, Some(32_768))
            .await
            .unwrap();
        let mut s2 = link
            .open_content(&env.node, sample_file, 65_536, Some(16_384))
            .await
            .unwrap();
        let mut s3 = link
            .open_content(&env.node, sample_file, 0, None)
            .await
            .unwrap();
        assert_eq!(s1.size(), expected.len() as u64);
        assert_eq!(s1.offset(), 1024);
        assert_eq!(s1.length(), 32_768);
        assert!(s1.mtime().secs > 0);

        // Cancel s3 mid-stream after reading 128 bytes.
        let mut head = [0u8; 128];
        s3.read_exact(&mut head).await.unwrap();
        assert_eq!(&head[..], &expected[..128]);
        s3.cancel();
        drop(s3);

        // s1 and s2 still finish cleanly on the same SSH connection.
        let (mut b1, mut b2) = (Vec::new(), Vec::new());
        let (r1, r2) = tokio::join!(s1.read_to_end(&mut b1), s2.read_to_end(&mut b2));
        r1.unwrap();
        r2.unwrap();
        assert_eq!(b1, &expected[1024..1024 + 32_768]);
        assert_eq!(b2, &expected[65_536..65_536 + 16_384]);

        // RPC on the same SSH connection keeps working after stream cancellation.
        let still_alive = link.hello(&env.node).await.unwrap();
        assert_eq!(still_alive["node"], json!(env.node));

        // Unread large stream (> 4 MiB, exceeding russh's 2 MiB window and channel buffer) must
        // never stall RPC calls or other streams on the same SSH connection; resuming it later
        // yields the exact bytes.
        assert!(
            expected.len() >= 4 * 1024 * 1024,
            "sample file should be >= 4 MiB to exercise SSH backpressure"
        );
        let mut paused = link
            .open_content(&env.node, sample_file, 0, None)
            .await
            .unwrap();
        assert_eq!(paused.size(), expected.len() as u64);
        assert_eq!(paused.length(), expected.len() as u64);
        // Leave `paused` completely unread while the remote pushes its first window.
        tokio::time::sleep(Duration::from_millis(200)).await;

        let quick_hello = tokio::time::timeout(Duration::from_secs(3), link.hello(&env.node))
            .await
            .expect("hello stalled behind unread content stream")
            .unwrap();
        assert_eq!(quick_hello["node"], json!(env.node));

        let quick_info = tokio::time::timeout(
            Duration::from_secs(3),
            link.call(&env.node, "sys.info", json!({}), None, |_| {}),
        )
        .await
        .expect("sys.info stalled behind unread content stream")
        .unwrap();
        assert!(quick_info.get("os").is_some());

        // A second stream on the exact same SSH connection flows freely while the first is paused.
        let mut second = tokio::time::timeout(
            Duration::from_secs(3),
            link.open_content(&env.node, sample_file, 2 * 1024 * 1024, Some(512 * 1024)),
        )
        .await
        .expect("second stream open stalled behind unread content stream")
        .unwrap();
        let mut second_buf = Vec::new();
        tokio::time::timeout(Duration::from_secs(5), second.read_to_end(&mut second_buf))
            .await
            .expect("second stream read stalled behind unread content stream")
            .unwrap();
        assert_eq!(
            second_buf,
            &expected[2 * 1024 * 1024..2 * 1024 * 1024 + 512 * 1024]
        );

        // Resuming the unread first stream delivers every byte accurately across windows.
        let mut paused_buf = Vec::with_capacity(expected.len());
        tokio::time::timeout(Duration::from_secs(15), paused.read_to_end(&mut paused_buf))
            .await
            .expect("resumed stream timed out")
            .unwrap();
        assert_eq!(paused_buf, expected);

        // Unread stream paused in the middle of a window (after consuming past window 0 so
        // read-ahead is active) must also never stall RPC calls or sibling streams on the same
        // SSH connection.
        let mut mid_paused = link
            .open_content(&env.node, sample_file, 0, None)
            .await
            .unwrap();
        let mut mid_prefix = vec![0u8; 400 * 1024];
        mid_paused.read_exact(&mut mid_prefix).await.unwrap();
        assert_eq!(mid_prefix, &expected[..mid_prefix.len()]);
        tokio::time::sleep(Duration::from_millis(200)).await;

        let mid_hello = tokio::time::timeout(Duration::from_secs(3), link.hello(&env.node))
            .await
            .expect("hello stalled behind mid-window unread content stream")
            .unwrap();
        assert_eq!(mid_hello["node"], json!(env.node));

        let mut mid_sibling = tokio::time::timeout(
            Duration::from_secs(3),
            link.open_content(&env.node, sample_file, 1024 * 1024, Some(256 * 1024)),
        )
        .await
        .expect("sibling stream open stalled behind mid-window unread stream")
        .unwrap();
        let mut mid_sibling_buf = Vec::new();
        tokio::time::timeout(
            Duration::from_secs(5),
            mid_sibling.read_to_end(&mut mid_sibling_buf),
        )
        .await
        .expect("sibling stream read stalled behind mid-window unread stream")
        .unwrap();
        assert_eq!(
            mid_sibling_buf,
            &expected[1024 * 1024..1024 * 1024 + 256 * 1024]
        );

        let mut mid_rest = Vec::with_capacity(expected.len() - mid_prefix.len());
        tokio::time::timeout(
            Duration::from_secs(15),
            mid_paused.read_to_end(&mut mid_rest),
        )
        .await
        .expect("resumed mid-window stream timed out")
        .unwrap();
        assert_eq!(mid_rest, &expected[mid_prefix.len()..]);

        // Opening 12 concurrent streams (exceeding sshd's default MaxSessions of 10) queues
        // cleanly without discarding the healthy SSH connection.
        let mut concurrent = tokio::task::JoinSet::new();
        for i in 0..12usize {
            let link_clone = link.clone();
            let node_name = env.node.clone();
            let path = sample_file.clone();
            let off = (i * 65_536) as u64;
            let len = 32_768u64;
            concurrent.spawn(async move {
                let mut s = link_clone
                    .open_content(&node_name, &path, off, Some(len))
                    .await
                    .unwrap();
                let mut out = Vec::new();
                s.read_to_end(&mut out).await.unwrap();
                (off as usize, out)
            });
        }
        while let Some(res) = concurrent.join_next().await {
            let (off, bytes) = res.unwrap();
            assert_eq!(bytes, &expected[off..off + 32_768]);
        }

        // Twenty streams held open at once on a single SSH connection (far above the 8 active
        // channel cap and sshd's MaxSessions = 10): unread streams park after ~2s to release their
        // channel slots so the rest can open and active readers + RPC keep flowing, and every
        // parked stream resumes cleanly with exact bytes.
        let mut twenty = Vec::with_capacity(20);
        for i in 0..20usize {
            let off = (i * 65_536) as u64;
            let len = 512 * 1024u64; // > initial 256 KiB window so reopening is exercised
            let mut s = tokio::time::timeout(
                Duration::from_secs(12),
                link.open_content(&env.node, sample_file, off, Some(len)),
            )
            .await
            .unwrap_or_else(|_| panic!("20-stream open #{i} timed out"))
            .unwrap();
            // Read only a small prefix on the first 4 streams; leave the rest completely unread.
            let mut prefix = Vec::new();
            if i < 4 {
                prefix.resize(64 * 1024, 0);
                s.read_exact(&mut prefix).await.unwrap();
                assert_eq!(prefix, &expected[off as usize..off as usize + prefix.len()]);
            }
            twenty.push((off as usize, len as usize, prefix, s));
        }
        let hello_with_20 = tokio::time::timeout(Duration::from_secs(3), link.hello(&env.node))
            .await
            .expect("hello stalled with 20 open streams")
            .unwrap();
        assert_eq!(hello_with_20["node"], json!(env.node));
        for (i, (off, len, mut buf, mut stream)) in twenty.into_iter().enumerate() {
            tokio::time::timeout(Duration::from_secs(15), stream.read_to_end(&mut buf))
                .await
                .unwrap_or_else(|_| panic!("resuming stream #{i} of 20 timed out"))
                .unwrap();
            assert_eq!(buf, &expected[off..off + len], "stream #{i} byte mismatch");
        }

        // Error cases: offset past EOF and unknown machine.
        let bad_off = link
            .open_content(&env.node, sample_file, expected.len() as u64 + 1, Some(10))
            .await
            .err()
            .unwrap();
        assert_eq!(bad_off.code, ErrorCode::Conflict);

        let unknown = link
            .open_content("no-such-machine", sample_file, 0, Some(10))
            .await
            .err()
            .unwrap();
        assert_eq!(unknown.code, ErrorCode::NotFound);

        // 6c. `Link::open_put` over real sshd: read-only principal is refused (`denied`);
        //     writable principal uploads cleanly, resumes from a partial `.legion-part` file,
        //     rejects a hash mismatch, and works over `NodeLink::local`.
        let sample_hash_val = link
            .call(
                &env.node,
                "fs.hash",
                json!({"path": sample_file}),
                None,
                |_| {},
            )
            .await
            .unwrap();
        let sample_hash = sample_hash_val["hash"].as_str().unwrap().to_owned();

        if let Some(upload_dir) = &env.upload_dir {
            // Read-only `phone` principal must be refused before any byte is sent.
            let denied = link
                .open_put(
                    &env.node,
                    PutRequest {
                        path: format!("{upload_dir}/denied.bin"),
                        size: expected.len() as u64,
                        hash: sample_hash.clone(),
                        mode: Some(0o644),
                        mtime: None,
                    },
                )
                .await
                .err()
                .unwrap();
            assert_eq!(denied.code, ErrorCode::Denied, "{denied}");

            if let Some(upload_key) = &env.upload_key {
                let writer_link = Link::new(LinkConfig {
                    device_key: Some(upload_key.to_openssh().unwrap()),
                    nodes: vec![NodeLink::ssh(
                        &env.node,
                        SshTarget {
                            hosts: vec![env.host.clone()],
                            port: env.port,
                            user: env.user.clone(),
                            host_keys: vec![env.ed25519_host_key.clone()],
                        },
                        Some("writer".to_owned()),
                    )],
                    ..LinkConfig::default()
                });

                // Full 5 MiB upload verified by remote BLAKE3 hash.
                let dest = format!("{upload_dir}/full.bin");
                let mut put = writer_link
                    .open_put(
                        &env.node,
                        PutRequest {
                            path: dest.clone(),
                            size: expected.len() as u64,
                            hash: sample_hash.clone(),
                            mode: Some(0o644),
                            mtime: None,
                        },
                    )
                    .await
                    .unwrap();
                assert_eq!(put.offset(), 0);
                put.write_all(&expected).await.unwrap();
                let done = put.finish().await.unwrap();
                assert_eq!(done.size, expected.len() as u64);
                assert_eq!(done.hash, sample_hash);
                assert_eq!(fs::read(&dest).unwrap(), expected);

                // Partial upload interrupted mid-stream leaves `.legion-part`; the second attempt
                // reports the saved offset and finishes cleanly.
                let resumed_dest = format!("{upload_dir}/resumed.bin");
                let mut partial = writer_link
                    .open_put(
                        &env.node,
                        PutRequest {
                            path: resumed_dest.clone(),
                            size: expected.len() as u64,
                            hash: sample_hash.clone(),
                            mode: Some(0o644),
                            mtime: None,
                        },
                    )
                    .await
                    .unwrap();
                assert_eq!(partial.offset(), 0);
                let first_half = 1_500_000usize;
                partial.write_all(&expected[..first_half]).await.unwrap();
                partial.flush().await.unwrap();
                tokio::time::sleep(Duration::from_millis(150)).await;
                partial.cancel();
                drop(partial);
                tokio::time::sleep(Duration::from_millis(150)).await;

                let mut resumed = writer_link
                    .open_put(
                        &env.node,
                        PutRequest {
                            path: resumed_dest.clone(),
                            size: expected.len() as u64,
                            hash: sample_hash.clone(),
                            mode: Some(0o644),
                            mtime: None,
                        },
                    )
                    .await
                    .unwrap();
                let off = resumed.offset() as usize;
                assert!(
                    off > 0 && off <= first_half,
                    "expected resumed upload offset in 1..={first_half}, got {off}"
                );
                resumed.write_all(&expected[off..]).await.unwrap();
                let done = resumed.finish().await.unwrap();
                assert_eq!(done.size, expected.len() as u64);
                assert_eq!(done.hash, sample_hash);
                assert_eq!(fs::read(&resumed_dest).unwrap(), expected);

                // Hash mismatch is rejected cleanly by the remote node.
                let bad_dest = format!("{upload_dir}/bad-hash.bin");
                let mut bad = writer_link
                    .open_put(
                        &env.node,
                        PutRequest {
                            path: bad_dest.clone(),
                            size: 64,
                            hash: "blake3:0000000000000000000000000000000000000000000000000000000000000000"
                                .to_owned(),
                            mode: Some(0o644),
                            mtime: None,
                        },
                    )
                    .await
                    .unwrap();
                bad.write_all(&[0x5Au8; 64]).await.unwrap();
                let err = bad.finish().await.unwrap_err();
                assert_ne!(err.code, ErrorCode::InvalidArgs);
                assert!(!std::path::Path::new(&bad_dest).exists());

                writer_link.close().await;
            }
        }

        // Local child pipe (`NodeLink::local`) when both `local` feature and binary are present.
        #[cfg(feature = "local")]
        if let Some(legion_bin) = &env.legion_bin {
            let local_link = Link::new(LinkConfig {
                nodes: vec![NodeLink::local("local-node", legion_bin)],
                ..LinkConfig::default()
            });
            let mut ls = local_link
                .open_content("local-node", sample_file, 512, Some(4096))
                .await
                .unwrap();
            assert_eq!(ls.size(), expected.len() as u64);
            assert_eq!(ls.length(), 4096);
            let mut lbuf = Vec::new();
            ls.read_to_end(&mut lbuf).await.unwrap();
            assert_eq!(lbuf, &expected[512..512 + 4096]);
            if let Some(upload_dir) = &env.upload_dir {
                let local_dest = format!("{upload_dir}/local-put.bin");
                let mut lp = local_link
                    .open_put(
                        "local-node",
                        PutRequest {
                            path: local_dest.clone(),
                            size: expected.len() as u64,
                            hash: sample_hash.clone(),
                            mode: Some(0o644),
                            mtime: None,
                        },
                    )
                    .await
                    .unwrap();
                assert_eq!(lp.offset(), 0);
                lp.write_all(&expected).await.unwrap();
                let done = lp.finish().await.unwrap();
                assert_eq!(done.hash, sample_hash);
                assert_eq!(fs::read(&local_dest).unwrap(), expected);
            }
            local_link.close().await;
        }
        #[cfg(not(feature = "local"))]
        let _ = &env.legion_bin;
    }

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

#[tokio::test]
#[ignore = "requires LEGION_LINK_SSHD... and LEGION_LINK_LONG_PAUSE=1 (sleeps 130s)"]
async fn real_sshd_paused_stream_resumes_after_130s() {
    if env::var("LEGION_LINK_LONG_PAUSE").ok().as_deref() != Some("1") {
        eprintln!("skipping 130s pause test (set LEGION_LINK_LONG_PAUSE=1 to run)");
        return;
    }
    let Some(env) = env_target() else {
        panic!(
            "Set LEGION_LINK_SSHD, LEGION_LINK_USER, LEGION_LINK_HOST_KEY, LEGION_LINK_KEY_FILE."
        );
    };
    let sample_file = env
        .sample_file
        .as_deref()
        .expect("LEGION_LINK_SAMPLE_FILE required");
    let expected = fs::read(sample_file).unwrap();
    assert!(expected.len() >= 4 * 1024 * 1024);

    let target = SshTarget {
        hosts: vec![env.host.clone()],
        port: env.port,
        user: env.user.clone(),
        host_keys: vec![env.ed25519_host_key.clone()],
    };
    let link = Link::new(LinkConfig {
        device_key: Some(env.key.to_openssh().unwrap()),
        nodes: vec![NodeLink::ssh(&env.node, target, Some("phone".to_owned()))],
        ..LinkConfig::default()
    });

    // Open three streams and advance them to three distinct positions before pausing 130 seconds
    // (past remote `legion stream`'s 120s idle timeout):
    //   1. Unread at offset 0.
    //   2. Mid-window after reading 400 KiB.
    //   3. Mid-file after reading 2 MiB.
    let s_start = link
        .open_content(&env.node, sample_file, 0, None)
        .await
        .unwrap();
    let mut s_mid = link
        .open_content(&env.node, sample_file, 0, None)
        .await
        .unwrap();
    let mut mid_buf = vec![0u8; 400 * 1024];
    s_mid.read_exact(&mut mid_buf).await.unwrap();
    assert_eq!(mid_buf, &expected[..mid_buf.len()]);

    let mut s_late = link
        .open_content(&env.node, sample_file, 0, None)
        .await
        .unwrap();
    let mut late_buf = vec![0u8; 2 * 1024 * 1024];
    s_late.read_exact(&mut late_buf).await.unwrap();
    assert_eq!(late_buf, &expected[..late_buf.len()]);

    eprintln!("Pausing 3 streams for 130s (past remote 120s idle timeout)...");
    tokio::time::sleep(Duration::from_secs(130)).await;

    for (label, mut stream, mut got) in [
        ("start (0 B)", s_start, Vec::with_capacity(expected.len())),
        ("mid-window (400 KiB)", s_mid, mid_buf),
        ("mid-file (2 MiB)", s_late, late_buf),
    ] {
        let t0 = std::time::Instant::now();
        let mut rest = Vec::with_capacity(expected.len() - got.len());
        tokio::time::timeout(Duration::from_secs(30), stream.read_to_end(&mut rest))
            .await
            .unwrap_or_else(|_| panic!("{label} timed out after 130s pause"))
            .unwrap();
        let elapsed = t0.elapsed();
        got.extend_from_slice(&rest);
        assert_eq!(got, expected, "{label} byte mismatch after 130s pause");
        eprintln!(
            "resumed {label} cleanly in {:.1} ms",
            elapsed.as_secs_f64() * 1000.0
        );
    }

    link.close().await;
}
