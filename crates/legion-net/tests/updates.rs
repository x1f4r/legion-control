//! Multi-node end-to-end tests for `update.publish`, `update.list`, `update.read`, and fleet
//! propagation over the real `Transport::Command` JSON-RPC pipe.
//!
//! Re-executes this test binary as a lightweight JSON-RPC peer node with explicit test
//! [`TrustAnchors`] so we can exercise preview-enabled vs release-only nodes, node-authority
//! propagation, offline catch-up, and tampered-peer refusal without touching any production trust
//! anchors.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::fmt::Write as _;
use std::fs;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::Path;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::{Duration, Instant};

use ed25519_dalek::{Signer, SigningKey};
use legion_core::{Call, Door, Node, Paths, Registry, VERSION};
use legion_net::Fleet;
use legion_net::updates::{ListResult, PublishResult, register_with_anchors, update_set_dir};
use legion_proto::{
    CallParams, ErrorBody, ErrorCode, HelloResult, METHOD_NOT_FOUND, PROTOCOL, Request, Response,
    RpcError, TOOL_ERROR, method,
};
use legion_update::{
    Arch, CHECKSUMS_FILENAME, Channel, InstallKind, MANIFEST_FILENAME, Os, Program,
    SIGNATURE_FILENAME, TrustAnchors, encode_signature_bytes, sha256_bytes,
};
use serde_json::{Value, json};
use tempfile::tempdir;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

const HELPER_FLAG: &str = "--update-peer";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some(HELPER_FLAG) {
        return run_peer_helper(&args[1..]);
    }

    let filter = args
        .iter()
        .find(|a| !a.starts_with('-'))
        .map(String::as_str);
    let tests: &[(&str, fn())] = &[
        (
            "publish_propagates_to_trusted_peer_and_release_only_peer_refuses_preview",
            publish_propagates_to_trusted_peer_and_release_only_peer_refuses_preview,
        ),
        (
            "pull_from_peer_refuses_tampered_artifact_and_stores_nothing",
            pull_from_peer_refuses_tampered_artifact_and_stores_nothing,
        ),
        (
            "pull_from_peer_refuses_wrong_offset_oversized_and_short_reads",
            pull_from_peer_refuses_wrong_offset_oversized_and_short_reads,
        ),
    ];

    let mut failed = Vec::new();
    let mut ran = 0;
    for (name, test) in tests {
        if filter.is_some_and(|f| !name.contains(f)) {
            continue;
        }
        ran += 1;
        let started = Instant::now();
        let outcome = catch_unwind(AssertUnwindSafe(test));
        let verdict = if outcome.is_ok() { "ok" } else { "FAILED" };
        println!(
            "test {name} ... {verdict} ({:.2}s)",
            started.elapsed().as_secs_f64()
        );
        if outcome.is_err() {
            failed.push(*name);
        }
    }

    println!(
        "\ntest result: {}. {} passed; {} failed",
        if failed.is_empty() { "ok" } else { "FAILED" },
        ran - failed.len(),
        failed.len()
    );
    if failed.is_empty() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

/// Helper invocation: `--update-peer <node_root> <release_hex> <preview_hex|none> [corrupt|rpc]`
/// (`Transport::Command` appends `"rpc"` as the final argument.)
fn run_peer_helper(args: &[String]) -> ExitCode {
    let root = Path::new(&args[0]);
    let rel_pub = decode_hex32(&args[1]);
    let prev_pub = if args[2] == "none" {
        None
    } else {
        Some(decode_hex32(&args[2]))
    };
    let corrupt_read = args.iter().any(|a| a == "corrupt");
    let wrong_offset = args.iter().any(|a| a == "wrong-offset");
    let short_read = args.iter().any(|a| a == "short-read");
    let oversized_chunk = args.iter().any(|a| a == "oversized-chunk");
    let anchors = TrustAnchors::with_keys(rel_pub, prev_pub);

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async move {
        let paths = Paths::under(root);
        let fleet = Fleet::new(&paths);
        let mut reg = Registry::new();
        legion_tools::register_all(&mut reg);
        register_with_anchors(&mut reg, fleet.clone(), anchors);
        let node = Arc::new(Node::new(paths, reg));

        let stdin = tokio::io::stdin();
        let stdout = Arc::new(tokio::sync::Mutex::new(tokio::io::stdout()));
        let mut lines = BufReader::new(stdin).lines();

        while let Ok(Some(line)) = lines.next_line().await {
            if line.trim().is_empty() {
                continue;
            }
            let req: Request = match serde_json::from_str(&line) {
                Ok(r) => r,
                Err(_) => continue,
            };
            let Some(id) = req.id else { continue };
            let node = Arc::clone(&node);
            let stdout = Arc::clone(&stdout);
            let method_name = req.method;
            let params = req.params;
            tokio::spawn(async move {
                let response = match method_name.as_str() {
                    method::HELLO => {
                        let hello = HelloResult {
                            protocol: PROTOCOL,
                            node: node.name(),
                            node_explicit: node.name_explicit(),
                            node_id: node.node_id(),
                            version: VERSION.to_owned(),
                            os: std::env::consts::OS.to_owned(),
                            arch: std::env::consts::ARCH.to_owned(),
                            system: None,
                            build: None,
                            boot_id: None,
                            systems: Vec::new(),
                        };
                        Response::ok(Some(id), serde_json::to_value(hello).unwrap())
                    }
                    method::CALL => {
                        let parsed: CallParams = match serde_json::from_value(params) {
                            Ok(p) => p,
                            Err(err) => {
                                let resp = Response::error(
                                    Some(id),
                                    RpcError::new(
                                        TOOL_ERROR,
                                        ErrorBody::new(ErrorCode::InvalidArgs, err.to_string()),
                                    ),
                                );
                                write_line(&stdout, &resp).await;
                                return;
                            }
                        };
                        let mut call = Call::new(Door::Rpc, parsed.tool.clone(), parsed.args);
                        call.as_principal = parsed.as_principal;
                        call.hops = parsed.hops;
                        match node.call(call).await {
                            Ok(mut val) => {
                                if parsed.tool == "update.read"
                                    && val.get("name").and_then(Value::as_str) == Some("Legion.apk")
                                {
                                    use base64::Engine as _;
                                    if corrupt_read {
                                        let mut raw = base64::engine::general_purpose::STANDARD
                                            .decode(val["content"].as_str().unwrap())
                                            .unwrap();
                                        if let Some(first) = raw.first_mut() {
                                            *first ^= 0xFF;
                                        }
                                        val["content"] = json!(
                                            base64::engine::general_purpose::STANDARD.encode(&raw)
                                        );
                                    } else if wrong_offset {
                                        val["offset"] = json!(7u64);
                                    } else if short_read {
                                        let raw = b"short";
                                        val["content"] = json!(
                                            base64::engine::general_purpose::STANDARD.encode(raw)
                                        );
                                        val["length"] = json!(raw.len() as u64);
                                        val["eof"] = json!(true);
                                    } else if oversized_chunk {
                                        let raw = vec![0x41u8; 64];
                                        val["content"] = json!(
                                            base64::engine::general_purpose::STANDARD.encode(&raw)
                                        );
                                        val["length"] = json!(raw.len() as u64);
                                        val["eof"] = json!(true);
                                    }
                                }
                                Response::ok(Some(id), val)
                            }
                            Err(err) => Response::error(Some(id), RpcError::new(TOOL_ERROR, err)),
                        }
                    }
                    other => Response::error(
                        Some(id),
                        RpcError::new(
                            METHOD_NOT_FOUND,
                            ErrorBody::new(
                                ErrorCode::UnknownTool,
                                format!("unknown method {other}"),
                            ),
                        ),
                    ),
                };
                write_line(&stdout, &response).await;
            });
        }
    });
    ExitCode::SUCCESS
}

async fn write_line(out: &Arc<tokio::sync::Mutex<tokio::io::Stdout>>, resp: &Response) {
    let mut line = serde_json::to_string(resp).unwrap();
    line.push('\n');
    let mut guard = out.lock().await;
    let _ = guard.write_all(line.as_bytes()).await;
    let _ = guard.flush().await;
}

fn encode_hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(&mut out, "{b:02x}");
    }
    out
}

fn decode_hex32(hex: &str) -> [u8; 32] {
    assert_eq!(hex.len(), 64);
    let mut out = [0u8; 32];
    for (i, chunk) in hex.as_bytes().as_chunks::<2>().0.iter().enumerate() {
        out[i] = u8::from_str_radix(std::str::from_utf8(chunk).unwrap(), 16).unwrap();
    }
    out
}

fn keypair(seed: u8) -> (SigningKey, [u8; 32]) {
    let signing = SigningKey::from_bytes(&[seed; 32]);
    let verifying = signing.verifying_key().to_bytes();
    (signing, verifying)
}

type TestArtifactSpec<'a> = (&'a str, Program, Os, Arch, InstallKind, &'a [u8]);

fn write_signed_set(
    dir: &Path,
    channel: Channel,
    version: &str,
    sequence: u64,
    artifacts: &[TestArtifactSpec<'_>],
    signing_key: &SigningKey,
) {
    fs::create_dir_all(dir).unwrap();
    let mut art_json = Vec::new();
    let mut sums = Vec::new();
    for (name, program, os, arch, kind, bytes) in artifacts {
        fs::write(dir.join(name), bytes).unwrap();
        sums.push(((*name).to_owned(), sha256_bytes(bytes)));
        art_json.push(json!({
            "name": name,
            "program": program,
            "os": os,
            "arch": arch,
            "kind": kind,
            "size": bytes.len(),
        }));
    }
    let manifest_bytes = serde_json::to_vec_pretty(&json!({
        "channel": channel,
        "version": version,
        "build": "a1b2c3d4e5f6",
        "sequence": sequence,
        "published_at": "2026-11-25T02:00:00Z",
        "notes": format!("Update {version} (seq {sequence})."),
        "artifacts": art_json,
    }))
    .unwrap();
    fs::write(dir.join(MANIFEST_FILENAME), &manifest_bytes).unwrap();
    sums.push((MANIFEST_FILENAME.to_owned(), sha256_bytes(&manifest_bytes)));
    sums.sort_by(|a, b| a.0.cmp(&b.0));

    let mut sums_text = String::new();
    for (name, hash) in sums {
        let _ = writeln!(&mut sums_text, "{hash}  {name}");
    }
    fs::write(dir.join(CHECKSUMS_FILENAME), sums_text.as_bytes()).unwrap();
    let sig = signing_key.sign(sums_text.as_bytes());
    fs::write(
        dir.join(SIGNATURE_FILENAME),
        encode_signature_bytes(&sig.to_bytes()),
    )
    .unwrap();
}

fn init_node_dir(root: &Path, name: &str, fleet_toml: &str) {
    let paths = Paths::under(root);
    fs::create_dir_all(paths.config_dir()).unwrap();
    fs::create_dir_all(paths.state_dir()).unwrap();
    fs::write(
        paths.config_file("node.toml"),
        format!("name = \"{name}\"\n"),
    )
    .unwrap();
    if !fleet_toml.is_empty() {
        fs::write(paths.config_file("fleet.toml"), fleet_toml).unwrap();
    }
}

fn toml_cmd(argv: &[String]) -> String {
    let quoted: Vec<String> = argv
        .iter()
        .map(|s| serde_json::to_string(s).unwrap())
        .collect();
    format!("[{}]", quoted.join(", "))
}

fn publish_propagates_to_trusted_peer_and_release_only_peer_refuses_preview() {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let dir = tempdir().unwrap();
        let exe = std::env::current_exe()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        let (rel_sign, rel_pub) = keypair(0x31);
        let (prev_sign, prev_pub) = keypair(0x32);
        let rel_hex = encode_hex(&rel_pub);
        let prev_hex = encode_hex(&prev_pub);

        let tower_root = dir.path().join("tower");
        let pi_root = dir.path().join("pi");
        let nas_root = dir.path().join("nas");

        let tower_cmd = toml_cmd(&[
            exe.clone(),
            HELPER_FLAG.to_owned(),
            tower_root.to_string_lossy().into_owned(),
            rel_hex.clone(),
            prev_hex.clone(),
        ]);
        let pi_cmd = toml_cmd(&[
            exe.clone(),
            HELPER_FLAG.to_owned(),
            pi_root.to_string_lossy().into_owned(),
            rel_hex.clone(),
            prev_hex.clone(),
        ]);
        let nas_cmd = toml_cmd(&[
            exe.clone(),
            HELPER_FLAG.to_owned(),
            nas_root.to_string_lossy().into_owned(),
            rel_hex.clone(),
            "none".to_owned(),
        ]);
        let offline_cmd = toml_cmd(&["/nonexistent/legion-offline-peer".to_owned()]);

        let fleet_toml = format!(
            "[machines.tower]\ncommand = {tower_cmd}\n\
             [machines.pi]\ncommand = {pi_cmd}\n\
             [machines.nas]\ncommand = {nas_cmd}\n\
             [machines.laptop]\ncommand = {offline_cmd}\n"
        );
        init_node_dir(&tower_root, "tower", &fleet_toml);
        init_node_dir(&pi_root, "pi", &fleet_toml);
        init_node_dir(&nas_root, "nas", &fleet_toml);

        let anchors_preview = TrustAnchors::with_keys(rel_pub, Some(prev_pub));
        let paths = Paths::under(&tower_root);
        let fleet = Fleet::new(&paths);
        let mut reg = Registry::new();
        legion_tools::register_all(&mut reg);
        register_with_anchors(&mut reg, fleet.clone(), anchors_preview);
        let tower = Node::new(paths, reg);

        // 1. Publish a preview set on tower: pi (preview-enabled) pulls & stores it, nas
        //    (release-only) refuses it and stores nothing, laptop is unreachable.
        let prev_set_dir = dir.path().join("set-preview");
        let apk_bytes = vec![0x42u8; 640 * 1024]; // > PULL_CHUNK_BYTES (512 KiB) to test multi-chunk pull
        write_signed_set(
            &prev_set_dir,
            Channel::Preview,
            "0.2.0",
            202611250201,
            &[(
                "Legion.apk",
                Program::App,
                Os::Android,
                Arch::Universal,
                InstallKind::Apk,
                &apk_bytes,
            )],
            &prev_sign,
        );

        let res: PublishResult = serde_json::from_value(
            tower
                .call(Call::new(
                    Door::Cli,
                    "update.publish",
                    json!({ "from": prev_set_dir.to_string_lossy() }),
                ))
                .await
                .unwrap(),
        )
        .unwrap();

        assert_eq!(res.sequence, 202611250201);
        assert!(!res.already_present);
        assert_eq!(res.fleet["pi"].status, "synced", "{:?}", res.fleet);
        assert_eq!(res.fleet["nas"].status, "refused", "{:?}", res.fleet);
        assert_eq!(res.fleet["laptop"].status, "not_reached", "{:?}", res.fleet);

        // Verify pi actually stored identical bytes and nas stored nothing.
        let pi_apk = update_set_dir(&Paths::under(&pi_root), Channel::Preview, 202611250201)
            .join("Legion.apk");
        assert_eq!(fs::read(&pi_apk).unwrap(), apk_bytes);
        assert!(!update_set_dir(&Paths::under(&nas_root), Channel::Preview, 202611250201).exists());

        // 2. Re-publishing the exact same preview set is idempotent (already_present on both).
        let res2: PublishResult = serde_json::from_value(
            tower
                .call(Call::new(
                    Door::Cli,
                    "update.publish",
                    json!({ "from": prev_set_dir.to_string_lossy(), "machines": ["pi"] }),
                ))
                .await
                .unwrap(),
        )
        .unwrap();
        assert!(res2.already_present);
        assert_eq!(res2.fleet["pi"].status, "already_present");

        // 3. Publish a stable set signed with the release key: BOTH pi and nas accept and store it.
        let stable_dir = dir.path().join("set-stable");
        write_signed_set(
            &stable_dir,
            Channel::Stable,
            "0.2.0",
            202611250202,
            &[(
                "Legion.AppImage",
                Program::App,
                Os::Linux,
                Arch::X86_64,
                InstallKind::AppImage,
                b"stable-appimage-bytes",
            )],
            &rel_sign,
        );
        let res3: PublishResult = serde_json::from_value(
            tower
                .call(Call::new(
                    Door::Cli,
                    "update.publish",
                    json!({ "from": stable_dir.to_string_lossy(), "machines": ["pi", "nas"] }),
                ))
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(res3.fleet["pi"].status, "synced", "{:?}", res3.fleet);
        assert_eq!(res3.fleet["nas"].status, "synced", "{:?}", res3.fleet);

        let nas_list: ListResult = serde_json::from_value(
            fleet
                .call_machine(
                    &tower,
                    "nas",
                    Call::new(Door::Cli, "update.list", json!({})),
                )
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(nas_list.sets.len(), 1);
        assert_eq!(nas_list.sets[0].channel, Channel::Stable);
        assert_eq!(nas_list.sets[0].sequence, 202611250202);

        fleet.close().await;
    });
}

fn pull_from_peer_refuses_tampered_artifact_and_stores_nothing() {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let dir = tempdir().unwrap();
        let exe = std::env::current_exe()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        let (_rel_sign, rel_pub) = keypair(0x41);
        let (prev_sign, prev_pub) = keypair(0x42);
        let rel_hex = encode_hex(&rel_pub);
        let prev_hex = encode_hex(&prev_pub);

        let rogue_root = dir.path().join("rogue");
        let receiver_root = dir.path().join("receiver");

        let rogue_cmd = toml_cmd(&[
            exe,
            HELPER_FLAG.to_owned(),
            rogue_root.to_string_lossy().into_owned(),
            rel_hex,
            prev_hex,
            "corrupt".to_owned(),
        ]);
        let fleet_toml = format!("[machines.rogue]\ncommand = {rogue_cmd}\n");
        init_node_dir(&rogue_root, "rogue", "");
        init_node_dir(&receiver_root, "receiver", &fleet_toml);

        // Pre-populate rogue's update store with a validly signed set.
        let rogue_stored =
            update_set_dir(&Paths::under(&rogue_root), Channel::Preview, 202611250300);
        write_signed_set(
            &rogue_stored,
            Channel::Preview,
            "0.3.0",
            202611250300,
            &[(
                "Legion.apk",
                Program::App,
                Os::Android,
                Arch::Universal,
                InstallKind::Apk,
                b"good-apk-bytes",
            )],
            &prev_sign,
        );

        let anchors = TrustAnchors::with_keys(rel_pub, Some(prev_pub));
        let paths = Paths::under(&receiver_root);
        let fleet = Fleet::new(&paths);
        let mut reg = Registry::new();
        legion_tools::register_all(&mut reg);
        register_with_anchors(&mut reg, fleet.clone(), anchors);
        let receiver = Node::new(paths.clone(), reg);

        let err = receiver
            .call(Call::new(
                Door::Cli,
                "update.publish",
                json!({
                    "from_peer": "rogue",
                    "channel": "preview",
                    "sequence": 202611250300u64,
                    "timeout_s": Duration::from_secs(30).as_secs_f64(),
                }),
            ))
            .await
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::Failed, "{err:?}");
        assert!(err.message.contains("SHA-256 mismatch"), "{err:?}");
        assert!(!update_set_dir(&paths, Channel::Preview, 202611250300).exists());

        // Ensure no leftover staging directories remain.
        let staging_leftovers: Vec<_> = fs::read_dir(paths.state_dir().join("updates"))
            .into_iter()
            .flatten()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().starts_with(".staging-"))
            .collect();
        assert!(staging_leftovers.is_empty(), "{staging_leftovers:?}");

        fleet.close().await;
    });
}

fn pull_from_peer_refuses_wrong_offset_oversized_and_short_reads() {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let dir = tempdir().unwrap();
        let exe = std::env::current_exe()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        let (_rel_sign, rel_pub) = keypair(0x51);
        let (prev_sign, prev_pub) = keypair(0x52);
        let rel_hex = encode_hex(&rel_pub);
        let prev_hex = encode_hex(&prev_pub);

        for (mode, expected_substr) in [
            ("wrong-offset", "unexpected offset"),
            ("short-read", "ended at"),
            ("oversized-chunk", "exceeding requested"),
        ] {
            let rogue_root = dir.path().join(format!("rogue-{mode}"));
            let receiver_root = dir.path().join(format!("receiver-{mode}"));
            let rogue_cmd = toml_cmd(&[
                exe.clone(),
                HELPER_FLAG.to_owned(),
                rogue_root.to_string_lossy().into_owned(),
                rel_hex.clone(),
                prev_hex.clone(),
                mode.to_owned(),
            ]);
            let fleet_toml = format!("[machines.rogue]\ncommand = {rogue_cmd}\n");
            init_node_dir(&rogue_root, "rogue", "");
            init_node_dir(&receiver_root, "receiver", &fleet_toml);

            let rogue_stored =
                update_set_dir(&Paths::under(&rogue_root), Channel::Preview, 202611250310);
            write_signed_set(
                &rogue_stored,
                Channel::Preview,
                "0.3.1",
                202611250310,
                &[(
                    "Legion.apk",
                    Program::App,
                    Os::Android,
                    Arch::Universal,
                    InstallKind::Apk,
                    b"good-apk-bytes",
                )],
                &prev_sign,
            );

            let anchors = TrustAnchors::with_keys(rel_pub, Some(prev_pub));
            let paths = Paths::under(&receiver_root);
            let fleet = Fleet::new(&paths);
            let mut reg = Registry::new();
            legion_tools::register_all(&mut reg);
            register_with_anchors(&mut reg, fleet.clone(), anchors);
            let receiver = Node::new(paths.clone(), reg);

            let err = receiver
                .call(Call::new(
                    Door::Cli,
                    "update.publish",
                    json!({
                        "from_peer": "rogue",
                        "channel": "preview",
                        "sequence": 202611250310u64,
                        "timeout_s": Duration::from_secs(30).as_secs_f64(),
                    }),
                ))
                .await
                .unwrap_err();
            assert_eq!(err.code, ErrorCode::Failed, "mode={mode}: {err:?}");
            assert!(
                err.message.contains(expected_substr),
                "mode={mode}: expected {expected_substr:?} in {:?}",
                err.message
            );
            assert!(!update_set_dir(&paths, Channel::Preview, 202611250310).exists());
            let leftovers: Vec<_> = fs::read_dir(paths.state_dir().join("updates"))
                .into_iter()
                .flatten()
                .flatten()
                .filter(|e| e.file_name().to_string_lossy().starts_with(".staging-"))
                .collect();
            assert!(leftovers.is_empty(), "mode={mode}: {leftovers:?}");
            fleet.close().await;
        }
    });
}
