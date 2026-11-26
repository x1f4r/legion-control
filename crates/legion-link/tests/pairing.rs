//! End-to-end proof of the node pairing flow and `legion-link` client working together over a
//! real loopback `sshd`:
//!
//! 1. `legion device.invite` on a node behind a throwaway `sshd` (`StrictModes yes`, without
//!    `SetEnv LEGION_HOME` so `--home` pinning on `legion gate` is exercised).
//! 2. `legion_link::Invite::parse` + `legion_link::enrol` with a fresh `DeviceKey` over real SSH.
//! 3. `legion_link::Link` session opening through `legion gate --as ... --device ... --home ...`:
//!    `hello`, `tools`, `meta.whoami`, streamed `exec.run` (verifying live output events), and
//!    denied `fs.write` (verifying role confinement).
//! 4. `legion device.remove` terminating the live paired session immediately and revoking the key
//!    so subsequent `Link` calls fail cleanly.
//!
//! Skipped cleanly on platforms or hosts where `sshd`/`ssh-keygen` or a built `legion` binary are
//! unavailable.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::fs::{self, File, OpenOptions};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// Generous backstop used only as a hang guard on explicit hand-over waits; never a timing budget.
const HANG_GUARD: Duration = Duration::from_secs(30);

use common::{
    current_user, find_sshd, find_tool, real_legion_bin_with_sshd, strict_tempdir, toml_literal,
};
use legion_link::proto::{ErrorCode, PROTOCOL};
use legion_link::session::TransportHandle;
use legion_link::ssh::open_exec_raw;
use legion_link::{
    CancellationToken, DETAIL_SENT, DeviceKey, Invite, Link, LinkConfig, NodeLink,
    REASON_AUTH_FAILED, REASON_DEVICE_REMOVED_OR_UNKNOWN, SshTimeouts, enrol,
};
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::io::AsyncWriteExt;

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

struct ThrowawaySshd {
    _dir: TempDir,
    child: Child,
    port: u16,
    user: String,
    host_key_pub: PathBuf,
    log_path: PathBuf,
}

impl ThrowawaySshd {
    fn start(authorized_keys: &Path, home_dir: &Path) -> Option<Self> {
        if !cfg!(unix) {
            return None;
        }
        let sshd_bin = find_sshd()?;
        let ssh_keygen_bin = find_tool("ssh-keygen")?;
        let user = current_user()?;

        let dir = strict_tempdir();
        if let Some(parent) = authorized_keys.parent() {
            fs::create_dir_all(parent).unwrap();
            #[cfg(unix)]
            {
                let _ = fs::set_permissions(parent, fs::Permissions::from_mode(0o700));
            }
        }
        if !authorized_keys.exists() {
            fs::write(authorized_keys, b"").unwrap();
        }
        #[cfg(unix)]
        {
            let _ = fs::set_permissions(authorized_keys, fs::Permissions::from_mode(0o600));
        }

        let host_key = dir.path().join("host_ed25519");
        let host_key_pub = dir.path().join("host_ed25519.pub");
        let keygen = Command::new(&ssh_keygen_bin)
            .args(["-q", "-t", "ed25519", "-N", "", "-C", "legion-link-test"])
            .arg("-f")
            .arg(&host_key)
            .stdin(Stdio::null())
            .output()
            .unwrap();
        assert!(
            keygen.status.success(),
            "ssh-keygen failed: {}",
            String::from_utf8_lossy(&keygen.stderr)
        );
        #[cfg(unix)]
        {
            let _ = fs::set_permissions(&host_key, fs::Permissions::from_mode(0o600));
        }

        let supports_no_penalties = Command::new(&sshd_bin)
            .args(["-G", "-f", "/dev/null", "-o", "PerSourcePenalties=no"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
            || Command::new(&sshd_bin)
                .args(["-T", "-f", "/dev/null", "-o", "PerSourcePenalties=no", "-h"])
                .arg(&host_key)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .is_ok_and(|s| s.success());

        let mut last_log = String::new();
        for _ in 0..3 {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let port = listener.local_addr().unwrap().port();
            drop(listener);

            let config_path = dir.path().join("sshd_config");
            let log_path = dir.path().join("sshd.log");
            let mut config = format!(
                "ListenAddress 127.0.0.1\n\
                 Port {port}\n\
                 HostKey {host_key}\n\
                 PidFile none\n\
                 AuthorizedKeysFile {auth_keys}\n\
                 AllowUsers {user}\n\
                 StrictModes yes\n\
                 PubkeyAuthentication yes\n\
                 PasswordAuthentication no\n\
                 KbdInteractiveAuthentication no\n\
                 ChallengeResponseAuthentication no\n\
                 PermitRootLogin no\n\
                 UsePAM no\n\
                 AllowAgentForwarding no\n\
                 AllowTcpForwarding no\n\
                 X11Forwarding no\n\
                 PrintMotd no\n\
                 LogLevel VERBOSE\n\
                 SetEnv HOME={home}\n",
                host_key = host_key.display(),
                auth_keys = authorized_keys.display(),
                home = home_dir.display(),
            );
            if supports_no_penalties {
                config.push_str("PerSourcePenalties no\n");
            }
            fs::write(&config_path, &config).unwrap();

            let log_file = fs::File::create(&log_path).unwrap();
            let mut child = Command::new(&sshd_bin)
                .args(["-D", "-e", "-f"])
                .arg(&config_path)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::from(log_file))
                .spawn()
                .unwrap();

            let deadline = Instant::now() + HANG_GUARD;
            let mut listening = false;
            while Instant::now() < deadline {
                if child.try_wait().unwrap().is_some() {
                    break;
                }
                if TcpStream::connect(("127.0.0.1", port)).is_ok() {
                    listening = true;
                    break;
                }
                std::thread::sleep(Duration::from_millis(25));
            }
            if listening {
                return Some(Self {
                    _dir: dir,
                    child,
                    port,
                    user,
                    host_key_pub,
                    log_path,
                });
            }
            let _ = child.kill();
            let _ = child.wait();
            last_log = fs::read_to_string(&log_path).unwrap_or_default();
        }
        panic!(
            "throwaway sshd failed to start listening within {}s:\n{last_log}",
            HANG_GUARD.as_secs()
        );
    }

    fn log(&self) -> String {
        fs::read_to_string(&self.log_path).unwrap_or_default()
    }
}

impl Drop for ThrowawaySshd {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn run_legion(legion_bin: &Path, legion_home: &Path, user_home: &Path, args: &[&str]) -> Value {
    let output = Command::new(legion_bin)
        .args(args)
        .arg("--json")
        .env("LEGION_HOME", legion_home)
        .env("HOME", user_home)
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("XDG_STATE_HOME")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    assert!(
        output.status.success(),
        "legion {args:?} failed ({:?}):\nstdout: {stdout}\nstderr: {stderr}",
        output.status.code()
    );
    serde_json::from_str(stdout.trim()).unwrap_or_else(|e| {
        panic!("legion {args:?} returned non-JSON ({e}):\nstdout: {stdout}\nstderr: {stderr}")
    })
}

#[tokio::test]
async fn real_sshd_pairing_with_legion_link_enrol_stream_and_revoke() {
    let Some(legion_bin) = real_legion_bin_with_sshd(
        "pairing::real_sshd_pairing_with_legion_link_enrol_stream_and_revoke",
    ) else {
        return;
    };

    let root = strict_tempdir();
    let user_home = root.path().join("home");
    let legion_home = root.path().join("legion");
    let config_dir = legion_home.join("config");
    let ssh_dir = user_home.join(".ssh");
    let auth_keys = ssh_dir.join("authorized_keys");
    fs::create_dir_all(&config_dir).unwrap();
    fs::create_dir_all(&ssh_dir).unwrap();
    #[cfg(unix)]
    {
        let _ = fs::set_permissions(&user_home, fs::Permissions::from_mode(0o700));
        let _ = fs::set_permissions(&ssh_dir, fs::Permissions::from_mode(0o700));
    }

    let Some(sshd) = ThrowawaySshd::start(&auth_keys, &user_home) else {
        eprintln!("skipping: throwaway sshd unavailable");
        return;
    };

    // Pin the throwaway sshd's ephemeral host key explicitly under `[devices] host_keys` (an
    // unconfigured invite against a high-port throwaway sshd would rightly be refused on hosts
    // where `/etc/ssh/ssh_host_*_key.pub` exists and does not contain the throwaway key).
    let node_toml = format!(
        "name = \"tower\"\n\n[devices]\nauthorized_keys = {}\nhost_keys = [{}]\n",
        toml_literal(&auth_keys),
        toml_literal(&sshd.host_key_pub),
    );
    fs::write(config_dir.join("node.toml"), node_toml).unwrap();
    fs::write(
        config_dir.join("policy.toml"),
        "default = \"owner\"\n\n\
         [roles.operator]\n\
         allow = [\"effect:read\", \"exec.run\"]\n\
         deny = [\"audit.*\", \"fs.write\"]\n\n\
         [principals.phone]\n\
         role = \"operator\"\n",
    )
    .unwrap();

    // 1. Issue a pairing invite on the node behind the throwaway sshd.
    let port_arg = format!("port={}", sshd.port);
    let user_arg = format!("user={}", sshd.user);
    let invite_res = run_legion(
        &legion_bin,
        &legion_home,
        &user_home,
        &[
            "device.invite",
            "principal=phone",
            "host=127.0.0.1",
            &port_arg,
            &user_arg,
        ],
    );
    let invite_text = invite_res["invite"].as_str().unwrap();
    assert_eq!(invite_res["host_keys_source"], json!("configured"));

    // 2. Parse the invite with `legion-link` and enrol a fresh DeviceKey over real SSH.
    let invite = Invite::parse(invite_text).expect("legion-link failed to parse node invite");
    assert_eq!(invite.node, "tower");
    assert_eq!(invite.principal, "phone");
    assert_eq!(invite.port, sshd.port);

    let device_key = DeviceKey::generate().unwrap();
    let enrolled = enrol(
        &invite,
        &device_key,
        "Link Phone",
        SshTimeouts::default(),
        CancellationToken::new(),
    )
    .await
    .unwrap_or_else(|err| {
        panic!(
            "legion_link::enrol failed: {err}\n--- sshd log ---\n{}",
            sshd.log()
        )
    });
    assert_eq!(enrolled.node, "tower");
    assert_eq!(enrolled.principal, "phone");
    assert_eq!(enrolled.role, "operator");
    assert_eq!(enrolled.device, "link-phone");

    // Verify the installed forced command carries `--as`, `--device`, `--pairing`, and `--home`.
    let installed_auth = fs::read_to_string(&auth_keys).unwrap();
    assert!(
        installed_auth.contains("--as phone")
            && installed_auth.contains("--device link-phone")
            && installed_auth.contains("--pairing ")
            && installed_auth.contains("--home "),
        "unexpected authorized_keys line:\n{installed_auth}"
    );

    // Reusing the single-use invite key must now be rejected at SSH auth.
    let second_key = DeviceKey::generate().unwrap();
    let replay_err = enrol(
        &invite,
        &second_key,
        "Second Phone",
        SshTimeouts::default(),
        CancellationToken::new(),
    )
    .await
    .unwrap_err();
    assert_eq!(replay_err.code, ErrorCode::Denied, "{replay_err}");
    assert_eq!(
        replay_err.details.as_ref().unwrap()["reason"],
        json!(REASON_AUTH_FAILED)
    );

    // 3. Connect with `legion_link::Link`, verify hello/tools/meta.whoami, streamed `exec.run`,
    //    and policy denial of `fs.write`.
    let config = LinkConfig {
        device_key: Some(device_key.to_openssh().unwrap()),
        nodes: vec![NodeLink::from(enrolled)],
        ..LinkConfig::default()
    };
    let link = Link::new(config);

    let hello = link
        .hello("tower")
        .await
        .unwrap_or_else(|err| panic!("link.hello failed: {err}\n--- sshd log ---\n{}", sshd.log()));
    assert_eq!(hello["protocol"], json!(PROTOCOL));
    assert_eq!(hello["node"], json!("tower"));

    let tools = link.tools("tower", None).await.unwrap();
    let tool_names: Vec<&str> = tools["tools"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|t| t["name"].as_str())
        .collect();
    assert!(tool_names.contains(&"meta.whoami"), "{tool_names:?}");
    assert!(tool_names.contains(&"exec.run"), "{tool_names:?}");

    let whoami = link
        .call("tower", "meta.whoami", json!({}), None, |_| {})
        .await
        .unwrap();
    assert_eq!(whoami["principal"], json!("phone"));
    assert_eq!(whoami["role"], json!("operator"));
    assert_eq!(whoami["door"], json!("ssh"));

    let mut streamed_events: Vec<Value> = Vec::new();
    let exec_res = link
        .call(
            "tower",
            "exec.run",
            json!({ "cmd": "printf 'alpha-line\\n'; sleep 0.05; printf 'beta-line\\n'" }),
            None,
            |ev| streamed_events.push(ev),
        )
        .await
        .unwrap();
    assert_eq!(exec_res["exit_code"], json!(0), "{exec_res}");
    assert_eq!(exec_res["stdout"], json!("alpha-line\nbeta-line\n"));
    let streamed_text: String = streamed_events
        .iter()
        .filter(|ev| ev["kind"] == json!("stdout"))
        .filter_map(|ev| ev["data"].as_str())
        .collect();
    assert!(
        streamed_text.contains("alpha-line") && streamed_text.contains("beta-line"),
        "expected streamed stdout events, got {streamed_events:?}"
    );

    let denied = link
        .call(
            "tower",
            "fs.write",
            json!({
                "path": user_home.join("nope.txt").to_string_lossy(),
                "content": "blocked"
            }),
            None,
            |_| {},
        )
        .await
        .unwrap_err();
    assert_eq!(denied.code, ErrorCode::Denied, "{denied}");

    // Audit records from the paired session carry door="ssh" and device="link-phone".
    let audit_lines: Vec<Value> = fs::read_to_string(legion_home.join("state").join("audit.jsonl"))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    for tool_name in ["meta.whoami", "exec.run", "fs.write"] {
        let entry = audit_lines
            .iter()
            .rfind(|e| e["tool"] == json!(tool_name))
            .unwrap_or_else(|| panic!("missing audit line for {tool_name}: {audit_lines:?}"));
        assert_eq!(entry["door"], json!("ssh"), "{entry}");
        assert_eq!(entry["device"], json!("link-phone"), "{entry}");
    }

    // 4. Remove the device on the node; the live paired session must be terminated immediately and
    //    subsequent `Link` calls must fail cleanly without hanging.
    let removed = run_legion(
        &legion_bin,
        &legion_home,
        &user_home,
        &["device.remove", "name=link-phone"],
    );
    assert_eq!(removed["removed"], json!(true), "{removed}");
    assert_eq!(removed["kind"], json!("device"), "{removed}");
    assert!(
        removed["sessions_ended"].as_u64().unwrap_or(0) >= 1,
        "expected live legion-link session to be ended on device.remove: {removed}"
    );

    tokio::time::sleep(Duration::from_millis(150)).await;

    let first_after = link
        .call("tower", "meta.whoami", json!({}), None, |_| {})
        .await
        .expect_err("call after device.remove unexpectedly succeeded");
    assert!(
        matches!(first_after.code, ErrorCode::Denied | ErrorCode::Unreachable),
        "unexpected error code after device.remove: {first_after}"
    );

    // Once the revoked session has been invalidated, any new call attempts a fresh SSH connect
    // and fails with Denied / device_removed_or_unknown (with sent=false because auth failed
    // before the request left the client).
    let reconnect_err = if first_after.code == ErrorCode::Denied {
        first_after
    } else {
        link.call("tower", "meta.whoami", json!({}), None, |_| {})
            .await
            .expect_err("reconnecting after device.remove unexpectedly succeeded")
    };
    assert_eq!(reconnect_err.code, ErrorCode::Denied, "{reconnect_err}");
    let details = reconnect_err.details.as_ref().unwrap();
    assert_eq!(details["reason"], json!(REASON_DEVICE_REMOVED_OR_UNKNOWN));
    assert_eq!(details[DETAIL_SENT], json!(false));

    link.close().await;
}

#[tokio::test]
async fn real_sshd_late_enrolment_confirmation_and_retry_recover_cleanly() {
    let Some(legion_bin) = real_legion_bin_with_sshd(
        "pairing::real_sshd_late_enrolment_confirmation_and_retry_recover_cleanly",
    ) else {
        return;
    };

    let root = strict_tempdir();
    let user_home = root.path().join("home");
    let legion_home = root.path().join("legion");
    let config_dir = legion_home.join("config");
    let ssh_dir = user_home.join(".ssh");
    let auth_keys = ssh_dir.join("authorized_keys");
    fs::create_dir_all(&config_dir).unwrap();
    fs::create_dir_all(&ssh_dir).unwrap();
    #[cfg(unix)]
    {
        let _ = fs::set_permissions(&user_home, fs::Permissions::from_mode(0o700));
        let _ = fs::set_permissions(&ssh_dir, fs::Permissions::from_mode(0o700));
    }

    let Some(sshd) = ThrowawaySshd::start(&auth_keys, &user_home) else {
        eprintln!("skipping: throwaway sshd unavailable");
        return;
    };

    let node_toml = format!(
        "name = \"tower\"\n\n[devices]\nauthorized_keys = {}\nhost_keys = [{}]\n",
        toml_literal(&auth_keys),
        toml_literal(&sshd.host_key_pub),
    );
    fs::write(config_dir.join("node.toml"), node_toml).unwrap();
    fs::write(
        config_dir.join("policy.toml"),
        "default = \"owner\"\n\n\
         [roles.operator]\n\
         allow = [\"effect:read\", \"exec.run\"]\n\
         deny = [\"audit.*\", \"fs.write\"]\n",
    )
    .unwrap();

    let port_arg = format!("port={}", sshd.port);
    let user_arg = format!("user={}", sshd.user);

    // Scenario A: Client gives up waiting for confirmation while the node is still blocked before
    // committing (`config.lock` held). Once `enrol` times out after sending the request (verified
    // via an explicit server-side hand-over marker), releasing `config.lock` lets the node finish
    // committing, and retrying the same invite recovers cleanly via the device-key probe.
    let invite_a_res = run_legion(
        &legion_bin,
        &legion_home,
        &user_home,
        &[
            "device.invite",
            "role=operator",
            "name=late-phone",
            "host=127.0.0.1",
            &port_arg,
            &user_arg,
        ],
    );
    let invite_a_text = invite_a_res["invite"].as_str().unwrap().to_owned();
    let invite_a_id = invite_a_res["id"].as_str().unwrap();
    let late_handover =
        EnrolHandover::install(root.path(), &auth_keys, &legion_bin, invite_a_id, "late-a");
    let config_lock = legion_home.join("state").join("sync").join("config.lock");
    let config_holder = HeldLock::acquire(&config_lock);

    let key_a = DeviceKey::generate().unwrap();
    let short_opts = legion_link::LinkOptions {
        ssh_timeouts: SshTimeouts {
            enrol: Duration::from_secs(1),
            ..SshTimeouts::default()
        },
        ..legion_link::LinkOptions::default()
    };
    let link_a = Link::with_options(
        LinkConfig {
            device_key: Some(key_a.to_openssh().unwrap()),
            ..LinkConfig::default()
        },
        short_opts,
    );

    let first_task = {
        let link = link_a.clone();
        let inv = invite_a_text.clone();
        tokio::spawn(async move { link.pair(&inv, "late-phone").await })
    };
    late_handover
        .wait_received(
            1,
            "late-enrolment test: server received client enrolment request while config.lock held",
            &sshd,
            Some(&first_task),
        )
        .await;

    let first_attempt = first_task.await.unwrap();
    let err = first_attempt.expect_err("pair while config.lock is held must hit enrol timeout");
    assert_eq!(
        err.code,
        ErrorCode::Timeout,
        "unexpected first error: {err}"
    );
    assert!(
        link_a.config().node("tower").is_none(),
        "timed-out pair must not save half-paired node locally"
    );

    // Release `config.lock` so the in-flight server enrolment finishes committing authorized_keys.
    drop(config_holder);
    wait_file_contains(
        &auth_keys,
        "--device late-phone",
        "late-enrolment test: node committing late-phone to authorized_keys after config.lock release",
        &sshd,
    )
    .await;

    // Retry with the exact same invite: temp key is already gone, so recovery probes
    // `device_key` + `meta.whoami` and completes pairing cleanly.
    let enrolled_a = link_a
        .pair(&invite_a_text, "late-phone")
        .await
        .unwrap_or_else(|e| {
            panic!(
                "retry after late enrolment failed: {e}\n--- sshd log ---\n{}",
                sshd.log()
            )
        });
    assert_eq!(enrolled_a.node, "tower");
    assert_eq!(enrolled_a.principal, "device:late-phone");
    assert_eq!(enrolled_a.role, "operator");
    let whoami_a = link_a
        .call("tower", "meta.whoami", json!({}), None, |_| {})
        .await
        .unwrap();
    assert_eq!(whoami_a["principal"], json!("device:late-phone"));
    assert_eq!(whoami_a["role"], json!("operator"));
    link_a.close().await;

    // Scenario B: Deterministic half-paired retry — the node has already consumed the single-use
    // invite and installed `key_b`, while the client only persisted `key_b` (no node entry yet).
    let invite_b_res = run_legion(
        &legion_bin,
        &legion_home,
        &user_home,
        &[
            "device.invite",
            "role=operator",
            "name=retry-phone",
            "host=127.0.0.1",
            &port_arg,
            &user_arg,
        ],
    );
    let invite_b_text = invite_b_res["invite"].as_str().unwrap();
    let invite_b = Invite::parse(invite_b_text).unwrap();

    let key_b = DeviceKey::generate().unwrap();
    let direct = enrol(
        &invite_b,
        &key_b,
        "retry-phone",
        SshTimeouts::default(),
        CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(direct.principal, "device:retry-phone");

    // Client has `key_b` persisted, but no `tower` node entry (simulating a dropped reply).
    let link_b = Link::new(LinkConfig {
        device_key: Some(key_b.to_openssh().unwrap()),
        ..LinkConfig::default()
    });
    let recovered_b = link_b
        .pair(invite_b_text, "retry-phone")
        .await
        .unwrap_or_else(|e| {
            panic!(
                "half-paired retry failed: {e}\n--- sshd log ---\n{}",
                sshd.log()
            )
        });
    assert_eq!(recovered_b.node, "tower");
    assert_eq!(recovered_b.principal, "device:retry-phone");
    assert_eq!(recovered_b.role, "operator");
    assert!(link_b.config().node("tower").is_some());

    // Now that `tower` IS paired under `device:retry-phone`, replaying the spent invite on `link_b`
    // must be rejected (never silently accepted a second time).
    let replay_same = link_b.pair(invite_b_text, "retry-phone").await.unwrap_err();
    assert_eq!(replay_same.code, ErrorCode::Denied, "{replay_same}");
    assert_eq!(
        replay_same.details.as_ref().unwrap()["reason"],
        json!(REASON_AUTH_FAILED)
    );

    // And a different device key trying the consumed invite must still be rejected.
    let stranger_key = DeviceKey::generate().unwrap();
    let replay_stranger = enrol(
        &invite_b,
        &stranger_key,
        "stranger",
        SshTimeouts::default(),
        CancellationToken::new(),
    )
    .await
    .unwrap_err();
    assert_eq!(replay_stranger.code, ErrorCode::Denied, "{replay_stranger}");
    assert_eq!(
        replay_stranger.details.as_ref().unwrap()["reason"],
        json!(REASON_AUTH_FAILED)
    );

    let whoami_b = link_b
        .call("tower", "meta.whoami", json!({}), None, |_| {})
        .await
        .unwrap();
    assert_eq!(whoami_b["principal"], json!("device:retry-phone"));
    link_b.close().await;
}

#[tokio::test]
async fn real_sshd_refused_single_use_key_recovers_only_for_device_principals() {
    let Some(legion_bin) = real_legion_bin_with_sshd(
        "pairing::real_sshd_refused_single_use_key_recovers_only_for_device_principals",
    ) else {
        return;
    };

    let root = strict_tempdir();
    let user_home = root.path().join("home");
    let legion_home = root.path().join("legion");
    let config_dir = legion_home.join("config");
    let ssh_dir = user_home.join(".ssh");
    let auth_keys = ssh_dir.join("authorized_keys");
    fs::create_dir_all(&config_dir).unwrap();
    fs::create_dir_all(&ssh_dir).unwrap();
    #[cfg(unix)]
    {
        let _ = fs::set_permissions(&user_home, fs::Permissions::from_mode(0o700));
        let _ = fs::set_permissions(&ssh_dir, fs::Permissions::from_mode(0o700));
    }

    let Some(sshd) = ThrowawaySshd::start(&auth_keys, &user_home) else {
        eprintln!("skipping: throwaway sshd unavailable");
        return;
    };

    let node_toml = format!(
        "name = \"tower\"\n\n[devices]\nauthorized_keys = {}\nhost_keys = [{}]\n",
        toml_literal(&auth_keys),
        toml_literal(&sshd.host_key_pub),
    );
    fs::write(config_dir.join("node.toml"), node_toml).unwrap();
    fs::write(
        config_dir.join("policy.toml"),
        "default = \"owner\"\n\n\
         [roles.operator]\n\
         allow = [\"effect:read\"]\n\n\
         [principals.me]\n\
         role = \"operator\"\n",
    )
    .unwrap();

    let port_arg = format!("port={}", sshd.port);
    let user_arg = format!("user={}", sshd.user);

    // 1. Unique `device:` principal: a half-paired device retrying its own consumed invite
    //    recovers via its installed key.
    let dev_invite_res = run_legion(
        &legion_bin,
        &legion_home,
        &user_home,
        &[
            "device.invite",
            "role=operator",
            "name=unique-phone",
            "host=127.0.0.1",
            &port_arg,
            &user_arg,
        ],
    );
    let dev_invite_text = dev_invite_res["invite"].as_str().unwrap();
    let dev_invite = Invite::parse(dev_invite_text).unwrap();
    assert_eq!(dev_invite.principal, "device:unique-phone");

    let dev_key = DeviceKey::generate().unwrap();
    enrol(
        &dev_invite,
        &dev_key,
        "unique-phone",
        SshTimeouts::default(),
        CancellationToken::new(),
    )
    .await
    .unwrap();

    let dev_link = Link::new(LinkConfig {
        device_key: Some(dev_key.to_openssh().unwrap()),
        ..LinkConfig::default()
    });
    let recovered = dev_link
        .pair(dev_invite_text, "unique-phone")
        .await
        .unwrap_or_else(|e| {
            panic!(
                "device: principal recovery failed: {e}\n--- sshd log ---\n{}",
                sshd.log()
            )
        });
    assert_eq!(recovered.principal, "device:unique-phone");
    assert!(dev_link.config().node("tower").is_some());
    dev_link.close().await;

    // 2. Shared principal (`me`): Y already holds `me` on the machine (no local entry yet),
    //    another device X consumes invite #2 for `me`, and Y calling `pair(invite2)` MUST get
    //    a plain `Denied` / `auth_failed` refusal instead of masking X's use of invite #2.
    let shared_inv1_res = run_legion(
        &legion_bin,
        &legion_home,
        &user_home,
        &[
            "device.invite",
            "principal=me",
            "host=127.0.0.1",
            &port_arg,
            &user_arg,
        ],
    );
    let shared_inv1 = Invite::parse(shared_inv1_res["invite"].as_str().unwrap()).unwrap();
    let key_y = DeviceKey::generate().unwrap();
    enrol(
        &shared_inv1,
        &key_y,
        "shared-y",
        SshTimeouts::default(),
        CancellationToken::new(),
    )
    .await
    .unwrap();

    let shared_inv2_res = run_legion(
        &legion_bin,
        &legion_home,
        &user_home,
        &[
            "device.invite",
            "principal=me",
            "host=127.0.0.1",
            &port_arg,
            &user_arg,
        ],
    );
    let shared_inv2_text = shared_inv2_res["invite"].as_str().unwrap();
    let shared_inv2 = Invite::parse(shared_inv2_text).unwrap();
    let key_x = DeviceKey::generate().unwrap();
    enrol(
        &shared_inv2,
        &key_x,
        "shared-x",
        SshTimeouts::default(),
        CancellationToken::new(),
    )
    .await
    .unwrap();

    let link_y = Link::new(LinkConfig {
        device_key: Some(key_y.to_openssh().unwrap()),
        ..LinkConfig::default()
    });
    let err = link_y.pair(shared_inv2_text, "shared-y").await.unwrap_err();
    assert_eq!(err.code, ErrorCode::Denied, "{err}");
    assert_eq!(
        err.details.as_ref().unwrap()["reason"],
        json!(REASON_AUTH_FAILED)
    );
    assert!(
        link_y.config().node("tower").is_none(),
        "shared-principal refused key must never save a local node entry"
    );
    link_y.close().await;
}

#[tokio::test]
async fn real_sshd_cancel_before_and_after_send_behaves_honestly() {
    let Some(legion_bin) = real_legion_bin_with_sshd(
        "pairing::real_sshd_cancel_before_and_after_send_behaves_honestly",
    ) else {
        return;
    };

    let root = strict_tempdir();
    let user_home = root.path().join("home");
    let legion_home = root.path().join("legion");
    let config_dir = legion_home.join("config");
    let ssh_dir = user_home.join(".ssh");
    let auth_keys = ssh_dir.join("authorized_keys");
    fs::create_dir_all(&config_dir).unwrap();
    fs::create_dir_all(&ssh_dir).unwrap();
    #[cfg(unix)]
    {
        let _ = fs::set_permissions(&user_home, fs::Permissions::from_mode(0o700));
        let _ = fs::set_permissions(&ssh_dir, fs::Permissions::from_mode(0o700));
    }

    let Some(sshd) = ThrowawaySshd::start(&auth_keys, &user_home) else {
        eprintln!("skipping: throwaway sshd unavailable");
        return;
    };

    let node_toml = format!(
        "name = \"tower\"\n\n[devices]\nauthorized_keys = {}\nhost_keys = [{}]\n",
        toml_literal(&auth_keys),
        toml_literal(&sshd.host_key_pub),
    );
    fs::write(config_dir.join("node.toml"), node_toml).unwrap();
    fs::write(
        config_dir.join("policy.toml"),
        "default = \"owner\"\n\n[roles.operator]\nallow = [\"effect:read\"]\n",
    )
    .unwrap();

    let port_arg = format!("port={}", sshd.port);
    let user_arg = format!("user={}", sshd.user);

    let inv_res = run_legion(
        &legion_bin,
        &legion_home,
        &user_home,
        &[
            "device.invite",
            "role=operator",
            "name=cancel-phone",
            "host=127.0.0.1",
            &port_arg,
            &user_arg,
        ],
    );
    let inv_text = inv_res["invite"].as_str().unwrap();

    let key = DeviceKey::generate().unwrap();
    let link = Link::new(LinkConfig {
        device_key: Some(key.to_openssh().unwrap()),
        ..LinkConfig::default()
    });

    // 1. Cancel BEFORE sending: nothing happens on machine or device, `sent: false`, invite still usable.
    let pre_cancel = CancellationToken::new();
    pre_cancel.cancel();
    let pre_err = link
        .pair_cancellable(inv_text, "cancel-phone", pre_cancel)
        .await
        .unwrap_err();
    assert_eq!(pre_err.code, ErrorCode::Cancelled);
    assert_eq!(pre_err.details.as_ref().unwrap()[DETAIL_SENT], json!(false));
    assert!(link.config().node("tower").is_none());
    assert!(
        !fs::read_to_string(&auth_keys)
            .unwrap()
            .contains("--device cancel-phone")
    );

    // 2. Cancel RIGHT AFTER the machine commits `authorized_keys` and BEFORE it writes the reply:
    //    pause `push_devices_to_fleet` on an explicit marker hand-over so cancel deterministically
    //    fires after commit and before reply. Pairing MUST succeed and save `tower` instead of
    //    leaving the device half-paired.
    let entered = root.path().join("post-cancel-entered");
    let release = root.path().join("post-cancel-release");
    let fleet_path = config_dir.join("fleet.toml");
    fs::write(
        &fleet_path,
        format!(
            "[machines.slowpeer]\ncommand = [\"/bin/sh\", \"-c\", \"touch '{}'; i=0; while [ ! -f '{}' ] && [ $i -lt 600 ]; do sleep 0.05; i=$((i+1)); done; exit 1\"]\n",
            entered.display(),
            release.display(),
        ),
    )
    .unwrap();

    let post_cancel = CancellationToken::new();
    let pair_task = {
        let link = link.clone();
        let inv = inv_text.to_owned();
        let cancel = post_cancel.clone();
        tokio::spawn(async move { link.pair_cancellable(&inv, "cancel-phone", cancel).await })
    };
    wait_path_exists(
        &entered,
        "cancel-after-send test: server reached fleet push after committing authorized_keys",
        &sshd,
        Some(&pair_task),
    )
    .await;
    assert!(
        fs::read_to_string(&auth_keys)
            .unwrap_or_default()
            .contains("--device cancel-phone"),
        "expected authorized_keys to contain cancel-phone before post-commit cancel"
    );
    post_cancel.cancel();
    let _ = fs::write(&release, b"go\n");

    let enrolled = pair_task.await.unwrap().unwrap_or_else(|e| {
        panic!(
            "cancel after send must save completed pairing instead of failing: {e}\n--- sshd log ---\n{}",
            sshd.log()
        )
    });
    let _ = fs::remove_file(&fleet_path);
    assert_eq!(enrolled.node, "tower");
    assert_eq!(enrolled.principal, "device:cancel-phone");
    assert!(
        link.config().node("tower").is_some(),
        "completed pairing must be saved in LinkConfig even when cancel fired late"
    );
    let whoami = link
        .call("tower", "meta.whoami", json!({}), None, |_| {})
        .await
        .unwrap();
    assert_eq!(whoami["principal"], json!("device:cancel-phone"));
    link.close().await;
}

#[tokio::test]
async fn real_sshd_unconfirmed_lifecycle_confirmed_conflict_restore_and_compat() {
    let Some(legion_bin) = real_legion_bin_with_sshd(
        "pairing::real_sshd_unconfirmed_lifecycle_confirmed_conflict_restore_and_compat",
    ) else {
        return;
    };

    let root = strict_tempdir();
    let user_home = root.path().join("home");
    let legion_home = root.path().join("legion");
    let config_dir = legion_home.join("config");
    let ssh_dir = user_home.join(".ssh");
    let auth_keys = ssh_dir.join("authorized_keys");
    fs::create_dir_all(&config_dir).unwrap();
    fs::create_dir_all(&ssh_dir).unwrap();
    #[cfg(unix)]
    {
        let _ = fs::set_permissions(&user_home, fs::Permissions::from_mode(0o700));
        let _ = fs::set_permissions(&ssh_dir, fs::Permissions::from_mode(0o700));
    }

    let Some(sshd) = ThrowawaySshd::start(&auth_keys, &user_home) else {
        eprintln!("skipping: throwaway sshd unavailable");
        return;
    };

    let node_toml = format!(
        "name = \"tower\"\n\n[devices]\nauthorized_keys = {}\nhost_keys = [{}]\n",
        toml_literal(&auth_keys),
        toml_literal(&sshd.host_key_pub),
    );
    fs::write(config_dir.join("node.toml"), node_toml).unwrap();
    fs::write(
        config_dir.join("policy.toml"),
        "default = \"owner\"\n\n[roles.operator]\nallow = [\"effect:read\"]\n",
    )
    .unwrap();

    let port_arg = format!("port={}", sshd.port);
    let user_arg = format!("user={}", sshd.user);
    let fleet_path = config_dir.join("fleet.toml");
    let audit_path = legion_home.join("state").join("audit.jsonl");

    // Helper: enrols a device key with `invite` over real `sshd`, waits until the machine enters
    // `push_devices_to_fleet` (after committing policy/devices/authorized_keys, before replying),
    // then abruptly drops the SSH connection so the client vanishes before receiving the reply.
    let vanish_after_send = |invite: &Invite,
                             device_key: &DeviceKey,
                             device_name: &str,
                             tag: &str| {
        let entered = root.path().join(format!("entered-{tag}"));
        let release = root.path().join(format!("release-{tag}"));
        let fleet_toml = format!(
            "[machines.slowpeer]\ncommand = [\"/bin/sh\", \"-c\", \"touch '{}'; i=0; while [ ! -f '{}' ] && [ $i -lt 600 ]; do sleep 0.05; i=$((i+1)); done; exit 1\"]\n",
            entered.display(),
            release.display(),
        );
        fs::write(&fleet_path, fleet_toml).unwrap();
        let enrol_audit_before = fs::read_to_string(&audit_path)
            .unwrap_or_default()
            .lines()
            .filter(|line| line.contains("\"tool\":\"device.enrol\""))
            .count();

        let invite = invite.clone();
        let pub_key = device_key.public_key_line(device_name).unwrap();
        let device_name = device_name.to_owned();
        let audit_path = audit_path.clone();
        let fleet_path = fleet_path.clone();
        let sshd_ref = &sshd;
        async move {
            let temp_key = DeviceKey::from_seed(*invite.key_seed().unwrap());
            let pipe = open_exec_raw(
                &invite.target(),
                &temp_key,
                "legion enrol",
                SshTimeouts::default(),
                CancellationToken::new(),
            )
            .await
            .unwrap();
            let (reader, mut writer, handle) = pipe.into_parts();
            let req = format!(
                "{}\n",
                json!({
                    "enrol": 1,
                    "id": invite.id,
                    "public_key": pub_key,
                    "name": device_name,
                })
            );
            writer.write_all(req.as_bytes()).await.unwrap();
            writer.flush().await.unwrap();
            writer.shutdown().await.unwrap();

            // Wait deterministically until commit finished and `push_devices_to_fleet` began.
            wait_path_exists(
                &entered,
                "vanish_after_send: enrol reached fleet push after commit",
                sshd_ref,
                None::<&tokio::task::JoinHandle<()>>,
            )
            .await;

            // Client vanishes while machine is still finishing before writing its stdout reply.
            let disconnects_before = sshd_ref.log().matches("Received disconnect from").count();
            drop(writer);
            drop(reader);
            Box::new(handle).close(Duration::from_millis(500)).await;
            wait_sshd_log_count(
                sshd_ref,
                "Received disconnect from",
                disconnects_before + 1,
                "vanish_after_send: sshd observed client disconnect before releasing fleet push",
            )
            .await;
            let _ = fs::write(&release, b"go\n");

            // Wait for `legion gate --enrol` to finish and append its own `tool: "device.enrol"` audit entry.
            let audit_deadline = Instant::now() + HANG_GUARD;
            loop {
                let enrol_count = fs::read_to_string(&audit_path)
                    .unwrap_or_default()
                    .lines()
                    .filter(|line| line.contains("\"tool\":\"device.enrol\""))
                    .count();
                if enrol_count > enrol_audit_before {
                    break;
                }
                assert!(
                    Instant::now() < audit_deadline,
                    "timed out after {}s at step [vanish_after_send: waiting for device.enrol audit entry]\n--- sshd log ---\n{}",
                    HANG_GUARD.as_secs(),
                    sshd_ref.log()
                );
                tokio::time::sleep(Duration::from_millis(15)).await;
            }
            let _ = fs::remove_file(&fleet_path);
        }
    };

    // 1. Client vanishes after sending -> recorded as `unconfirmed = true` in devices.toml,
    //    `device.list`, and `audit.jsonl`.
    let inv1_res = run_legion(
        &legion_bin,
        &legion_home,
        &user_home,
        &[
            "device.invite",
            "role=operator",
            "name=vanish-a",
            "host=127.0.0.1",
            &port_arg,
            &user_arg,
        ],
    );
    let inv1 = Invite::parse(inv1_res["invite"].as_str().unwrap()).unwrap();
    let key_a = DeviceKey::generate().unwrap();
    vanish_after_send(&inv1, &key_a, "vanish-a", "a").await;

    let devices_toml_1 = fs::read_to_string(config_dir.join("devices.toml")).unwrap();
    assert!(
        !devices_toml_1.contains("unconfirmed"),
        "devices.toml must NEVER carry an unconfirmed field:\n{devices_toml_1}"
    );
    let list_1 = run_legion(&legion_bin, &legion_home, &user_home, &["device.list"]);
    let devs_1 = list_1["devices"].as_array().unwrap();
    assert_eq!(devs_1.len(), 1);
    assert_eq!(devs_1[0]["unconfirmed"], json!(true));
    assert_eq!(devs_1[0]["status"], json!("Pairing not finished"));

    let audit_1 = fs::read_to_string(&audit_path).unwrap();
    assert!(
        audit_1.contains("\"reply_delivered\":false") && audit_1.contains("\"unconfirmed\":true"),
        "audit.jsonl must record undelivered/unconfirmed enrolment:\n{audit_1}"
    );

    // Older-client compatibility: strict struct without `unconfirmed` deserializes `device.list`.
    #[derive(serde::Deserialize)]
    struct LegacyListEntry {
        name: String,
        principal: String,
    }
    let legacy_entry: LegacyListEntry = serde_json::from_value(devs_1[0].clone()).unwrap();
    assert_eq!(legacy_entry.name, "vanish-a");
    assert_eq!(legacy_entry.principal, "device:vanish-a");

    // 2. Fresh invite presented by the SAME device key (with proof of possession) replaces the
    //    unconfirmed leftover and succeeds cleanly over real `sshd`. The replacement starts
    //    locally unconfirmed until its first real authenticated `legion gate` session.
    let inv2_res = run_legion(
        &legion_bin,
        &legion_home,
        &user_home,
        &[
            "device.invite",
            "role=operator",
            "name=vanish-a-fresh",
            "host=127.0.0.1",
            &port_arg,
            &user_arg,
        ],
    );
    let inv2_text = inv2_res["invite"].as_str().unwrap();
    let link_a = Link::new(LinkConfig {
        device_key: Some(key_a.to_openssh().unwrap()),
        ..LinkConfig::default()
    });
    let enrolled_a = link_a
        .pair(inv2_text, "vanish-a-fresh")
        .await
        .unwrap_or_else(|e| {
            panic!(
                "fresh invite must replace unconfirmed leftover: {e}\n--- sshd log ---\n{}",
                sshd.log()
            )
        });
    assert_eq!(enrolled_a.principal, "device:vanish-a-fresh");

    let list_2_before = run_legion(&legion_bin, &legion_home, &user_home, &["device.list"]);
    let devs_2_before = list_2_before["devices"].as_array().unwrap();
    assert_eq!(devs_2_before.len(), 1);
    assert_eq!(
        devs_2_before[0]["principal"],
        json!("device:vanish-a-fresh")
    );
    assert_eq!(devs_2_before[0]["unconfirmed"], json!(true));

    // Opening its first real session confirms `vanish-a-fresh`.
    let _ = link_a
        .call("tower", "meta.whoami", json!({}), None, |_| {})
        .await
        .unwrap();
    link_a.close().await;

    let list_2 = run_legion(&legion_bin, &legion_home, &user_home, &["device.list"]);
    let devs_2 = list_2["devices"].as_array().unwrap();
    assert_eq!(devs_2.len(), 1);
    assert_eq!(devs_2[0]["principal"], json!("device:vanish-a-fresh"));
    assert!(devs_2[0].get("unconfirmed").is_none());

    // 3. First session from an unconfirmed device marks it confirmed on the machine.
    let inv3_res = run_legion(
        &legion_bin,
        &legion_home,
        &user_home,
        &[
            "device.invite",
            "role=operator",
            "name=session-b",
            "host=127.0.0.1",
            &port_arg,
            &user_arg,
        ],
    );
    let inv3_text = inv3_res["invite"].as_str().unwrap();
    let inv3 = Invite::parse(inv3_text).unwrap();
    let key_b = DeviceKey::generate().unwrap();
    vanish_after_send(&inv3, &key_b, "session-b", "b").await;

    let list_3_before = run_legion(&legion_bin, &legion_home, &user_home, &["device.list"]);
    let dev_b_before = list_3_before["devices"]
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["principal"] == json!("device:session-b"))
        .unwrap();
    assert_eq!(dev_b_before["unconfirmed"], json!(true));

    // Device B opens its first RPC session (`meta.whoami`) using its key.
    let link_b = Link::new(LinkConfig {
        device_key: Some(key_b.to_openssh().unwrap()),
        nodes: vec![NodeLink::ssh(
            "tower",
            inv3.target(),
            Some("device:session-b".to_owned()),
        )],
        ..LinkConfig::default()
    });
    let whoami_b = link_b
        .call("tower", "meta.whoami", json!({}), None, |_| {})
        .await
        .unwrap();
    assert_eq!(whoami_b["principal"], json!("device:session-b"));
    link_b.close().await;

    let list_3_after = run_legion(&legion_bin, &legion_home, &user_home, &["device.list"]);
    let dev_b_after = list_3_after["devices"]
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["principal"] == json!("device:session-b"))
        .unwrap();
    assert!(
        dev_b_after.get("unconfirmed").is_none(),
        "first session open must clear unconfirmed flag: {dev_b_after}"
    );
    let devices_toml_after = fs::read_to_string(config_dir.join("devices.toml")).unwrap();
    assert!(
        !devices_toml_after.contains("unconfirmed = true"),
        "devices.toml must clear unconfirmed after session:\n{devices_toml_after}"
    );

    // 4. Confirmed device presenting a second fresh invite is refused with `ErrorCode::Conflict`
    //    and `details.existing_principal`, and restores via the pinned-key probe.
    let inv4_res = run_legion(
        &legion_bin,
        &legion_home,
        &user_home,
        &[
            "device.invite",
            "role=operator",
            "name=session-b-dup",
            "host=127.0.0.1",
            &port_arg,
            &user_arg,
        ],
    );
    let inv4_text = inv4_res["invite"].as_str().unwrap();
    let link_b_fresh = Link::new(LinkConfig {
        device_key: Some(key_b.to_openssh().unwrap()),
        ..LinkConfig::default()
    });
    let conflict_err = link_b_fresh
        .pair(inv4_text, "session-b-dup")
        .await
        .unwrap_err();
    assert_eq!(conflict_err.code, ErrorCode::Conflict, "{conflict_err}");
    let details = conflict_err
        .details
        .as_ref()
        .expect("confirmed conflict must include details");
    assert_eq!(details["existing_principal"], json!("device:session-b"));
    assert_eq!(details["existing_device"], json!("session-b"));
    assert_eq!(details["node"], json!("tower"));
    assert!(link_b_fresh.config().node("tower").is_none());

    // Restore via pinned-key probe using the reported `existing_principal`.
    let restored = link_b_fresh
        .restore_pairing(
            inv4_text,
            details["existing_principal"].as_str().unwrap(),
            "session-b",
            CancellationToken::new(),
        )
        .await
        .unwrap_or_else(|e| panic!("restore_pairing failed: {e}"));
    assert_eq!(restored.node, "tower");
    assert_eq!(restored.principal, "device:session-b");
    assert!(restored.recovered);
    assert!(link_b_fresh.config().node("tower").is_some());
    let whoami_restored = link_b_fresh
        .call("tower", "meta.whoami", json!({}), None, |_| {})
        .await
        .unwrap();
    assert_eq!(whoami_restored["principal"], json!("device:session-b"));
    link_b_fresh.close().await;

    // 5. New-device vs older-machine compatibility: an older machine returning `Conflict`
    //    WITHOUT `details` decodes cleanly into `ErrorBody` with `details: None`.
    let older_machine_reply: legion_link::proto::ErrorBody = serde_json::from_value(json!({
        "code": "conflict",
        "message": "That public key is already present in authorized_keys.",
        "hint": "Generate a fresh key pair."
    }))
    .unwrap();
    assert_eq!(older_machine_reply.code, ErrorCode::Conflict);
    assert!(older_machine_reply.details.is_none());
}

/// Holds an exclusive kernel lock (`flock` on Unix, `LockFileEx` on Windows) on `lock_path` until
/// dropped.
///
/// Acquires the lock directly via `File::lock()` (the same kernel primitive `legion_os::FileLock`
/// uses), so ownership is guaranteed before returning with no child process, no external `flock`
/// tool, no polling, and automatic release on drop or panic.
struct HeldLock {
    _file: File,
}

impl HeldLock {
    fn acquire(lock_path: &Path) -> Self {
        if let Some(parent) = lock_path.parent() {
            fs::create_dir_all(parent).unwrap();
            #[cfg(unix)]
            {
                let _ = fs::set_permissions(parent, fs::Permissions::from_mode(0o700));
            }
        }
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(lock_path)
            .unwrap_or_else(|err| {
                panic!("failed to open lock file {}: {err}", lock_path.display())
            });
        #[cfg(unix)]
        {
            let _ = fs::set_permissions(lock_path, fs::Permissions::from_mode(0o600));
        }
        file.lock()
            .unwrap_or_else(|err| panic!("failed to acquire lock {}: {err}", lock_path.display()));
        Self { _file: file }
    }
}

fn quote_posix_token(token: &str) -> String {
    let plain_safe = token.bytes().all(|b| {
        b.is_ascii_alphanumeric() || matches!(b, b'/' | b'.' | b'-' | b'_' | b'+' | b':' | b'@')
    });
    if plain_safe {
        token.to_owned()
    } else {
        format!("'{}'", token.replace('\'', "'\\''"))
    }
}

fn sh_single_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', "'\\''"))
}

/// Wraps a single invite's forced command executable in `authorized_keys` with a tiny `0o700`
/// helper script that reads the client's JSON enrolment line, atomically creates a marker file
/// under `marker_dir`, and `exec`s the real `legion` binary with stdin redirected from that file.
///
/// Because `authorized_keys` retains the exact forced-command grammar (only the executable path
/// changes), `legion` still parses and consumes the invite line as `LineKind::Invite`, while tests
/// get an explicit hand-over proving the client sent its request and the server started enrolling.
struct EnrolHandover {
    marker_dir: PathBuf,
}

impl EnrolHandover {
    fn install(
        root: &Path,
        auth_keys: &Path,
        legion_bin: &Path,
        invite_id: &str,
        tag: &str,
    ) -> Self {
        let marker_dir = root.join(format!("enrol-handover-{tag}"));
        fs::create_dir_all(&marker_dir).unwrap();
        #[cfg(unix)]
        {
            let _ = fs::set_permissions(&marker_dir, fs::Permissions::from_mode(0o700));
        }
        // Basename must be `legion` so `authkeys` continues to parse the wrapped line as `LineKind::Invite`.
        let wrapper_path = marker_dir.join("legion");
        let script = format!(
            "#!/bin/sh\n\
             set -eu\n\
             IFS= read -r line\n\
             tmp={dir}/req-\"$$\".tmp\n\
             ready={dir}/req-\"$$\".ready\n\
             printf '%s\\n' \"$line\" > \"$tmp\"\n\
             mv \"$tmp\" \"$ready\"\n\
             exec {bin} \"$@\" < \"$ready\"\n",
            dir = sh_single_quote(&marker_dir.to_string_lossy()),
            bin = sh_single_quote(&legion_bin.to_string_lossy()),
        );
        fs::write(&wrapper_path, script).unwrap();
        #[cfg(unix)]
        {
            let _ = fs::set_permissions(&wrapper_path, fs::Permissions::from_mode(0o700));
        }

        let old_prefix = format!(
            "command=\"{} gate --enrol ",
            quote_posix_token(&legion_bin.to_string_lossy())
        );
        let new_prefix = format!(
            "command=\"{} gate --enrol ",
            quote_posix_token(&wrapper_path.to_string_lossy())
        );
        let invite_tag = format!("legion-invite:{invite_id}");
        let raw = fs::read_to_string(auth_keys).unwrap();
        let mut replaced = false;
        let updated: Vec<String> = raw
            .lines()
            .map(|line| {
                if line.contains(&invite_tag) && line.contains(&old_prefix) {
                    replaced = true;
                    line.replacen(&old_prefix, &new_prefix, 1)
                } else {
                    line.to_owned()
                }
            })
            .collect();
        assert!(
            replaced,
            "could not wrap forced command for {invite_tag} (expected {old_prefix:?}) in:\n{raw}"
        );
        fs::write(auth_keys, format!("{}\n", updated.join("\n"))).unwrap();
        #[cfg(unix)]
        {
            let _ = fs::set_permissions(auth_keys, fs::Permissions::from_mode(0o600));
        }
        Self { marker_dir }
    }

    fn ready_count(&self) -> usize {
        fs::read_dir(&self.marker_dir)
            .ok()
            .into_iter()
            .flatten()
            .filter_map(Result::ok)
            .filter(|entry| entry.path().extension().and_then(|ext| ext.to_str()) == Some("ready"))
            .count()
    }

    async fn wait_received<T>(
        &self,
        expected: usize,
        step: &str,
        sshd: &ThrowawaySshd,
        in_flight: Option<&tokio::task::JoinHandle<T>>,
    ) {
        let deadline = Instant::now() + HANG_GUARD;
        loop {
            let count = self.ready_count();
            if count >= expected {
                return;
            }
            if let Some(handle) = in_flight {
                assert!(
                    !handle.is_finished(),
                    "in-flight task exited before step [{step}] (received {count}/{expected} markers)\n--- sshd log ---\n{}",
                    sshd.log()
                );
            }
            assert!(
                Instant::now() < deadline,
                "timed out after {}s at step [{step}] (received {count}/{expected} markers in {})\n--- sshd log ---\n{}",
                HANG_GUARD.as_secs(),
                self.marker_dir.display(),
                sshd.log()
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
}

async fn wait_path_exists<T>(
    path: &Path,
    step: &str,
    sshd: &ThrowawaySshd,
    in_flight: Option<&tokio::task::JoinHandle<T>>,
) {
    let deadline = Instant::now() + HANG_GUARD;
    while !path.exists() {
        if let Some(handle) = in_flight {
            assert!(
                !handle.is_finished(),
                "in-flight task exited before step [{step}] (missing {})\n--- sshd log ---\n{}",
                path.display(),
                sshd.log()
            );
        }
        assert!(
            Instant::now() < deadline,
            "timed out after {}s at step [{step}] (missing {})\n--- sshd log ---\n{}",
            HANG_GUARD.as_secs(),
            path.display(),
            sshd.log()
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

async fn wait_file_contains(path: &Path, needle: &str, step: &str, sshd: &ThrowawaySshd) {
    let deadline = Instant::now() + HANG_GUARD;
    loop {
        if fs::read_to_string(path)
            .unwrap_or_default()
            .contains(needle)
        {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "timed out after {}s at step [{step}] ({} never contained {needle:?})\n--- sshd log ---\n{}",
            HANG_GUARD.as_secs(),
            path.display(),
            sshd.log()
        );
        tokio::time::sleep(Duration::from_millis(15)).await;
    }
}

async fn wait_sshd_log_count(sshd: &ThrowawaySshd, needle: &str, target_count: usize, step: &str) {
    let deadline = Instant::now() + HANG_GUARD;
    loop {
        let log = sshd.log();
        let count = log.matches(needle).count();
        if count >= target_count {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "timed out after {}s at step [{step}] (sshd log had {count}/{target_count} occurrences of {needle:?})\n--- sshd log ---\n{log}",
            HANG_GUARD.as_secs(),
        );
        tokio::time::sleep(Duration::from_millis(15)).await;
    }
}

/// Yields to the Tokio runtime while waiting for another process (such as `legion gate --enrol`)
/// to hold the exclusive kernel lock on `lock_path`.
///
/// Must stay `async` so spawned client tasks on `#[tokio::test]`'s single-threaded runtime continue
/// running while waiting for the server process to reach the lock.
#[cfg(unix)]
async fn wait_lock_held<T>(
    lock_path: &Path,
    step: &str,
    sshd: &ThrowawaySshd,
    holder_task: &tokio::task::JoinHandle<T>,
) {
    let deadline = Instant::now() + HANG_GUARD;
    loop {
        match OpenOptions::new().read(true).write(true).open(lock_path) {
            Ok(file) => match file.try_lock() {
                Err(std::fs::TryLockError::WouldBlock) => return,
                Ok(()) => drop(file),
                Err(std::fs::TryLockError::Error(err)) => {
                    panic!(
                        "step [{step}]: probing lock {} failed: {err}\n--- sshd log ---\n{}",
                        lock_path.display(),
                        sshd.log()
                    );
                }
            },
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => {
                panic!(
                    "step [{step}]: opening lock {} failed: {err}\n--- sshd log ---\n{}",
                    lock_path.display(),
                    sshd.log()
                );
            }
        }
        assert!(
            !holder_task.is_finished(),
            "in-flight task exited before step [{step}] (lock {} was never held)\n--- sshd log ---\n{}",
            lock_path.display(),
            sshd.log()
        );
        assert!(
            Instant::now() < deadline,
            "timed out after {}s at step [{step}] (lock {} never became held)\n--- sshd log ---\n{}",
            HANG_GUARD.as_secs(),
            lock_path.display(),
            sshd.log()
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[cfg(unix)]
#[tokio::test]
async fn real_sshd_shared_principal_post_send_not_found_does_not_recover() {
    let Some(legion_bin) = real_legion_bin_with_sshd(
        "pairing::real_sshd_shared_principal_post_send_not_found_does_not_recover",
    ) else {
        return;
    };

    let root = strict_tempdir();
    let user_home = root.path().join("home");
    let legion_home = root.path().join("legion");
    let config_dir = legion_home.join("config");
    let ssh_dir = user_home.join(".ssh");
    let auth_keys = ssh_dir.join("authorized_keys");
    fs::create_dir_all(&config_dir).unwrap();
    fs::create_dir_all(&ssh_dir).unwrap();
    let _ = fs::set_permissions(&user_home, fs::Permissions::from_mode(0o700));
    let _ = fs::set_permissions(&ssh_dir, fs::Permissions::from_mode(0o700));

    let Some(sshd) = ThrowawaySshd::start(&auth_keys, &user_home) else {
        return;
    };
    fs::write(
        config_dir.join("node.toml"),
        format!(
            "name = \"tower\"\n\n[devices]\nauthorized_keys = {}\nhost_keys = [{}]\n",
            toml_literal(&auth_keys),
            toml_literal(&sshd.host_key_pub),
        ),
    )
    .unwrap();
    fs::write(
        config_dir.join("policy.toml"),
        "default = \"owner\"\n\n\
         [roles.operator]\n\
         allow = [\"effect:read\"]\n\n\
         [principals.me]\n\
         role = \"operator\"\n",
    )
    .unwrap();

    let port_arg = format!("port={}", sshd.port);
    let user_arg = format!("user={}", sshd.user);

    // Key Y is already enrolled under shared principal `me` (and confirmed via a real session).
    let inv1_res = run_legion(
        &legion_bin,
        &legion_home,
        &user_home,
        &[
            "device.invite",
            "principal=me",
            "host=127.0.0.1",
            &port_arg,
            &user_arg,
        ],
    );
    let inv1 = Invite::parse(inv1_res["invite"].as_str().unwrap()).unwrap();
    let key_y = DeviceKey::generate().unwrap();
    let enrolled_y = enrol(
        &inv1,
        &key_y,
        "phone",
        SshTimeouts::default(),
        CancellationToken::new(),
    )
    .await
    .unwrap();
    let confirm_y = Link::new(LinkConfig {
        device_key: Some(key_y.to_openssh().unwrap()),
        nodes: vec![NodeLink::from(enrolled_y)],
        ..LinkConfig::default()
    });
    let _ = confirm_y
        .call("tower", "meta.whoami", json!({}), None, |_| {})
        .await
        .unwrap();
    confirm_y.close().await;

    // Two holders of a fresh shared-principal invite race deterministically via explicit hand-over:
    // 1. Hold `.authorized_keys.legion.lock` so intruder X acquires `config.lock` first and blocks
    //    before editing `authorized_keys` (leaving the invite key accepted by sshd).
    // 2. Wait explicitly for X's request marker AND for X's `legion gate --enrol` to hold
    //    `config.lock` (yielding to Tokio so X's task makes progress).
    // 3. Start victim Y (`link_y.pair`) and wait explicitly for Y's server-side request marker,
    //    proving sshd accepted Y and Y already sent its enrolment line before releasing the lock.
    // 4. Release `.authorized_keys.legion.lock`: X is guaranteed to finish enrolling first and
    //    consume the invite, so Y reliably receives post-send `ErrorCode::NotFound` and MUST NOT
    //    probe/recover (which would hide that X consumed the invite).
    let inv2_res = run_legion(
        &legion_bin,
        &legion_home,
        &user_home,
        &[
            "device.invite",
            "principal=me",
            "host=127.0.0.1",
            &port_arg,
            &user_arg,
        ],
    );
    let inv2_text = inv2_res["invite"].as_str().unwrap().to_owned();
    let inv2_id = inv2_res["id"].as_str().unwrap();
    let inv2 = Invite::parse(&inv2_text).unwrap();
    let handover =
        EnrolHandover::install(root.path(), &auth_keys, &legion_bin, inv2_id, "shared-race");
    let auth_lock = ssh_dir.join(".authorized_keys.legion.lock");
    let config_lock = legion_home.join("state").join("sync").join("config.lock");
    let auth_holder = HeldLock::acquire(&auth_lock);

    let key_x = DeviceKey::generate().unwrap();
    let inv_x = inv2.clone();
    let tx = tokio::spawn(async move {
        enrol(
            &inv_x,
            &key_x,
            "intruder",
            SshTimeouts::default(),
            CancellationToken::new(),
        )
        .await
    });
    handover
        .wait_received(
            1,
            "shared-principal race: server received intruder X enrolment request",
            &sshd,
            Some(&tx),
        )
        .await;
    wait_lock_held(
        &config_lock,
        "shared-principal race: intruder X acquired config.lock before blocking on auth_lock",
        &sshd,
        &tx,
    )
    .await;

    let link_y = Link::new(LinkConfig {
        device_key: Some(key_y.to_openssh().unwrap()),
        ..LinkConfig::default()
    });
    let ly = link_y.clone();
    let inv_y = inv2_text.clone();
    let ty = tokio::spawn(async move { ly.pair(&inv_y, "phone").await });
    handover
        .wait_received(
            2,
            "shared-principal race: server received victim Y enrolment request",
            &sshd,
            Some(&ty),
        )
        .await;
    tokio::task::yield_now().await;

    drop(auth_holder);

    let rx = tx.await.unwrap();
    let ry = ty.await.unwrap();
    assert!(rx.is_ok(), "intruder X should win the invite: {rx:?}");
    let err = ry.expect_err(
        "intruder consumed the shared-principal invite; post-send NotFound must NOT recover",
    );
    assert_eq!(err.code, ErrorCode::NotFound, "{err}");
    assert!(
        link_y.config().node("tower").is_none(),
        "refused shared-principal pairing must not save node"
    );
    link_y.close().await;
}

#[cfg(unix)]
#[tokio::test]
async fn real_sshd_cancel_before_machine_commits_stays_honest_and_retry_picks_up_pairing() {
    let Some(legion_bin) = real_legion_bin_with_sshd(
        "pairing::real_sshd_cancel_before_machine_commits_stays_honest_and_retry_picks_up_pairing",
    ) else {
        return;
    };

    let root = strict_tempdir();
    let user_home = root.path().join("home");
    let legion_home = root.path().join("legion");
    let config_dir = legion_home.join("config");
    let ssh_dir = user_home.join(".ssh");
    let auth_keys = ssh_dir.join("authorized_keys");
    fs::create_dir_all(&config_dir).unwrap();
    fs::create_dir_all(&ssh_dir).unwrap();
    let _ = fs::set_permissions(&user_home, fs::Permissions::from_mode(0o700));
    let _ = fs::set_permissions(&ssh_dir, fs::Permissions::from_mode(0o700));

    let Some(sshd) = ThrowawaySshd::start(&auth_keys, &user_home) else {
        return;
    };
    fs::write(
        config_dir.join("node.toml"),
        format!(
            "name = \"tower\"\n\n[devices]\nauthorized_keys = {}\nhost_keys = [{}]\n",
            toml_literal(&auth_keys),
            toml_literal(&sshd.host_key_pub),
        ),
    )
    .unwrap();
    fs::write(
        config_dir.join("policy.toml"),
        "default = \"owner\"\n\n[roles.operator]\nallow = [\"effect:read\"]\n",
    )
    .unwrap();

    let port_arg = format!("port={}", sshd.port);
    let user_arg = format!("user={}", sshd.user);
    let inv_res = run_legion(
        &legion_bin,
        &legion_home,
        &user_home,
        &[
            "device.invite",
            "role=operator",
            "name=phone",
            "host=127.0.0.1",
            &port_arg,
            &user_arg,
        ],
    );
    let inv_text = inv_res["invite"].as_str().unwrap().to_owned();
    let inv_id = inv_res["id"].as_str().unwrap();
    let handover = EnrolHandover::install(
        root.path(),
        &auth_keys,
        &legion_bin,
        inv_id,
        "cancel-before-commit",
    );

    // Hold `config.lock` so the machine receives the enrolment request and waits before committing.
    let config_lock = legion_home.join("state").join("sync").join("config.lock");
    let holder = HeldLock::acquire(&config_lock);

    let key = DeviceKey::generate().unwrap();
    let link = Link::new(LinkConfig {
        device_key: Some(key.to_openssh().unwrap()),
        ..LinkConfig::default()
    });
    let cancel = CancellationToken::new();
    let task = {
        let link = link.clone();
        let inv = inv_text.clone();
        let cancel = cancel.clone();
        tokio::spawn(async move { link.pair_cancellable(&inv, "phone", cancel).await })
    };
    handover
        .wait_received(
            1,
            "cancel-before-commit test: server received client enrolment request while config.lock held",
            &sshd,
            Some(&task),
        )
        .await;
    // Yield once so the client task finishes its post-write `sent = true` store before cancel fires.
    tokio::task::yield_now().await;
    cancel.cancel();

    let err = task
        .await
        .unwrap()
        .expect_err("cancel while blocked on config.lock must stop waiting");
    assert_eq!(err.code, ErrorCode::Cancelled, "{err}");
    let details = err
        .details
        .as_ref()
        .expect("post-send cancel must include details");
    assert_eq!(details[DETAIL_SENT], json!(true));
    assert_eq!(
        details["outcome"],
        json!("not_finished"),
        "post-send cancel must report outcome `not_finished` rather than falsely claiming nothing was saved: {err}"
    );
    assert!(
        !err.message
            .to_ascii_lowercase()
            .contains("nothing was saved"),
        "post-send cancel must never claim 'Nothing was saved': {err}"
    );
    assert!(
        err.message
            .contains("had not finished pairing when Legion checked"),
        "expected honest post-send cancel message: {err}"
    );

    // Release `config.lock`: the machine now finishes the in-flight enrolment cleanly.
    drop(holder);
    let key_b64 = key
        .public_key_line("phone")
        .unwrap()
        .split_whitespace()
        .nth(1)
        .unwrap()
        .to_owned();
    wait_file_contains(
        &auth_keys,
        &key_b64,
        "cancel-before-commit test: machine committing in-flight enrolment after config.lock release",
        &sshd,
    )
    .await;

    // Pairing again with the SAME kept invite picks up the finished pairing automatically.
    let picked_up = link.pair(&inv_text, "phone").await.unwrap_or_else(|e| {
        panic!("retrying the same invite should pick up finished pairing: {e}")
    });
    assert_eq!(picked_up.node, "tower");
    assert_eq!(picked_up.principal, "device:phone");
    assert!(picked_up.recovered);
    assert!(link.config().node("tower").is_some());
    link.close().await;
}

#[cfg(unix)]
fn forced_enrol_raw(
    auth_keys: &Path,
    user_home: &Path,
    invite_text: &str,
    request: &Value,
    deliver: bool,
) -> Option<Value> {
    use std::io::{Read, Write};

    let id = Invite::parse(invite_text).unwrap().id;
    let keys = fs::read_to_string(auth_keys).unwrap_or_default();
    let line = keys
        .lines()
        .find(|l| l.contains(&format!("--enrol {id}")) || l.contains(&format!("--enrol '{id}'")))
        .unwrap_or_else(|| panic!("no invite line for {id} in:\n{keys}"));
    let start = line.find("command=\"").unwrap() + "command=\"".len();
    let end = start + line[start..].find('"').unwrap();
    let cmd = line[start..end].replace("\\\"", "\"");
    let mut child = Command::new("sh")
        .arg("-c")
        .arg(&cmd)
        .env("HOME", user_home)
        .env_remove("LEGION_HOME")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let stdout = child.stdout.take().unwrap();
    if !deliver {
        drop(stdout);
    } else {
        let mut stdin = child.stdin.take().unwrap();
        stdin.write_all(format!("{request}\n").as_bytes()).unwrap();
        drop(stdin);
        let mut text = String::new();
        let mut out = stdout;
        out.read_to_string(&mut text).unwrap();
        let _ = child.wait().unwrap();
        return serde_json::from_str(text.trim()).ok();
    }
    let mut stdin = child.stdin.take().unwrap();
    stdin.write_all(format!("{request}\n").as_bytes()).unwrap();
    drop(stdin);
    let _ = child.wait().unwrap();
    None
}

#[cfg(unix)]
#[tokio::test]
async fn real_sshd_unauthenticated_invite_cannot_take_over_unconfirmed_device() {
    let Some(legion_bin) = real_legion_bin_with_sshd(
        "pairing::real_sshd_unauthenticated_invite_cannot_take_over_unconfirmed_device",
    ) else {
        return;
    };

    let root = strict_tempdir();
    let user_home = root.path().join("home");
    let legion_home = root.path().join("legion");
    let config_dir = legion_home.join("config");
    let ssh_dir = user_home.join(".ssh");
    let auth_keys = ssh_dir.join("authorized_keys");
    fs::create_dir_all(&config_dir).unwrap();
    fs::create_dir_all(&ssh_dir).unwrap();
    let _ = fs::set_permissions(&user_home, fs::Permissions::from_mode(0o700));
    let _ = fs::set_permissions(&ssh_dir, fs::Permissions::from_mode(0o700));

    let Some(sshd) = ThrowawaySshd::start(&auth_keys, &user_home) else {
        return;
    };
    fs::write(
        config_dir.join("node.toml"),
        format!(
            "name = \"tower\"\n\n[devices]\nauthorized_keys = {}\nhost_keys = [{}]\n",
            toml_literal(&auth_keys),
            toml_literal(&sshd.host_key_pub),
        ),
    )
    .unwrap();
    fs::write(
        config_dir.join("policy.toml"),
        "default = \"owner\"\n\n\
         [roles.operator]\n\
         allow = [\"effect:read\"]\n\n\
         [roles.guest]\n\
         allow = [\"meta.whoami\"]\n",
    )
    .unwrap();

    let port_arg = format!("port={}", sshd.port);
    let user_arg = format!("user={}", sshd.user);

    // 1. Victim enrols with operator invite `inv1`, reply is lost (device is locally unconfirmed).
    let inv1_res = run_legion(
        &legion_bin,
        &legion_home,
        &user_home,
        &[
            "device.invite",
            "role=operator",
            "name=phone",
            "host=127.0.0.1",
            &port_arg,
            &user_arg,
        ],
    );
    let inv1_text = inv1_res["invite"].as_str().unwrap();
    let inv1_id = inv1_res["id"].as_str().unwrap();
    let victim = DeviceKey::generate().unwrap();
    let victim_pub = victim.public_key_line("phone").unwrap();
    forced_enrol_raw(
        &auth_keys,
        &user_home,
        inv1_text,
        &json!({"enrol": 1, "id": inv1_id, "public_key": victim_pub, "name": "phone"}),
        false,
    );
    let list_before = run_legion(&legion_bin, &legion_home, &user_home, &["device.list"]);
    assert_eq!(
        list_before["devices"][0]["principal"],
        json!("device:phone")
    );
    assert_eq!(list_before["devices"][0]["unconfirmed"], json!(true));

    // 2. Intruder holds a guest invite and submits the victim's PUBLIC key without proof of
    //    possession. Machine MUST refuse with Conflict (NO details) and consume `inv2`.
    let inv2_res = run_legion(
        &legion_bin,
        &legion_home,
        &user_home,
        &[
            "device.invite",
            "role=guest",
            "name=intruder",
            "host=127.0.0.1",
            &port_arg,
            &user_arg,
        ],
    );
    let inv2_text = inv2_res["invite"].as_str().unwrap();
    let inv2_id = inv2_res["id"].as_str().unwrap();
    let reply = forced_enrol_raw(
        &auth_keys,
        &user_home,
        inv2_text,
        &json!({"enrol": 1, "id": inv2_id, "public_key": victim_pub, "name": "intruder"}),
        true,
    )
    .unwrap();
    assert_eq!(
        reply["ok"],
        json!(false),
        "unauthenticated guest invite must NEVER take over unconfirmed operator device: {reply}"
    );
    assert_eq!(reply["error"]["code"], json!("conflict"), "{reply}");
    assert!(
        reply["error"].get("details").is_none(),
        "conflict without proof must NOT leak existing_principal or device details: {reply}"
    );
    assert!(
        !fs::read_to_string(&auth_keys)
            .unwrap()
            .contains(&format!("legion-invite:{inv2_id}")),
        "conflict must consume the invite so it cannot be reused to probe keys"
    );

    // Victim's original operator pairing is completely untouched.
    let list_after_attack = run_legion(&legion_bin, &legion_home, &user_home, &["device.list"]);
    assert_eq!(list_after_attack["devices"].as_array().unwrap().len(), 1);
    assert_eq!(
        list_after_attack["devices"][0]["principal"],
        json!("device:phone")
    );
    assert_eq!(list_after_attack["devices"][0]["role"], json!("operator"));

    // 3. The victim ITSELF holding the private key presents a fresh operator invite with valid
    //    proof of possession -> cleanly replaces its own unconfirmed leftover.
    let inv3_res = run_legion(
        &legion_bin,
        &legion_home,
        &user_home,
        &[
            "device.invite",
            "role=operator",
            "name=phone2",
            "host=127.0.0.1",
            &port_arg,
            &user_arg,
        ],
    );
    let inv3_text = inv3_res["invite"].as_str().unwrap();
    let victim_link = Link::new(LinkConfig {
        device_key: Some(victim.to_openssh().unwrap()),
        ..LinkConfig::default()
    });
    let replaced = victim_link
        .pair(inv3_text, "phone2")
        .await
        .unwrap_or_else(|e| {
            panic!("victim with valid proof must replace its own unconfirmed leftover: {e}")
        });
    assert_eq!(replaced.principal, "device:phone2");
    assert_eq!(replaced.role, "operator");
    victim_link.close().await;
}

#[cfg(unix)]
#[tokio::test]
async fn real_sshd_mixed_versions_and_restore_pairing_probe() {
    // Runs against whatever `LEGION_BIN` points to (both this branch and `origin/main`!):
    // verifies normal pairing, spent-invite refusal, confirmed duplicate-key conflict offering
    // Restore pairing (even when the machine omits `details`), restoring cleanly, and malformed
    // key refusal — all completing promptly with zero hangs.
    let Some(legion_bin) =
        real_legion_bin_with_sshd("pairing::real_sshd_mixed_versions_and_restore_pairing_probe")
    else {
        return;
    };

    let root = strict_tempdir();
    let user_home = root.path().join("home");
    let legion_home = root.path().join("legion");
    let config_dir = legion_home.join("config");
    let ssh_dir = user_home.join(".ssh");
    let auth_keys = ssh_dir.join("authorized_keys");
    fs::create_dir_all(&config_dir).unwrap();
    fs::create_dir_all(&ssh_dir).unwrap();
    let _ = fs::set_permissions(&user_home, fs::Permissions::from_mode(0o700));
    let _ = fs::set_permissions(&ssh_dir, fs::Permissions::from_mode(0o700));

    let Some(sshd) = ThrowawaySshd::start(&auth_keys, &user_home) else {
        return;
    };
    fs::write(
        config_dir.join("node.toml"),
        format!(
            "name = \"tower\"\n\n[devices]\nauthorized_keys = {}\nhost_keys = [{}]\n",
            toml_literal(&auth_keys),
            toml_literal(&sshd.host_key_pub),
        ),
    )
    .unwrap();
    fs::write(
        config_dir.join("policy.toml"),
        "default = \"owner\"\n\n[roles.operator]\nallow = [\"effect:read\"]\n",
    )
    .unwrap();

    let port_arg = format!("port={}", sshd.port);
    let user_arg = format!("user={}", sshd.user);

    // 1. Plain success + first session confirming the pairing.
    let inv1_res = run_legion(
        &legion_bin,
        &legion_home,
        &user_home,
        &[
            "device.invite",
            "role=operator",
            "name=phone",
            "host=127.0.0.1",
            &port_arg,
            &user_arg,
        ],
    );
    let inv1_text = inv1_res["invite"].as_str().unwrap();
    let key = DeviceKey::generate().unwrap();
    let link = Link::new(LinkConfig {
        device_key: Some(key.to_openssh().unwrap()),
        ..LinkConfig::default()
    });
    let enrolled = link
        .pair(inv1_text, "phone")
        .await
        .unwrap_or_else(|e| panic!("pair failed against {}: {e}", legion_bin.display()));
    assert_eq!(enrolled.principal, "device:phone");
    let whoami = link
        .call("tower", "meta.whoami", json!({}), None, |_| {})
        .await
        .unwrap();
    assert_eq!(whoami["principal"], json!("device:phone"));
    link.close().await;

    // 2. Spent invite refused promptly for a different key.
    let other_key = DeviceKey::generate().unwrap();
    let other_link = Link::new(LinkConfig {
        device_key: Some(other_key.to_openssh().unwrap()),
        ..LinkConfig::default()
    });
    let spent = other_link.pair(inv1_text, "tablet").await.unwrap_err();
    assert_eq!(spent.code, ErrorCode::Denied, "{spent}");
    other_link.close().await;

    // 3. Fresh invite presented by the already-confirmed key from a fresh LinkConfig (lost local
    //    state): hits Conflict AND probes the machine with `key` against pinned host keys so
    //    `details.restorable == true` and `details.existing_principal == "device:phone"` even
    //    when running against an `origin/main` machine!
    let inv2_res = run_legion(
        &legion_bin,
        &legion_home,
        &user_home,
        &[
            "device.invite",
            "role=operator",
            "name=phone2",
            "host=127.0.0.1",
            &port_arg,
            &user_arg,
        ],
    );
    let inv2_text = inv2_res["invite"].as_str().unwrap();
    let fresh_link = Link::new(LinkConfig {
        device_key: Some(key.to_openssh().unwrap()),
        ..LinkConfig::default()
    });
    let conflict = fresh_link.pair(inv2_text, "phone").await.unwrap_err();
    assert_eq!(conflict.code, ErrorCode::Conflict, "{conflict}");
    let details = conflict
        .details
        .as_ref()
        .expect("Conflict on device: invite must carry restorable details from probe/machine");
    assert_eq!(details["existing_principal"], json!("device:phone"));
    assert_eq!(details["node"], json!("tower"));
    assert_eq!(details["restorable"], json!(true));
    assert!(fresh_link.config().node("tower").is_none());

    // Restore pairing succeeds cleanly (even passing empty expected_principal).
    let restored = fresh_link
        .restore_pairing(inv2_text, "", "phone", CancellationToken::new())
        .await
        .unwrap_or_else(|e| panic!("restore_pairing failed: {e}"));
    assert_eq!(restored.principal, "device:phone");
    assert!(restored.recovered);
    assert!(fresh_link.config().node("tower").is_some());
    fresh_link.close().await;

    // 4. Malformed public key refused cleanly without hanging.
    let inv3_res = run_legion(
        &legion_bin,
        &legion_home,
        &user_home,
        &[
            "device.invite",
            "role=operator",
            "name=badkey",
            "host=127.0.0.1",
            &port_arg,
            &user_arg,
        ],
    );
    let inv3_text = inv3_res["invite"].as_str().unwrap();
    let inv3_id = inv3_res["id"].as_str().unwrap();
    let bad = forced_enrol_raw(
        &auth_keys,
        &user_home,
        inv3_text,
        &json!({"enrol": 1, "id": inv3_id, "public_key": "ssh-ed25519 AAAA", "name": "badkey"}),
        true,
    )
    .unwrap();
    assert_eq!(bad["ok"], json!(false), "{bad}");
    assert_eq!(bad["error"]["code"], json!("invalid_args"), "{bad}");
}

#[cfg(unix)]
fn ssh_keygen_sign_proof(key: &DeviceKey, namespace: &str, msg: &[u8]) -> String {
    let dir = strict_tempdir();
    let kp = dir.path().join("k");
    fs::write(&kp, key.to_openssh().unwrap().as_bytes()).unwrap();
    fs::set_permissions(&kp, fs::Permissions::from_mode(0o600)).unwrap();
    let mp = dir.path().join("m");
    fs::write(&mp, msg).unwrap();
    let out = Command::new(find_tool("ssh-keygen").unwrap())
        .args(["-Y", "sign", "-n", namespace, "-f"])
        .arg(&kp)
        .arg(&mp)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "ssh-keygen -Y sign failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    fs::read_to_string(dir.path().join("m.sig")).unwrap()
}

#[cfg(unix)]
#[tokio::test]
async fn real_sshd_pre_upgrade_invite_and_invalid_proof_denied_not_absent() {
    let Some(legion_bin) = real_legion_bin_with_sshd(
        "pairing::real_sshd_pre_upgrade_invite_and_invalid_proof_denied_not_absent",
    ) else {
        return;
    };

    let root = strict_tempdir();
    let user_home = root.path().join("home");
    let legion_home = root.path().join("legion");
    let config_dir = legion_home.join("config");
    let ssh_dir = user_home.join(".ssh");
    let auth_keys = ssh_dir.join("authorized_keys");
    fs::create_dir_all(&config_dir).unwrap();
    fs::create_dir_all(&ssh_dir).unwrap();
    let _ = fs::set_permissions(&user_home, fs::Permissions::from_mode(0o700));
    let _ = fs::set_permissions(&ssh_dir, fs::Permissions::from_mode(0o700));

    let Some(sshd) = ThrowawaySshd::start(&auth_keys, &user_home) else {
        return;
    };
    fs::write(
        config_dir.join("node.toml"),
        format!(
            "name = \"tower\"\n\n[devices]\nauthorized_keys = {}\nhost_keys = [{}]\n",
            toml_literal(&auth_keys),
            toml_literal(&sshd.host_key_pub),
        ),
    )
    .unwrap();
    fs::write(
        config_dir.join("policy.toml"),
        "default = \"owner\"\n\n\
         [roles.operator]\n\
         allow = [\"effect:read\"]\n\n\
         [roles.guest]\n\
         allow = [\"meta.whoami\"]\n",
    )
    .unwrap();

    let port_arg = format!("port={}", sshd.port);
    let user_arg = format!("user={}", sshd.user);
    let make_invite = |role: &str, name: &str| -> (String, String) {
        let res = run_legion(
            &legion_bin,
            &legion_home,
            &user_home,
            &[
                "device.invite",
                &format!("role={role}"),
                &format!("name={name}"),
                "host=127.0.0.1",
                &port_arg,
                &user_arg,
            ],
        );
        (
            res["invite"].as_str().unwrap().to_owned(),
            res["id"].as_str().unwrap().to_owned(),
        )
    };

    // Part 1: On a FRESH key (never enrolled before), every invalid proof MUST return `denied`,
    // enrol NOTHING, and keep the invite intact — directly catching any mutation that treats a
    // failing proof as absent.
    let (inv1_text, inv1_id) = make_invite("operator", "fresh-proof");
    let inv1 = Invite::parse(&inv1_text).unwrap();
    let fresh_key = DeviceKey::generate().unwrap();
    let fresh_pub = fresh_key.public_key_line("fresh-proof").unwrap();
    let attacker_key = DeviceKey::generate().unwrap();
    let mut host_fps: Vec<String> = inv1
        .host_keys
        .iter()
        .filter_map(|k| legion_link::public_key_fingerprint(k))
        .collect();
    host_fps.sort();
    host_fps.dedup();
    let canonical_msg = format!(
        "legion-enrol-proof-v1\nid={}\npublic_key={}\nnode=tower\nhost_keys={}\n",
        inv1.id,
        fresh_key.public_key_line("").unwrap(),
        host_fps.join(",")
    );
    let fake_hk = DeviceKey::generate().unwrap().public_key_line("").unwrap();
    let invalid_proofs = [
        (
            "wrong_key",
            attacker_key
                .sign_enrol_proof(&inv1.id, &inv1.node, &inv1.host_keys)
                .unwrap(),
        ),
        (
            "wrong_invite",
            fresh_key
                .sign_enrol_proof("other-invite", &inv1.node, &inv1.host_keys)
                .unwrap(),
        ),
        (
            "wrong_node",
            fresh_key
                .sign_enrol_proof(&inv1.id, "pi", &inv1.host_keys)
                .unwrap(),
        ),
        (
            "wrong_host_keys",
            fresh_key
                .sign_enrol_proof(&inv1.id, &inv1.node, &[fake_hk])
                .unwrap(),
        ),
        (
            "wrong_namespace",
            ssh_keygen_sign_proof(&fresh_key, "file", canonical_msg.as_bytes()),
        ),
        ("garbage", "not-a-valid-sshsig".to_owned()),
    ];
    for (label, bad_proof) in &invalid_proofs {
        let reply = forced_enrol_raw(
            &auth_keys,
            &user_home,
            &inv1_text,
            &json!({
                "enrol": 1,
                "id": inv1_id,
                "public_key": fresh_pub,
                "name": "fresh-proof",
                "proof": bad_proof,
            }),
            true,
        )
        .unwrap();
        assert_eq!(reply["ok"], json!(false), "{label}: {reply}");
        assert_eq!(
            reply["error"]["code"],
            json!("denied"),
            "{label}: invalid proof on fresh key must return denied, never treated as absent: {reply}"
        );
        assert!(
            !config_dir.join("devices.toml").exists(),
            "{label}: invalid proof must write nothing"
        );
        assert!(
            fs::read_to_string(&auth_keys)
                .unwrap()
                .contains(&format!("legion-invite:{inv1_id}")),
            "{label}: denied proof must leave invite intact"
        );
    }

    // Valid `ssh-keygen -Y sign -n legion-enrol` proof on that same invite succeeds!
    let valid_keygen_sig =
        ssh_keygen_sign_proof(&fresh_key, "legion-enrol", canonical_msg.as_bytes());
    let ok_reply = forced_enrol_raw(
        &auth_keys,
        &user_home,
        &inv1_text,
        &json!({
            "enrol": 1,
            "id": inv1_id,
            "public_key": fresh_pub,
            "name": "fresh-proof",
            "proof": valid_keygen_sig,
        }),
        true,
    )
    .unwrap();
    assert_eq!(ok_reply["ok"], json!(true), "{ok_reply}");
    assert_eq!(ok_reply["principal"], json!("device:fresh-proof"));

    // Part 2: On the now-unconfirmed leftover, an invalid proof MUST also return `denied` (NOT
    // `conflict`) and leave the invite intact and leftover unchanged.
    let (inv2_text, inv2_id) = make_invite("guest", "leftover-check");
    let inv2 = Invite::parse(&inv2_text).unwrap();
    let bad_on_existing = forced_enrol_raw(
        &auth_keys,
        &user_home,
        &inv2_text,
        &json!({
            "enrol": 1,
            "id": inv2_id,
            "public_key": fresh_pub,
            "name": "leftover-check",
            "proof": attacker_key.sign_enrol_proof(&inv2.id, &inv2.node, &inv2.host_keys).unwrap(),
        }),
        true,
    )
    .unwrap();
    assert_eq!(
        bad_on_existing["error"]["code"],
        json!("denied"),
        "invalid proof on existing unconfirmed key must be denied, not conflict: {bad_on_existing}"
    );
    assert!(
        fs::read_to_string(&auth_keys)
            .unwrap()
            .contains(&format!("legion-invite:{inv2_id}"))
    );

    // Part 3: Pre-upgrade invite (no `host_key_fingerprints` recorded in `state/devices.json`,
    // simulating an invite created before upgrading):
    //   3a. New phone sending its normal proof with a FRESH key succeeds cleanly!
    let (pre_inv_text, pre_inv_id) = make_invite("operator", "pre-upgrade");
    let state_file = legion_home.join("state").join("devices.json");
    let mut state_val: Value =
        serde_json::from_str(&fs::read_to_string(&state_file).unwrap()).unwrap();
    state_val["invites"][&pre_inv_id]
        .as_object_mut()
        .unwrap()
        .remove("host_key_fingerprints");
    fs::write(
        &state_file,
        serde_json::to_string_pretty(&state_val).unwrap(),
    )
    .unwrap();

    let pre_key = DeviceKey::generate().unwrap();
    let pre_link = Link::new(LinkConfig {
        device_key: Some(pre_key.to_openssh().unwrap()),
        ..LinkConfig::default()
    });
    let pre_enrolled = pre_link
        .pair(&pre_inv_text, "pre-upgrade")
        .await
        .unwrap_or_else(|e| {
            panic!("new phone must pair cleanly with a pre-upgrade invite (no host_key_fingerprints): {e}")
        });
    assert_eq!(pre_enrolled.principal, "device:pre-upgrade");
    assert_eq!(pre_enrolled.role, "operator");
    pre_link.close().await;

    //   3b. New phone sending its normal proof against a pre-upgrade invite for an EXISTING
    //       unconfirmed key takes the no-proof path: refuses with Conflict (no replacement),
    //       consumes the invite, and leaves the original leftover untouched.
    let (pre_dup_text, pre_dup_id) = make_invite("guest", "pre-upgrade-dup");
    let mut state_val2: Value =
        serde_json::from_str(&fs::read_to_string(&state_file).unwrap()).unwrap();
    state_val2["invites"][&pre_dup_id]
        .as_object_mut()
        .unwrap()
        .remove("host_key_fingerprints");
    fs::write(
        &state_file,
        serde_json::to_string_pretty(&state_val2).unwrap(),
    )
    .unwrap();
    let pre_dup_inv = Invite::parse(&pre_dup_text).unwrap();
    let dup_reply = forced_enrol_raw(
        &auth_keys,
        &user_home,
        &pre_dup_text,
        &json!({
            "enrol": 1,
            "id": pre_dup_id,
            "public_key": pre_key.public_key_line("pre-upgrade-dup").unwrap(),
            "name": "pre-upgrade-dup",
            "proof": pre_key.sign_enrol_proof(&pre_dup_inv.id, &pre_dup_inv.node, &pre_dup_inv.host_keys).unwrap(),
        }),
        true,
    )
    .unwrap();
    assert_eq!(dup_reply["error"]["code"], json!("conflict"), "{dup_reply}");
    assert!(
        dup_reply["error"].get("details").is_none(),
        "pre-upgrade invite conflict must omit details: {dup_reply}"
    );
    assert!(
        !fs::read_to_string(&auth_keys)
            .unwrap()
            .contains(&format!("legion-invite:{pre_dup_id}")),
        "conflict on pre-upgrade invite must still consume the invite"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn real_sshd_concurrent_pair_same_machine_refuses_second_and_keeps_consistent_state() {
    let Some(legion_bin) = real_legion_bin_with_sshd(
        "pairing::real_sshd_concurrent_pair_same_machine_refuses_second_and_keeps_consistent_state",
    ) else {
        return;
    };

    let root = strict_tempdir();
    let user_home = root.path().join("home");
    let legion_home = root.path().join("legion");
    let config_dir = legion_home.join("config");
    let ssh_dir = user_home.join(".ssh");
    let auth_keys = ssh_dir.join("authorized_keys");
    fs::create_dir_all(&config_dir).unwrap();
    fs::create_dir_all(&ssh_dir).unwrap();
    let _ = fs::set_permissions(&user_home, fs::Permissions::from_mode(0o700));
    let _ = fs::set_permissions(&ssh_dir, fs::Permissions::from_mode(0o700));

    let Some(sshd) = ThrowawaySshd::start(&auth_keys, &user_home) else {
        return;
    };
    fs::write(
        config_dir.join("node.toml"),
        format!(
            "name = \"tower\"\n\n[devices]\nauthorized_keys = {}\nhost_keys = [{}]\n",
            toml_literal(&auth_keys),
            toml_literal(&sshd.host_key_pub),
        ),
    )
    .unwrap();
    fs::write(
        config_dir.join("policy.toml"),
        "default = \"owner\"\n\n\
         [roles.operator]\n\
         allow = [\"effect:read\"]\n\n\
         [roles.guest]\n\
         allow = [\"meta.whoami\"]\n",
    )
    .unwrap();

    let port_arg = format!("port={}", sshd.port);
    let user_arg = format!("user={}", sshd.user);
    let inv1 = run_legion(
        &legion_bin,
        &legion_home,
        &user_home,
        &[
            "device.invite",
            "role=operator",
            "name=phone1",
            "host=127.0.0.1",
            &port_arg,
            &user_arg,
        ],
    );
    let inv2 = run_legion(
        &legion_bin,
        &legion_home,
        &user_home,
        &[
            "device.invite",
            "role=guest",
            "name=phone2",
            "host=127.0.0.1",
            &port_arg,
            &user_arg,
        ],
    );
    let inv1_text = inv1["invite"].as_str().unwrap().to_owned();
    let inv1_id = inv1["id"].as_str().unwrap();
    let inv2_text = inv2["invite"].as_str().unwrap().to_owned();
    let inv2_id = inv2["id"].as_str().unwrap().to_owned();
    let handover = EnrolHandover::install(
        root.path(),
        &auth_keys,
        &legion_bin,
        inv1_id,
        "concurrent-pair",
    );

    let config_lock = legion_home.join("state").join("sync").join("config.lock");
    let holder = HeldLock::acquire(&config_lock);

    let key = DeviceKey::generate().unwrap();
    let link = Link::new(LinkConfig {
        device_key: Some(key.to_openssh().unwrap()),
        ..LinkConfig::default()
    });
    let first_task = {
        let link = link.clone();
        let inv = inv1_text.clone();
        tokio::spawn(async move { link.pair(&inv, "phone1").await })
    };
    handover
        .wait_received(
            1,
            "concurrent-pair test: server received first pairing request while config.lock held",
            &sshd,
            Some(&first_task),
        )
        .await;

    // While first pairing is in flight for `tower`, a second concurrent `pair` for `tower` MUST
    // fail immediately with Busy without sending anything or spending `inv2`.
    let second_err = link
        .pair(&inv2_text, "phone2")
        .await
        .expect_err("second concurrent pair for same machine must refuse with Busy");
    assert_eq!(second_err.code, ErrorCode::Busy, "{second_err}");
    assert_eq!(
        second_err.details.as_ref().and_then(|d| d.get(DETAIL_SENT)),
        Some(&json!(false))
    );
    assert!(
        fs::read_to_string(&auth_keys)
            .unwrap()
            .contains(&format!("legion-invite:{inv2_id}")),
        "refused second invite must remain unspent"
    );

    // Release `config.lock`: first pairing finishes and `Link` matches `meta.whoami`.
    drop(holder);
    let first_ok = first_task
        .await
        .unwrap()
        .unwrap_or_else(|e| panic!("first pairing failed: {e}"));
    assert_eq!(first_ok.principal, "device:phone1");
    assert_eq!(
        link.config()
            .node("tower")
            .and_then(|n| n.principal.clone()),
        Some("device:phone1".to_owned())
    );
    let who = link
        .call("tower", "meta.whoami", json!({}), None, |_| {})
        .await
        .unwrap();
    assert_eq!(who["principal"], json!("device:phone1"));
    link.close().await;
}

#[cfg(unix)]
#[tokio::test]
async fn real_sshd_kill_point_recovery_same_invite_fresh_and_replacement() {
    let Some(legion_bin) = real_legion_bin_with_sshd(
        "pairing::real_sshd_kill_point_recovery_same_invite_fresh_and_replacement",
    ) else {
        return;
    };

    let root = strict_tempdir();
    let user_home = root.path().join("home");
    let legion_home = root.path().join("legion");
    let config_dir = legion_home.join("config");
    let ssh_dir = user_home.join(".ssh");
    let auth_keys = ssh_dir.join("authorized_keys");
    fs::create_dir_all(&config_dir).unwrap();
    fs::create_dir_all(&ssh_dir).unwrap();
    let _ = fs::set_permissions(&user_home, fs::Permissions::from_mode(0o700));
    let _ = fs::set_permissions(&ssh_dir, fs::Permissions::from_mode(0o700));

    let Some(sshd) = ThrowawaySshd::start(&auth_keys, &user_home) else {
        return;
    };
    fs::write(
        config_dir.join("node.toml"),
        format!(
            "name = \"tower\"\n\n[devices]\nauthorized_keys = {}\nhost_keys = [{}]\n",
            toml_literal(&auth_keys),
            toml_literal(&sshd.host_key_pub),
        ),
    )
    .unwrap();
    fs::write(
        config_dir.join("policy.toml"),
        "default = \"owner\"\n\n[roles.operator]\nallow = [\"effect:read\"]\n",
    )
    .unwrap();

    let port_arg = format!("port={}", sshd.port);
    let user_arg = format!("user={}", sshd.user);
    let make_invite = |name: &str| -> (String, String) {
        let res = run_legion(
            &legion_bin,
            &legion_home,
            &user_home,
            &[
                "device.invite",
                "role=operator",
                &format!("name={name}"),
                "host=127.0.0.1",
                &port_arg,
                &user_arg,
            ],
        );
        (
            res["invite"].as_str().unwrap().to_owned(),
            res["id"].as_str().unwrap().to_owned(),
        )
    };
    let state_file = legion_home.join("state").join("devices.json");

    // 1. Fresh enrolment killed after step 1 (uncommitted marker in state.unconfirmed, invite still
    //    present, devices.toml absent): phone retries the SAME invite -> repair rolls back the
    //    marker, keeps invite usable, and pairing succeeds!
    let key1 = DeviceKey::generate().unwrap();
    let (inv1_text, inv1_id) = make_invite("fresh-s1");
    {
        let mut st: Value =
            serde_json::from_str(&fs::read_to_string(&state_file).unwrap()).unwrap();
        st["unconfirmed"] = json!({
            "11111111111111111111111111111111": {
                "pairing_id": "11111111111111111111111111111111",
                "invite_id": inv1_id,
                "name": "fresh-s1",
                "principal": "device:fresh-s1",
                "fingerprint": "SHA256:placeholder",
                "created": "2026-11-26T00:00:00Z"
            }
        });
        fs::write(&state_file, serde_json::to_string_pretty(&st).unwrap()).unwrap();
    }
    let link1 = Link::new(LinkConfig {
        device_key: Some(key1.to_openssh().unwrap()),
        ..LinkConfig::default()
    });
    let enrolled1 = link1
        .pair(&inv1_text, "fresh-s1")
        .await
        .unwrap_or_else(|e| {
            panic!("fresh step-1 rollback retry with same invite must succeed: {e}")
        });
    assert_eq!(enrolled1.principal, "device:fresh-s1");
    assert!(!enrolled1.recovered);
    link1.close().await;

    // 2. Replacement killed after step 1 (`fresh-s1` is still unconfirmed because link1 never
    //    opened a gate session; step 1 added `2222...` with `replaces_pairing_id` pointing at
    //    `fresh-s1` without modifying `devices.toml`): phone retries the SAME invite `inv2` ->
    //    repair rolls back `2222...`, keeps `fresh-s1` unconfirmed and `inv2` usable, and `inv2`
    //    replaces `fresh-s1` cleanly!
    let (inv2_text, inv2_id) = make_invite("rep-s1");
    let pid_fresh1 = {
        let st: Value = serde_json::from_str(&fs::read_to_string(&state_file).unwrap()).unwrap();
        st["unconfirmed"]
            .as_object()
            .unwrap()
            .keys()
            .next()
            .unwrap()
            .clone()
    };
    {
        let mut st: Value =
            serde_json::from_str(&fs::read_to_string(&state_file).unwrap()).unwrap();
        st["unconfirmed"].as_object_mut().unwrap().insert(
            "22222222222222222222222222222222".to_owned(),
            json!({
                "pairing_id": "22222222222222222222222222222222",
                "invite_id": inv2_id,
                "name": "rep-s1",
                "principal": "device:rep-s1",
                "fingerprint": "SHA256:placeholder",
                "created": "2026-11-26T00:00:01Z",
                "replaces_pairing_id": pid_fresh1,
            }),
        );
        fs::write(&state_file, serde_json::to_string_pretty(&st).unwrap()).unwrap();
    }
    let link2 = Link::new(LinkConfig {
        device_key: Some(key1.to_openssh().unwrap()),
        ..LinkConfig::default()
    });
    let enrolled2 = link2.pair(&inv2_text, "rep-s1").await.unwrap_or_else(|e| {
        panic!("replacement step-1 rollback retry with same invite must succeed: {e}")
    });
    assert_eq!(enrolled2.principal, "device:rep-s1");
    assert!(!enrolled2.recovered);
    link2.close().await;

    // 3. Fresh enrolment killed after step 2 (committed to devices.toml + policy.toml +
    //    state.unconfirmed, leftover invite still in authorized_keys/state.invites, permanent key
    //    line MISSING from authorized_keys): phone retries the SAME invite -> repair rolls
    //    forward (adds key line, removes invite), enrolment sees NotFound, recovery probe picks up
    //    the finished pairing!
    let key3 = DeviceKey::generate().unwrap();
    let (inv3_text, inv3_id) = make_invite("fresh-s2");
    let inv3_line = fs::read_to_string(&auth_keys)
        .unwrap()
        .lines()
        .find(|l| l.contains(&format!("legion-invite:{inv3_id}")))
        .unwrap()
        .to_owned();
    let inv3_state_rec = {
        let st: Value = serde_json::from_str(&fs::read_to_string(&state_file).unwrap()).unwrap();
        st["invites"][&inv3_id].clone()
    };
    let inv3 = Invite::parse(&inv3_text).unwrap();
    let r3 = forced_enrol_raw(
        &auth_keys,
        &user_home,
        &inv3_text,
        &json!({
            "enrol": 1,
            "id": inv3_id,
            "public_key": key3.public_key_line("fresh-s2").unwrap(),
            "name": "fresh-s2",
            "proof": key3.sign_enrol_proof(&inv3.id, &inv3.node, &inv3.host_keys).unwrap(),
        }),
        false,
    );
    assert!(r3.is_none());
    // Rewind authorized_keys + state.invites to exact post-step-2 crash state (no key line, invite present).
    {
        let ak = fs::read_to_string(&auth_keys).unwrap();
        let mut lines: Vec<String> = ak
            .lines()
            .filter(|l| !l.contains("legion-device:fresh-s2"))
            .map(str::to_owned)
            .collect();
        lines.push(inv3_line);
        fs::write(&auth_keys, format!("{}\n", lines.join("\n"))).unwrap();
        let mut st: Value =
            serde_json::from_str(&fs::read_to_string(&state_file).unwrap()).unwrap();
        st["invites"]
            .as_object_mut()
            .unwrap()
            .insert(inv3_id.clone(), inv3_state_rec);
        fs::write(&state_file, serde_json::to_string_pretty(&st).unwrap()).unwrap();
    }
    let link3 = Link::new(LinkConfig {
        device_key: Some(key3.to_openssh().unwrap()),
        ..LinkConfig::default()
    });
    let recovered3 = link3
        .pair(&inv3_text, "fresh-s2")
        .await
        .unwrap_or_else(|e| {
            panic!("fresh step-2 roll-forward retry with same invite must recover: {e}")
        });
    assert_eq!(recovered3.principal, "device:fresh-s2");
    assert!(recovered3.recovered);
    link3.close().await;

    // 4. Replacement killed after step 2 (`rep-s1` still unconfirmed -> replaced by `rep-s2` in
    //    devices.toml, but authorized_keys still has `rep-s1` + `inv4` and lacks `rep-s2`):
    //    phone retries the SAME invite `inv4` -> repair rolls forward (`rep-s2` line added,
    //    `rep-s1` line removed, `inv4` removed, old unconfirmed mark dropped) -> recovery probe
    //    succeeds under `device:rep-s2`!
    let (inv4_text, inv4_id) = make_invite("rep-s2");
    let inv4_line = fs::read_to_string(&auth_keys)
        .unwrap()
        .lines()
        .find(|l| l.contains(&format!("legion-invite:{inv4_id}")))
        .unwrap()
        .to_owned();
    let old_rep1_line = fs::read_to_string(&auth_keys)
        .unwrap()
        .lines()
        .find(|l| l.contains("legion-device:rep-s1"))
        .unwrap()
        .to_owned();
    let (inv4_state_rec, old_rep1_pid, old_rep1_unconf) = {
        let st: Value = serde_json::from_str(&fs::read_to_string(&state_file).unwrap()).unwrap();
        let (pid, rec) = st["unconfirmed"]
            .as_object()
            .unwrap()
            .iter()
            .find(|(_, v)| v["name"] == json!("rep-s1"))
            .unwrap();
        (st["invites"][&inv4_id].clone(), pid.clone(), rec.clone())
    };
    let inv4 = Invite::parse(&inv4_text).unwrap();
    let r4 = forced_enrol_raw(
        &auth_keys,
        &user_home,
        &inv4_text,
        &json!({
            "enrol": 1,
            "id": inv4_id,
            "public_key": key1.public_key_line("rep-s2").unwrap(),
            "name": "rep-s2",
            "proof": key1.sign_enrol_proof(&inv4.id, &inv4.node, &inv4.host_keys).unwrap(),
        }),
        false,
    );
    assert!(r4.is_none());
    // Rewind authorized_keys + state to exact post-step-2 replacement crash state.
    {
        let ak = fs::read_to_string(&auth_keys).unwrap();
        let mut lines: Vec<String> = ak
            .lines()
            .filter(|l| !l.contains("legion-device:rep-s2"))
            .map(str::to_owned)
            .collect();
        lines.push(old_rep1_line);
        lines.push(inv4_line);
        fs::write(&auth_keys, format!("{}\n", lines.join("\n"))).unwrap();
        let mut st: Value =
            serde_json::from_str(&fs::read_to_string(&state_file).unwrap()).unwrap();
        st["invites"]
            .as_object_mut()
            .unwrap()
            .insert(inv4_id.clone(), inv4_state_rec);
        let unconf = st["unconfirmed"].as_object_mut().unwrap();
        for (_, val) in unconf.iter_mut() {
            if val["name"] == json!("rep-s2") {
                val["replaces_pairing_id"] = json!(old_rep1_pid);
            }
        }
        unconf.insert(old_rep1_pid.clone(), old_rep1_unconf);
        fs::write(&state_file, serde_json::to_string_pretty(&st).unwrap()).unwrap();
    }
    let link4 = Link::new(LinkConfig {
        device_key: Some(key1.to_openssh().unwrap()),
        ..LinkConfig::default()
    });
    let recovered4 = link4.pair(&inv4_text, "rep-s2").await.unwrap_or_else(|e| {
        panic!("replacement step-2 roll-forward retry with same invite must recover: {e}")
    });
    assert_eq!(recovered4.principal, "device:rep-s2");
    assert!(recovered4.recovered);
    let ak_final = fs::read_to_string(&auth_keys).unwrap();
    let key1_b64 = key1
        .public_key_line("rep-s2")
        .unwrap()
        .split_whitespace()
        .nth(1)
        .unwrap()
        .to_owned();
    let key1_lines: Vec<_> = ak_final.lines().filter(|l| l.contains(&key1_b64)).collect();
    assert_eq!(key1_lines.len(), 1, "{key1_lines:?}");
    assert!(key1_lines[0].contains("device:rep-s2"), "{key1_lines:?}");
    assert!(!ak_final.contains(&format!("legion-invite:{inv4_id}")));
    link4.close().await;
}
