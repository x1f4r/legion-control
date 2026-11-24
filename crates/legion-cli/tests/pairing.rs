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
    assert_eq!(enrol_entry["device"], json!("my-pixel-phone"));
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
    assert_eq!(whoami["result"]["door"], json!("ssh"));

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

    // Every call served through the paired device gate records door="ssh" and device="my-pixel-phone".
    let audit_after_rpc = home.audit();
    for tool_name in ["meta.whoami", "fs.read", "fs.write", "audit.tail"] {
        let entry = audit_after_rpc
            .iter()
            .rfind(|e| e["tool"] == json!(tool_name))
            .unwrap_or_else(|| panic!("missing audit entry for {tool_name}: {audit_after_rpc:?}"));
        assert_eq!(entry["door"], json!("ssh"), "{entry}");
        assert_eq!(entry["device"], json!("my-pixel-phone"), "{entry}");
    }

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
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    loop {
        if let Some(_status) = live_child.try_wait().unwrap() {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "held-open ssh session did not exit within 15s after device.remove"
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
    // Poll for sshd to flush its log, then verify sshd refused the key via its options
    // (`expiry-time`), rather than missing the key or launching `legion gate --enrol`.
    let log_deadline = std::time::Instant::now() + Duration::from_secs(5);
    let sshd_log = loop {
        let log = sshd.logs().to_ascii_lowercase();
        if log.contains("refused by key options") || log.contains("expired") {
            break log;
        }
        if std::time::Instant::now() >= log_deadline {
            break log;
        }
        std::thread::sleep(Duration::from_millis(25));
    };
    assert!(
        sshd_log.contains("refused by key options") || sshd_log.contains("expired"),
        "timed out waiting 5s for sshd log to note key-option/expiry refusal; log:\n{sshd_log}"
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

    // `--name` + `--role` creates `device:<name>` in policy.toml and issues the invite.
    let role_pair = home.run(&[
        "pair",
        "--name",
        "Work Tablet",
        "--role",
        "observer",
        "--host",
        "tower.example",
        "--json",
    ]);
    assert_eq!(role_pair.code, 0, "stderr={}", role_pair.stderr);
    let role_json = role_pair.json();
    assert_eq!(role_json["principal"], json!("device:work-tablet"));
    assert_eq!(role_json["role"], json!("observer"));

    // Running plain `legion pair` non-interactively refuses rather than assuming a preset role.
    let non_tty = home.run(&["pair"]);
    assert_ne!(non_tty.code, 0);

    // `legion help pair` explains pairing in plain words.
    let help = home.run(&["help", "pair"]);
    assert_eq!(help.code, 0);
    assert!(help.stdout.contains("legion pair --as PRINCIPAL"));
    assert!(help.stdout.contains("--role ROLE"));
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
    let exe = Path::new(env!("CARGO_BIN_EXE_legion"));
    let shell = legion_tools::device::authkeys::ForcedCommandShell::host().unwrap();
    let pinned_home = legion_tools::device::authkeys::PinnedHome::Root(
        home.state().parent().unwrap().to_path_buf(),
    );
    let my_line = legion_tools::device::authkeys::format_device_line(
        exe,
        "phone",
        &my_pub,
        "my-phone",
        "11111111111111111111111111111111",
        &pinned_home,
        Some(&me),
        shell,
    )
    .unwrap();
    let other_line = legion_tools::device::authkeys::format_device_line(
        exe,
        "phone",
        &other_pub,
        "other-phone",
        "22222222222222222222222222222222",
        &pinned_home,
        Some(&other_account),
        shell,
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
    let home_root = home
        .state()
        .parent()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    let bad_as = home
        .command(&[
            "gate",
            "--as",
            "phone",
            "--account",
            &other_account,
            "--home",
            &home_root,
        ])
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
            "--home",
            &home_root,
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
        assert_eq!(json["host_keys_source"], json!("ssh-keyscan only"));
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

#[test]
fn accept_env_cannot_redirect_gate_home_to_crafted_policy() {
    if !TestSshd::available() {
        eprintln!(
            "skipping accept_env_cannot_redirect_gate_home_to_crafted_policy: sshd unavailable"
        );
        return;
    }

    let root = Home::new_strict();
    let user_home = root.home().to_path_buf();
    let default_cfg = user_home.join(".config").join("legion");
    let default_state = user_home.join(".local").join("state").join("legion");
    std::fs::create_dir_all(&default_cfg).unwrap();
    std::fs::create_dir_all(&default_state).unwrap();
    let auth_keys = user_home.join(".ssh").join("authorized_keys");
    std::fs::create_dir_all(auth_keys.parent().unwrap()).unwrap();

    // Start sshd WITH `AcceptEnv LEGION_HOME XDG_CONFIG_HOME XDG_STATE_HOME` so a connecting SSH
    // client can pass arbitrary LEGION_HOME / XDG_* values into the remote session environment.
    let home_dir_str = user_home.to_string_lossy().into_owned();
    let Some(sshd) = TestSshd::start_with_accept_env(
        auth_keys.clone(),
        &[("HOME", &home_dir_str)],
        &["LEGION_HOME", "XDG_CONFIG_HOME", "XDG_STATE_HOME"],
    )
    .expect("throwaway sshd failed to start") else {
        return;
    };

    // Real default-home node: `phone` is restricted to `observer` (read-only).
    std::fs::write(
        default_cfg.join("node.toml"),
        format!(
            "name = \"default-tower\"\n\n[devices]\nauthorized_keys = {}\nhost_keys = [{}]\n",
            toml_literal(&auth_keys),
            toml_literal(sshd.host_key_path()),
        ),
    )
    .unwrap();
    std::fs::write(
        default_cfg.join("policy.toml"),
        "default = \"owner\"\n\n\
         [roles.observer]\n\
         allow = [\"effect:read\"]\n\n\
         [principals.phone]\n\
         role = \"observer\"\n",
    )
    .unwrap();

    // Attacker-controlled policy directories granting `phone` the `owner` role.
    let evil_legion_home = user_home.join("evil-home");
    std::fs::create_dir_all(evil_legion_home.join("config")).unwrap();
    std::fs::write(
        evil_legion_home.join("config").join("node.toml"),
        "name = \"evil-tower\"\n",
    )
    .unwrap();
    std::fs::write(
        evil_legion_home.join("config").join("policy.toml"),
        "default = \"owner\"\n\n[principals.phone]\nrole = \"owner\"\n",
    )
    .unwrap();

    let evil_xdg_cfg = user_home.join("evil-xdg-cfg");
    let evil_xdg_state = user_home.join("evil-xdg-state");
    std::fs::create_dir_all(evil_xdg_cfg.join("legion")).unwrap();
    std::fs::create_dir_all(evil_xdg_state.join("legion")).unwrap();
    std::fs::write(
        evil_xdg_cfg.join("legion").join("node.toml"),
        "name = \"evil-xdg\"\n",
    )
    .unwrap();
    std::fs::write(
        evil_xdg_cfg.join("legion").join("policy.toml"),
        "default = \"owner\"\n\n[principals.phone]\nrole = \"owner\"\n",
    )
    .unwrap();

    // 1. Run `device.invite` with NO LEGION_HOME and NO XDG_* so it uses the default home; the
    //    invite and resulting device line MUST still pin `--config-dir` and `--state-dir`.
    let legion_bin = std::path::PathBuf::from(env!("CARGO_BIN_EXE_legion"));
    let invite_out = std::process::Command::new(&legion_bin)
        .args([
            "device.invite",
            "principal=phone",
            "host=127.0.0.1",
            &format!("port={}", sshd.port()),
            &format!("user={}", sshd.user()),
            "--json",
        ])
        .env("HOME", &user_home)
        .env_remove("LEGION_HOME")
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("XDG_STATE_HOME")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(
        invite_out.status.success(),
        "default-home device.invite failed: stdout={} stderr={}",
        String::from_utf8_lossy(&invite_out.stdout),
        String::from_utf8_lossy(&invite_out.stderr)
    );
    let invite_json: Value = serde_json::from_slice(&invite_out.stdout).expect("invite json");
    let payload = InvitePayload::decode(invite_json["invite"].as_str().unwrap()).unwrap();

    let auth_after_invite = std::fs::read_to_string(&auth_keys).unwrap();
    assert!(
        auth_after_invite.contains("--config-dir") && auth_after_invite.contains("--state-dir"),
        "default-home invite line must pin --config-dir and --state-dir:\n{auth_after_invite}"
    );

    // 2. Enrol over SSH while sending attacker LEGION_HOME and XDG_* via SendEnv.
    let client_dir = tempfile::tempdir().unwrap();
    let temp_key = client_dir.path().join("temp_key");
    write_openssh_ed25519_private_key(&temp_key, &payload.decode_seed().unwrap(), "acceptenv")
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
        generate_ed25519_keypair(sshd.ssh_keygen_bin(), &device_key, "phone@acceptenv").unwrap();

    let mut enrol_child = sshd
        .ssh_command(&temp_key, &known_hosts)
        .args([
            "-o",
            "SendEnv=LEGION_HOME",
            "-o",
            "SendEnv=XDG_CONFIG_HOME",
            "-o",
            "SendEnv=XDG_STATE_HOME",
        ])
        .env("LEGION_HOME", &evil_legion_home)
        .env("XDG_CONFIG_HOME", &evil_xdg_cfg)
        .env("XDG_STATE_HOME", &evil_xdg_state)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    writeln!(
        enrol_child.stdin.take().unwrap(),
        "{}",
        json!({
            "enrol": 1,
            "id": payload.id,
            "public_key": device_pub,
            "name": "Default Home Phone",
        })
    )
    .unwrap();
    let enrol_out = enrol_child.wait_with_output().unwrap();
    assert!(
        enrol_out.status.success(),
        "default-home enrolment failed: stdout={} stderr={}\nsshd:\n{}",
        String::from_utf8_lossy(&enrol_out.stdout),
        String::from_utf8_lossy(&enrol_out.stderr),
        sshd.logs(),
    );
    let enrol_reply: Value =
        serde_json::from_str(String::from_utf8_lossy(&enrol_out.stdout).trim()).unwrap();
    assert_eq!(enrol_reply["node"], json!("default-tower"));
    assert_eq!(enrol_reply["role"], json!("observer"));

    let auth_after_enrol = std::fs::read_to_string(&auth_keys).unwrap();
    assert!(
        auth_after_enrol.contains("--config-dir") && auth_after_enrol.contains("--state-dir"),
        "default-home device line must pin --config-dir and --state-dir:\n{auth_after_enrol}"
    );

    // 3. Also append a hand-written `legion gate --as phone` line WITHOUT --home/--config-dir to
    //    prove `legion gate` ignores LEGION_HOME and XDG_* from the environment even without flags.
    let hand_key = client_dir.path().join("hand_ed25519");
    let hand_pub =
        generate_ed25519_keypair(sshd.ssh_keygen_bin(), &hand_key, "phone@hand").unwrap();
    let hand_line = format!(
        "restrict,command=\"'{}' gate --as phone\" {hand_pub}\n",
        legion_bin.display()
    );
    let mut current_auth = std::fs::read_to_string(&auth_keys).unwrap();
    current_auth.push_str(&hand_line);
    std::fs::write(&auth_keys, current_auth).unwrap();

    for (label, key_path) in [
        ("paired-default-home", &device_key),
        ("handwritten-no-home-flag", &hand_key),
    ] {
        let mut rpc_child = sshd
            .ssh_command(key_path, &known_hosts)
            .args([
                "-o",
                "SendEnv=LEGION_HOME",
                "-o",
                "SendEnv=XDG_CONFIG_HOME",
                "-o",
                "SendEnv=XDG_STATE_HOME",
                "legion rpc",
            ])
            .env("LEGION_HOME", &evil_legion_home)
            .env("XDG_CONFIG_HOME", &evil_xdg_cfg)
            .env("XDG_STATE_HOME", &evil_xdg_state)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        {
            let mut stdin = rpc_child.stdin.take().unwrap();
            writeln!(
                stdin,
                "{}",
                rpc_request(json!(1), "call", json!({"tool": "meta.whoami", "args": {}}))
            )
            .unwrap();
            writeln!(
                stdin,
                "{}",
                rpc_request(
                    json!(2),
                    "call",
                    json!({
                        "tool": "fs.write",
                        "args": {
                            "path": user_home.join("pwned.txt").to_string_lossy(),
                            "content": "should not write",
                        }
                    })
                )
            )
            .unwrap();
        }
        let mut stdout = BufReader::new(rpc_child.stdout.take().unwrap());
        let mut replies = std::collections::HashMap::new();
        for n in 0..2 {
            let msg = read_json_line(&mut stdout, &format!("{label} reply {n}"));
            let id = msg["id"].as_i64().unwrap();
            replies.insert(id, msg);
        }
        let whoami = &replies[&1];
        assert_eq!(
            whoami["result"]["principal"],
            json!("phone"),
            "{label}: {whoami}"
        );
        assert_eq!(
            whoami["result"]["role"],
            json!("observer"),
            "{label}: SendEnv escalated role! {whoami}"
        );
        let write_reply = &replies[&2];
        assert_eq!(
            write_reply["error"]["data"]["code"],
            json!("denied"),
            "{label}: SendEnv bypassed observer confinement! {write_reply}"
        );
        let status = rpc_child.wait().unwrap();
        assert!(status.success(), "{label}: ssh exited {status}");
        assert!(!user_home.join("pwned.txt").exists(), "{label}");
    }
}

#[test]
fn gate_refuses_missing_pinned_home_without_creating_anything() {
    let dir = tempfile::tempdir().unwrap();
    let legion_bin = env!("CARGO_BIN_EXE_legion");

    // 1. Deleted --home directory: neither `gate --as` nor `gate --enrol` may recreate it.
    let gone_home = dir.path().join("deleted-legion-home");
    let gone_str = gone_home.to_string_lossy().into_owned();
    let out_as = std::process::Command::new(legion_bin)
        .args(["gate", "--as", "phone", "--home", &gone_str])
        .env("SSH_ORIGINAL_COMMAND", "legion rpc")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(!out_as.status.success());
    assert!(
        String::from_utf8_lossy(&out_as.stderr).contains("missing its configuration"),
        "stderr={}",
        String::from_utf8_lossy(&out_as.stderr)
    );
    assert!(
        !gone_home.exists(),
        "gate --as recreated missing pinned home at {}",
        gone_home.display()
    );

    let out_enrol = std::process::Command::new(legion_bin)
        .args([
            "gate",
            "--enrol",
            "0123456789abcdef0123456789abcdef",
            "--home",
            &gone_str,
        ])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(!out_enrol.status.success());
    let enrol_reply: Value =
        serde_json::from_str(String::from_utf8_lossy(&out_enrol.stdout).trim()).unwrap();
    assert_eq!(enrol_reply["ok"], json!(false));
    assert!(!gone_home.exists(), "gate --enrol recreated missing home");

    // 2. Existing config dir that is missing `node.toml`: refused without creating `state/`.
    let partial_home = dir.path().join("partial-home");
    std::fs::create_dir_all(partial_home.join("config")).unwrap();
    let partial_str = partial_home.to_string_lossy().into_owned();
    let out_partial = std::process::Command::new(legion_bin)
        .args(["gate", "--as", "phone", "--home", &partial_str])
        .env("SSH_ORIGINAL_COMMAND", "legion rpc")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(!out_partial.status.success());
    assert!(
        !partial_home.join("state").exists(),
        "gate created state/ despite missing node.toml"
    );

    // 3. Deleted split `--config-dir` / `--state-dir`: refused without creating either dir.
    let gone_cfg = dir.path().join("gone-cfg");
    let gone_state = dir.path().join("gone-state");
    let out_split = std::process::Command::new(legion_bin)
        .args([
            "gate",
            "--as",
            "phone",
            "--config-dir",
            &gone_cfg.to_string_lossy(),
            "--state-dir",
            &gone_state.to_string_lossy(),
        ])
        .env("SSH_ORIGINAL_COMMAND", "legion rpc")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(!out_split.status.success());
    assert!(!gone_cfg.exists() && !gone_state.exists());
}

#[cfg(unix)]
#[test]
fn real_sshd_control_master_revocation_ends_connection_and_refuses_new_channels() {
    if !TestSshd::available() {
        eprintln!(
            "skipping real_sshd_control_master_revocation_ends_connection_and_refuses_new_channels: sshd unavailable"
        );
        return;
    }

    struct ControlMasterGuard {
        ssh_bin: std::path::PathBuf,
        ctl_path: std::path::PathBuf,
        target: String,
    }

    impl Drop for ControlMasterGuard {
        fn drop(&mut self) {
            let _ = std::process::Command::new(&self.ssh_bin)
                .args(["-S"])
                .arg(&self.ctl_path)
                .args(["-O", "exit", &self.target])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
            let _ = std::fs::remove_file(&self.ctl_path);
        }
    }

    let home = Home::new_strict();
    let auth_keys = home.home().join("mux_authorized_keys");
    let home_dir_str = home.home().to_string_lossy().into_owned();
    let Some(sshd) = TestSshd::start(auth_keys.clone(), &[("HOME", &home_dir_str)])
        .expect("throwaway sshd failed to start")
    else {
        return;
    };

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
        "default = \"owner\"\n\n\
         [roles.observer]\n\
         allow = [\"effect:read\"]\n\n\
         [principals.phone]\n\
         role = \"observer\"\n",
    );

    let client_dir = tempfile::tempdir().unwrap();
    let known_hosts = client_dir.path().join("known_hosts");
    write_known_hosts(
        &known_hosts,
        sshd.host(),
        sshd.port(),
        &[sshd.host_key_line().to_owned()],
    )
    .unwrap();

    // Helper to enrol a fresh device key under `label` via real sshd.
    let key_seq = std::sync::atomic::AtomicUsize::new(0);
    let enrol_device = |label: &str| -> std::path::PathBuf {
        let seq = key_seq.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let invite_run = home.run(&[
            "device.invite",
            "principal=phone",
            "host=127.0.0.1",
            &format!("port={}", sshd.port()),
            &format!("user={}", sshd.user()),
        ]);
        assert_eq!(
            invite_run.code, 0,
            "invite {label}: {} {}",
            invite_run.stdout, invite_run.stderr
        );
        let payload = InvitePayload::decode(invite_run.json()["invite"].as_str().unwrap()).unwrap();
        let temp_key = client_dir.path().join(format!("{label}_{seq}_temp_key"));
        write_openssh_ed25519_private_key(&temp_key, &payload.decode_seed().unwrap(), label)
            .unwrap();
        let device_key = client_dir.path().join(format!("{label}_{seq}_ed25519"));
        let device_pub =
            generate_ed25519_keypair(sshd.ssh_keygen_bin(), &device_key, label).unwrap();

        let mut child = sshd
            .ssh_command(&temp_key, &known_hosts)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        writeln!(
            child.stdin.take().unwrap(),
            "{}",
            json!({
                "enrol": 1,
                "id": payload.id,
                "public_key": device_pub,
                "name": label,
            })
        )
        .unwrap();
        let out = child.wait_with_output().unwrap();
        assert!(
            out.status.success(),
            "enrol {label} failed: stdout={} stderr={}\nsshd:\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr),
            sshd.logs(),
        );
        device_key
    };

    // Short directory under `/tmp` so Unix domain socket paths stay well below SUN_LEN (~104 bytes).
    let ctl_dir = tempfile::Builder::new()
        .prefix("lgm-")
        .tempdir_in(std::env::temp_dir())
        .unwrap();
    let target = format!("{}@{}", sshd.user(), sshd.host());

    let start_master = |key: &Path, name: &str| -> ControlMasterGuard {
        let ctl_path = ctl_dir.path().join(format!("{name}.sock"));
        let out = std::process::Command::new(sshd.ssh_bin())
            .args([
                "-T",
                "-f",
                "-N",
                "-o",
                "BatchMode=yes",
                "-o",
                "IdentitiesOnly=yes",
                "-o",
                "StrictHostKeyChecking=yes",
                "-o",
                "GlobalKnownHostsFile=none",
                "-o",
                "ConnectTimeout=10",
                "-o",
                "PubkeyAuthentication=yes",
                "-o",
                "PasswordAuthentication=no",
                "-o",
                "KbdInteractiveAuthentication=no",
                "-o",
                "ControlMaster=yes",
                "-o",
                "ControlPersist=yes",
                "-o",
                "LogLevel=ERROR",
                "-o",
                &format!("UserKnownHostsFile={}", known_hosts.display()),
                "-S",
            ])
            .arg(&ctl_path)
            .args(["-p", &sshd.port().to_string(), "-i"])
            .arg(key)
            .arg(&target)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "ControlMaster {name} failed to start: stdout={} stderr={}\nsshd:\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr),
            sshd.logs(),
        );
        ControlMasterGuard {
            ssh_bin: sshd.ssh_bin().to_path_buf(),
            ctl_path,
            target: target.clone(),
        }
    };

    let run_whoami_via_ctl = |ctl_path: &Path| -> std::process::Output {
        let mut child = std::process::Command::new(sshd.ssh_bin())
            .args([
                "-T",
                "-o",
                "BatchMode=yes",
                "-o",
                "ControlMaster=no",
                "-o",
                "LogLevel=ERROR",
                "-S",
            ])
            .arg(ctl_path)
            .args([&target, "legion rpc"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let req = rpc_request(json!(1), "call", json!({"tool": "meta.whoami", "args": {}}));
        if let Some(mut stdin) = child.stdin.take() {
            let _ = writeln!(stdin, "{req}");
            let _ = stdin.flush();
        }
        child.wait_with_output().unwrap()
    };

    let ctl_is_alive = |ctl_path: &Path| -> bool {
        std::process::Command::new(sshd.ssh_bin())
            .args(["-S"])
            .arg(ctl_path)
            .args(["-O", "check", &target])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    };

    // Enrol an unrelated bystander device whose ControlMaster must stay alive across removals.
    let bystander_key = enrol_device("bystander-phone");
    let bystander_ctl = start_master(&bystander_key, "bystander");
    let bystander_before = run_whoami_via_ctl(&bystander_ctl.ctl_path);
    assert!(
        bystander_before.status.success(),
        "bystander before failed: {}",
        String::from_utf8_lossy(&bystander_before.stderr)
    );

    // 1. Active session + ControlMaster: removing the device kills both the active gate AND the
    //    parent sshd connection so neither the held channel nor any new channel on `-S <ctl>` works.
    let active_key = enrol_device("mux-active");
    let active_ctl = start_master(&active_key, "active");
    let mut held = std::process::Command::new(sshd.ssh_bin())
        .args([
            "-T",
            "-o",
            "BatchMode=yes",
            "-o",
            "ControlMaster=no",
            "-o",
            "LogLevel=ERROR",
            "-S",
        ])
        .arg(&active_ctl.ctl_path)
        .args([&target, "legion rpc"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut held_stdin = held.stdin.take().unwrap();
    let mut held_stdout = BufReader::new(held.stdout.take().unwrap());
    writeln!(
        held_stdin,
        "{}",
        rpc_request(json!(1), "call", json!({"tool": "meta.whoami", "args": {}}))
    )
    .unwrap();
    held_stdin.flush().unwrap();
    let reply = read_json_line(&mut held_stdout, "active mux whoami");
    assert_eq!(reply["result"]["principal"], json!("phone"));

    let rm_active = home.run(&["device.remove", "name=mux-active"]);
    assert_eq!(rm_active.code, 0, "rm_active: {}", rm_active.stderr);
    assert!(
        rm_active.json()["sessions_ended"].as_u64().unwrap() >= 1,
        "expected sessions_ended >= 1, got {}",
        rm_active.stdout
    );

    // Held session must terminate promptly, and the ControlMaster connection itself must die.
    drop(held_stdin);
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let held_exited = loop {
        if held.try_wait().unwrap().is_some() {
            break true;
        }
        if std::time::Instant::now() >= deadline {
            let _ = held.kill();
            let _ = held.wait();
            break false;
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    assert!(
        held_exited,
        "held multiplexed ssh session did not exit after device.remove"
    );

    // Wait briefly for the ssh ControlMaster client to observe the closed sshd connection.
    let ctl_deadline = std::time::Instant::now() + Duration::from_secs(5);
    while std::time::Instant::now() < ctl_deadline && ctl_is_alive(&active_ctl.ctl_path) {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(
        !ctl_is_alive(&active_ctl.ctl_path),
        "ControlMaster connection stayed alive after device.remove"
    );

    let after_active = run_whoami_via_ctl(&active_ctl.ctl_path);
    assert!(
        !after_active.status.success(),
        "ssh -S <ctl> still answered after device.remove: stdout={} stderr={}",
        String::from_utf8_lossy(&after_active.stdout),
        String::from_utf8_lossy(&after_active.stderr)
    );

    // 2. Idle previously-used ControlMaster: gate exited cleanly earlier, leaving only the
    //    multiplexed sshd connection open. `device.remove` must still terminate that parent sshd.
    let idle_key = enrol_device("mux-idle");
    let idle_ctl = start_master(&idle_key, "idle");
    let idle_before = run_whoami_via_ctl(&idle_ctl.ctl_path);
    assert!(
        idle_before.status.success(),
        "idle before failed: {}",
        String::from_utf8_lossy(&idle_before.stderr)
    );
    assert!(ctl_is_alive(&idle_ctl.ctl_path));

    let rm_idle = home.run(&["device.remove", "name=mux-idle"]);
    assert_eq!(rm_idle.code, 0, "rm_idle: {}", rm_idle.stderr);
    assert!(
        rm_idle.json()["sessions_ended"].as_u64().unwrap() >= 1,
        "expected idle ControlMaster connection to be ended, got {}",
        rm_idle.stdout
    );

    let idle_deadline = std::time::Instant::now() + Duration::from_secs(5);
    while std::time::Instant::now() < idle_deadline && ctl_is_alive(&idle_ctl.ctl_path) {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(
        !ctl_is_alive(&idle_ctl.ctl_path),
        "idle ControlMaster connection stayed alive after device.remove"
    );
    let after_idle = run_whoami_via_ctl(&idle_ctl.ctl_path);
    assert!(
        !after_idle.status.success(),
        "idle ControlMaster still answered after device.remove: stdout={}",
        String::from_utf8_lossy(&after_idle.stdout)
    );

    // 3. Never-used `-fN` ControlMaster (opened before removal, no gate ran yet): `device.remove`
    //    cannot know its sshd PID yet, so when the device later opens a channel over `-S <ctl>`,
    //    `legion gate` re-checks authorized_keys after registering and refuses to serve.
    let fresh_key = enrol_device("mux-fresh");
    let fresh_ctl = start_master(&fresh_key, "fresh");
    assert!(ctl_is_alive(&fresh_ctl.ctl_path));

    let rm_fresh = home.run(&["device.remove", "name=mux-fresh"]);
    assert_eq!(rm_fresh.code, 0, "rm_fresh: {}", rm_fresh.stderr);

    let after_fresh = run_whoami_via_ctl(&fresh_ctl.ctl_path);
    assert!(
        !after_fresh.status.success(),
        "fresh -fN ControlMaster answered as phone after device.remove: stdout={} stderr={}",
        String::from_utf8_lossy(&after_fresh.stdout),
        String::from_utf8_lossy(&after_fresh.stderr)
    );
    let fresh_stderr = String::from_utf8_lossy(&after_fresh.stderr);
    assert!(
        fresh_stderr.contains("no longer authorized"),
        "expected gate re-check refusal on stderr, got: {fresh_stderr:?}"
    );
    assert!(
        after_fresh.stdout.is_empty(),
        "refused gate must not write RPC output: {:?}",
        String::from_utf8_lossy(&after_fresh.stdout)
    );
    // The refusing gate must also end its verified parent sshd connection so the ControlMaster
    // cannot linger afterwards.
    let fresh_dead_deadline = std::time::Instant::now() + Duration::from_secs(5);
    while std::time::Instant::now() < fresh_dead_deadline && ctl_is_alive(&fresh_ctl.ctl_path) {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(
        !ctl_is_alive(&fresh_ctl.ctl_path),
        "never-used ControlMaster stayed alive after refusing gate exited"
    );

    // 4. Re-pairing under the exact same device name and principal MUST NOT revive an earlier
    //    removed pairing's open ControlMaster connection (each pairing carries its own random
    //    `--pairing <id>`), and the refusing gate must terminate the stale parent sshd too.
    let repair_old_key = enrol_device("mux-repair");
    let repair_old_ctl = start_master(&repair_old_key, "repair-old");
    assert!(ctl_is_alive(&repair_old_ctl.ctl_path));

    let rm_repair = home.run(&["device.remove", "name=mux-repair"]);
    assert_eq!(rm_repair.code, 0, "rm_repair: {}", rm_repair.stderr);

    // Pair a brand-new phone under the EXACT SAME name (`mux-repair`) and principal (`phone`).
    let repair_new_key = enrol_device("mux-repair");

    let old_after_repair = run_whoami_via_ctl(&repair_old_ctl.ctl_path);
    assert!(
        !old_after_repair.status.success(),
        "old ControlMaster was revived after re-pairing under the same name! stdout={} stderr={}",
        String::from_utf8_lossy(&old_after_repair.stdout),
        String::from_utf8_lossy(&old_after_repair.stderr)
    );
    let repair_stderr = String::from_utf8_lossy(&old_after_repair.stderr);
    assert!(
        repair_stderr.contains("no longer authorized"),
        "expected pairing-mismatch refusal on stderr, got: {repair_stderr:?}"
    );
    assert!(
        old_after_repair.stdout.is_empty(),
        "refused old pairing must not emit RPC output: {:?}",
        String::from_utf8_lossy(&old_after_repair.stdout)
    );
    let repair_dead_deadline = std::time::Instant::now() + Duration::from_secs(5);
    while std::time::Instant::now() < repair_dead_deadline && ctl_is_alive(&repair_old_ctl.ctl_path)
    {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(
        !ctl_is_alive(&repair_old_ctl.ctl_path),
        "old ControlMaster stayed alive after re-pair refusal"
    );

    // The newly paired device under the same name still connects and works normally.
    let new_ctl = start_master(&repair_new_key, "repair-new");
    let new_ok = run_whoami_via_ctl(&new_ctl.ctl_path);
    assert!(
        new_ok.status.success(),
        "newly paired mux-repair failed: {}",
        String::from_utf8_lossy(&new_ok.stderr)
    );
    let new_reply: Value =
        serde_json::from_str(String::from_utf8_lossy(&new_ok.stdout).trim()).unwrap();
    assert_eq!(new_reply["result"]["principal"], json!("phone"));

    // 5. A transient non-Denied error during the gate re-check (e.g. a temporarily unreadable or
    //    corrupt node.toml returning ErrorCode::Failed) must refuse only that session without
    //    terminating the parent sshd ControlMaster connection, so restoring node.toml lets the
    //    next channel on the same ControlMaster succeed.
    let node_toml_path = home
        .state()
        .parent()
        .unwrap()
        .join("config")
        .join("node.toml");
    let original_node_toml = std::fs::read_to_string(&node_toml_path).unwrap();
    std::fs::write(&node_toml_path, "name = [invalid toml\n").unwrap();
    let during_transient = run_whoami_via_ctl(&new_ctl.ctl_path);
    std::fs::write(&node_toml_path, &original_node_toml).unwrap();
    assert!(
        !during_transient.status.success(),
        "gate unexpectedly served while node.toml was corrupt: stdout={}",
        String::from_utf8_lossy(&during_transient.stdout)
    );
    assert!(
        during_transient.stdout.is_empty(),
        "refused gate must not write RPC output on transient error: {:?}",
        String::from_utf8_lossy(&during_transient.stdout)
    );
    let transient_stderr = String::from_utf8_lossy(&during_transient.stderr);
    assert!(
        transient_stderr.contains("legion gate:"),
        "expected gate error on stderr during transient failure, got: {transient_stderr:?}"
    );
    // Give any wrongful parent-termination signal a moment to take effect, then verify the
    // ControlMaster survived and answers normally once node.toml is restored.
    std::thread::sleep(Duration::from_millis(250));
    assert!(
        ctl_is_alive(&new_ctl.ctl_path),
        "transient non-Denied error killed the parent sshd ControlMaster connection"
    );
    let after_transient = run_whoami_via_ctl(&new_ctl.ctl_path);
    assert!(
        after_transient.status.success(),
        "ControlMaster failed after restoring node.toml: {}",
        String::from_utf8_lossy(&after_transient.stderr)
    );
    let after_transient_reply: Value =
        serde_json::from_str(String::from_utf8_lossy(&after_transient.stdout).trim()).unwrap();
    assert_eq!(after_transient_reply["result"]["principal"], json!("phone"));

    // Unrelated bystander ControlMaster and the sshd listener must both remain intact.
    assert!(
        ctl_is_alive(&bystander_ctl.ctl_path),
        "bystander ControlMaster was disrupted by removing another device"
    );
    let bystander_after = run_whoami_via_ctl(&bystander_ctl.ctl_path);
    assert!(
        bystander_after.status.success(),
        "bystander RPC failed after removing another device: {}",
        String::from_utf8_lossy(&bystander_after.stderr)
    );
    let bystander_reply: Value =
        serde_json::from_str(String::from_utf8_lossy(&bystander_after.stdout).trim()).unwrap();
    assert_eq!(bystander_reply["result"]["principal"], json!("phone"));
}

#[test]
fn device_remove_leaves_running_jobs_alive_and_reports_them() {
    let home = Home::new();
    let auth_keys = home.home().join("jobs_authorized_keys");
    let home_root = home.state().parent().unwrap().to_path_buf();
    let pinned_home = legion_tools::device::authkeys::PinnedHome::Root(home_root.clone());
    let exe = Path::new(env!("CARGO_BIN_EXE_legion"));
    let shell = legion_tools::device::authkeys::ForcedCommandShell::host().unwrap();

    let key_a = ed25519_public_key_from_seed(&[31_u8; 32]);
    let key_b = ed25519_public_key_from_seed(&[32_u8; 32]);
    let pairing_a = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    let pairing_b = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    let line_a = legion_tools::device::authkeys::format_device_line(
        exe,
        "phone",
        &key_a,
        "job-phone",
        pairing_a,
        &pinned_home,
        None,
        shell,
    )
    .unwrap();
    let line_b = legion_tools::device::authkeys::format_device_line(
        exe,
        "phone",
        &key_b,
        "other-phone",
        pairing_b,
        &pinned_home,
        None,
        shell,
    )
    .unwrap();
    std::fs::write(&auth_keys, format!("{line_a}\n{line_b}\n")).unwrap();

    home.write_config(
        "node.toml",
        &format!(
            "name = \"tower\"\n\n[devices]\nauthorized_keys = {}\n",
            toml_literal(&auth_keys),
        ),
    );
    home.write_config(
        "policy.toml",
        "default = \"owner\"\n\n\
         [roles.operator]\n\
         allow = [\"effect:read\", \"effect:exec\"]\n\n\
         [principals.phone]\n\
         role = \"operator\"\n",
    );

    let sleep_cmd = if cfg!(windows) {
        "Start-Sleep -Seconds 300"
    } else {
        "sleep 300"
    };

    let start_job_via_gate = |device: &str, pairing_id: &str| -> String {
        let mut child = home
            .command(&[
                "gate",
                "--as",
                "phone",
                "--device",
                device,
                "--pairing",
                pairing_id,
                "--home",
                &home_root.to_string_lossy(),
            ])
            .env("SSH_ORIGINAL_COMMAND", "legion rpc")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        {
            let mut stdin = child.stdin.take().unwrap();
            writeln!(
                stdin,
                "{}",
                rpc_request(
                    json!(1),
                    "call",
                    json!({"tool": "job.start", "args": {"cmd": sleep_cmd}})
                )
            )
            .unwrap();
        }
        let out = child.wait_with_output().unwrap();
        assert!(
            out.status.success(),
            "gate job.start for {device} failed: stdout={} stderr={}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        let reply: Value =
            serde_json::from_str(String::from_utf8_lossy(&out.stdout).trim()).unwrap();
        reply["result"]["id"]
            .as_str()
            .unwrap_or_else(|| panic!("missing job id in {reply}"))
            .to_owned()
    };

    let job_a = start_job_via_gate("job-phone", pairing_a);
    let job_b = start_job_via_gate("other-phone", pairing_b);
    let owner_job_run = home.run(&["job.start", &format!("cmd={sleep_cmd}")]);
    assert_eq!(
        owner_job_run.code, 0,
        "owner job.start failed: {}",
        owner_job_run.stderr
    );
    let job_owner = owner_job_run.json()["id"].as_str().unwrap().to_owned();

    // Ensure all three background jobs are cancelled even if an assertion fails below.
    struct CancelJobs<'a> {
        home: &'a Home,
        ids: Vec<String>,
    }
    impl Drop for CancelJobs<'_> {
        fn drop(&mut self) {
            for id in &self.ids {
                let _ = self
                    .home
                    .run(&["job.cancel", &format!("id={id}"), "force=true"]);
            }
        }
    }
    let _cleanup = CancelJobs {
        home: &home,
        ids: vec![job_a.clone(), job_b.clone(), job_owner.clone()],
    };

    // Remove `job-phone`: must report ONLY `job_a` in `jobs_left_running` and leave it running.
    let removed = home.run(&["device.remove", "name=job-phone"]);
    assert_eq!(removed.code, 0, "device.remove failed: {}", removed.stderr);
    let removed_json = removed.json();
    assert_eq!(removed_json["removed"], json!(true));
    assert_eq!(
        removed_json["jobs_left_running"],
        json!([job_a]),
        "expected only job-phone's running job in jobs_left_running: {removed_json}"
    );

    // Confirm `job_a` is still running (device.remove must NOT kill it).
    let status_a = home.run(&["job.status", &format!("id={job_a}")]);
    assert_eq!(status_a.code, 0, "job.status failed: {}", status_a.stderr);
    assert_eq!(status_a.json()["state"], json!("running"));
}

/// Exercises all fleet-wide paired-device flows across independent nodes backed by real loopback
/// `sshd` daemons (separate Legion homes, separate `authorized_keys`, connected only through
/// `fleet.toml` over real SSH):
///
/// 1. Role-based pairing on A -> automatic push to B -> `device.endpoint` with `on: "all"` ->
///    direct SSH connection from the device to B.
/// 2. B unreachable during pairing -> `fleet.b.status == "not_reached"`, `meta.doctor` warns on
///    drift once B is reachable -> `fleet.sync` converges -> direct SSH to B succeeds.
/// 3. `device.remove` on B while A is unreachable -> `still_trusted_by == ["a"]`, honest limit
///    that A's open `ControlMaster` still answers until sync -> `fleet.sync` immediately tears
///    down A's `ControlMaster`, removes A's `authorized_keys` line & `device:*` principal, and
///    refuses both multiplexed and fresh connections.
/// 4. An `owner`-role paired device calls `device.invite` over SSH on A -> enrolled device works
///    directly on both A and B.
/// 5. `device.set` changes role (`observer` -> `editor`) and renames on A -> propagates to B and
///    takes effect immediately.
/// 6. Legacy first-run adoption on a third node C (pre-existing `--pairing` line, no `devices.toml`)
///    merges bidirectionally on `fleet.sync` without dropping either side's devices.
#[cfg(unix)]
#[test]
fn real_sshd_two_node_fleet_devices_sync_roles_revocation_and_legacy_adoption() {
    if !TestSshd::available() {
        eprintln!(
            "skipping real_sshd_two_node_fleet_devices_sync_roles_revocation_and_legacy_adoption: sshd unavailable"
        );
        return;
    }

    let legion_bin = Path::new(env!("CARGO_BIN_EXE_legion"));

    let node_a = Home::new_strict();
    let node_b = Home::new_strict();
    let auth_a = node_a.home().join("authorized_keys_a");
    let auth_b = node_b.home().join("authorized_keys_b");

    let home_a_str = node_a.home().to_string_lossy().into_owned();
    let home_b_str = node_b.home().to_string_lossy().into_owned();
    let Some(sshd_a) =
        TestSshd::start(auth_a.clone(), &[("HOME", &home_a_str)]).expect("sshd_a failed to start")
    else {
        return;
    };
    let Some(sshd_b) =
        TestSshd::start(auth_b.clone(), &[("HOME", &home_b_str)]).expect("sshd_b failed to start")
    else {
        return;
    };

    // Shared client/fleet scratch directory (keys and known_hosts only; nodes never share config/state).
    let client_dir = tempfile::tempdir().unwrap();
    let fleet_key = client_dir.path().join("fleet_ed25519");
    let fleet_pub =
        generate_ed25519_keypair(sshd_a.ssh_keygen_bin(), &fleet_key, "fleet@test").unwrap();

    let fleet_kh_a = client_dir.path().join("fleet_known_hosts_a");
    write_known_hosts(
        &fleet_kh_a,
        "127.0.0.1",
        sshd_a.port(),
        &[sshd_a.host_key_line().to_owned()],
    )
    .unwrap();
    let fleet_kh_b = client_dir.path().join("fleet_known_hosts_b");
    write_known_hosts(
        &fleet_kh_b,
        "127.0.0.1",
        sshd_b.port(),
        &[sshd_b.host_key_line().to_owned()],
    )
    .unwrap();

    // Put a foreign fleet-link key on each node's authorized_keys so `a <-> b` speaks real SSH.
    let home_a_root = node_a.state().parent().unwrap().to_path_buf();
    let home_b_root = node_b.state().parent().unwrap().to_path_buf();
    let fleet_line = |legion_home: &Path| -> String {
        let q_home = legion_home.display().to_string().replace('\'', "'\\''");
        let q_bin = legion_bin.display().to_string().replace('\'', "'\\''");
        let cmd = format!("env LEGION_HOME='{q_home}' '{q_bin}' rpc")
            .replace('\\', "\\\\")
            .replace('"', "\\\"");
        format!("restrict,command=\"{cmd}\" {fleet_pub} fleet-link\n")
    };
    std::fs::write(&auth_a, fleet_line(&home_a_root)).unwrap();
    std::fs::write(&auth_b, fleet_line(&home_b_root)).unwrap();

    let ssh_cmd_toml = |port: u16, known_hosts: &Path, user: &str| -> String {
        let argv = vec![
            sshd_a.ssh_bin().to_string_lossy().into_owned(),
            "-T".to_owned(),
            "-o".to_owned(),
            "BatchMode=yes".to_owned(),
            "-o".to_owned(),
            "IdentitiesOnly=yes".to_owned(),
            "-o".to_owned(),
            "StrictHostKeyChecking=yes".to_owned(),
            "-o".to_owned(),
            format!("UserKnownHostsFile={}", known_hosts.display()),
            "-o".to_owned(),
            "GlobalKnownHostsFile=none".to_owned(),
            "-o".to_owned(),
            "ControlMaster=no".to_owned(),
            "-o".to_owned(),
            "ConnectTimeout=3".to_owned(),
            "-o".to_owned(),
            "LogLevel=ERROR".to_owned(),
            "-p".to_owned(),
            port.to_string(),
            "-i".to_owned(),
            fleet_key.display().to_string(),
            format!("{user}@127.0.0.1"),
            "legion rpc".to_owned(),
        ];
        let elems: Vec<String> = argv
            .iter()
            .map(|a| format!("'{}'", a.replace('\'', "''")))
            .collect();
        format!("[{}]", elems.join(", "))
    };

    // Allocate an unused loopback port for simulating an unreachable peer quickly (connection refused).
    let closed_port = {
        let sock = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        sock.local_addr().unwrap().port()
    };

    let write_node_cfg = |home: &Home, name: &str, auth: &Path, sshd: &TestSshd| {
        home.write_config(
            "node.toml",
            &format!(
                "name = \"{name}\"\n\n[devices]\nauthorized_keys = {}\nhost_keys = [{}]\nhosts = [\"127.0.0.1\"]\nport = {}\nuser = \"{}\"\n",
                toml_literal(auth),
                toml_literal(sshd.host_key_path()),
                sshd.port(),
                sshd.user(),
            ),
        );
    };
    write_node_cfg(&node_a, "a", &auth_a, &sshd_a);
    write_node_cfg(&node_b, "b", &auth_b, &sshd_b);

    let policy_for = |projects_dir: &Path| -> String {
        format!(
            "default = \"owner\"\n\n\
             [roles.observer]\n\
             allow = [\"effect:read\"]\n\
             deny = [\"audit.*\"]\n\
             paths = [{}]\n\n\
             [roles.editor]\n\
             allow = [\"effect:read\", \"effect:write\"]\n\
             deny = [\"audit.*\"]\n\
             paths = [{}]\n\n\
             [principals.legacy_phone]\n\
             role = \"observer\"\n",
            toml_literal(projects_dir),
            toml_literal(projects_dir),
        )
    };
    let projects_a = node_a.home().join("projects");
    let projects_b = node_b.home().join("projects");
    std::fs::write(projects_a.join("app").join("note.txt"), "from-a\n").unwrap();
    std::fs::write(projects_b.join("app").join("note.txt"), "from-b\n").unwrap();
    node_a.write_config("policy.toml", &policy_for(&projects_a));
    node_b.write_config("policy.toml", &policy_for(&projects_b));

    let write_fleet_a = |b_port: u16| {
        node_a.write_config(
            "fleet.toml",
            &format!(
                "[machines.b]\ncommand = {}\n",
                ssh_cmd_toml(b_port, &fleet_kh_b, sshd_b.user()),
            ),
        );
    };
    let write_fleet_b = |a_port: u16| {
        node_b.write_config(
            "fleet.toml",
            &format!(
                "[machines.a]\ncommand = {}\n",
                ssh_cmd_toml(a_port, &fleet_kh_a, sshd_a.user()),
            ),
        );
    };
    write_fleet_a(sshd_b.port());
    write_fleet_b(sshd_a.port());

    let enrol_from_invite = |invite_text: &str,
                             sshd: &TestSshd,
                             key_stem: &str,
                             req_name: &str|
     -> (std::path::PathBuf, Value) {
        let payload = InvitePayload::decode(invite_text).unwrap();
        let temp_key = client_dir.path().join(format!("{key_stem}_temp"));
        write_openssh_ed25519_private_key(&temp_key, &payload.decode_seed().unwrap(), key_stem)
            .unwrap();
        let kh = client_dir.path().join(format!("{key_stem}_kh"));
        write_known_hosts(&kh, &payload.hosts[0], payload.port, &payload.host_keys).unwrap();
        let dev_key = client_dir.path().join(format!("{key_stem}_key"));
        let dev_pub = generate_ed25519_keypair(sshd.ssh_keygen_bin(), &dev_key, key_stem).unwrap();

        let mut child = sshd
            .ssh_command(&temp_key, &kh)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        {
            let mut stdin = child.stdin.take().unwrap();
            writeln!(
                stdin,
                "{}",
                json!({
                    "enrol": 1,
                    "id": payload.id,
                    "public_key": dev_pub,
                    "name": req_name,
                })
            )
            .unwrap();
        }
        let out = child.wait_with_output().unwrap();
        assert!(
            out.status.success(),
            "enrol {key_stem} failed: stdout={} stderr={}\nsshd:\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr),
            sshd.logs(),
        );
        let reply: Value =
            serde_json::from_str(String::from_utf8_lossy(&out.stdout).trim()).unwrap();
        (dev_key, reply)
    };

    let rpc_call_direct = |key: &Path,
                           kh: &Path,
                           port: u16,
                           user: &str,
                           tool: &str,
                           args: Value,
                           on: Option<&str>|
     -> Value {
        let mut params = json!({"tool": tool, "args": args});
        if let Some(target) = on {
            params["on"] = json!(target);
        }
        let mut child = std::process::Command::new(sshd_a.ssh_bin())
            .args([
                "-T",
                "-o",
                "BatchMode=yes",
                "-o",
                "IdentitiesOnly=yes",
                "-o",
                "StrictHostKeyChecking=yes",
                "-o",
                &format!("UserKnownHostsFile={}", kh.display()),
                "-o",
                "GlobalKnownHostsFile=none",
                "-o",
                "ControlMaster=no",
                "-o",
                "ConnectTimeout=10",
                "-o",
                "LogLevel=ERROR",
                "-p",
                &port.to_string(),
                "-i",
            ])
            .arg(key)
            .args([&format!("{user}@127.0.0.1"), "legion rpc"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        {
            let mut stdin = child.stdin.take().unwrap();
            writeln!(stdin, "{}", rpc_request(json!(1), "call", params)).unwrap();
        }
        let out = child.wait_with_output().unwrap();
        assert!(
            out.status.success(),
            "direct rpc {tool} on port {port} failed: stdout={} stderr={}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr),
        );
        serde_json::from_str(String::from_utf8_lossy(&out.stdout).trim()).unwrap()
    };

    // === Scenario 1: Pair on A with --name/--role -> auto-push -> device.endpoint --on all -> direct SSH to B ===
    let inv_a = node_a.run(&["pair", "--name", "phone-a", "--role", "observer", "--json"]);
    assert_eq!(inv_a.code, 0, "pair phone-a failed: {}", inv_a.stderr);
    let inv_a_json = inv_a.json();
    assert_eq!(inv_a_json["principal"], json!("device:phone-a"));
    assert_eq!(inv_a_json["role"], json!("observer"));

    let (phone_a_key, phone_a_enrol) = enrol_from_invite(
        inv_a_json["invite"].as_str().unwrap(),
        &sshd_a,
        "phone_a",
        "ignored",
    );
    assert_eq!(phone_a_enrol["ok"], json!(true));
    assert_eq!(phone_a_enrol["principal"], json!("device:phone-a"));
    assert_eq!(phone_a_enrol["role"], json!("observer"));
    assert_eq!(phone_a_enrol["device"], json!("phone-a"));
    assert_eq!(
        phone_a_enrol["fleet"]["b"]["status"],
        json!("synced"),
        "expected phone-a enrolment to push to b: {phone_a_enrol}"
    );

    // Call `device.endpoint` with `on: "all"` as `phone-a` through A to learn B's endpoint and host key.
    let ep_all = rpc_call_direct(
        &phone_a_key,
        &fleet_kh_a,
        sshd_a.port(),
        sshd_a.user(),
        "device.endpoint",
        json!({}),
        Some("all"),
    );
    let ep_results = &ep_all["result"]["results"];
    assert_eq!(ep_results["a"]["ok"]["node"], json!("a"), "{ep_all}");
    assert_eq!(ep_results["b"]["ok"]["node"], json!("b"), "{ep_all}");
    let ep_b = &ep_results["b"]["ok"];
    let ep_b_host = ep_b["hosts"][0].as_str().unwrap();
    let ep_b_port = ep_b["port"].as_u64().unwrap() as u16;
    let ep_b_user = ep_b["user"].as_str().unwrap();
    let ep_b_hks: Vec<String> = ep_b["host_keys"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_owned())
        .collect();
    assert_eq!(ep_b_port, sshd_b.port());
    assert_eq!(ep_b_hks, vec![sshd_b.host_key_line().to_owned()]);

    // Pin B's host key from what A reported and connect directly from phone-a to B!
    let learned_kh_b = client_dir.path().join("learned_kh_b");
    write_known_hosts(&learned_kh_b, ep_b_host, ep_b_port, &ep_b_hks).unwrap();
    let whoami_b = rpc_call_direct(
        &phone_a_key,
        &learned_kh_b,
        ep_b_port,
        ep_b_user,
        "meta.whoami",
        json!({}),
        None,
    );
    assert_eq!(whoami_b["result"]["node"], json!("b"), "{whoami_b}");
    assert_eq!(whoami_b["result"]["principal"], json!("device:phone-a"));
    assert_eq!(whoami_b["result"]["role"], json!("observer"));
    assert_eq!(whoami_b["result"]["door"], json!("ssh"));

    let read_b = rpc_call_direct(
        &phone_a_key,
        &learned_kh_b,
        ep_b_port,
        ep_b_user,
        "fs.read",
        json!({"path": projects_b.join("app").join("note.txt").to_string_lossy()}),
        None,
    );
    assert_eq!(read_b["result"]["content"], json!("from-b\n"), "{read_b}");
    let denied_write_b = rpc_call_direct(
        &phone_a_key,
        &learned_kh_b,
        ep_b_port,
        ep_b_user,
        "fs.write",
        json!({"path": projects_b.join("app").join("nope.txt").to_string_lossy(), "content": "x"}),
        None,
    );
    assert_eq!(denied_write_b["error"]["data"]["code"], json!("denied"));

    // === Scenario 2: B unreachable during pairing -> meta.doctor drift warning -> fleet.sync -> B accepts ===
    write_fleet_a(closed_port);
    let inv_off = node_a.run(&[
        "pair",
        "--name",
        "phone-offline",
        "--role",
        "observer",
        "--json",
    ]);
    assert_eq!(
        inv_off.code, 0,
        "pair phone-offline failed: {}",
        inv_off.stderr
    );
    let (phone_off_key, phone_off_enrol) = enrol_from_invite(
        inv_off.json()["invite"].as_str().unwrap(),
        &sshd_a,
        "phone_off",
        "ignored",
    );
    assert_eq!(phone_off_enrol["ok"], json!(true));
    assert_eq!(
        phone_off_enrol["fleet"]["b"]["status"],
        json!("not_reached"),
        "expected unreachable b during enrolment: {phone_off_enrol}"
    );
    let refused_b_before = sshd_b
        .ssh_command(&phone_off_key, &learned_kh_b)
        .arg("legion rpc")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(
        !refused_b_before.status.success(),
        "phone-offline unexpectedly authenticated to B before sync"
    );

    // Restore reachability and check `meta.doctor` warns about device drift before syncing.
    write_fleet_a(sshd_b.port());
    let doc_drifted = node_a.run(&["meta.doctor"]).json();
    let drift_check = doc_drifted["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == json!("fleet devices"));
    assert!(
        drift_check.is_some_and(|c| c["ok"] == json!(true)
            && c["warning"] == json!(true)
            && c["detail"].as_str().unwrap_or_default().contains('b')),
        "expected meta.doctor fleet devices drift warning: {doc_drifted}"
    );

    let synced_a_to_b = node_a.run(&[
        "fleet.sync",
        "direction=push",
        "documents=[\"devices.toml\"]",
    ]);
    assert_eq!(
        synced_a_to_b.code, 0,
        "fleet.sync push failed: {}",
        synced_a_to_b.stderr
    );
    assert_eq!(
        synced_a_to_b.json()["results"]["b"]["devices.toml"]["status"],
        json!("pushed")
    );

    let whoami_off_b = rpc_call_direct(
        &phone_off_key,
        &learned_kh_b,
        ep_b_port,
        ep_b_user,
        "meta.whoami",
        json!({}),
        None,
    );
    assert_eq!(whoami_off_b["result"]["node"], json!("b"));
    assert_eq!(
        whoami_off_b["result"]["principal"],
        json!("device:phone-offline")
    );
    assert_eq!(whoami_off_b["result"]["role"], json!("observer"));

    // Once in sync, `meta.doctor` no longer emits the drift warning.
    let doc_clean = node_a.run(&["meta.doctor"]).json();
    assert!(
        !doc_clean["checks"]
            .as_array()
            .unwrap()
            .iter()
            .any(|c| c["name"] == json!("fleet devices")),
        "expected fleet devices warning to clear after sync: {doc_clean}"
    );

    // === Scenario 3: Remove on B while A is unreachable -> still_trusted_by -> ControlMaster killed on sync ===
    let ctl_dir = tempfile::Builder::new()
        .prefix("lg-f-")
        .tempdir_in("/tmp")
        .unwrap();
    let ctl_path = ctl_dir.path().join("a.sock");
    let target_a = format!("{}@127.0.0.1", sshd_a.user());
    let ctl_start = std::process::Command::new(sshd_a.ssh_bin())
        .args([
            "-T",
            "-f",
            "-N",
            "-o",
            "BatchMode=yes",
            "-o",
            "IdentitiesOnly=yes",
            "-o",
            "StrictHostKeyChecking=yes",
            "-o",
            &format!("UserKnownHostsFile={}", fleet_kh_a.display()),
            "-o",
            "GlobalKnownHostsFile=none",
            "-o",
            "ConnectTimeout=10",
            "-o",
            "LogLevel=ERROR",
            "-o",
            "ControlMaster=yes",
            "-o",
            "ControlPersist=yes",
            "-S",
        ])
        .arg(&ctl_path)
        .args(["-p", &sshd_a.port().to_string(), "-i"])
        .arg(&phone_a_key)
        .arg(&target_a)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .unwrap();
    assert!(
        ctl_start.status.success(),
        "ControlMaster start failed: {}",
        String::from_utf8_lossy(&ctl_start.stderr)
    );

    struct CleanCtl {
        ssh: std::path::PathBuf,
        ctl: std::path::PathBuf,
        target: String,
    }
    impl Drop for CleanCtl {
        fn drop(&mut self) {
            let _ = std::process::Command::new(&self.ssh)
                .args(["-S"])
                .arg(&self.ctl)
                .args(["-O", "exit", &self.target])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
    }
    let _ctl_guard = CleanCtl {
        ssh: sshd_a.ssh_bin().to_path_buf(),
        ctl: ctl_path.clone(),
        target: target_a.clone(),
    };

    let ctl_is_alive = || -> bool {
        std::process::Command::new(sshd_a.ssh_bin())
            .args(["-S"])
            .arg(&ctl_path)
            .args(["-O", "check", &target_a])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    };
    let whoami_via_ctl = || -> std::process::Output {
        let mut child = std::process::Command::new(sshd_a.ssh_bin())
            .args([
                "-T",
                "-o",
                "BatchMode=yes",
                "-o",
                "ControlMaster=no",
                "-o",
                "LogLevel=ERROR",
                "-S",
            ])
            .arg(&ctl_path)
            .args([&target_a, "legion rpc"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        {
            let mut stdin = child.stdin.take().unwrap();
            writeln!(
                stdin,
                "{}",
                rpc_request(json!(1), "call", json!({"tool": "meta.whoami", "args": {}}))
            )
            .unwrap();
        }
        child.wait_with_output().unwrap()
    };

    // Prime the ControlMaster so A records its parent sshd session before B revokes phone-a.
    let before_revoke = whoami_via_ctl();
    assert!(before_revoke.status.success());
    assert!(ctl_is_alive());

    // Make A unreachable from B while keeping sshd_a and phone-a's ControlMaster live.
    write_fleet_b(closed_port);
    let rm_on_b = node_b.run(&["device.remove", "name=phone-a"]);
    assert_eq!(rm_on_b.code, 0, "remove phone-a on b: {}", rm_on_b.stderr);
    let rm_json = rm_on_b.json();
    assert_eq!(rm_json["removed"], json!(true));
    assert_eq!(rm_json["still_trusted_by"], json!(["a"]), "{rm_json}");
    assert_eq!(rm_json["fleet"]["a"]["status"], json!("not_reached"));
    assert!(
        rm_json["hint"]
            .as_str()
            .is_some_and(|h| h.contains('a') && h.contains("fleet.sync")),
        "expected still_trusted_by hint: {rm_json}"
    );

    // Honest limit: B refuses phone-a at once, while A still accepts it until synced.
    let refused_b_after_rm = sshd_b
        .ssh_command(&phone_a_key, &learned_kh_b)
        .arg("legion rpc")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(!refused_b_after_rm.status.success());
    let still_ok_on_a = whoami_via_ctl();
    assert!(
        still_ok_on_a.status.success(),
        "expected A to still trust phone-a before syncing"
    );

    // Restore reachability and sync from B -> A: A's ControlMaster must die immediately!
    write_fleet_b(sshd_a.port());
    let sync_revoke = node_b.run(&[
        "fleet.sync",
        "direction=push",
        "documents=[\"devices.toml\"]",
    ]);
    assert_eq!(
        sync_revoke.code, 0,
        "fleet.sync revocation failed: {}",
        sync_revoke.stderr
    );
    assert_eq!(
        sync_revoke.json()["results"]["a"]["devices.toml"]["status"],
        json!("pushed")
    );

    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while std::time::Instant::now() < deadline && ctl_is_alive() {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(
        !ctl_is_alive(),
        "phone-a ControlMaster on A stayed alive after fleet.sync delivered its tombstone"
    );
    assert!(!whoami_via_ctl().status.success());
    let fresh_a_after_sync = sshd_a
        .ssh_command(&phone_a_key, &fleet_kh_a)
        .arg("legion rpc")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(!fresh_a_after_sync.status.success());
    let policy_a_after =
        std::fs::read_to_string(home_a_root.join("config").join("policy.toml")).unwrap();
    assert!(
        !policy_a_after.contains("device:phone-a"),
        "stale device:phone-a principal was not cleaned up from A's policy.toml:\n{policy_a_after}"
    );

    // === Scenario 4: Owner-role paired device pairs another device over SSH -> works on A and B ===
    let inv_owner = node_a.run(&[
        "pair",
        "--name",
        "owner-pad",
        "--role",
        "owner",
        "--allow-owner",
        "--json",
    ]);
    assert_eq!(
        inv_owner.code, 0,
        "pair owner-pad failed: {}",
        inv_owner.stderr
    );
    let (owner_pad_key, owner_enrol) = enrol_from_invite(
        inv_owner.json()["invite"].as_str().unwrap(),
        &sshd_a,
        "owner_pad",
        "ignored",
    );
    assert_eq!(owner_enrol["ok"], json!(true));
    assert_eq!(owner_enrol["role"], json!("owner"));
    assert_eq!(owner_enrol["fleet"]["b"]["status"], json!("synced"));

    // `owner-pad` calls `device.invite` over SSH on A to pair `guest-phone`.
    let guest_inv_rpc = rpc_call_direct(
        &owner_pad_key,
        &fleet_kh_a,
        sshd_a.port(),
        sshd_a.user(),
        "device.invite",
        json!({"name": "guest-phone", "role": "observer"}),
        None,
    );
    let guest_invite_str = guest_inv_rpc["result"]["invite"]
        .as_str()
        .unwrap_or_else(|| panic!("owner-pad device.invite failed: {guest_inv_rpc}"));
    let (guest_key, guest_enrol) =
        enrol_from_invite(guest_invite_str, &sshd_a, "guest_phone", "ignored");
    assert_eq!(guest_enrol["ok"], json!(true));
    assert_eq!(guest_enrol["principal"], json!("device:guest-phone"));
    assert_eq!(guest_enrol["role"], json!("observer"));
    assert_eq!(
        guest_enrol["fleet"]["b"]["status"],
        json!("synced"),
        "expected owner-pad's invite enrolment to push to b: {guest_enrol}"
    );

    let guest_on_a = rpc_call_direct(
        &guest_key,
        &fleet_kh_a,
        sshd_a.port(),
        sshd_a.user(),
        "meta.whoami",
        json!({}),
        None,
    );
    assert_eq!(guest_on_a["result"]["node"], json!("a"));
    assert_eq!(
        guest_on_a["result"]["principal"],
        json!("device:guest-phone")
    );
    let guest_on_b = rpc_call_direct(
        &guest_key,
        &learned_kh_b,
        ep_b_port,
        ep_b_user,
        "meta.whoami",
        json!({}),
        None,
    );
    assert_eq!(guest_on_b["result"]["node"], json!("b"));
    assert_eq!(
        guest_on_b["result"]["principal"],
        json!("device:guest-phone")
    );

    // === Scenario 5: `device.set` changes role (`observer` -> `editor`) and renames, propagating to B ===
    let denied_before_set = rpc_call_direct(
        &phone_off_key,
        &learned_kh_b,
        ep_b_port,
        ep_b_user,
        "fs.write",
        json!({"path": projects_b.join("app").join("edited.txt").to_string_lossy(), "content": "ok\n"}),
        None,
    );
    assert_eq!(denied_before_set["error"]["data"]["code"], json!("denied"));

    let set_role = node_a.run(&[
        "device.set",
        "name=phone-offline",
        "role=editor",
        "rename=field-phone",
    ]);
    assert_eq!(set_role.code, 0, "device.set failed: {}", set_role.stderr);
    let set_json = set_role.json();
    assert_eq!(set_json["name"], json!("field-phone"));
    assert_eq!(set_json["previous_name"], json!("phone-offline"));
    assert_eq!(set_json["role"], json!("editor"));
    assert_eq!(set_json["previous_role"], json!("observer"));
    assert_eq!(set_json["fleet"]["b"]["status"], json!("synced"));

    let allowed_after_set = rpc_call_direct(
        &phone_off_key,
        &learned_kh_b,
        ep_b_port,
        ep_b_user,
        "fs.write",
        json!({"path": projects_b.join("app").join("edited.txt").to_string_lossy(), "content": "ok\n"}),
        None,
    );
    assert_eq!(
        allowed_after_set["result"]["size"],
        json!(3),
        "expected fs.write to succeed on B after device.set role=editor: {allowed_after_set}"
    );
    let list_b = node_b.run(&["device.list"]).json();
    assert!(
        list_b["devices"]
            .as_array()
            .unwrap()
            .iter()
            .any(|d| d["name"] == json!("field-phone") && d["role"] == json!("editor")),
        "expected renamed field-phone (editor) on B: {list_b}"
    );

    // === Scenario 6: Legacy migration/adoption on node C with pre-existing `--pairing` line and no devices.toml ===
    let node_c = Home::new_strict();
    let auth_c = node_c.home().join("authorized_keys_c");
    let home_c_str = node_c.home().to_string_lossy().into_owned();
    let Some(sshd_c) =
        TestSshd::start(auth_c.clone(), &[("HOME", &home_c_str)]).expect("sshd_c failed to start")
    else {
        return;
    };
    let home_c_root = node_c.state().parent().unwrap().to_path_buf();
    let projects_c = node_c.home().join("projects");
    std::fs::create_dir_all(&projects_c).unwrap();
    write_node_cfg(&node_c, "c", &auth_c, &sshd_c);
    node_c.write_config("policy.toml", &policy_for(&projects_c));

    let fleet_kh_c = client_dir.path().join("fleet_known_hosts_c");
    write_known_hosts(
        &fleet_kh_c,
        "127.0.0.1",
        sshd_c.port(),
        &[sshd_c.host_key_line().to_owned()],
    )
    .unwrap();

    // Write two pre-existing legacy `--pairing` lines on C with NO `devices.toml` on disk:
    // 1. `legacy-phone` WITH a matching `state/devices.json` record (must be adopted).
    // 2. `untracked-local` WITHOUT a `state/devices.json` record (must stay local-only).
    let legacy_key = client_dir.path().join("legacy_c_key");
    let legacy_pub_raw =
        generate_ed25519_keypair(sshd_c.ssh_keygen_bin(), &legacy_key, "legacy@c").unwrap();
    let legacy_validated =
        legion_tools::device::keys::validate_ed25519_public_key(&legacy_pub_raw).unwrap();
    let legacy_pub = legacy_validated.canonical;
    let legacy_pairing_id = "cccccccccccccccccccccccccccccccc";

    let local_only_key = client_dir.path().join("local_only_c_key");
    let local_only_pub_raw =
        generate_ed25519_keypair(sshd_c.ssh_keygen_bin(), &local_only_key, "local@c").unwrap();
    let local_only_pub =
        legion_tools::device::keys::validate_ed25519_public_key(&local_only_pub_raw)
            .unwrap()
            .canonical;
    let local_only_id = "dddddddddddddddddddddddddddddddd";

    let pinned_c = legion_tools::device::authkeys::PinnedHome::Root(home_c_root.clone());
    let shell = legion_tools::device::authkeys::ForcedCommandShell::host().unwrap();
    let legacy_line = legion_tools::device::authkeys::format_device_line(
        legion_bin,
        "legacy_phone",
        &legacy_pub,
        "legacy-phone",
        legacy_pairing_id,
        &pinned_c,
        None,
        shell,
    )
    .unwrap();
    let local_only_line = legion_tools::device::authkeys::format_device_line(
        legion_bin,
        "legacy_phone",
        &local_only_pub,
        "untracked-local",
        local_only_id,
        &pinned_c,
        None,
        shell,
    )
    .unwrap();
    std::fs::write(
        &auth_c,
        format!(
            "{}{legacy_line}\n{local_only_line}\n",
            fleet_line(&home_c_root)
        ),
    )
    .unwrap();
    let mut c_state = legion_tools::device::store::DevicesState::default();
    c_state.devices.insert(
        "legacy-phone".to_owned(),
        legion_tools::device::store::DeviceRecord {
            name: "legacy-phone".to_owned(),
            principal: "legacy_phone".to_owned(),
            added: "2026-11-24T10:00:00Z".to_owned(),
            fingerprint: legacy_validated.fingerprint,
            pairing_id: Some(legacy_pairing_id.to_owned()),
        },
    );
    let paths_c = legion_core::Paths::under(home_c_root.clone());
    legion_tools::device::store::save_state(&paths_c, &c_state).unwrap();
    assert!(!home_c_root.join("config").join("devices.toml").exists());

    // `config.read devices.toml` on C before adoption must report exists=false (never synthesize
    // an unsaved adopted list). Running `device.list` on C adopts the state-matched line to disk.
    let c_read_before = node_c.run(&["config.read", "document=devices.toml"]).json();
    assert_eq!(c_read_before["exists"], json!(false), "{c_read_before}");
    let list_c_initial = node_c.run(&["device.list"]).json();
    assert!(
        list_c_initial["devices"]
            .as_array()
            .unwrap()
            .iter()
            .any(|d| d["name"] == json!("untracked-local") && d["local_only"] == json!(true)),
        "expected untracked-local marked local_only in C's device.list: {list_c_initial}"
    );
    assert!(home_c_root.join("config").join("devices.toml").is_file());

    // Wire `c` into A's `fleet.toml`. Under the one-way trust rule:
    // - `pull from=c` imports C's adopted `legacy-phone` onto A without pushing to C.
    // - `push machines=["c"]` sends A's `field-phone` onto C without importing live devices back.
    node_a.write_config(
        "fleet.toml",
        &format!(
            "[machines.b]\ncommand = {}\n\n[machines.c]\ncommand = {}\n",
            ssh_cmd_toml(sshd_b.port(), &fleet_kh_b, sshd_b.user()),
            ssh_cmd_toml(sshd_c.port(), &fleet_kh_c, sshd_c.user()),
        ),
    );
    let pull_c = node_a.run(&[
        "fleet.sync",
        "direction=pull",
        "from=c",
        "documents=[\"devices.toml\"]",
    ]);
    assert_eq!(pull_c.code, 0, "pull from c failed: {}", pull_c.stderr);
    assert_eq!(
        pull_c.json()["results"]["c"]["devices.toml"]["status"],
        json!("pulled")
    );
    let push_c = node_a.run(&[
        "fleet.sync",
        "direction=push",
        "machines=[\"c\"]",
        "documents=[\"devices.toml\"]",
    ]);
    assert_eq!(push_c.code, 0, "push to c failed: {}", push_c.stderr);
    assert_eq!(
        push_c.json()["results"]["c"]["devices.toml"]["status"],
        json!("pushed")
    );

    // Legacy device from C now works directly against A, and field-phone from A now works against C!
    let legacy_on_a = rpc_call_direct(
        &legacy_key,
        &fleet_kh_a,
        sshd_a.port(),
        sshd_a.user(),
        "meta.whoami",
        json!({}),
        None,
    );
    assert_eq!(legacy_on_a["result"]["node"], json!("a"), "{legacy_on_a}");
    assert_eq!(legacy_on_a["result"]["principal"], json!("legacy_phone"));
    assert_eq!(legacy_on_a["result"]["role"], json!("observer"));

    let field_on_c = rpc_call_direct(
        &phone_off_key,
        &fleet_kh_c,
        sshd_c.port(),
        sshd_c.user(),
        "meta.whoami",
        json!({}),
        None,
    );
    assert_eq!(field_on_c["result"]["node"], json!("c"), "{field_on_c}");
    assert_eq!(
        field_on_c["result"]["principal"],
        json!("device:phone-offline")
    );
    assert_eq!(field_on_c["result"]["role"], json!("editor"));
    // untracked-local stayed local-only on C (working on C, refused on A).
    let local_on_c = rpc_call_direct(
        &local_only_key,
        &fleet_kh_c,
        sshd_c.port(),
        sshd_c.user(),
        "meta.whoami",
        json!({}),
        None,
    );
    assert_eq!(local_on_c["result"]["node"], json!("c"), "{local_on_c}");
    let local_refused_on_a = sshd_a
        .ssh_command(&local_only_key, &fleet_kh_a)
        .arg("legion rpc")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(
        !local_refused_on_a.status.success(),
        "local-only line from C must never be adopted onto A"
    );

    // === Scenario 7: Planted-owner trust-rule probe against real sshd ===
    // Plant a rogue `principal = "owner"` device entry directly on B. Neither manual `push` nor
    // any automatic push (`pair`/enrol, `device.set`, `device.remove`) from A may EVER import B's
    // live owner entry onto A, while tombstones from B DO flow back to A on push, `status` reports
    // the difference without importing, and explicit `pull from=b` DOES import it.
    let rogue_key = client_dir.path().join("planted_owner_key");
    let rogue_pub_raw =
        generate_ed25519_keypair(sshd_b.ssh_keygen_bin(), &rogue_key, "planted@b").unwrap();
    let rogue_pub = legion_tools::device::keys::validate_ed25519_public_key(&rogue_pub_raw)
        .unwrap()
        .canonical;
    let rogue_id = "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee";
    {
        let b_devices_path = home_b_root.join("config").join("devices.toml");
        let mut b_devices: legion_tools::device::registry::DevicesFile =
            legion_core::config::parse(&b_devices_path, &std::fs::read(&b_devices_path).unwrap())
                .unwrap();
        let now_str = legion_tools::device::registry::next_updated_timestamp(None).unwrap();
        b_devices.devices.insert(
            rogue_id.to_owned(),
            legion_tools::device::registry::FleetDevice {
                name: "planted-owner".to_owned(),
                public_key: rogue_pub,
                principal: "owner".to_owned(),
                role: Some("owner".to_owned()),
                paired_at: now_str.clone(),
                paired_on: "b".to_owned(),
                paired_by: None,
                confirmed_by: None,
                confirmed_at: None,
                updated_at: now_str.clone(),
                updated_on: "b".to_owned(),
                name_updated_at: now_str.clone(),
                name_updated_on: "b".to_owned(),
                role_updated_at: now_str,
                role_updated_on: "b".to_owned(),
                conflict: false,
                role_conflict: false,
            },
        );
        std::fs::write(&b_devices_path, b_devices.to_toml_string()).unwrap();
        assert_eq!(node_b.run(&["device.list"]).code, 0);
    }

    let assert_rogue_refused_on_a = |step: &str| {
        let a_devices_text =
            std::fs::read_to_string(home_a_root.join("config").join("devices.toml")).unwrap();
        assert!(
            !a_devices_text.contains(rogue_id),
            "{step}: A imported planted owner entry from B:\n{a_devices_text}"
        );
        let refused = sshd_a
            .ssh_command(&rogue_key, &fleet_kh_a)
            .arg("legion rpc")
            .stdin(Stdio::null())
            .output()
            .unwrap();
        assert!(
            !refused.status.success(),
            "{step}: planted owner key from B unexpectedly authenticated to A"
        );
    };

    // 7a. `fleet.sync direction=status` shows difference without importing.
    let status_ab = node_a
        .run(&[
            "fleet.sync",
            "direction=status",
            "machines=[\"b\"]",
            "documents=[\"devices.toml\"]",
        ])
        .json();
    assert_ne!(
        status_ab["results"]["b"]["devices.toml"]["status"],
        json!("same"),
        "{status_ab}"
    );
    assert_rogue_refused_on_a("status");

    // 7b. Manual `fleet.sync direction=push` does NOT import B's planted owner entry.
    let manual_push = node_a.run(&[
        "fleet.sync",
        "direction=push",
        "machines=[\"b\"]",
        "documents=[\"devices.toml\"]",
    ]);
    assert_eq!(manual_push.code, 0, "{}", manual_push.stderr);
    assert_rogue_refused_on_a("manual push");

    // 7c. Automatic push on `pair` / enrol does NOT import B's planted owner entry.
    let inv_probe = node_a.run(&[
        "pair",
        "--name",
        "probe-dev",
        "--role",
        "observer",
        "--json",
    ]);
    assert_eq!(inv_probe.code, 0, "{}", inv_probe.stderr);
    let (_probe_key, probe_enrol) = enrol_from_invite(
        inv_probe.json()["invite"].as_str().unwrap(),
        &sshd_a,
        "probe_dev",
        "ignored",
    );
    assert_eq!(probe_enrol["ok"], json!(true));
    assert_rogue_refused_on_a("auto push after enrol");

    // 7d. Automatic push on `device.set` does NOT import B's planted owner entry.
    let set_probe = node_a.run(&["device.set", "name=probe-dev", "rename=probe-dev-2"]);
    assert_eq!(set_probe.code, 0, "{}", set_probe.stderr);
    assert_rogue_refused_on_a("auto push after device.set");

    // 7e. Automatic push on `device.remove` does NOT import B's planted owner entry.
    let rem_probe = node_a.run(&["device.remove", "name=probe-dev-2"]);
    assert_eq!(rem_probe.code, 0, "{}", rem_probe.stderr);
    assert_rogue_refused_on_a("auto push after device.remove");

    // 7f. Tombstone created on B (while B does NOT push to A) DOES flow back to A during A's
    //     `fleet.sync direction=push`, while B's planted owner entry is STILL NOT imported onto A.
    node_b.write_config("fleet.toml", "[machines]\n");
    let rem_field_on_b = node_b.run(&["device.remove", "name=field-phone"]);
    assert_eq!(rem_field_on_b.code, 0, "{}", rem_field_on_b.stderr);
    let push_gets_tomb = node_a.run(&[
        "fleet.sync",
        "direction=push",
        "machines=[\"b\"]",
        "documents=[\"devices.toml\"]",
    ]);
    assert_eq!(push_gets_tomb.code, 0, "{}", push_gets_tomb.stderr);
    let field_refused_on_a = sshd_a
        .ssh_command(&phone_off_key, &fleet_kh_a)
        .arg("legion rpc")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(
        !field_refused_on_a.status.success(),
        "tombstone from B should have flowed back to A during push"
    );
    assert_rogue_refused_on_a("push returning tombstone");

    // 7g. Explicit `fleet.sync direction=pull from=b` run by owner on A DOES import B's live entry.
    let explicit_pull = node_a.run(&[
        "fleet.sync",
        "direction=pull",
        "from=b",
        "documents=[\"devices.toml\"]",
    ]);
    assert_eq!(explicit_pull.code, 0, "{}", explicit_pull.stderr);
    let rogue_now_on_a = rpc_call_direct(
        &rogue_key,
        &fleet_kh_a,
        sshd_a.port(),
        sshd_a.user(),
        "meta.whoami",
        json!({}),
        None,
    );
    assert_eq!(
        rogue_now_on_a["result"]["node"],
        json!("a"),
        "{rogue_now_on_a}"
    );
    assert_eq!(rogue_now_on_a["result"]["principal"], json!("owner"));
}

#[test]
fn non_owner_device_remove_pushes_with_node_authority_and_audits_both_sides() {
    use legion_tools::device::registry::{DeviceTombstone, DevicesFile, FleetDevice};

    let node_a = Home::new();
    let node_b = Home::new();
    let auth_a = node_a.home().join("authorized_keys");
    let auth_b = node_b.home().join("authorized_keys");
    let home_b_legion = node_b.state().parent().unwrap().to_path_buf();

    let policy = "\
default = \"owner\"

[roles.phone_admin]
allow = [\"effect:read\", \"effect:admin\"]

[principals.tablet]
role = \"phone_admin\"
";
    node_a.write_config("policy.toml", policy);
    node_b.write_config("policy.toml", policy);
    node_a.write_config("projects.toml", "[projects.demo]\ndescription = \"demo\"\n");
    node_a.write_config(
        "node.toml",
        &format!(
            "name = \"a\"\n[devices]\nauthorized_keys = {}\n",
            toml_literal(&auth_a)
        ),
    );
    node_b.write_config(
        "node.toml",
        &format!(
            "name = \"b\"\n[devices]\nauthorized_keys = {}\n",
            toml_literal(&auth_b)
        ),
    );

    let bin = env!("CARGO_BIN_EXE_legion");
    let fleet_entry = |name: &str, extra_args: &[&str]| {
        if cfg!(windows) {
            let script = node_b.home().join(format!("run-{name}-rpc.cmd"));
            let extra = if extra_args.is_empty() {
                String::new()
            } else {
                format!(" {}", extra_args.join(" "))
            };
            let body = format!(
                "@echo off\r\n\
                 set \"LEGION_HOME={}\"\r\n\
                 set \"HOME={}\"\r\n\
                 set \"USERPROFILE={}\"\r\n\
                 set \"XDG_CONFIG_HOME=\"\r\n\
                 set \"XDG_STATE_HOME=\"\r\n\
                 \"{}\" rpc{extra}\r\n",
                home_b_legion.display(),
                node_b.home().display(),
                node_b.home().display(),
                bin,
            );
            std::fs::write(&script, body).unwrap();
            format!(
                "[machines.{name}]\ncommand = [\"cmd.exe\", \"/D\", \"/C\", {}]\n",
                toml_literal(&script),
            )
        } else {
            let env_arg = format!("LEGION_HOME={}", home_b_legion.display());
            let mut parts = vec![
                "\"env\"".to_owned(),
                toml_literal(Path::new(&env_arg)),
                toml_literal(Path::new(bin)),
                "\"rpc\"".to_owned(),
            ];
            for arg in extra_args {
                parts.push(format!("\"{arg}\""));
            }
            format!("[machines.{name}]\ncommand = [{}]\n", parts.join(", "))
        }
    };
    node_a.write_config("fleet.toml", &fleet_entry("b", &[]));

    let id_owner = "11".repeat(16);
    let id_tablet = "22".repeat(16);
    let id_old_owner = "33".repeat(16);
    let now_str = legion_tools::device::registry::next_updated_timestamp(None).unwrap();

    let mut devices_a = DevicesFile::default();
    devices_a.devices.insert(
        id_owner.clone(),
        FleetDevice {
            name: "owner-laptop".to_owned(),
            public_key: ed25519_public_key_from_seed(&[0x41; 32]),
            principal: "owner".to_owned(),
            role: Some("owner".to_owned()),
            paired_at: now_str.clone(),
            paired_on: "a".to_owned(),
            paired_by: None,
            confirmed_by: None,
            confirmed_at: None,
            updated_at: now_str.clone(),
            updated_on: "a".to_owned(),
            name_updated_at: now_str.clone(),
            name_updated_on: "a".to_owned(),
            role_updated_at: now_str.clone(),
            role_updated_on: "a".to_owned(),
            conflict: false,
            role_conflict: false,
        },
    );
    devices_a.devices.insert(
        id_tablet.clone(),
        FleetDevice {
            name: "tablet-dev".to_owned(),
            public_key: ed25519_public_key_from_seed(&[0x42; 32]),
            principal: "tablet".to_owned(),
            role: None,
            paired_at: now_str.clone(),
            paired_on: "a".to_owned(),
            paired_by: None,
            confirmed_by: None,
            confirmed_at: None,
            updated_at: now_str.clone(),
            updated_on: "a".to_owned(),
            name_updated_at: now_str.clone(),
            name_updated_on: "a".to_owned(),
            role_updated_at: now_str.clone(),
            role_updated_on: "a".to_owned(),
            conflict: false,
            role_conflict: false,
        },
    );
    node_a.write_config("devices.toml", &devices_a.to_toml_string());

    let mut devices_b = DevicesFile::default();
    devices_b.devices.insert(
        id_tablet.clone(),
        FleetDevice {
            name: "tablet-dev".to_owned(),
            public_key: ed25519_public_key_from_seed(&[0x42; 32]),
            principal: "tablet".to_owned(),
            role: None,
            paired_at: now_str.clone(),
            paired_on: "a".to_owned(),
            paired_by: None,
            confirmed_by: None,
            confirmed_at: None,
            updated_at: now_str.clone(),
            updated_on: "a".to_owned(),
            name_updated_at: now_str.clone(),
            name_updated_on: "a".to_owned(),
            role_updated_at: now_str.clone(),
            role_updated_on: "a".to_owned(),
            conflict: false,
            role_conflict: false,
        },
    );
    devices_b.removed.insert(
        id_old_owner.clone(),
        DeviceTombstone {
            name: "old-owner".to_owned(),
            principal: "owner".to_owned(),
            fingerprint: None,
            paired_at: None,
            paired_on: None,
            paired_by: None,
            removed_by: None,
            removed_at: now_str,
            removed_on: "b".to_owned(),
        },
    );
    node_b.write_config("devices.toml", &devices_b.to_toml_string());
    assert_eq!(node_a.run(&["device.list"]).code, 0);
    assert_eq!(node_b.run(&["device.list"]).code, 0);
    assert!(
        std::fs::read_to_string(&auth_b)
            .unwrap()
            .contains("legion-device:tablet-dev")
    );

    // 1. Non-owner `tablet` running `fleet.sync direction=push` syncs `projects.toml` and skips
    //    `devices.toml` with a clear owner-role reason.
    let sync = node_a.run(&[
        "fleet.sync",
        "direction=push",
        "machines=[\"b\"]",
        "documents=[\"projects.toml\",\"devices.toml\"]",
        "--as",
        "tablet",
    ]);
    assert_eq!(sync.code, 0, "non-owner fleet.sync failed: {}", sync.stderr);
    let sync_json = sync.json();
    assert_eq!(
        sync_json["results"]["b"]["projects.toml"]["status"],
        json!("pushed"),
        "{sync_json}"
    );
    assert_eq!(
        sync_json["results"]["b"]["devices.toml"]["status"],
        json!("skipped"),
        "{sync_json}"
    );
    assert!(
        sync_json["results"]["b"]["devices.toml"]["detail"]
            .as_str()
            .is_some_and(|d| d.contains("owner")),
        "{sync_json}"
    );

    // 2. Non-owner `tablet` calling `config.write devices.toml` is refused both locally and on `b`.
    let local_write = node_a.run(&[
        "config.write",
        "document=devices.toml",
        "content=version = 1\n",
        "--as",
        "tablet",
    ]);
    assert_ne!(local_write.code, 0);
    let remote_write = node_a.run(&[
        "config.write",
        "document=devices.toml",
        "content=version = 1\n",
        "--as",
        "tablet",
        "--on",
        "b",
    ]);
    assert_ne!(remote_write.code, 0);

    // 3. Non-owner `tablet` runs `device.remove` on A: node A pushes its registry to B with A's
    //    own owner authority, B tombstones `tablet-dev`, strips its key, and BOTH nodes record
    //    audit entries naming `device_registry_push` + triggering principal `tablet`.
    let rm = node_a.run(&["device.remove", "name=tablet-dev", "--as", "tablet"]);
    assert_eq!(rm.code, 0, "tablet remove failed: {}", rm.stderr);
    let rm_json = rm.json();
    assert_eq!(rm_json["removed"], json!(true), "{rm_json}");
    assert_eq!(
        rm_json["fleet"]["b"]["status"],
        json!("synced"),
        "{rm_json}"
    );
    assert!(rm_json.get("still_trusted_by").is_none(), "{rm_json}");

    let b_saved: DevicesFile = legion_core::config::parse(
        &home_b_legion.join("config").join("devices.toml"),
        &std::fs::read(home_b_legion.join("config").join("devices.toml")).unwrap(),
    )
    .unwrap();
    assert!(!b_saved.devices.contains_key(&id_tablet), "{b_saved:?}");
    assert!(b_saved.removed.contains_key(&id_tablet), "{b_saved:?}");
    assert!(
        !std::fs::read_to_string(&auth_b)
            .unwrap()
            .contains("legion-device:tablet-dev")
    );

    let audit_a = std::fs::read_to_string(node_a.state().join("audit.jsonl")).unwrap();
    assert!(
        audit_a.contains("\"fleet.sync\"")
            && audit_a.contains("\"device_registry_push\"")
            && audit_a.contains("\"device.remove\"")
            && audit_a.contains("\"tablet\""),
        "expected sender audit record naming device_registry_push and trigger tablet: {audit_a}"
    );
    let audit_b = std::fs::read_to_string(node_b.state().join("audit.jsonl")).unwrap();
    assert!(
        audit_b.contains("\"config.write\"")
            && audit_b.contains("\"config.write.applied\"")
            && audit_b.contains("\"device_registry_push\"")
            && audit_b.contains("\"device.remove\"")
            && audit_b.contains("\"tablet\""),
        "expected receiver audit records naming device_registry_push and trigger tablet: {audit_b}"
    );

    // 4. When the receiving peer's fleet pipe is pinned to a non-owner (`--as tablet`), the peer
    //    refuses `config.write devices.toml` and A reports it in `still_trusted_by`.
    node_a.write_config("fleet.toml", &fleet_entry("b", &["--as", "tablet"]));
    let rm_owner = node_a.run(&["device.remove", "name=owner-laptop"]);
    assert_eq!(rm_owner.code, 0, "owner remove failed: {}", rm_owner.stderr);
    let rm_owner_json = rm_owner.json();
    assert_eq!(
        rm_owner_json["still_trusted_by"],
        json!(["b"]),
        "{rm_owner_json}"
    );
}

#[test]
fn stamping_records_owner_device_provenance_and_quarantines_backdated_non_owner_child() {
    use legion_tools::device::registry::{DevicesFile, PairedBy};

    let home = Home::new();
    let auth = home.home().join("authorized_keys");
    let host_key_path = home.home().join("host_ed25519.pub");
    std::fs::write(
        &host_key_path,
        format!(
            "{} host@stamp-box\n",
            ed25519_public_key_from_seed(&[0x99; 32])
        ),
    )
    .unwrap();
    let legion_home = home.state().parent().unwrap().to_path_buf();
    home.write_config(
        "node.toml",
        &format!(
            "name = \"stamp-box\"\n[devices]\nauthorized_keys = {}\nhost_keys = [{}]\n",
            toml_literal(&auth),
            toml_literal(&host_key_path),
        ),
    );
    home.write_config(
        "policy.toml",
        "\
default = \"owner\"

[roles.ops]
allow = [\"effect:read\", \"effect:admin\"]

[principals.tablet]
role = \"ops\"
",
    );

    // 1. Seed a paired device holding the `owner` role and an admin-capable non-owner paired device.
    let id_owner_dev = "a1".repeat(16);
    let id_non_owner_dev = "b1".repeat(16);
    let t0 = legion_tools::device::registry::next_updated_timestamp(None).unwrap();
    let mut initial = DevicesFile::default();
    initial.devices.insert(
        id_owner_dev.clone(),
        legion_tools::device::registry::FleetDevice {
            name: "owner-phone".to_owned(),
            public_key: ed25519_public_key_from_seed(&[0xa1; 32]),
            principal: "owner".to_owned(),
            role: Some("owner".to_owned()),
            paired_at: t0.clone(),
            paired_on: "stamp-box".to_owned(),
            paired_by: None,
            confirmed_by: None,
            confirmed_at: None,
            updated_at: t0.clone(),
            updated_on: "stamp-box".to_owned(),
            name_updated_at: t0.clone(),
            name_updated_on: "stamp-box".to_owned(),
            role_updated_at: t0.clone(),
            role_updated_on: "stamp-box".to_owned(),
            conflict: false,
            role_conflict: false,
        },
    );
    initial.devices.insert(
        id_non_owner_dev.clone(),
        legion_tools::device::registry::FleetDevice {
            name: "tablet-parent".to_owned(),
            public_key: ed25519_public_key_from_seed(&[0xb1; 32]),
            principal: "tablet".to_owned(),
            role: None,
            paired_at: t0.clone(),
            paired_on: "stamp-box".to_owned(),
            paired_by: None,
            confirmed_by: None,
            confirmed_at: None,
            updated_at: t0.clone(),
            updated_on: "stamp-box".to_owned(),
            name_updated_at: t0.clone(),
            name_updated_on: "stamp-box".to_owned(),
            role_updated_at: t0,
            role_updated_on: "stamp-box".to_owned(),
            conflict: false,
            role_conflict: false,
        },
    );
    home.write_config("devices.toml", &initial.to_toml_string());
    assert_eq!(home.run(&["device.list"]).code, 0);

    // Helper: invoke one RPC call through `legion gate --as <principal> --device <dev> --pairing <id>`.
    let gate_call =
        |principal: &str, dev_name: &str, pairing_id: &str, tool: &str, args: serde_json::Value| {
            let req_hello = rpc_request(
                json!(1),
                "hello",
                json!({"protocol": legion_core::proto::PROTOCOL, "client": "test", "version": "0"}),
            );
            let req_call = rpc_request(json!(2), "call", json!({"tool": tool, "args": args}));
            let input = format!("{req_hello}\n{req_call}\n");
            let mut cmd = std::process::Command::new(env!("CARGO_BIN_EXE_legion"));
            cmd.args([
                "gate",
                "--home",
                &legion_home.display().to_string(),
                "--as",
                principal,
                "--device",
                dev_name,
                "--pairing",
                pairing_id,
            ])
            .env("SSH_ORIGINAL_COMMAND", "legion rpc")
            .env("HOME", home.home())
            .env("USERPROFILE", home.home())
            .env_remove("XDG_CONFIG_HOME")
            .env_remove("XDG_STATE_HOME")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
            let mut child = cmd.spawn().unwrap();
            child
                .stdin
                .take()
                .unwrap()
                .write_all(input.as_bytes())
                .unwrap();
            let output = child.wait_with_output().unwrap();
            assert!(
                output.status.success(),
                "gate failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            let lines: Vec<Value> = String::from_utf8_lossy(&output.stdout)
                .lines()
                .filter(|l| !l.trim().is_empty())
                .map(|l| serde_json::from_str(l).unwrap())
                .collect();
            lines.into_iter().find(|v| v["id"] == json!(2)).unwrap()
        };

    let run_enrol_gate = |invite_id: &str, name: &str, pub_key: &str| {
        let mut cmd = std::process::Command::new(env!("CARGO_BIN_EXE_legion"));
        cmd.args([
            "gate",
            "--home",
            &legion_home.display().to_string(),
            "--enrol",
            invite_id,
        ])
        .env("HOME", home.home())
        .env("USERPROFILE", home.home())
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("XDG_STATE_HOME")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
        let mut child = cmd.spawn().unwrap();
        let line = format!(
            "{}\n",
            json!({"enrol": 1, "id": invite_id, "public_key": pub_key, "name": name})
        );
        child
            .stdin
            .take()
            .unwrap()
            .write_all(line.as_bytes())
            .unwrap();
        let output = child.wait_with_output().unwrap();
        assert!(
            output.status.success(),
            "gate --enrol failed: stdout={} stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        let reply: Value =
            serde_json::from_str(String::from_utf8_lossy(&output.stdout).trim()).unwrap();
        assert_eq!(reply["ok"], json!(true), "enrol rejected: {reply}");
        reply
    };

    // 2. Owner-role paired device creates an invite and enrols `owner-child`:
    //    `paired_by` MUST record `{ principal: "owner", device: Some(id_owner_dev) }` and node-stamped metadata.
    let inv_owner = gate_call(
        "owner",
        "owner-phone",
        &id_owner_dev,
        "device.invite",
        json!({"name": "owner-child", "role": "owner", "allow_owner": true, "host": "127.0.0.1", "port": 22, "user": "tester"}),
    );
    let owner_inv_id = inv_owner["result"]["id"]
        .as_str()
        .unwrap_or_else(|| panic!("{inv_owner}"))
        .to_owned();
    let owner_child_key = ed25519_public_key_from_seed(&[0xa2; 32]);
    run_enrol_gate(&owner_inv_id, "owner-child", &owner_child_key);

    let after_owner_enrol: DevicesFile = legion_core::config::parse(
        &legion_home.join("config").join("devices.toml"),
        &std::fs::read(legion_home.join("config").join("devices.toml")).unwrap(),
    )
    .unwrap();
    let owner_child = after_owner_enrol
        .devices
        .values()
        .find(|d| d.name == "owner-child")
        .expect("owner-child missing");
    assert_eq!(
        owner_child.paired_by,
        Some(PairedBy {
            principal: "owner".to_owned(),
            device: Some(id_owner_dev.clone()),
        }),
        "owner-role paired device creator must still record paired_by.device"
    );
    assert_eq!(owner_child.paired_on, "stamp-box");
    assert_eq!(owner_child.updated_on, "stamp-box");
    assert_eq!(owner_child.paired_at, owner_child.updated_at);

    // 3. Non-owner admin (`tablet`, role `ops` with `effect:admin`) is now refused from
    //    creating invites for itself under the owner-strength guard AND refused from backdating
    //    via `config.write devices.toml`. Removing `owner-phone` shortly afterwards quarantines
    //    its paired child `owner-child` and strips its key from authorized_keys.
    for allow_owner in [false, true] {
        let inv_denied = gate_call(
            "tablet",
            "tablet-parent",
            &id_non_owner_dev,
            "device.invite",
            json!({
                "principal": "tablet",
                "allow_owner": allow_owner,
                "host": "127.0.0.1",
                "port": 22,
                "user": "tester"
            }),
        );
        assert_eq!(
            inv_denied["error"]["data"]["code"],
            json!("denied"),
            "{inv_denied}"
        );
        let msg = inv_denied["error"]["data"]["message"]
            .as_str()
            .unwrap_or("");
        assert!(
            msg.contains("ops")
                && msg.contains(
                    "this role can change who may do what, which makes it as strong as owner"
                ),
            "{inv_denied}"
        );
    }

    let backdate_res = gate_call(
        "tablet",
        "tablet-parent",
        &id_non_owner_dev,
        "config.write",
        json!({"document": "devices.toml", "content": "version = 1\n"}),
    );
    assert_eq!(
        backdate_res["error"]["data"]["code"],
        json!("denied"),
        "{backdate_res}"
    );

    let rm_parent = home.run(&["device.remove", "name=owner-phone"]);
    assert_eq!(
        rm_parent.code, 0,
        "remove owner-phone failed: {}",
        rm_parent.stderr
    );

    let list = home.run(&["device.list"]).json();
    let child_entry = list["devices"]
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["name"] == json!("owner-child"))
        .expect("owner-child missing in device.list");
    assert_eq!(child_entry["quarantined"], json!(true), "{child_entry}");
    assert!(
        !std::fs::read_to_string(&auth)
            .unwrap()
            .contains("legion-device:owner-child"),
        "quarantined child key must be absent from authorized_keys"
    );
}

#[test]
fn nit2_fleet_sync_pull_preserves_local_confirmation_or_clears_and_warns_on_mismatch() {
    use legion_tools::device::registry::{
        DeviceTombstone, DevicesFile, FleetDevice, PairedBy, next_updated_timestamp,
    };

    let node_a = Home::new();
    let node_b = Home::new();
    let auth_a = node_a.home().join("authorized_keys");
    let auth_b = node_b.home().join("authorized_keys");
    let home_a_legion = node_a.state().parent().unwrap().to_path_buf();
    let home_b_legion = node_b.state().parent().unwrap().to_path_buf();

    let policy = "\
default = \"owner\"

[roles.ops]
allow = [\"effect:read\", \"effect:admin\"]

[principals.tablet]
role = \"ops\"
";
    node_a.write_config("policy.toml", policy);
    node_b.write_config("policy.toml", policy);
    node_a.write_config(
        "node.toml",
        &format!(
            "name = \"a\"\n[devices]\nauthorized_keys = {}\n",
            toml_literal(&auth_a)
        ),
    );
    node_b.write_config(
        "node.toml",
        &format!(
            "name = \"b\"\n[devices]\nauthorized_keys = {}\n",
            toml_literal(&auth_b)
        ),
    );
    let bin = env!("CARGO_BIN_EXE_legion");
    let fleet_a = if cfg!(windows) {
        let script = node_b.home().join("run-b-pull.cmd");
        let body = format!(
            "@echo off\r\n\
             set \"LEGION_HOME={}\"\r\n\
             set \"HOME={}\"\r\n\
             set \"USERPROFILE={}\"\r\n\
             set \"XDG_CONFIG_HOME=\"\r\n\
             set \"XDG_STATE_HOME=\"\r\n\
             \"{}\" rpc\r\n",
            home_b_legion.display(),
            node_b.home().display(),
            node_b.home().display(),
            bin,
        );
        std::fs::write(&script, body).unwrap();
        format!(
            "[machines.b]\ncommand = [\"cmd.exe\", \"/D\", \"/C\", {}]\n",
            toml_literal(&script),
        )
    } else {
        let env_arg = format!("LEGION_HOME={}", home_b_legion.display());
        format!(
            "[machines.b]\ncommand = [\"env\", {}, {}, \"rpc\"]\n",
            toml_literal(Path::new(&env_arg)),
            toml_literal(Path::new(bin)),
        )
    };
    node_a.write_config("fleet.toml", &fleet_a);

    let id_parent = "aa".repeat(16);
    let id_child = "bb".repeat(16);
    let t0 = next_updated_timestamp(None).unwrap();
    let t1 = next_updated_timestamp(Some(&t0)).unwrap();
    let t2 = next_updated_timestamp(Some(&t1)).unwrap();
    let t3 = next_updated_timestamp(Some(&t2)).unwrap();
    let key1 = ed25519_public_key_from_seed(&[0xc1; 32]);
    let key2 = ed25519_public_key_from_seed(&[0xc2; 32]);

    let tomb_parent = DeviceTombstone {
        name: "parent".to_owned(),
        principal: "tablet".to_owned(),
        fingerprint: None,
        paired_at: Some(t0.clone()),
        paired_on: Some("a".to_owned()),
        paired_by: None,
        removed_by: None,
        removed_at: t0.clone(),
        removed_on: "a".to_owned(),
    };
    let mut dev_a = DevicesFile::default();
    dev_a.removed.insert(id_parent.clone(), tomb_parent.clone());
    dev_a.devices.insert(
        id_child.clone(),
        FleetDevice {
            name: "child-v1".to_owned(),
            public_key: key1.clone(),
            principal: "tablet".to_owned(),
            role: None,
            paired_at: t0.clone(),
            paired_on: "a".to_owned(),
            paired_by: Some(PairedBy {
                principal: "tablet".to_owned(),
                device: Some(id_parent.clone()),
            }),
            confirmed_by: Some("owner".to_owned()),
            confirmed_at: Some(t1.clone()),
            updated_at: t1.clone(),
            updated_on: "a".to_owned(),
            name_updated_at: t0.clone(),
            name_updated_on: "a".to_owned(),
            role_updated_at: t0.clone(),
            role_updated_on: "a".to_owned(),
            conflict: false,
            role_conflict: false,
        },
    );
    node_a.write_config("devices.toml", &dev_a.to_toml_string());

    // Node B has a newer rename of the same child WITHOUT confirmation (same identity & provenance).
    let mut dev_b = DevicesFile::default();
    dev_b.removed.insert(id_parent.clone(), tomb_parent.clone());
    dev_b.devices.insert(
        id_child.clone(),
        FleetDevice {
            name: "child-renamed".to_owned(),
            public_key: key1,
            principal: "tablet".to_owned(),
            role: None,
            paired_at: t0.clone(),
            paired_on: "a".to_owned(),
            paired_by: Some(PairedBy {
                principal: "tablet".to_owned(),
                device: Some(id_parent.clone()),
            }),
            confirmed_by: None,
            confirmed_at: None,
            updated_at: t2.clone(),
            updated_on: "b".to_owned(),
            name_updated_at: t2,
            name_updated_on: "b".to_owned(),
            role_updated_at: t0.clone(),
            role_updated_on: "a".to_owned(),
            conflict: false,
            role_conflict: false,
        },
    );
    node_b.write_config("devices.toml", &dev_b.to_toml_string());

    let pull1 = node_a.run(&[
        "fleet.sync",
        "direction=pull",
        "from=b",
        "documents=[\"devices.toml\"]",
    ]);
    assert_eq!(pull1.code, 0, "pull1 failed: {}", pull1.stderr);
    let pull1_json = pull1.json();
    let doc1 = &pull1_json["results"]["b"]["devices.toml"];
    assert_eq!(doc1["status"], json!("pulled"), "{pull1_json}");
    assert!(
        doc1.get("warnings")
            .and_then(|v| v.as_array())
            .is_none_or(|w| w.is_empty()),
        "matching pull must not warn: {pull1_json}"
    );
    let a_after1: DevicesFile = legion_core::config::parse(
        &home_a_legion.join("config").join("devices.toml"),
        &std::fs::read(home_a_legion.join("config").join("devices.toml")).unwrap(),
    )
    .unwrap();
    assert_eq!(a_after1.devices[&id_child].name, "child-renamed");
    assert_eq!(
        a_after1.devices[&id_child].confirmed_by.as_deref(),
        Some("owner")
    );
    assert_eq!(
        a_after1.devices[&id_child].confirmed_at.as_deref(),
        Some(t1.as_str())
    );

    // Now Node B has a newer edit that changes the child's public key: pull MUST clear
    // confirmation and include a warning in the fleet.sync result.
    let mut dev_b2 = DevicesFile::default();
    dev_b2.removed.insert(id_parent.clone(), tomb_parent);
    dev_b2.devices.insert(
        id_child.clone(),
        FleetDevice {
            name: "child-renamed".to_owned(),
            public_key: key2,
            principal: "tablet".to_owned(),
            role: None,
            paired_at: t0,
            paired_on: "a".to_owned(),
            paired_by: Some(PairedBy {
                principal: "tablet".to_owned(),
                device: Some(id_parent),
            }),
            confirmed_by: None,
            confirmed_at: None,
            updated_at: t3.clone(),
            updated_on: "b".to_owned(),
            name_updated_at: t3.clone(),
            name_updated_on: "b".to_owned(),
            role_updated_at: t3,
            role_updated_on: "b".to_owned(),
            conflict: false,
            role_conflict: false,
        },
    );
    node_b.write_config("devices.toml", &dev_b2.to_toml_string());

    let pull2 = node_a.run(&[
        "fleet.sync",
        "direction=pull",
        "from=b",
        "documents=[\"devices.toml\"]",
    ]);
    assert_eq!(pull2.code, 0, "pull2 failed: {}", pull2.stderr);
    let pull2_json = pull2.json();
    let doc2 = &pull2_json["results"]["b"]["devices.toml"];
    assert_eq!(doc2["status"], json!("pulled"), "{pull2_json}");
    let warnings2: Vec<&str> = doc2["warnings"]
        .as_array()
        .map(|arr| arr.iter().filter_map(|v| v.as_str()).collect())
        .unwrap_or_default();
    assert!(
        warnings2
            .iter()
            .any(|w| w.to_ascii_lowercase().contains("confirmation") && w.contains(&id_child)),
        "mismatching pull must warn that confirmation was cleared: {pull2_json}"
    );
    let a_after2: DevicesFile = legion_core::config::parse(
        &home_a_legion.join("config").join("devices.toml"),
        &std::fs::read(home_a_legion.join("config").join("devices.toml")).unwrap(),
    )
    .unwrap();
    assert!(a_after2.devices[&id_child].conflict);
    assert_eq!(a_after2.devices[&id_child].confirmed_by, None);
    assert_eq!(a_after2.devices[&id_child].confirmed_at, None);
}

#[test]
fn f1_independent_field_stamps_prevent_stale_rename_raising_role_and_stale_role_undoing_rename() {
    use legion_tools::device::registry::DevicesFile;

    let node_a = Home::new();
    let node_b = Home::new();
    let auth_a = node_a.home().join("authorized_keys");
    let auth_b = node_b.home().join("authorized_keys");
    let host_key_a = node_a.home().join("host_a.pub");
    let host_key_b = node_b.home().join("host_b.pub");
    std::fs::write(
        &host_key_a,
        format!("{} host@a\n", ed25519_public_key_from_seed(&[0x91; 32])),
    )
    .unwrap();
    std::fs::write(
        &host_key_b,
        format!("{} host@b\n", ed25519_public_key_from_seed(&[0x92; 32])),
    )
    .unwrap();
    let home_a_legion = node_a.state().parent().unwrap().to_path_buf();
    let home_b_legion = node_b.state().parent().unwrap().to_path_buf();

    let policy = "\
default = \"owner\"

[roles.admin]
allow = [\"effect:read\", \"effect:admin\"]

[roles.observer]
allow = [\"effect:read\"]
";
    node_a.write_config("policy.toml", policy);
    node_b.write_config("policy.toml", policy);
    node_a.write_config(
        "node.toml",
        &format!(
            "name = \"a\"\n[devices]\nauthorized_keys = {}\nhost_keys = [{}]\n",
            toml_literal(&auth_a),
            toml_literal(&host_key_a),
        ),
    );
    node_b.write_config(
        "node.toml",
        &format!(
            "name = \"b\"\n[devices]\nauthorized_keys = {}\nhost_keys = [{}]\n",
            toml_literal(&auth_b),
            toml_literal(&host_key_b),
        ),
    );

    let bin = env!("CARGO_BIN_EXE_legion");
    let make_fleet_entry = |peer_name: &str, peer_legion: &Path, peer_home: &Path| {
        if cfg!(windows) {
            let script = peer_home.join(format!("run-{peer_name}-f1.cmd"));
            let body = format!(
                "@echo off\r\n\
                 set \"LEGION_HOME={}\"\r\n\
                 set \"HOME={}\"\r\n\
                 set \"USERPROFILE={}\"\r\n\
                 set \"XDG_CONFIG_HOME=\"\r\n\
                 set \"XDG_STATE_HOME=\"\r\n\
                 \"{}\" rpc\r\n",
                peer_legion.display(),
                peer_home.display(),
                peer_home.display(),
                bin,
            );
            std::fs::write(&script, body).unwrap();
            format!(
                "[machines.{peer_name}]\ncommand = [\"cmd.exe\", \"/D\", \"/C\", {}]\n",
                toml_literal(&script),
            )
        } else {
            let env_arg = format!("LEGION_HOME={}", peer_legion.display());
            format!(
                "[machines.{peer_name}]\ncommand = [\"env\", {}, {}, \"rpc\"]\n",
                toml_literal(Path::new(&env_arg)),
                toml_literal(Path::new(bin)),
            )
        }
    };

    let run_enrol_on =
        |home: &Home, legion_home: &Path, invite_id: &str, name: &str, pub_key: &str| {
            let mut cmd = std::process::Command::new(bin);
            cmd.args([
                "gate",
                "--home",
                &legion_home.display().to_string(),
                "--enrol",
                invite_id,
            ])
            .env("HOME", home.home())
            .env("USERPROFILE", home.home())
            .env_remove("XDG_CONFIG_HOME")
            .env_remove("XDG_STATE_HOME")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
            let mut child = cmd.spawn().unwrap();
            let line = format!(
                "{}\n",
                json!({"enrol": 1, "id": invite_id, "public_key": pub_key, "name": name})
            );
            child
                .stdin
                .take()
                .unwrap()
                .write_all(line.as_bytes())
                .unwrap();
            let output = child.wait_with_output().unwrap();
            assert!(
                output.status.success(),
                "enrol failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            let reply: Value =
                serde_json::from_str(String::from_utf8_lossy(&output.stdout).trim()).unwrap();
            assert_eq!(reply["ok"], json!(true), "{reply}");
        };

    // Direction 1 (reproduced attack):
    // Enrol X (`role = "admin"`) on A, push to B so both A and B have X as `admin`.
    node_a.write_config(
        "fleet.toml",
        &make_fleet_entry("b", &home_b_legion, &node_b.home()),
    );
    let inv_x = node_a.run(&[
        "device.invite",
        "name=x",
        "role=admin",
        "allow_owner=true",
        "host=127.0.0.1",
        "port=22",
        "user=tester",
    ]);
    assert_eq!(inv_x.code, 0, "{}", inv_x.stderr);
    let inv_x_id = inv_x.json()["id"].as_str().unwrap().to_owned();
    run_enrol_on(
        &node_a,
        &home_a_legion,
        &inv_x_id,
        "x",
        &ed25519_public_key_from_seed(&[0xd1; 32]),
    );

    // Confirm B has X as `admin`, then lower X to `observer` on B WITHOUT pushing to A (B has no fleet.toml yet).
    let b_lower = node_b.run(&["device.set", "name=x", "role=observer"]);
    assert_eq!(b_lower.code, 0, "{}", b_lower.stderr);
    assert_eq!(b_lower.json()["role"], json!("observer"));

    // On A (which missed B's role downgrade and still thinks X is `admin`), X itself renames
    // `x` -> `x2`, which pushes with node authority to B.
    let a_rename = node_a.run(&["device.set", "name=x", "rename=x2", "--as", "device:x"]);
    assert_eq!(a_rename.code, 0, "rename on A failed: {}", a_rename.stderr);
    assert_eq!(a_rename.json()["fleet"]["b"]["status"], json!("synced"));

    // B MUST keep `role = "observer"` in BOTH `devices.toml` AND `policy.toml`, while accepting
    // the newer rename `x2`!
    let b_devices: DevicesFile = legion_core::config::parse(
        &home_b_legion.join("config").join("devices.toml"),
        &std::fs::read(home_b_legion.join("config").join("devices.toml")).unwrap(),
    )
    .unwrap();
    let b_x = b_devices
        .devices
        .values()
        .find(|d| d.principal == "device:x")
        .expect("device:x missing on B");
    assert_eq!(b_x.name, "x2", "newer rename must apply on B: {b_x:?}");
    assert_eq!(
        b_x.role.as_deref(),
        Some("observer"),
        "stale rename from A MUST NOT raise role back to admin on B: {b_x:?}"
    );
    let b_policy: legion_core::policy::Policy = legion_core::config::parse(
        &home_b_legion.join("config").join("policy.toml"),
        &std::fs::read(home_b_legion.join("config").join("policy.toml")).unwrap(),
    )
    .unwrap();
    assert_eq!(
        b_policy.role_of("device:x").0,
        "observer",
        "B's policy.toml MUST remain observer after stale rename push from A"
    );

    // Direction 2 (symmetric):
    // Enrol Y (`role = "admin"`) on A and sync to B; disconnect A -> B; rename Y -> y2 on A;
    // then on B (which missed the rename and still calls it `y`), owner lowers Y to `observer`
    // and pushes B -> A. A MUST keep `name = "y2"` while applying `role = "observer"`!
    let inv_y = node_a.run(&[
        "device.invite",
        "name=y",
        "role=admin",
        "allow_owner=true",
        "host=127.0.0.1",
        "port=22",
        "user=tester",
    ]);
    assert_eq!(inv_y.code, 0, "{}", inv_y.stderr);
    let inv_y_id = inv_y.json()["id"].as_str().unwrap().to_owned();
    run_enrol_on(
        &node_a,
        &home_a_legion,
        &inv_y_id,
        "y",
        &ed25519_public_key_from_seed(&[0xd2; 32]),
    );

    // Detach A from B so A's rename does not reach B yet; attach B -> A.
    node_a.write_config("fleet.toml", "");
    let a_rename_y = node_a.run(&["device.set", "name=y", "rename=y2"]);
    assert_eq!(a_rename_y.code, 0, "{}", a_rename_y.stderr);

    node_b.write_config(
        "fleet.toml",
        &make_fleet_entry("a", &home_a_legion, &node_a.home()),
    );
    let b_lower_y = node_b.run(&["device.set", "name=y", "role=observer"]);
    assert_eq!(b_lower_y.code, 0, "{}", b_lower_y.stderr);
    assert_eq!(b_lower_y.json()["fleet"]["a"]["status"], json!("synced"));

    let a_devices: DevicesFile = legion_core::config::parse(
        &home_a_legion.join("config").join("devices.toml"),
        &std::fs::read(home_a_legion.join("config").join("devices.toml")).unwrap(),
    )
    .unwrap();
    let a_y = a_devices
        .devices
        .values()
        .find(|d| d.principal == "device:y")
        .expect("device:y missing on A");
    assert_eq!(
        a_y.name, "y2",
        "stale role change from B MUST NOT undo newer rename y2 on A: {a_y:?}"
    );
    assert_eq!(
        a_y.role.as_deref(),
        Some("observer"),
        "newer role change from B must apply on A: {a_y:?}"
    );
}

#[test]
fn real_sshd_gate_denies_stale_key_on_disk_when_registry_key_differs() {
    use legion_tools::device::registry::DevicesFile;

    if !TestSshd::available() {
        eprintln!(
            "skipping real_sshd_gate_denies_stale_key_on_disk_when_registry_key_differs: sshd/ssh/ssh-keygen unavailable"
        );
        return;
    }

    let home = Home::new_strict();
    let auth_keys = home.home().join("authorized_keys");
    std::fs::write(&auth_keys, "").unwrap();
    let home_dir_str = home.home().to_string_lossy().into_owned();
    let Some(sshd) = TestSshd::start(auth_keys.clone(), &[("HOME", &home_dir_str)])
        .expect("throwaway sshd failed to start")
    else {
        eprintln!("skipping: throwaway sshd unavailable");
        return;
    };

    home.write_config(
        "policy.toml",
        "default = \"owner\"\n\n[roles.observer]\nallow = [\"effect:read\"]\n",
    );
    home.write_config(
        "node.toml",
        &format!(
            "name = \"probe-p5\"\n[devices]\nauthorized_keys = {}\nhost_keys = [{}]\nhosts = [\"127.0.0.1\"]\nport = {}\n",
            toml_literal(&auth_keys),
            toml_literal(sshd.host_key_path()),
            sshd.port(),
        ),
    );

    let inv_run = home.run(&["device.invite", "name=pixel", "role=observer"]);
    assert_eq!(inv_run.code, 0, "{}", inv_run.stderr);
    let payload = InvitePayload::decode(inv_run.json()["invite"].as_str().unwrap()).unwrap();

    let key_dir = home.home().join("p5_keys");
    std::fs::create_dir_all(&key_dir).unwrap();
    let kh = key_dir.join("known_hosts");
    write_known_hosts(&kh, &payload.hosts[0], sshd.port(), &payload.host_keys).unwrap();

    let temp_key = key_dir.join("temp_key");
    write_openssh_ed25519_private_key(&temp_key, &payload.decode_seed().unwrap(), "temp").unwrap();

    let k1_path = key_dir.join("k1");
    let k1_pub = generate_ed25519_keypair(sshd.ssh_keygen_bin(), &k1_path, "k1@test").unwrap();
    let k2_path = key_dir.join("k2");
    let k2_pub = generate_ed25519_keypair(sshd.ssh_keygen_bin(), &k2_path, "k2@test").unwrap();

    // Enrol pixel with K1 over real sshd.
    let enrol_line = format!(
        "{}\n",
        json!({"enrol": 1, "id": payload.id, "public_key": k1_pub, "name": "pixel"})
    );
    let mut child = sshd
        .ssh_command(&temp_key, &kh)
        .arg("ignored")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(enrol_line.as_bytes())
        .unwrap();
    drop(child.stdin.take());
    let enrol_out = child.wait_with_output().unwrap();
    assert!(enrol_out.status.success(), "{enrol_out:?}");

    // Update devices.toml to record K2 for the same pairing id while leaving stale K1 in
    // authorized_keys (simulating a stale authorized_keys file before reconciliation).
    let devices_path = home
        .state()
        .parent()
        .unwrap()
        .join("config")
        .join("devices.toml");
    let mut devices: DevicesFile =
        legion_core::config::parse(&devices_path, &std::fs::read(&devices_path).unwrap()).unwrap();
    let entry = devices.devices.values_mut().next().unwrap();
    let k2_canonical = legion_tools::device::keys::validate_ed25519_public_key(&k2_pub)
        .unwrap()
        .canonical;
    entry.public_key = k2_canonical;
    entry.updated_at =
        legion_tools::device::registry::next_updated_timestamp(Some(&entry.updated_at)).unwrap();
    std::fs::write(&devices_path, devices.to_toml_string()).unwrap();

    // Connecting with stale K1 MUST be denied by `legion gate` (NOT served!) and reconcile
    // authorized_keys to K2.
    let mut k1_rpc = sshd
        .ssh_command(&k1_path, &kh)
        .arg("legion rpc")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let _ = k1_rpc.stdin.as_mut().unwrap().write_all(
        format!(
            "{}\n",
            rpc_request(json!(1), "call", json!({"tool": "meta.whoami", "args": {}}))
        )
        .as_bytes(),
    );
    drop(k1_rpc.stdin.take());
    let k1_out = k1_rpc.wait_with_output().unwrap();
    let k1_stderr = String::from_utf8_lossy(&k1_out.stderr);
    let k1_stdout = String::from_utf8_lossy(&k1_out.stdout);
    assert!(
        !k1_out.status.success(),
        "stale key K1 must be denied by legion gate! stdout={k1_stdout} stderr={k1_stderr}"
    );
    assert!(
        k1_stderr.contains("does not match the public key")
            || k1_stderr.contains("stale authorized_keys"),
        "expected mismatch/stale denial on stderr, got: {k1_stderr}"
    );

    // K2 now succeeds over real sshd after gate reconciled authorized_keys.
    let mut k2_rpc = sshd
        .ssh_command(&k2_path, &kh)
        .arg("legion rpc")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    k2_rpc
        .stdin
        .as_mut()
        .unwrap()
        .write_all(
            format!(
                "{}\n",
                rpc_request(json!(1), "call", json!({"tool": "meta.whoami", "args": {}}))
            )
            .as_bytes(),
        )
        .unwrap();
    drop(k2_rpc.stdin.take());
    let k2_out = k2_rpc.wait_with_output().unwrap();
    assert!(k2_out.status.success(), "{k2_out:?}");
    let k2_stdout = String::from_utf8_lossy(&k2_out.stdout);
    let reply: Value = serde_json::from_str(k2_stdout.trim()).unwrap();
    assert_eq!(
        reply["result"]["principal"],
        json!("device:pixel"),
        "{reply}"
    );
    assert_eq!(reply["result"]["door"], json!("ssh"), "{reply}");
}

#[test]
fn f3_cli_invite_enrol_push_audit_omits_triggering_device_while_paired_device_invite_records_creator()
 {
    let node_a = Home::new();
    let node_b = Home::new();
    let auth_a = node_a.home().join("authorized_keys");
    let auth_b = node_b.home().join("authorized_keys");
    let host_key_a = node_a.home().join("host_a.pub");
    let host_key_b = node_b.home().join("host_b.pub");
    std::fs::write(
        &host_key_a,
        format!("{} host@a\n", ed25519_public_key_from_seed(&[0x71; 32])),
    )
    .unwrap();
    std::fs::write(
        &host_key_b,
        format!("{} host@b\n", ed25519_public_key_from_seed(&[0x72; 32])),
    )
    .unwrap();
    let home_a_legion = node_a.state().parent().unwrap().to_path_buf();
    let home_b_legion = node_b.state().parent().unwrap().to_path_buf();

    let policy = "default = \"owner\"\n\n[roles.observer]\nallow = [\"effect:read\"]\n";
    node_a.write_config("policy.toml", policy);
    node_b.write_config("policy.toml", policy);
    node_a.write_config(
        "node.toml",
        &format!(
            "name = \"a\"\n[devices]\nauthorized_keys = {}\nhost_keys = [{}]\n",
            toml_literal(&auth_a),
            toml_literal(&host_key_a),
        ),
    );
    node_b.write_config(
        "node.toml",
        &format!(
            "name = \"b\"\n[devices]\nauthorized_keys = {}\nhost_keys = [{}]\n",
            toml_literal(&auth_b),
            toml_literal(&host_key_b),
        ),
    );

    let bin = env!("CARGO_BIN_EXE_legion");
    let fleet_a = if cfg!(windows) {
        let script = node_b.home().join("run-b-f3.cmd");
        let body = format!(
            "@echo off\r\n\
             set \"LEGION_HOME={}\"\r\n\
             set \"HOME={}\"\r\n\
             set \"USERPROFILE={}\"\r\n\
             set \"XDG_CONFIG_HOME=\"\r\n\
             set \"XDG_STATE_HOME=\"\r\n\
             \"{}\" rpc\r\n",
            home_b_legion.display(),
            node_b.home().display(),
            node_b.home().display(),
            bin,
        );
        std::fs::write(&script, body).unwrap();
        format!(
            "[machines.b]\ncommand = [\"cmd.exe\", \"/D\", \"/C\", {}]\n",
            toml_literal(&script),
        )
    } else {
        let env_arg = format!("LEGION_HOME={}", home_b_legion.display());
        format!(
            "[machines.b]\ncommand = [\"env\", {}, {}, \"rpc\"]\n",
            toml_literal(Path::new(&env_arg)),
            toml_literal(Path::new(bin)),
        )
    };
    node_a.write_config("fleet.toml", &fleet_a);

    let run_enrol_on_a = |invite_id: &str, name: &str, pub_key: &str| {
        let mut cmd = std::process::Command::new(bin);
        cmd.args([
            "gate",
            "--home",
            &home_a_legion.display().to_string(),
            "--enrol",
            invite_id,
        ])
        .env("HOME", node_a.home())
        .env("USERPROFILE", node_a.home())
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("XDG_STATE_HOME")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
        let mut child = cmd.spawn().unwrap();
        let line = format!(
            "{}\n",
            json!({"enrol": 1, "id": invite_id, "public_key": pub_key, "name": name})
        );
        child
            .stdin
            .take()
            .unwrap()
            .write_all(line.as_bytes())
            .unwrap();
        let output = child.wait_with_output().unwrap();
        assert!(
            output.status.success(),
            "enrol failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let reply: Value =
            serde_json::from_str(String::from_utf8_lossy(&output.stdout).trim()).unwrap();
        assert_eq!(reply["ok"], json!(true), "{reply}");
    };

    // 1. CLI owner (no paired device) creates an invite and enrols `fresh-phone`.
    let inv_cli = node_a.run(&[
        "device.invite",
        "name=fresh-phone",
        "role=owner",
        "allow_owner=true",
        "host=127.0.0.1",
        "port=22",
        "user=tester",
    ]);
    assert_eq!(inv_cli.code, 0, "{}", inv_cli.stderr);
    let inv_cli_id = inv_cli.json()["id"].as_str().unwrap().to_owned();
    run_enrol_on_a(
        &inv_cli_id,
        "fresh-phone",
        &ed25519_public_key_from_seed(&[0x73; 32]),
    );

    let parse_audit = |path: &Path| -> Vec<Value> {
        std::fs::read_to_string(path)
            .unwrap()
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    };

    // Both A (`fleet.sync`) and B (`config.write` + `config.write.applied`) MUST leave
    // `device_registry_push.device` and `device_registry_push.pairing_id` empty/absent!
    let audit_a1 = parse_audit(&node_a.state().join("audit.jsonl"));
    let sync_a1 = audit_a1
        .iter()
        .find(|r| {
            r["tool"] == json!("fleet.sync")
                && r["args"]["device_registry_push"]["trigger"] == json!("device.enrol")
        })
        .unwrap_or_else(|| panic!("missing fleet.sync audit on A: {audit_a1:?}"));
    let push_a1 = &sync_a1["args"]["device_registry_push"];
    assert_eq!(push_a1["principal"], json!("owner"), "{push_a1}");
    assert!(
        push_a1.get("device").is_none_or(Value::is_null),
        "CLI invite enrolment push audit on A MUST NOT blame the newly enrolled device: {push_a1}"
    );
    assert!(
        push_a1.get("pairing_id").is_none_or(Value::is_null),
        "CLI invite enrolment push audit on A MUST NOT record a pairing_id: {push_a1}"
    );
    assert!(
        sync_a1.get("device").is_none_or(Value::is_null),
        "outer fleet.sync audit record on A MUST NOT claim the newly enrolled device triggered it: {sync_a1}"
    );

    let audit_b1 = parse_audit(&node_b.state().join("audit.jsonl"));
    for tool in ["config.write", "config.write.applied"] {
        let rec = audit_b1
            .iter()
            .find(|r| {
                r["tool"] == json!(tool)
                    && r["args"]["device_registry_push"]["trigger"] == json!("device.enrol")
            })
            .unwrap_or_else(|| panic!("missing {tool} audit on B: {audit_b1:?}"));
        let push = &rec["args"]["device_registry_push"];
        assert_eq!(push["principal"], json!("owner"), "{push}");
        assert!(
            push.get("device").is_none_or(Value::is_null),
            "CLI invite enrolment push audit ({tool}) on B MUST leave device empty: {push}"
        );
        assert!(
            push.get("pairing_id").is_none_or(Value::is_null),
            "CLI invite enrolment push audit ({tool}) on B MUST leave pairing_id empty: {push}"
        );
    }

    // 2. Now `fresh-phone` (an actual paired device) creates an invite through `legion gate`
    //    and enrols `second-phone`: the push audit MUST record `fresh-phone` and its pairing id.
    let a_devices: legion_tools::device::registry::DevicesFile = legion_core::config::parse(
        &home_a_legion.join("config").join("devices.toml"),
        &std::fs::read(home_a_legion.join("config").join("devices.toml")).unwrap(),
    )
    .unwrap();
    let fresh_id = a_devices
        .devices
        .iter()
        .find(|(_, d)| d.name == "fresh-phone")
        .map(|(id, _)| id.clone())
        .unwrap();

    let req_hello = rpc_request(
        json!(1),
        "hello",
        json!({"protocol": legion_core::proto::PROTOCOL, "client": "test", "version": "0"}),
    );
    let req_inv = rpc_request(
        json!(2),
        "call",
        json!({
            "tool": "device.invite",
            "args": {"name": "second-phone", "role": "observer", "host": "127.0.0.1", "port": 22, "user": "tester"}
        }),
    );
    let mut gate_cmd = std::process::Command::new(bin);
    gate_cmd
        .args([
            "gate",
            "--home",
            &home_a_legion.display().to_string(),
            "--as",
            "device:fresh-phone",
            "--device",
            "fresh-phone",
            "--pairing",
            &fresh_id,
        ])
        .env("SSH_ORIGINAL_COMMAND", "legion rpc")
        .env("HOME", node_a.home())
        .env("USERPROFILE", node_a.home())
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("XDG_STATE_HOME")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut gate_child = gate_cmd.spawn().unwrap();
    gate_child
        .stdin
        .take()
        .unwrap()
        .write_all(format!("{req_hello}\n{req_inv}\n").as_bytes())
        .unwrap();
    let gate_out = gate_child.wait_with_output().unwrap();
    assert!(
        gate_out.status.success(),
        "{}",
        String::from_utf8_lossy(&gate_out.stderr)
    );
    let inv2_reply: Value = String::from_utf8_lossy(&gate_out.stdout)
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str::<Value>(l).unwrap())
        .find(|v| v["id"] == json!(2))
        .unwrap();
    let inv2_id = inv2_reply["result"]["id"].as_str().unwrap().to_owned();
    run_enrol_on_a(
        &inv2_id,
        "second-phone",
        &ed25519_public_key_from_seed(&[0x74; 32]),
    );

    let audit_b2 = parse_audit(&node_b.state().join("audit.jsonl"));
    let second_applied = audit_b2
        .iter()
        .rev()
        .find(|r| {
            r["tool"] == json!("config.write.applied")
                && r["args"]["device_registry_push"]["trigger"] == json!("device.enrol")
        })
        .unwrap();
    let push2 = &second_applied["args"]["device_registry_push"];
    assert_eq!(push2["principal"], json!("device:fresh-phone"), "{push2}");
    assert_eq!(push2["device"], json!("fresh-phone"), "{push2}");
    assert_eq!(push2["pairing_id"], json!(fresh_id), "{push2}");
}

#[test]
fn tombstones_deliver_to_near_full_peer_when_live_addition_is_refused_and_report_partial_sync() {
    use legion_tools::device::registry::{
        DeviceTombstone, DevicesFile, MAX_DEVICES_TOML_BYTES, next_updated_timestamp,
    };

    let node_a = Home::new();
    let node_b = Home::new();
    let auth_a = node_a.home().join("authorized_keys");
    let auth_b = node_b.home().join("authorized_keys");
    let host_key_a = node_a.home().join("host_a.pub");
    let host_key_b = node_b.home().join("host_b.pub");
    std::fs::write(
        &host_key_a,
        format!("{} host@a\n", ed25519_public_key_from_seed(&[0x93; 32])),
    )
    .unwrap();
    std::fs::write(
        &host_key_b,
        format!("{} host@b\n", ed25519_public_key_from_seed(&[0x94; 32])),
    )
    .unwrap();
    let home_a_legion = node_a.state().parent().unwrap().to_path_buf();
    let home_b_legion = node_b.state().parent().unwrap().to_path_buf();

    let policy = "\
default = \"owner\"

[roles.observer]
allow = [\"effect:read\"]
";
    node_a.write_config("policy.toml", policy);
    node_b.write_config("policy.toml", policy);
    node_a.write_config(
        "node.toml",
        &format!(
            "name = \"a\"\n\n[devices]\nauthorized_keys = {}\nhost_keys = [{}]\n",
            toml_literal(&auth_a),
            toml_literal(&host_key_a),
        ),
    );
    node_b.write_config(
        "node.toml",
        &format!(
            "name = \"b\"\n\n[devices]\nauthorized_keys = {}\nhost_keys = [{}]\n",
            toml_literal(&auth_b),
            toml_literal(&host_key_b),
        ),
    );

    let bin = env!("CARGO_BIN_EXE_legion");
    let make_fleet = |target_home: &Path, name: &str, script_dir: &Path| -> String {
        if cfg!(windows) {
            let script = script_dir.join(format!("call-{name}.cmd"));
            let body = format!(
                "@echo off\r\nset \"LEGION_HOME={}\"\r\n\"{}\" rpc\r\n",
                target_home.display(),
                bin,
            );
            std::fs::write(&script, body).unwrap();
            format!(
                "[machines.{name}]\ncommand = [\"cmd.exe\", \"/D\", \"/C\", {}]\n",
                toml_literal(&script),
            )
        } else {
            let env_arg = format!("LEGION_HOME={}", target_home.display());
            format!(
                "[machines.{name}]\ncommand = [\"env\", {}, {}, \"rpc\"]\n",
                toml_literal(Path::new(&env_arg)),
                toml_literal(Path::new(bin)),
            )
        }
    };
    let fleet_a_online = make_fleet(&home_b_legion, "b", &node_a.home());
    node_a.write_config("fleet.toml", &fleet_a_online);

    let enrol_on = |home: &Home, legion_home: &Path, name: &str, role: &str, seed: u8| -> String {
        let inv = home.run(&[
            "device.invite",
            &format!("name={name}"),
            &format!("role={role}"),
            "host=127.0.0.1",
            "port=22",
            "user=tester",
        ]);
        assert_eq!(inv.code, 0, "invite {name} failed: {}", inv.stderr);
        let inv_id = inv.json()["id"].as_str().unwrap().to_owned();
        let pub_key = ed25519_public_key_from_seed(&[seed; 32]);
        let req = format!(
            "{{\"enrol\":1,\"id\":\"{inv_id}\",\"public_key\":\"{pub_key}\",\"name\":\"{name}\"}}\n"
        );
        let mut child = std::process::Command::new(bin)
            .args([
                "gate",
                "--home",
                &legion_home.display().to_string(),
                "--enrol",
                &inv_id,
            ])
            .env("HOME", home.home())
            .env("USERPROFILE", home.home())
            .env_remove("XDG_CONFIG_HOME")
            .env_remove("XDG_STATE_HOME")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(req.as_bytes())
            .unwrap();
        let out = child.wait_with_output().unwrap();
        assert!(
            out.status.success(),
            "enrol {name} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let reply: Value = serde_json::from_slice(&out.stdout).unwrap();
        assert_eq!(reply["ok"], json!(true), "{reply}");
        let devs: DevicesFile = legion_core::config::parse(
            &legion_home.join("config").join("devices.toml"),
            &std::fs::read(legion_home.join("config").join("devices.toml")).unwrap(),
        )
        .unwrap();
        devs.devices
            .iter()
            .find(|(_, d)| d.name == name)
            .map(|(id, _)| id.clone())
            .unwrap()
    };

    // 1. Pair `x` and `x2` on A -> pushed to B, active on both A and B.
    let id_x = enrol_on(&node_a, &home_a_legion, "x", "observer", 0x31);
    let id_x2 = enrol_on(&node_a, &home_a_legion, "x2", "observer", 0x32);
    let ak_b_initial = std::fs::read_to_string(&auth_b).unwrap();
    assert!(ak_b_initial.contains(&id_x) && ak_b_initial.contains(&id_x2));

    // 2. Disconnect A from B, fill B near 1 MiB so adding live device `z` violates B's reserve.
    std::fs::remove_file(home_a_legion.join("config").join("fleet.toml")).unwrap();
    let b_path = home_b_legion.join("config").join("devices.toml");
    let mut b_devs: DevicesFile =
        legion_core::config::parse(&b_path, &std::fs::read(&b_path).unwrap()).unwrap();
    let ts = next_updated_timestamp(None).unwrap();
    let make_pad_tomb = |idx: u32| DeviceTombstone {
        name: format!("pad-{idx:04}-{}", "p".repeat(100)),
        principal: "owner".to_owned(),
        fingerprint: Some("SHA256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_owned()),
        paired_at: Some(ts.clone()),
        paired_on: Some("b".to_owned()),
        paired_by: None,
        removed_by: None,
        removed_at: ts.clone(),
        removed_on: "b".to_owned(),
    };
    let per_tomb = {
        let mut one = DevicesFile::default();
        one.removed
            .insert(format!("e{:031x}", 0u32), make_pad_tomb(0));
        one.to_toml_string().len() - "version = 1\n".len()
    };
    let init_n = ((MAX_DEVICES_TOML_BYTES - 4500) / per_tomb) as u32;
    for i in 0..init_n {
        b_devs
            .removed
            .insert(format!("e{i:031x}"), make_pad_tomb(i));
    }
    let mut idx = init_n;
    loop {
        let mut probe = b_devs.clone();
        probe
            .removed
            .insert(format!("e{idx:031x}"), make_pad_tomb(idx));
        if probe.to_toml_string().len() > MAX_DEVICES_TOML_BYTES - 150 {
            break;
        }
        b_devs = probe;
        idx += 1;
    }
    node_b.write_config("devices.toml", &b_devs.to_toml_string());

    // 3. While offline from B, A removes `x` and pairs new live device `z`.
    let rem_x = node_a.run(&["device.remove", &format!("id={id_x}")]);
    assert_eq!(rem_x.code, 0, "remove x failed: {}", rem_x.stderr);
    let id_z = enrol_on(&node_a, &home_a_legion, "z", "observer", 0x33);

    // 4. Reconnect A -> B and push `devices.toml`:
    //    `x`'s tombstone MUST land on B (and strip `x` from B's authorized_keys),
    //    `z` MUST NOT be added to B, and `fleet.sync` MUST report B as `"partial"`.
    node_a.write_config("fleet.toml", &fleet_a_online);
    let push = node_a.run(&[
        "fleet.sync",
        "direction=push",
        "documents=[\"devices.toml\"]",
    ]);
    assert_eq!(push.code, 0, "push failed: {}", push.stderr);
    let push_json = push.json();
    let doc_b = &push_json["results"]["b"]["devices.toml"];
    assert_eq!(
        doc_b["status"],
        json!("partial"),
        "expected partial sync status when removals land and live addition is refused: {push_json}"
    );
    assert!(
        doc_b["detail"]
            .as_str()
            .is_some_and(|d| d.contains("Removals delivered") && d.contains("refused")),
        "expected clear partial sync detail: {doc_b}"
    );
    assert!(
        doc_b["error"]["message"]
            .as_str()
            .is_some_and(|m| m.contains("1048576") || m.contains("1 MiB")),
        "expected size-limit refusal reason: {doc_b}"
    );

    let b_after: DevicesFile =
        legion_core::config::parse(&b_path, &std::fs::read(&b_path).unwrap()).unwrap();
    assert!(
        b_after.removed.contains_key(&id_x) && !b_after.devices.contains_key(&id_x),
        "x MUST be tombstoned on near-full peer B"
    );
    assert!(
        !b_after.devices.contains_key(&id_z),
        "z MUST NOT be added to near-full peer B"
    );
    let ak_b_after = std::fs::read_to_string(&auth_b).unwrap();
    assert!(
        !ak_b_after.contains(&id_x),
        "x MUST be stripped from B's authorized_keys"
    );
    assert!(
        !ak_b_after.contains(&id_z),
        "z MUST NOT appear in B's authorized_keys"
    );

    // 5. Removing `x2` on A while A still carries refused `z` MUST deliver `x2`'s tombstone to B,
    //    report `fleet.b.status == "partial"`, and NOT falsely list `b` in `still_trusted_by`!
    let rem_x2 = node_a.run(&["device.remove", &format!("id={id_x2}")]);
    assert_eq!(rem_x2.code, 0, "remove x2 failed: {}", rem_x2.stderr);
    let rem_x2_json = rem_x2.json();
    assert_eq!(
        rem_x2_json["fleet"]["b"]["status"],
        json!("partial"),
        "expected partial fleet status on device.remove: {rem_x2_json}"
    );
    assert!(
        rem_x2_json
            .get("still_trusted_by")
            .and_then(|v| v.as_array())
            .is_none_or(|a| a.is_empty()),
        "peer B received x2's removal and must NOT appear in still_trusted_by: {rem_x2_json}"
    );
    let b_after2: DevicesFile =
        legion_core::config::parse(&b_path, &std::fs::read(&b_path).unwrap()).unwrap();
    assert!(b_after2.removed.contains_key(&id_x2) && !b_after2.devices.contains_key(&id_x2));
    assert!(!std::fs::read_to_string(&auth_b).unwrap().contains(&id_x2));
}

#[test]
fn tombstones_deliver_across_principal_name_collision_and_report_partial_sync() {
    use legion_tools::device::registry::DevicesFile;

    let node_a = Home::new();
    let node_b = Home::new();
    let auth_a = node_a.home().join("authorized_keys");
    let auth_b = node_b.home().join("authorized_keys");
    let host_key_a = node_a.home().join("host_a.pub");
    let host_key_b = node_b.home().join("host_b.pub");
    std::fs::write(
        &host_key_a,
        format!("{} host@a\n", ed25519_public_key_from_seed(&[0x95; 32])),
    )
    .unwrap();
    std::fs::write(
        &host_key_b,
        format!("{} host@b\n", ed25519_public_key_from_seed(&[0x96; 32])),
    )
    .unwrap();
    let home_a_legion = node_a.state().parent().unwrap().to_path_buf();
    let home_b_legion = node_b.state().parent().unwrap().to_path_buf();

    let policy = "\
default = \"owner\"

[roles.observer]
allow = [\"effect:read\"]
";
    node_a.write_config("policy.toml", policy);
    node_b.write_config("policy.toml", policy);
    node_a.write_config(
        "node.toml",
        &format!(
            "name = \"a\"\n\n[devices]\nauthorized_keys = {}\nhost_keys = [{}]\n",
            toml_literal(&auth_a),
            toml_literal(&host_key_a),
        ),
    );
    node_b.write_config(
        "node.toml",
        &format!(
            "name = \"b\"\n\n[devices]\nauthorized_keys = {}\nhost_keys = [{}]\n",
            toml_literal(&auth_b),
            toml_literal(&host_key_b),
        ),
    );

    let bin = env!("CARGO_BIN_EXE_legion");
    let fleet_a_online = if cfg!(windows) {
        let script = node_a.home().join("call-b.cmd");
        let body = format!(
            "@echo off\r\nset \"LEGION_HOME={}\"\r\n\"{}\" rpc\r\n",
            home_b_legion.display(),
            bin,
        );
        std::fs::write(&script, body).unwrap();
        format!(
            "[machines.b]\ncommand = [\"cmd.exe\", \"/D\", \"/C\", {}]\n",
            toml_literal(&script),
        )
    } else {
        let env_arg = format!("LEGION_HOME={}", home_b_legion.display());
        format!(
            "[machines.b]\ncommand = [\"env\", {}, {}, \"rpc\"]\n",
            toml_literal(Path::new(&env_arg)),
            toml_literal(Path::new(bin)),
        )
    };
    node_a.write_config("fleet.toml", &fleet_a_online);

    let enrol_on = |home: &Home, legion_home: &Path, name: &str, seed: u8| -> String {
        let inv = home.run(&[
            "device.invite",
            &format!("name={name}"),
            "role=observer",
            "host=127.0.0.1",
            "port=22",
            "user=tester",
        ]);
        assert_eq!(inv.code, 0, "invite {name} failed: {}", inv.stderr);
        let inv_id = inv.json()["id"].as_str().unwrap().to_owned();
        let pub_key = ed25519_public_key_from_seed(&[seed; 32]);
        let req = format!(
            "{{\"enrol\":1,\"id\":\"{inv_id}\",\"public_key\":\"{pub_key}\",\"name\":\"{name}\"}}\n"
        );
        let mut child = std::process::Command::new(bin)
            .args([
                "gate",
                "--home",
                &legion_home.display().to_string(),
                "--enrol",
                &inv_id,
            ])
            .env("HOME", home.home())
            .env("USERPROFILE", home.home())
            .env_remove("XDG_CONFIG_HOME")
            .env_remove("XDG_STATE_HOME")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(req.as_bytes())
            .unwrap();
        let out = child.wait_with_output().unwrap();
        assert!(
            out.status.success(),
            "enrol {name} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let reply: Value = serde_json::from_slice(&out.stdout).unwrap();
        assert_eq!(reply["ok"], json!(true), "{reply}");
        let devs: DevicesFile = legion_core::config::parse(
            &legion_home.join("config").join("devices.toml"),
            &std::fs::read(legion_home.join("config").join("devices.toml")).unwrap(),
        )
        .unwrap();
        devs.devices
            .iter()
            .find(|(_, d)| d.name == name)
            .map(|(id, _)| id.clone())
            .unwrap()
    };

    // 1. Both A and B trust shared device `x`.
    let id_x = enrol_on(&node_a, &home_a_legion, "x", 0x41);
    assert!(std::fs::read_to_string(&auth_b).unwrap().contains(&id_x));

    // 2. While out of touch:
    //    - A pairs `phone` (`device:phone`) and removes `x`.
    //    - B independently pairs its own `phone` (`device:phone`).
    std::fs::remove_file(home_a_legion.join("config").join("fleet.toml")).unwrap();
    let id_phone_a = enrol_on(&node_a, &home_a_legion, "phone", 0x42);
    let rem_x = node_a.run(&["device.remove", &format!("id={id_x}")]);
    assert_eq!(rem_x.code, 0, "remove x failed: {}", rem_x.stderr);

    let id_phone_b = enrol_on(&node_b, &home_b_legion, "phone", 0x43);
    assert_ne!(id_phone_a, id_phone_b);

    // 3. Reconnect A -> B and push `devices.toml`:
    //    `x` MUST be tombstoned and stripped on B even though `phone` collides on `device:phone`,
    //    B's own `phone` stays intact and active, and sender reports `"partial"` with the
    //    principal-collision reason!
    node_a.write_config("fleet.toml", &fleet_a_online);
    let push = node_a.run(&[
        "fleet.sync",
        "direction=push",
        "documents=[\"devices.toml\"]",
    ]);
    assert_eq!(push.code, 0, "push failed: {}", push.stderr);
    let push_json = push.json();
    let doc_b = &push_json["results"]["b"]["devices.toml"];
    assert_eq!(
        doc_b["status"],
        json!("partial"),
        "expected partial status across principal collision: {push_json}"
    );
    assert!(
        doc_b["error"]["message"]
            .as_str()
            .is_some_and(|m| m.contains("device:phone")),
        "expected device:phone principal collision reason: {doc_b}"
    );

    let b_path = home_b_legion.join("config").join("devices.toml");
    let b_after: DevicesFile =
        legion_core::config::parse(&b_path, &std::fs::read(&b_path).unwrap()).unwrap();
    assert!(
        b_after.removed.contains_key(&id_x) && !b_after.devices.contains_key(&id_x),
        "x MUST be tombstoned on B despite device:phone collision"
    );
    assert!(
        b_after.devices.contains_key(&id_phone_b) && !b_after.devices.contains_key(&id_phone_a),
        "B's own phone must remain intact while colliding phone_a is refused"
    );
    let ak_b_after = std::fs::read_to_string(&auth_b).unwrap();
    assert!(!ak_b_after.contains(&id_x), "x must be stripped on B");
    assert!(
        ak_b_after.contains(&id_phone_b),
        "B's own phone must stay active on B"
    );
}
