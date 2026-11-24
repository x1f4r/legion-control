//! End-to-end tests for device pairing: real loopback `sshd` enrolment, `legion gate`,
//! `expiry-time` enforcement, `legion pair` QR output, and non-admin file-tool confinement.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use common::Home;
use legion_tools::device::keys::{InvitePayload, ed25519_public_key_from_seed};
use legion_tools::testing::sshd::{
    TestSshd, generate_ed25519_keypair, key_fingerprint, write_known_hosts,
    write_openssh_ed25519_private_key,
};
use serde_json::{Value, json};

fn toml_literal(path: &Path) -> String {
    let s = path.display().to_string();
    format!("'{}'", s.replace('\'', "''"))
}

fn rpc_request(id: Value, method: &str, params: Value) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": method,
        "params": params,
    })
}

fn read_json_line(reader: &mut BufReader<impl std::io::Read>, what: &str) -> Value {
    let mut line = String::new();
    loop {
        line.clear();
        let n = reader
            .read_line(&mut line)
            .unwrap_or_else(|e| panic!("failed reading {what}: {e}"));
        assert!(n > 0, "EOF before receiving {what}");
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Ok(value) = serde_json::from_str::<Value>(trimmed) {
            return value;
        }
    }
}

#[test]
fn real_sshd_pairing_enrol_rpc_gate_revoke_and_expiry() {
    if !TestSshd::available() {
        eprintln!(
            "skipping real_sshd_pairing_enrol_rpc_gate_revoke_and_expiry: sshd/ssh/ssh-keygen unavailable"
        );
        return;
    }

    let home = Home::new_strict();
    let auth_keys = home.home().join("test_authorized_keys");
    let foreign_comment = "# foreign key kept verbatim\n";
    let foreign_pub = ed25519_public_key_from_seed(&[7_u8; 32]);
    let foreign_key = format!("{foreign_pub} foreign@host\n");
    std::fs::write(&auth_keys, format!("{foreign_comment}{foreign_key}")).unwrap();

    // Intentionally omit `SetEnv LEGION_HOME=...` from sshd_config: because `legion` is running
    // under a non-default LEGION_HOME, the forced commands must pin `--home <abs path>` so
    // `legion gate --enrol` and `legion gate --as` always reach the right config and state
    // regardless of the SSH session's environment.
    let home_dir_str = home.home().to_string_lossy().into_owned();
    let Some(sshd) = TestSshd::start(auth_keys.clone(), &[("HOME", &home_dir_str)])
        .expect("throwaway sshd failed to start")
    else {
        eprintln!("skipping: throwaway sshd unavailable");
        return;
    };

    let projects_dir = home.home().join("projects");
    let note_path = projects_dir.join("app").join("note.txt");
    std::fs::write(&note_path, "paired-read-ok\n").unwrap();

    home.write_config(
        "node.toml",
        &format!(
            "name = \"tower\"\n\n[devices]\nauthorized_keys = {}\nhost_keys = [{}]\n",
            toml_literal(&auth_keys),
            toml_literal(sshd.host_key_path()),
        ),
    );
    home.write_config(
        "policy.toml",
        &format!(
            "default = \"owner\"\n\n\
             [roles.observer]\n\
             allow = [\"effect:read\"]\n\
             deny = [\"audit.*\"]\n\
             paths = [{}]\n\n\
             [principals.phone]\n\
             role = \"observer\"\n",
            toml_literal(&projects_dir),
        ),
    );

    // 1. Issue an invite via `device.invite`.
    let invite_run = home.run(&[
        "device.invite",
        "principal=phone",
        "host=127.0.0.1",
        &format!("port={}", sshd.port()),
        &format!("user={}", sshd.user()),
    ]);
    assert_eq!(
        invite_run.code, 0,
        "device.invite failed: stdout={} stderr={}",
        invite_run.stdout, invite_run.stderr
    );
    let invite_json = invite_run.json();
    let invite_text = invite_json["invite"].as_str().unwrap().to_owned();
    let invite_id = invite_json["id"].as_str().unwrap().to_owned();
    assert_eq!(invite_json["principal"], json!("phone"));
    assert_eq!(invite_json["role"], json!("observer"));
    assert_eq!(invite_json["node"], json!("tower"));

    // Decode the invite payload and verify pinned host keys match our throwaway sshd.
    let payload = InvitePayload::decode(&invite_text).unwrap();
    assert_eq!(payload.v, 1);
    assert_eq!(payload.id, invite_id);
    assert_eq!(payload.node, "tower");
    assert_eq!(payload.hosts, vec!["127.0.0.1".to_owned()]);
    assert_eq!(payload.port, sshd.port());
    assert_eq!(payload.user, sshd.user());
    assert_eq!(payload.principal, "phone");
    assert_eq!(payload.host_keys, vec![sshd.host_key_line().to_owned()]);

    // Verify neither the invite string nor the raw key seed appears in audit.jsonl.
    let audit_raw = std::fs::read_to_string(home.state().join("audit.jsonl")).unwrap();
    assert!(!audit_raw.contains(&invite_text));
    assert!(!audit_raw.contains(&payload.key));

    // Verify `device.list` shows the pending invite.
    let before_list = home.run(&["device.list"]).json();
    assert_eq!(before_list["devices"].as_array().unwrap().len(), 0);
    assert_eq!(before_list["invites"].as_array().unwrap().len(), 1);
    assert_eq!(before_list["invites"][0]["id"], json!(invite_id));
    assert_eq!(before_list["invites"][0]["principal"], json!("phone"));

    // 2. Client side: write temp private key from invite seed, known_hosts from invite host_keys,
    //    and generate a fresh Ed25519 device key pair.
    let client_dir = tempfile::tempdir().unwrap();
    let temp_key = client_dir.path().join("temp_invite_key");
    let seed = payload.decode_seed().unwrap();
    write_openssh_ed25519_private_key(&temp_key, &seed, "invite").unwrap();

    let known_hosts = client_dir.path().join("known_hosts");
    write_known_hosts(
        &known_hosts,
        &payload.hosts[0],
        payload.port,
        &payload.host_keys,
    )
    .unwrap();

    let device_key = client_dir.path().join("device_ed25519");
    let device_pub_path = client_dir.path().join("device_ed25519.pub");
    let device_pub =
        generate_ed25519_keypair(sshd.ssh_keygen_bin(), &device_key, "phone@controller").unwrap();
    let expected_fp = key_fingerprint(sshd.ssh_keygen_bin(), &device_pub_path).unwrap();

    // 3a. Refusal checks over real ssh: enrolling the invite's own temporary key or an already-present
    //     foreign key must fail cleanly and leave the invite usable.
    let temp_pub = ed25519_public_key_from_seed(&seed);
    for (bad_pub, expected_code) in [(&temp_pub, "denied"), (&foreign_pub, "conflict")] {
        let bad_line = json!({
            "enrol": 1,
            "id": payload.id,
            "public_key": bad_pub,
            "name": "Rejected Key",
        })
        .to_string();
        let mut child = sshd
            .ssh_command(&temp_key, &known_hosts)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        {
            let mut stdin = child.stdin.take().unwrap();
            writeln!(stdin, "{bad_line}").unwrap();
            stdin.flush().unwrap();
        }
        let out = child.wait_with_output().unwrap();
        assert!(
            !out.status.success(),
            "enrolment unexpectedly accepted {bad_pub}"
        );
        let reply: Value =
            serde_json::from_str(String::from_utf8_lossy(&out.stdout).trim()).unwrap();
        assert_eq!(reply["ok"], json!(false));
        assert_eq!(reply["error"]["code"], json!(expected_code));
    }

    // 3b. Connect over real ssh using the temporary key and pass an arbitrary remote command (`id`)
    //    to confirm `legion gate --enrol` ignores SSH_ORIGINAL_COMMAND and completes enrolment.
    let enrol_line = json!({
        "enrol": 1,
        "id": payload.id,
        "public_key": device_pub,
        "name": "My Pixel Phone",
    })
    .to_string();
    let mut enrol_child = sshd
        .ssh_command(&temp_key, &known_hosts)
        .arg("id")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    {
        let mut stdin = enrol_child.stdin.take().unwrap();
        writeln!(stdin, "{enrol_line}").unwrap();
        stdin.flush().unwrap();
    }
    let enrol_out = enrol_child.wait_with_output().unwrap();
    assert!(
        enrol_out.status.success(),
        "ssh enrolment failed ({}): stdout={} stderr={}\nsshd:\n{}",
        enrol_out.status,
        String::from_utf8_lossy(&enrol_out.stdout),
        String::from_utf8_lossy(&enrol_out.stderr),
        sshd.logs(),
    );
    let enrol_reply: Value =
        serde_json::from_str(String::from_utf8_lossy(&enrol_out.stdout).trim()).unwrap();
    assert_eq!(
        enrol_reply,
        json!({
            "ok": true,
            "node": "tower",
            "principal": "phone",
            "role": "observer",
            "device": "my-pixel-phone",
        })
    );

    // Check enrolment audit records (denied temp key, conflict foreign key, then ok; door: ssh).
    let audit = home.audit();
    let enrol_entries: Vec<&Value> = audit
        .iter()
        .filter(|e| e["tool"] == json!("device.enrol"))
        .collect();
    assert_eq!(
        enrol_entries.len(),
        3,
        "enrol audit entries: {enrol_entries:?}"
    );
    assert_eq!(enrol_entries[0]["outcome"], json!("denied"));
    assert_eq!(enrol_entries[1]["outcome"], json!("conflict"));
    let enrol_entry = enrol_entries[2];
    assert_eq!(enrol_entry["door"], json!("ssh"));
    assert_eq!(enrol_entry["principal"], json!("phone"));
    assert_eq!(enrol_entry["outcome"], json!("ok"));

    // 4. (a) Connect with the newly enrolled device key and run a real `legion rpc` session.
    let mut rpc_child = sshd
        .ssh_command(&device_key, &known_hosts)
        .arg("legion rpc")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut rpc_stdin = rpc_child.stdin.take().unwrap();
    let mut rpc_stdout = BufReader::new(rpc_child.stdout.take().unwrap());

    writeln!(
        rpc_stdin,
        "{}",
        rpc_request(
            json!(1),
            "hello",
            json!({"client": "pairing-test", "version": "1", "protocol": 1})
        )
    )
    .unwrap();
    rpc_stdin.flush().unwrap();
    let hello = read_json_line(&mut rpc_stdout, "hello reply");
    assert_eq!(hello["id"], json!(1));
    assert_eq!(hello["result"]["node"], json!("tower"));

    // Attempt to escalate via `"as": "owner"` — must stay pinned to `phone` / `observer`.
    writeln!(
        rpc_stdin,
        "{}",
        rpc_request(
            json!(2),
            "call",
            json!({"tool": "meta.whoami", "args": {}, "as": "owner"})
        )
    )
    .unwrap();
    rpc_stdin.flush().unwrap();
    let whoami = read_json_line(&mut rpc_stdout, "whoami reply");
    assert_eq!(whoami["result"]["principal"], json!("phone"));
    assert_eq!(whoami["result"]["role"], json!("observer"));
    assert_eq!(whoami["result"]["door"], json!("rpc"));

    // Allowed read inside observer's `paths` succeeds.
    writeln!(
        rpc_stdin,
        "{}",
        rpc_request(
            json!(3),
            "call",
            json!({"tool": "fs.read", "args": {"path": note_path.to_string_lossy()}})
        )
    )
    .unwrap();
    rpc_stdin.flush().unwrap();
    let read_reply = read_json_line(&mut rpc_stdout, "fs.read reply");
    assert_eq!(read_reply["result"]["content"], json!("paired-read-ok\n"));

    // Disallowed write is denied.
    writeln!(
        rpc_stdin,
        "{}",
        rpc_request(
            json!(4),
            "call",
            json!({
                "tool": "fs.write",
                "args": {"path": projects_dir.join("app").join("nope.txt").to_string_lossy(), "content": "x"}
            })
        )
    )
    .unwrap();
    rpc_stdin.flush().unwrap();
    let write_reply = read_json_line(&mut rpc_stdout, "fs.write reply");
    assert_eq!(write_reply["error"]["data"]["code"], json!("denied"));

    // Disallowed audit.tail is denied.
    writeln!(
        rpc_stdin,
        "{}",
        rpc_request(json!(5), "call", json!({"tool": "audit.tail", "args": {}}))
    )
    .unwrap();
    rpc_stdin.flush().unwrap();
    let audit_reply = read_json_line(&mut rpc_stdout, "audit.tail reply");
    assert_eq!(audit_reply["error"]["data"]["code"], json!("denied"));

    drop(rpc_stdin);
    let rpc_status = rpc_child.wait().unwrap();
    assert!(rpc_status.success(), "legion rpc exited {rpc_status}");

    // 5. (b) Any other command over the enrolled device key gets `legion gate:` refusal.
    for cmd in ["whoami", "echo pwned", "legion rpc --as owner"] {
        let out = sshd
            .ssh_command(&device_key, &known_hosts)
            .arg(cmd)
            .stdin(Stdio::null())
            .output()
            .unwrap();
        assert!(
            !out.status.success(),
            "command `{cmd}` unexpectedly succeeded"
        );
        let stderr = String::from_utf8_lossy(&out.stderr);
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            stderr.contains("legion gate:"),
            "expected gate refusal for `{cmd}`, got stdout={stdout:?} stderr={stderr:?}"
        );
        assert!(!stdout.contains("pwned"));
    }

    // 6. (c) The temporary invite key no longer authenticates.
    let reuse = sshd
        .ssh_command(&temp_key, &known_hosts)
        .arg("legion rpc")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(
        !reuse.status.success(),
        "temporary invite key still authenticated after enrolment!"
    );

    // 7. (d) `device.list` shows the device with the exact `ssh-keygen -l` fingerprint, and
    //    `device.remove` both ends any held-open SSH session immediately and prevents reconnect.
    let listed = home.run(&["device.list"]).json();
    assert_eq!(listed["invites"].as_array().unwrap().len(), 0);
    let devices = listed["devices"].as_array().unwrap();
    assert_eq!(devices.len(), 1);
    assert_eq!(devices[0]["name"], json!("my-pixel-phone"));
    assert_eq!(devices[0]["principal"], json!("phone"));
    assert_eq!(devices[0]["fingerprint"], json!(expected_fp));

    // Hold open a live SSH `legion rpc` session across `device.remove`.
    let mut live_child = sshd
        .ssh_command(&device_key, &known_hosts)
        .arg("legion rpc")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut live_stdin = live_child.stdin.take().unwrap();
    let mut live_stdout = BufReader::new(live_child.stdout.take().unwrap());
    writeln!(
        live_stdin,
        "{}",
        rpc_request(
            json!(10),
            "hello",
            json!({"client": "live-session-test", "version": "1", "protocol": 1})
        )
    )
    .unwrap();
    live_stdin.flush().unwrap();
    let live_hello = read_json_line(&mut live_stdout, "live session hello reply");
    assert_eq!(live_hello["id"], json!(10));

    let removed = home.run(&["device.remove", "name=my-pixel-phone"]);
    assert_eq!(removed.code, 0, "device.remove failed: {}", removed.stderr);
    let removed_json = removed.json();
    assert_eq!(removed_json["removed"], json!(true));
    assert_eq!(removed_json["kind"], json!("device"));
    assert_eq!(removed_json["name"], json!("my-pixel-phone"));
    assert_eq!(removed_json["sessions_ended"], json!(1));

    // The held-open session must terminate promptly and must not answer further calls.
    let _ = writeln!(
        live_stdin,
        "{}",
        rpc_request(
            json!(11),
            "call",
            json!({"tool": "meta.whoami", "args": {}})
        )
    );
    let _ = live_stdin.flush();
    let mut after_revoke_line = String::new();
    let bytes_read = live_stdout.read_line(&mut after_revoke_line).unwrap_or(0);
    assert_eq!(
        bytes_read, 0,
        "revoked live session unexpectedly answered: {after_revoke_line:?}"
    );
    drop(live_stdin);
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(_status) = live_child.try_wait().unwrap() {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "held-open ssh session did not exit after device.remove"
        );
        std::thread::sleep(Duration::from_millis(25));
    }

    let after_revoke = sshd
        .ssh_command(&device_key, &known_hosts)
        .arg("legion rpc")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(
        !after_revoke.status.success(),
        "revoked device key still authenticated!"
    );

    // Foreign lines in authorized_keys survived the entire lifecycle byte for byte.
    let final_auth = std::fs::read_to_string(&auth_keys).unwrap();
    assert_eq!(final_auth, format!("{foreign_comment}{foreign_key}"));

    // 8. (e) sshd itself refuses a temporary invite key after `expiry-time`.
    let invite2 = home
        .run(&[
            "device.invite",
            "principal=phone",
            "host=127.0.0.1",
            &format!("port={}", sshd.port()),
            &format!("user={}", sshd.user()),
        ])
        .json();
    let payload2 = InvitePayload::decode(invite2["invite"].as_str().unwrap()).unwrap();
    let temp_key2 = client_dir.path().join("temp_invite_key_2");
    write_openssh_ed25519_private_key(&temp_key2, &payload2.decode_seed().unwrap(), "expired")
        .unwrap();

    // Rewrite only sshd's `expiry-time="..."` option on the invite line to a past 12-digit time
    // (portable across OpenSSH 8.9+) while keeping state/devices.json unexpired, proving sshd
    // itself enforces `expiry-time`.
    let auth_before = std::fs::read_to_string(&auth_keys).unwrap();
    let marker = "expiry-time=\"";
    let start = auth_before
        .find(marker)
        .expect("invite line must carry expiry-time");
    let val_start = start + marker.len();
    let val_end = val_start + auth_before[val_start..].find('"').unwrap();
    let written_expiry = &auth_before[val_start..val_end];
    assert_eq!(
        written_expiry.len(),
        12,
        "expiry-time must be 12 digits (YYYYMMDDHHMM without Z): {written_expiry}"
    );
    assert!(
        written_expiry.bytes().all(|b| b.is_ascii_digit()),
        "expiry-time must be all ASCII digits: {written_expiry}"
    );
    let patched = format!(
        "{}202001010000{}",
        &auth_before[..val_start],
        &auth_before[val_end..]
    );
    std::fs::write(&auth_keys, patched).unwrap();

    let mut expired_child = sshd
        .ssh_command(&temp_key2, &known_hosts)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    {
        let mut stdin = expired_child.stdin.take().unwrap();
        let req = json!({
            "enrol": 1,
            "id": payload2.id,
            "public_key": device_pub,
            "name": "Expired Phone",
        });
        let _ = writeln!(stdin, "{req}");
    }
    let expired_out = expired_child.wait_with_output().unwrap();
    assert!(
        !expired_out.status.success(),
        "sshd accepted an invite key past expiry-time!"
    );
    assert!(
        expired_out.stdout.is_empty(),
        "legion gate --enrol should never have been launched after expiry-time; stdout={}",
        String::from_utf8_lossy(&expired_out.stdout)
    );
    // Give sshd a brief moment to flush its log, then verify sshd refused the key via its options
    // (`expiry-time`), rather than missing the key or launching `legion gate --enrol`.
    std::thread::sleep(Duration::from_millis(50));
    let sshd_log = sshd.logs().to_ascii_lowercase();
    assert!(
        sshd_log.contains("refused by key options") || sshd_log.contains("expired"),
        "expected sshd log to note key-option/expiry refusal; log:\n{sshd_log}"
    );
    // And state/devices.json still has no device enrolled from the expired attempt.
    let after_expired = home.run(&["device.list"]).json();
    assert_eq!(after_expired["devices"].as_array().unwrap().len(), 0);
}

#[test]
fn legion_pair_prints_summary_qr_invite_and_commands_and_supports_invert_and_json() {
    let home = Home::new();
    let auth_keys = home.home().join("pair_authorized_keys");
    let host_pub = home.home().join("ssh_host_ed25519_key.pub");
    std::fs::write(
        &host_pub,
        "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIOMqqnkVzrm0SdG6UOoqKLsabgH5C9okWi0dh2l9GKJl host@tower\n",
    )
    .unwrap();
    home.write_config(
        "node.toml",
        &format!(
            "name = \"tower\"\n\n[devices]\nauthorized_keys = {}\nhost_keys = [{}]\n",
            toml_literal(&auth_keys),
            toml_literal(&host_pub),
        ),
    );
    home.write_config(
        "policy.toml",
        "default = \"owner\"\n\n[roles.observer]\nallow = [\"effect:read\"]\n\n[principals.phone]\nrole = \"observer\"\n",
    );

    let normal = home.run(&[
        "pair",
        "--as",
        "phone",
        "--valid",
        "30m",
        "--host",
        "tower.example",
        "--host",
        "100.64.0.10",
        "--port",
        "2222",
        "--user",
        "me",
    ]);
    assert_eq!(normal.code, 0, "stderr={}", normal.stderr);
    let lines: Vec<&str> = normal.stdout.lines().collect();
    assert!(lines.len() > 15, "output too short:\n{}", normal.stdout);
    assert!(
        lines[0].contains("phone") && lines[0].contains("observer") && lines[0].contains("tower"),
        "summary line 0: {}",
        lines[0]
    );
    assert!(
        lines[1].contains("valid until") && lines[1].contains("single use"),
        "summary line 1: {}",
        lines[1]
    );
    // QR code contains Unicode half blocks and solid quiet-zone blocks.
    assert!(
        normal.stdout.contains('▀') || normal.stdout.contains('▄'),
        "missing Unicode half-block QR characters:\n{}",
        normal.stdout
    );
    assert!(normal.stdout.contains('█'));

    // Find the standalone invite text line and decode it cleanly.
    let invite_line = lines
        .iter()
        .copied()
        .find(|line| line.starts_with("legion-invite-1."))
        .expect("missing invite text line in legion pair output");
    let decoded = InvitePayload::decode(invite_line).unwrap();
    assert_eq!(decoded.v, 1);
    assert_eq!(decoded.node, "tower");
    assert_eq!(
        decoded.hosts,
        vec!["tower.example".to_owned(), "100.64.0.10".to_owned()]
    );
    assert_eq!(decoded.port, 2222);
    assert_eq!(decoded.user, "me");
    assert_eq!(decoded.principal, "phone");
    assert_eq!(decoded.host_keys.len(), 1);
    assert!(normal.stdout.contains("legion device.list"));
    assert!(normal.stdout.contains("legion device.remove"));

    // `--invert` flips the QR polarity (first QR line becomes spaces instead of `█`).
    let inverted = home.run(&[
        "pair",
        "--as",
        "phone",
        "--host",
        "tower.example",
        "--invert",
    ]);
    assert_eq!(inverted.code, 0, "stderr={}", inverted.stderr);
    let qr_start_normal = lines[3];
    let qr_start_inverted = inverted.stdout.lines().nth(3).unwrap();
    assert!(qr_start_normal.chars().all(|c| c == '█'));
    assert!(qr_start_inverted.chars().all(|c| c == ' '));

    // `--json` prints the raw tool result.
    let as_json = home.run(&["pair", "--as", "phone", "--host", "tower.example", "--json"]);
    assert_eq!(as_json.code, 0, "stderr={}", as_json.stderr);
    let parsed = as_json.json();
    assert_eq!(parsed["principal"], json!("phone"));
    assert_eq!(parsed["role"], json!("observer"));
    assert_eq!(parsed["node"], json!("tower"));
    assert!(
        parsed["invite"]
            .as_str()
            .unwrap()
            .starts_with("legion-invite-1.")
    );

    // `legion help pair` explains pairing in plain words.
    let help = home.run(&["help", "pair"]);
    assert_eq!(help.code, 0);
    assert!(help.stdout.contains("legion pair --as PRINCIPAL"));
    assert!(help.stdout.contains("--invert"));
}

#[test]
fn non_admin_roles_cannot_edit_ssh_or_configured_authorized_keys_via_file_tools() {
    let home = Home::new();
    let custom_dir = home.home().join("custom-keys");
    std::fs::create_dir_all(&custom_dir).unwrap();
    let custom_auth = custom_dir.join("authorized_keys");
    std::fs::write(&custom_auth, "# existing\n").unwrap();
    let default_auth = home.home().join(".ssh").join("authorized_keys");
    std::fs::write(&default_auth, "# default\n").unwrap();

    home.write_config(
        "node.toml",
        &format!(
            "name = \"tower\"\n\n[devices]\nauthorized_keys = {}\n",
            toml_literal(&custom_auth),
        ),
    );
    home.write_config(
        "policy.toml",
        &format!(
            "default = \"owner\"\n\n\
             [roles.editor]\n\
             allow = [\"effect:read\", \"effect:write\"]\n\
             paths = [\"~/**\", {}]\n\n\
             [principals.helper]\n\
             role = \"editor\"\n",
            toml_literal(&custom_dir),
        ),
    );

    // Non-admin role cannot read or write ~/.ssh/authorized_keys or [devices] authorized_keys.
    for target in [&default_auth, &custom_auth] {
        let p = target.to_string_lossy();
        let read = home.run(&["fs.read", &format!("path={p}"), "--as", "helper"]);
        assert_eq!(read.code, 1);
        assert_eq!(read.json()["code"], json!("denied"));

        let write = home.run(&[
            "fs.write",
            &format!("path={p}"),
            "content=ssh-ed25519 AAAA evil",
            "--as",
            "helper",
        ]);
        assert_eq!(write.code, 1);
        assert_eq!(write.json()["code"], json!("denied"));
    }

    // Lock and atomic-write staging siblings beside [devices] authorized_keys are also denied.
    for sibling_name in [
        ".authorized_keys.legion.lock",
        ".authorized_keys.legion-tmp-1-0-0",
    ] {
        let p = custom_dir.join(sibling_name);
        let write = home.run(&[
            "fs.write",
            &format!("path={}", p.to_string_lossy()),
            "content=evil",
            "--as",
            "helper",
        ]);
        assert_eq!(write.code, 1, "sibling {sibling_name} should be denied");
        assert_eq!(write.json()["code"], json!("denied"));
    }

    // Sibling file in custom_dir is still writable by editor, proving only authorized_keys
    // and its lock/staging siblings are blocked.
    let sibling = custom_dir.join("ok.txt");
    let ok_write = home.run(&[
        "fs.write",
        &format!("path={}", sibling.to_string_lossy()),
        "content=hello",
        "--as",
        "helper",
    ]);
    assert_eq!(ok_write.code, 0, "stderr={}", ok_write.stderr);

    // On Unix, a symlinked ~/.ssh/authorized_keys target (e.g. ~/dotfiles/authorized_keys) and
    // its lock/staging siblings are also closed to a non-admin role with write access on `~/**`.
    #[cfg(unix)]
    {
        use std::os::unix::fs::symlink;

        let dotfiles = home.home().join("dotfiles");
        std::fs::create_dir_all(&dotfiles).unwrap();
        let linked_target = dotfiles.join("authorized_keys");
        std::fs::write(&linked_target, "# linked\n").unwrap();
        std::fs::remove_file(&default_auth).unwrap();
        symlink(&linked_target, &default_auth).unwrap();

        for target in [
            linked_target.clone(),
            dotfiles.join(".authorized_keys.legion.lock"),
            dotfiles.join(".authorized_keys.legion-tmp-42-0-999"),
        ] {
            let write = home.run(&[
                "fs.write",
                &format!("path={}", target.to_string_lossy()),
                "content=ssh-ed25519 AAAA evil",
                "--as",
                "helper",
            ]);
            assert_eq!(
                write.code,
                1,
                "symlink target/sibling {} should be denied",
                target.display()
            );
            assert_eq!(write.json()["code"], json!("denied"));
        }
        assert_eq!(
            std::fs::read_to_string(&linked_target).unwrap(),
            "# linked\n"
        );

        let other_dotfile = dotfiles.join("other.txt");
        let ok_dotfile = home.run(&[
            "fs.write",
            &format!("path={}", other_dotfile.to_string_lossy()),
            "content=ok",
            "--as",
            "helper",
        ]);
        assert_eq!(ok_dotfile.code, 0, "stderr={}", ok_dotfile.stderr);
    }

    // And the device.* admin tools themselves are denied to editor.
    for tool in ["device.invite", "device.list", "device.remove"] {
        let run = home.run(&[tool, "--as", "helper"]);
        assert_eq!(run.code, 1);
        assert_eq!(run.json()["code"], json!("denied"));
    }
}

#[test]
fn gate_account_pinning_refuses_other_accounts_and_scopes_list_and_remove() {
    let home = Home::new();
    let auth_keys = home.home().join("shared_authorized_keys");
    let me = legion_tools::device::authkeys::current_account_name().unwrap();
    let other_account = format!("{me}-other");

    let my_pub = ed25519_public_key_from_seed(&[21_u8; 32]);
    let other_pub = ed25519_public_key_from_seed(&[22_u8; 32]);
    let exe = Path::new("/usr/local/bin/legion");
    let my_line = legion_tools::device::authkeys::format_device_line(
        exe,
        "phone",
        &my_pub,
        "my-phone",
        Some(home.state().parent().unwrap()),
        Some(&me),
        legion_tools::device::authkeys::ForcedCommandShell::Posix,
    )
    .unwrap();
    let other_line = legion_tools::device::authkeys::format_device_line(
        exe,
        "phone",
        &other_pub,
        "other-phone",
        Some(home.state().parent().unwrap()),
        Some(&other_account),
        legion_tools::device::authkeys::ForcedCommandShell::Posix,
    )
    .unwrap();
    std::fs::write(&auth_keys, format!("{my_line}\n{other_line}\n")).unwrap();

    home.write_config(
        "node.toml",
        &format!(
            "name = \"tower\"\n\n[devices]\nauthorized_keys = {}\n",
            toml_literal(&auth_keys)
        ),
    );
    home.write_config(
        "policy.toml",
        "default = \"owner\"\n\n[roles.observer]\nallow = [\"effect:read\"]\n\n[principals.phone]\nrole = \"observer\"\n",
    );

    // `device.list` only shows devices pinned to this account, not another account's lines.
    let listed = home.run(&["device.list"]).json();
    let names: Vec<&str> = listed["devices"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, vec!["my-phone"]);

    // `device.remove` refuses to remove another account's device and leaves its line intact.
    let remove_other = home.run(&["device.remove", "name=other-phone"]);
    assert_eq!(remove_other.code, 1);
    assert_eq!(remove_other.json()["code"], json!("not_found"));
    assert!(
        std::fs::read_to_string(&auth_keys)
            .unwrap()
            .contains("legion-device:other-phone")
    );

    // `legion gate --as` and `legion gate --enrol` both refuse a mismatched `--account`.
    let bad_as = home
        .command(&["gate", "--as", "phone", "--account", &other_account])
        .env("SSH_ORIGINAL_COMMAND", "legion rpc")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(!bad_as.status.success());
    let bad_as_stderr = String::from_utf8_lossy(&bad_as.stderr);
    assert!(
        bad_as_stderr.contains("legion gate:") && bad_as_stderr.contains(&other_account),
        "stderr={bad_as_stderr:?}"
    );

    let bad_enrol = home
        .command(&[
            "gate",
            "--enrol",
            "0123456789abcdef0123456789abcdef",
            "--account",
            &other_account,
        ])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(!bad_enrol.status.success());
    let enrol_reply: Value =
        serde_json::from_str(String::from_utf8_lossy(&bad_enrol.stdout).trim()).unwrap();
    assert_eq!(enrol_reply["ok"], json!(false));
    assert_eq!(enrol_reply["error"]["code"], json!("denied"));
}

#[test]
fn device_invite_refuses_disjoint_keyscan_when_host_key_files_exist_and_falls_back_when_absent() {
    if !TestSshd::available() {
        eprintln!(
            "skipping device_invite_refuses_disjoint_keyscan_when_host_key_files_exist_and_falls_back_when_absent: sshd unavailable"
        );
        return;
    }
    let has_keyscan = std::process::Command::new("ssh-keyscan")
        .arg("-h")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok();
    if !has_keyscan {
        eprintln!(
            "skipping device_invite_refuses_disjoint_keyscan_when_host_key_files_exist_and_falls_back_when_absent: ssh-keyscan unavailable"
        );
        return;
    }

    let home = Home::new_strict();
    let auth_keys = home.home().join("scan_authorized_keys");
    let home_dir_str = home.home().to_string_lossy().into_owned();
    let Some(sshd) = TestSshd::start(auth_keys.clone(), &[("HOME", &home_dir_str)])
        .expect("throwaway sshd failed to start")
    else {
        return;
    };

    // Configure ONLY [devices] authorized_keys — intentionally omit [devices] host_keys.
    home.write_config(
        "node.toml",
        &format!(
            "name = \"tower\"\n\n[devices]\nauthorized_keys = {}\n",
            toml_literal(&auth_keys),
        ),
    );
    home.write_config(
        "policy.toml",
        "default = \"owner\"\n\n[roles.observer]\nallow = [\"effect:read\"]\n\n[principals.phone]\nrole = \"observer\"\n",
    );

    let default_files_readable = legion_tools::device::keys::read_host_keys(&[], None)
        .map(|k| !k.is_empty())
        .unwrap_or(false);

    let invite_run = home.run(&[
        "device.invite",
        "principal=phone",
        "host=127.0.0.1",
        &format!("port={}", sshd.port()),
        &format!("user={}", sshd.user()),
    ]);

    let invite_json = if default_files_readable {
        // When default /etc/ssh/ssh_host_*_key.pub files exist on the machine, a throwaway high-port
        // sshd presenting an unrelated ephemeral host key MUST be refused with a hint to configure
        // `[devices] host_keys`, rather than letting ssh-keyscan override host-key files.
        assert_eq!(
            invite_run.code, 1,
            "expected disjoint keyscan vs default host key files to be refused: stdout={} stderr={}",
            invite_run.stdout, invite_run.stderr
        );
        let err = invite_run.json();
        assert_eq!(err["code"], json!("denied"), "{err}");
        assert!(
            err["hint"]
                .as_str()
                .is_some_and(|h| h.contains("[devices] host_keys")),
            "expected [devices] host_keys hint, got {err}"
        );

        // Explicitly configuring [devices] host_keys succeeds and pins the throwaway sshd's key.
        home.write_config(
            "node.toml",
            &format!(
                "name = \"tower\"\n\n[devices]\nauthorized_keys = {}\nhost_keys = [{}]\n",
                toml_literal(&auth_keys),
                toml_literal(sshd.host_key_path()),
            ),
        );
        let configured_run = home.run(&[
            "device.invite",
            "principal=phone",
            "host=127.0.0.1",
            &format!("port={}", sshd.port()),
            &format!("user={}", sshd.user()),
        ]);
        assert_eq!(
            configured_run.code, 0,
            "configured device.invite failed: stdout={} stderr={}",
            configured_run.stdout, configured_run.stderr
        );
        let json = configured_run.json();
        assert_eq!(json["host_keys_source"], json!("configured"));
        assert_eq!(json["host_keys"], json!([sshd.host_key_line()]));
        json
    } else {
        // When no default host-key files are readable, ssh-keyscan alone is used.
        assert_eq!(
            invite_run.code, 0,
            "device.invite failed without readable default host-key files: stdout={} stderr={}",
            invite_run.stdout, invite_run.stderr
        );
        let json = invite_run.json();
        assert_eq!(json["host_keys_source"], json!("ssh-keyscan"));
        assert_eq!(json["host_keys"], json!([sshd.host_key_line()]));
        json
    };

    // Confirm the scanned host key works for real StrictHostKeyChecking=yes enrolment.
    let payload = InvitePayload::decode(invite_json["invite"].as_str().unwrap()).unwrap();
    let client_dir = tempfile::tempdir().unwrap();
    let temp_key = client_dir.path().join("temp_key");
    write_openssh_ed25519_private_key(&temp_key, &payload.decode_seed().unwrap(), "scan-invite")
        .unwrap();
    let known_hosts = client_dir.path().join("known_hosts");
    write_known_hosts(
        &known_hosts,
        &payload.hosts[0],
        payload.port,
        &payload.host_keys,
    )
    .unwrap();
    let device_key = client_dir.path().join("device_ed25519");
    let device_pub =
        generate_ed25519_keypair(sshd.ssh_keygen_bin(), &device_key, "phone@scan").unwrap();

    let mut enrol_child = sshd
        .ssh_command(&temp_key, &known_hosts)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    {
        let mut stdin = enrol_child.stdin.take().unwrap();
        let req = json!({
            "enrol": 1,
            "id": payload.id,
            "public_key": device_pub,
            "name": "Scanned Phone",
        });
        writeln!(stdin, "{req}").unwrap();
    }
    let out = enrol_child.wait_with_output().unwrap();
    assert!(
        out.status.success(),
        "enrolment with scanned host key failed: stdout={} stderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let reply: Value = serde_json::from_str(String::from_utf8_lossy(&out.stdout).trim()).unwrap();
    assert_eq!(reply["ok"], json!(true));
    assert_eq!(reply["device"], json!("scanned-phone"));
}
