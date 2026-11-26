//! End-to-end two-sshd fleet routing integration test for `legion-link`:
//!
//! 1. Starts two real loopback `sshd` instances (`tower` and `pi`) configured as a mutual fleet.
//! 2. Pairs an unpaired `Link` with `tower` only; verifies enrolment pushes the device to `pi`.
//! 3. Calls `Link::learn()` to learn `pi` directly from `tower`.
//! 4. Calls `Link::call_machine("pi", ...)` and verifies it goes `Route::Direct`.
//! 5. Stops `tower`'s `sshd`; verifies `pi` still works directly while calls to `tower` fail
//!    honestly (never silently running on `pi`).
//! 6. Restarts `tower` and stops `pi`; verifies the reverse (`tower` works directly).
//! 7. Tests relay fallback when a learned node has not received the device key yet (shared
//!    principal), then tests removing the device on `pi` while `tower` is stopped (refused by `pi`
//!    with `device_removed_or_unknown`) and syncing `tower` afterwards (refused by `tower` too).
//!
//! Skipped cleanly when `sshd`/`ssh-keygen` or a built `legion` binary are unavailable.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::fs;
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use common::{
    current_user, find_sshd, find_tool, real_legion_bin_with_sshd, strict_tempdir, toml_literal,
};
use legion_link::proto::ErrorCode;
use legion_link::{
    CAP_IF_EXISTS, Link, LinkConfig, LinkOptions, MachineRouteStatus, NodeOrigin, PutIfExists,
    PutRequest, REASON_DEVICE_REMOVED_OR_UNKNOWN, Route,
};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

struct ManagedSshd {
    sshd_bin: PathBuf,
    config_path: PathBuf,
    log_path: PathBuf,
    host_key_pub: PathBuf,
    port: u16,
    user: String,
    child: Option<Child>,
}

impl ManagedSshd {
    fn new(dir: &Path, authorized_keys: &Path, home_dir: &Path) -> Option<Self> {
        if !cfg!(unix) {
            return None;
        }
        let sshd_bin = find_sshd()?;
        let ssh_keygen_bin = find_tool("ssh-keygen")?;
        let user = current_user()?;

        fs::create_dir_all(dir).unwrap();
        #[cfg(unix)]
        {
            let _ = fs::set_permissions(dir, fs::Permissions::from_mode(0o700));
        }
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

        let host_key = dir.join("host_ed25519");
        let host_key_pub = dir.join("host_ed25519.pub");
        let keygen = Command::new(&ssh_keygen_bin)
            .args(["-q", "-t", "ed25519", "-N", "", "-C", "legion-fleet-test"])
            .arg("-f")
            .arg(&host_key)
            .stdin(Stdio::null())
            .output()
            .unwrap();
        assert!(keygen.status.success());
        #[cfg(unix)]
        {
            let _ = fs::set_permissions(&host_key, fs::Permissions::from_mode(0o600));
        }

        let supports_no_penalties = Command::new(&sshd_bin)
            .args(["-T", "-f", "/dev/null", "-o", "PerSourcePenalties=no", "-h"])
            .arg(&host_key)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success());

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);

        let config_path = dir.join("sshd_config");
        let log_path = dir.join("sshd.log");
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

        let mut sshd = Self {
            sshd_bin,
            config_path,
            log_path,
            host_key_pub,
            port,
            user,
            child: None,
        };
        sshd.start();
        Some(sshd)
    }

    fn start(&mut self) {
        if self.child.is_some() {
            return;
        }
        let log_file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.log_path)
            .unwrap();
        let mut child = Command::new(&self.sshd_bin)
            .args(["-D", "-e", "-f"])
            .arg(&self.config_path)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::from(log_file))
            .spawn()
            .unwrap();

        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if child.try_wait().unwrap().is_some() {
                panic!(
                    "sshd exited early:\n{}",
                    fs::read_to_string(&self.log_path).unwrap_or_default()
                );
            }
            if TcpStream::connect(("127.0.0.1", self.port)).is_ok() {
                self.child = Some(child);
                return;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        let _ = child.kill();
        let _ = child.wait();
        panic!("sshd failed to listen on port {}", self.port);
    }

    fn stop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline {
            if TcpStream::connect(("127.0.0.1", self.port)).is_err() {
                break;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
    }
}

impl Drop for ManagedSshd {
    fn drop(&mut self) {
        self.stop();
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

fn write_rpc_wrapper(
    path: &Path,
    legion_bin: &Path,
    target_legion_home: &Path,
    target_home: &Path,
) {
    let script = format!(
        "#!/bin/sh\nexport LEGION_HOME='{}'\nexport HOME='{}'\nunset XDG_CONFIG_HOME XDG_STATE_HOME\nexec '{}' rpc\n",
        target_legion_home.display(),
        target_home.display(),
        legion_bin.display(),
    );
    fs::write(path, script).unwrap();
    #[cfg(unix)]
    {
        let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o700));
    }
}

#[tokio::test]
async fn real_two_sshd_fleet_learn_direct_routing_failover_and_revocation() {
    let Some(legion_bin) = real_legion_bin_with_sshd(
        "fleet_routes::real_two_sshd_fleet_learn_direct_routing_failover_and_revocation",
    ) else {
        return;
    };

    let root = strict_tempdir();
    let user_a = root.path().join("home-a");
    let user_b = root.path().join("home-b");
    let home_a = root.path().join("legion-a");
    let home_b = root.path().join("legion-b");
    let auth_a = user_a.join(".ssh").join("authorized_keys");
    let auth_b = user_b.join(".ssh").join("authorized_keys");

    for dir in [
        home_a.join("config"),
        home_b.join("config"),
        user_a.join(".ssh"),
        user_b.join(".ssh"),
    ] {
        fs::create_dir_all(&dir).unwrap();
    }
    #[cfg(unix)]
    for dir in [&user_a, &user_b, &user_a.join(".ssh"), &user_b.join(".ssh")] {
        let _ = fs::set_permissions(dir, fs::Permissions::from_mode(0o700));
    }

    let Some(mut sshd_a) = ManagedSshd::new(&root.path().join("sshd-a"), &auth_a, &user_a) else {
        return;
    };
    let Some(mut sshd_b) = ManagedSshd::new(&root.path().join("sshd-b"), &auth_b, &user_b) else {
        return;
    };

    let policy = "default = \"owner\"\n\n\
                  [roles.operator]\n\
                  allow = [\"effect:read\", \"effect:exec\"]\n\n\
                  [principals.phone]\n\
                  role = \"operator\"\n";

    fs::write(
        home_a.join("config/node.toml"),
        format!(
            "name = \"tower\"\n\n[devices]\nhosts = [\"127.0.0.1\"]\nport = {}\nuser = {:?}\nauthorized_keys = {}\nhost_keys = [{}]\n",
            sshd_a.port,
            sshd_a.user,
            toml_literal(&auth_a),
            toml_literal(&sshd_a.host_key_pub),
        ),
    )
    .unwrap();
    fs::write(
        home_b.join("config/node.toml"),
        format!(
            "name = \"pi\"\n\n[devices]\nhosts = [\"127.0.0.1\"]\nport = {}\nuser = {:?}\nauthorized_keys = {}\nhost_keys = [{}]\n",
            sshd_b.port,
            sshd_b.user,
            toml_literal(&auth_b),
            toml_literal(&sshd_b.host_key_pub),
        ),
    )
    .unwrap();
    fs::write(home_a.join("config/policy.toml"), policy).unwrap();
    fs::write(home_b.join("config/policy.toml"), policy).unwrap();

    // Wire A <-> B as a mutual fleet using local rpc wrapper scripts.
    let rpc_to_b = root.path().join("rpc-to-b.sh");
    let rpc_to_a = root.path().join("rpc-to-a.sh");
    write_rpc_wrapper(&rpc_to_b, &legion_bin, &home_b, &user_b);
    write_rpc_wrapper(&rpc_to_a, &legion_bin, &home_a, &user_a);

    fs::write(
        home_a.join("config/fleet.toml"),
        format!(
            "[machines.tower]\nssh = \"127.0.0.1\"\n\n[machines.pi]\ncommand = [{}]\n",
            toml_literal(&rpc_to_b),
        ),
    )
    .unwrap();
    fs::write(
        home_b.join("config/fleet.toml"),
        format!(
            "[machines.pi]\nssh = \"127.0.0.1\"\n\n[machines.tower]\ncommand = [{}]\n",
            toml_literal(&rpc_to_a),
        ),
    )
    .unwrap();

    // 1. Pair with A (`tower`) as owner; enrolment pushes the device to B (`pi`).
    let invite_out = run_legion(
        &legion_bin,
        &home_a,
        &user_a,
        &[
            "device.invite",
            "name=pixel",
            "role=owner",
            "allow_owner=true",
        ],
    );
    let invite_text = invite_out["invite"].as_str().unwrap();

    let link = Link::with_options(
        LinkConfig::default(),
        LinkOptions {
            unsynced_backoff: Duration::from_secs(60),
            allow_loopback_endpoints: true,
            ..LinkOptions::default()
        },
    );
    let enrolled = link.pair(invite_text, "pixel").await.unwrap();
    assert_eq!(enrolled.node, "tower");
    assert_eq!(enrolled.role, "owner");
    assert_eq!(
        enrolled.fleet.get("pi").map(|o| o.status.as_str()),
        Some("synced"),
        "expected pi synced in enrolment reply: {enrolled:?}"
    );

    // 2. Learn fleet endpoints from A -> learns `pi` directly.
    let learned = link.learn().await.unwrap();
    assert_eq!(learned.introducer, "tower");
    assert_eq!(learned.added, vec!["pi".to_owned()]);
    assert!(learned.identity_changed.is_empty());
    let cfg = link.config();
    assert_eq!(cfg.node("tower").unwrap().origin, NodeOrigin::Paired);
    assert_eq!(cfg.node("pi").unwrap().origin, NodeOrigin::Learned);
    assert_eq!(
        cfg.node("pi").unwrap().learned_from.as_deref(),
        Some("tower")
    );

    // 3. Call B (`pi`) directly.
    let res_b = link
        .call_machine("pi", "meta.whoami", json!({}), |_| {})
        .await
        .unwrap();
    assert_eq!(res_b.route, Route::Direct);
    assert_eq!(res_b.value["node"], json!("pi"));

    // 4a. Stop A's sshd while B's local rpc wrapper to A still answers: B still answers directly,
    //     and calls to A seamlessly relay through B (`Route::Relay { via: "pi" }`).
    link.close().await;
    sshd_a.stop();

    let res_b_no_a = link
        .call_machine("pi", "meta.whoami", json!({}), |_| {})
        .await
        .unwrap();
    assert_eq!(res_b_no_a.route, Route::Direct);
    assert_eq!(res_b_no_a.value["node"], json!("pi"));

    let res_a_relayed = link
        .call_machine("tower", "meta.whoami", json!({}), |_| {})
        .await
        .unwrap();
    assert_eq!(
        res_a_relayed.route,
        Route::Relay {
            via: "pi".to_owned()
        }
    );
    assert_eq!(res_a_relayed.value["node"], json!("tower"));

    // 4b. Make A unreachable from B too (simulating A completely off): calls to A fail honestly
    //     (never running on B), while B still answers directly.
    fs::write(&rpc_to_a, "#!/bin/sh\nexit 255\n").unwrap();
    link.close().await;
    let err_a_down = link
        .call_machine("tower", "meta.whoami", json!({}), |_| {})
        .await
        .unwrap_err();
    assert_eq!(err_a_down.code, ErrorCode::Unreachable, "{err_a_down}");
    let res_b_still_ok = link
        .call_machine("pi", "meta.whoami", json!({}), |_| {})
        .await
        .unwrap();
    assert_eq!(res_b_still_ok.route, Route::Direct);
    assert_eq!(res_b_still_ok.value["node"], json!("pi"));
    // Restore B -> A rpc wrapper before restarting A.
    write_rpc_wrapper(&rpc_to_a, &legion_bin, &home_a, &user_a);

    // 5. Start A, stop B: A answers directly.
    sshd_a.start();
    sshd_b.stop();
    link.close().await;

    let res_a_no_b = link
        .call_machine("tower", "meta.whoami", json!({}), |_| {})
        .await
        .unwrap();
    assert_eq!(res_a_no_b.route, Route::Direct);
    assert_eq!(res_a_no_b.value["node"], json!("tower"));

    // 6. Start B, remove our key from B's authorized_keys temporarily (simulating a learned node
    //    that has not received this device yet while A still trusts it): direct B auth refusal
    //    falls back to relay through A (for shared principal `phone`).
    sshd_b.start();
    let invite_shared = run_legion(
        &legion_bin,
        &home_a,
        &user_a,
        &["device.invite", "principal=phone"],
    );
    let shared_link = Link::with_options(
        LinkConfig::default(),
        LinkOptions {
            allow_loopback_endpoints: true,
            ..LinkOptions::default()
        },
    );
    shared_link
        .pair(invite_shared["invite"].as_str().unwrap(), "tablet")
        .await
        .unwrap();
    shared_link.learn().await.unwrap();
    // Strip `tablet`'s line from B's authorized_keys so B rejects direct SSH auth for `tablet`,
    // while `phone` is still a known principal in B's policy.toml for relay via A.
    let auth_b_text = fs::read_to_string(&auth_b).unwrap();
    let stripped: String = auth_b_text
        .lines()
        .filter(|line| !line.contains("--device tablet"))
        .map(|l| format!("{l}\n"))
        .collect();
    fs::write(&auth_b, stripped).unwrap();
    shared_link.close().await;

    let relayed = shared_link
        .call_machine("pi", "meta.whoami", json!({}), |_| {})
        .await
        .unwrap();
    assert_eq!(
        relayed.route,
        Route::Relay {
            via: "tower".to_owned()
        }
    );
    assert_eq!(relayed.value["node"], json!("pi"));
    assert!(matches!(
        shared_link.route_status().get("pi"),
        Some(MachineRouteStatus::NotSyncedYet {
            introducer: Some(intro),
            via: Some(via),
        }) if intro == "tower" && via == "tower"
    ));
    shared_link.close().await;

    // 7. Stop A, remove `pixel` on B while A is stopped: B refuses with `device_removed_or_unknown`.
    //    Then start A and push `devices.toml` from B to A: A also refuses with
    //    `device_removed_or_unknown`.
    link.close().await;
    sshd_a.stop();
    run_legion(
        &legion_bin,
        &home_b,
        &user_b,
        &["device.remove", "name=pixel"],
    );

    let err_b_revoked = link
        .call_machine("pi", "meta.whoami", json!({}), |_| {})
        .await
        .unwrap_err();
    assert_eq!(err_b_revoked.code, ErrorCode::Denied, "{err_b_revoked}");
    assert_eq!(
        err_b_revoked.details.as_ref().unwrap()["reason"],
        json!(REASON_DEVICE_REMOVED_OR_UNKNOWN),
        "{err_b_revoked}"
    );

    sshd_a.start();
    run_legion(
        &legion_bin,
        &home_b,
        &user_b,
        &[
            "fleet.sync",
            "direction=push",
            "documents=[\"devices.toml\"]",
        ],
    );
    link.close().await;

    let err_a_revoked = link
        .call_machine("tower", "meta.whoami", json!({}), |_| {})
        .await
        .unwrap_err();
    assert_eq!(err_a_revoked.code, ErrorCode::Denied, "{err_a_revoked}");
    assert_eq!(
        err_a_revoked.details.as_ref().unwrap()["reason"],
        json!(REASON_DEVICE_REMOVED_OR_UNKNOWN),
        "{err_a_revoked}"
    );

    link.close().await;
}

#[tokio::test]
async fn real_node_endpoint_superset_host_keys_does_not_flag_identity_changed() {
    let Some(legion_bin) = real_legion_bin_with_sshd(
        "fleet_routes::real_node_endpoint_superset_host_keys_does_not_flag_identity_changed",
    ) else {
        return;
    };
    let ssh_keygen_bin = find_tool("ssh-keygen").unwrap();

    let root = strict_tempdir();
    let user_a = root.path().join("home-a");
    let home_a = root.path().join("legion-a");
    let auth_a = user_a.join(".ssh").join("authorized_keys");
    for dir in [home_a.join("config"), user_a.join(".ssh")] {
        fs::create_dir_all(&dir).unwrap();
    }
    #[cfg(unix)]
    for dir in [&user_a, &user_a.join(".ssh")] {
        let _ = fs::set_permissions(dir, fs::Permissions::from_mode(0o700));
    }

    let Some(sshd_a) = ManagedSshd::new(&root.path().join("sshd-a"), &auth_a, &user_a) else {
        return;
    };

    // Generate a second valid host key so `node.toml` can advertise a superset `[key1, key2]`
    // after pairing pinned only `[key1]`.
    let extra_key = root.path().join("sshd-a").join("host_ed25519_extra");
    let extra_key_pub = root.path().join("sshd-a").join("host_ed25519_extra.pub");
    let keygen = Command::new(&ssh_keygen_bin)
        .args(["-q", "-t", "ed25519", "-N", "", "-C", "extra-host-key"])
        .arg("-f")
        .arg(&extra_key)
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(keygen.status.success());

    // Start with only the primary host key pinned in the invite.
    fs::write(
        home_a.join("config/node.toml"),
        format!(
            "name = \"tower\"\n\n[devices]\nhosts = [\"127.0.0.1\"]\nport = {}\nuser = {:?}\nauthorized_keys = {}\nhost_keys = [{}]\n",
            sshd_a.port,
            sshd_a.user,
            toml_literal(&auth_a),
            toml_literal(&sshd_a.host_key_pub),
        ),
    )
    .unwrap();
    fs::write(home_a.join("config/policy.toml"), "default = \"owner\"\n").unwrap();
    fs::write(
        home_a.join("config/fleet.toml"),
        "[machines.tower]\nssh = \"127.0.0.1\"\n",
    )
    .unwrap();

    let invite_out = run_legion(
        &legion_bin,
        &home_a,
        &user_a,
        &[
            "device.invite",
            "name=pixel",
            "role=owner",
            "allow_owner=true",
        ],
    );
    let invite_text = invite_out["invite"].as_str().unwrap();

    let link = Link::with_options(
        LinkConfig::default(),
        LinkOptions {
            allow_loopback_endpoints: true,
            ..LinkOptions::default()
        },
    );
    link.pair(invite_text, "pixel").await.unwrap();
    assert_eq!(
        link.config()
            .node("tower")
            .unwrap()
            .target
            .as_ref()
            .unwrap()
            .host_keys
            .len(),
        1
    );

    // Now configure the real node so `device.endpoint` advertises BOTH host keys (a superset of
    // the pinned key), and verify `learn()` does NOT falsely flag `identity_changed`.
    fs::write(
        home_a.join("config/node.toml"),
        format!(
            "name = \"tower\"\n\n[devices]\nhosts = [\"127.0.0.1\"]\nport = {}\nuser = {:?}\nauthorized_keys = {}\nhost_keys = [{}, {}]\n",
            sshd_a.port,
            sshd_a.user,
            toml_literal(&auth_a),
            toml_literal(&sshd_a.host_key_pub),
            toml_literal(&extra_key_pub),
        ),
    )
    .unwrap();

    let report = link.learn().await.unwrap();
    assert!(
        report.identity_changed.is_empty(),
        "superset of pinned host keys from real node must not flag identity_changed: {report:?}"
    );
    assert!(
        link.config()
            .node("tower")
            .unwrap()
            .identity_changed
            .is_none()
    );
    link.close().await;
}

fn write_node_wrapper(
    path: &Path,
    legion_bin: &Path,
    target_legion_home: &Path,
    target_home: &Path,
) {
    // Passes all arguments (`rpc` or `stream`, swapped by `legion-net`'s command connector) straight
    // through to the target node's `legion` binary.
    let script = format!(
        "#!/bin/sh\nexport LEGION_HOME='{}'\nexport HOME='{}'\nunset XDG_CONFIG_HOME XDG_STATE_HOME\nexec '{}' \"$@\"\n",
        target_legion_home.display(),
        target_home.display(),
        legion_bin.display(),
    );
    fs::write(path, script).unwrap();
    #[cfg(unix)]
    {
        let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o700));
    }
}

fn blake3_via_legion_stream(
    legion_bin: &Path,
    legion_home: &Path,
    user_home: &Path,
    path_in_home: &str,
) -> String {
    use std::io::Write;
    let mut child = Command::new(legion_bin)
        .arg("stream")
        .env("LEGION_HOME", legion_home)
        .env("HOME", user_home)
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("XDG_STATE_HOME")
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
                "protocol": 1,
                "op": "stat",
                "path": path_in_home,
                "checksum": true,
            })
        )
        .unwrap();
    }
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    let reply_line = stdout
        .lines()
        .nth(1)
        .expect("expected greeting + stat reply");
    let reply: Value = serde_json::from_str(reply_line).unwrap();
    reply["hash"].as_str().unwrap().to_owned()
}

#[tokio::test]
async fn real_two_sshd_relayed_streams_then_learn_switches_to_direct() {
    let Some(legion_bin) = real_legion_bin_with_sshd(
        "fleet_routes::real_two_sshd_relayed_streams_then_learn_switches_to_direct",
    ) else {
        return;
    };

    let root = strict_tempdir();
    let user_a = root.path().join("home-a");
    let user_b = root.path().join("home-b");
    let home_a = root.path().join("legion-a");
    let home_b = root.path().join("legion-b");
    let auth_a = user_a.join(".ssh").join("authorized_keys");
    let auth_b = user_b.join(".ssh").join("authorized_keys");

    for dir in [
        home_a.join("config"),
        home_b.join("config"),
        user_a.join(".ssh"),
        user_b.join(".ssh"),
        user_b.join("media"),
        user_b.join("uploads"),
    ] {
        fs::create_dir_all(&dir).unwrap();
    }
    #[cfg(unix)]
    for dir in [&user_a, &user_b, &user_a.join(".ssh"), &user_b.join(".ssh")] {
        let _ = fs::set_permissions(dir, fs::Permissions::from_mode(0o700));
    }

    let Some(sshd_a) = ManagedSshd::new(&root.path().join("sshd-a"), &auth_a, &user_a) else {
        return;
    };
    let Some(sshd_b) = ManagedSshd::new(&root.path().join("sshd-b"), &auth_b, &user_b) else {
        return;
    };

    fs::write(
        home_a.join("config/node.toml"),
        format!(
            "name = \"tower\"\n\n[devices]\nhosts = [\"127.0.0.1\"]\nport = {}\nuser = {:?}\nauthorized_keys = {}\nhost_keys = [{}]\n",
            sshd_a.port,
            sshd_a.user,
            toml_literal(&auth_a),
            toml_literal(&sshd_a.host_key_pub),
        ),
    )
    .unwrap();
    fs::write(
        home_b.join("config/node.toml"),
        format!(
            "name = \"pi\"\n\n[devices]\nhosts = [\"127.0.0.1\"]\nport = {}\nuser = {:?}\nauthorized_keys = {}\nhost_keys = [{}]\n",
            sshd_b.port,
            sshd_b.user,
            toml_literal(&auth_b),
            toml_literal(&sshd_b.host_key_pub),
        ),
    )
    .unwrap();
    let policy = "default = \"owner\"\n";
    fs::write(home_a.join("config/policy.toml"), policy).unwrap();
    fs::write(home_b.join("config/policy.toml"), policy).unwrap();

    // Wire A <-> B using wrappers that accept `"rpc"` OR `"stream"` as their trailing arg.
    let node_b_exec = root.path().join("node-b.sh");
    let node_a_exec = root.path().join("node-a.sh");
    write_node_wrapper(&node_b_exec, &legion_bin, &home_b, &user_b);
    write_node_wrapper(&node_a_exec, &legion_bin, &home_a, &user_a);

    fs::write(
        home_a.join("config/fleet.toml"),
        format!(
            "[machines.tower]\nssh = \"127.0.0.1\"\n\n[machines.pi]\ncommand = [{}, \"rpc\"]\n",
            toml_literal(&node_b_exec),
        ),
    )
    .unwrap();
    fs::write(
        home_b.join("config/fleet.toml"),
        format!(
            "[machines.pi]\nssh = \"127.0.0.1\"\n\n[machines.tower]\ncommand = [{}, \"rpc\"]\n",
            toml_literal(&node_a_exec),
        ),
    )
    .unwrap();

    // Seed a sample file on `pi` and a second payload to upload.
    let sample_media: Vec<u8> = (0..180_000usize)
        .map(|i| ((i * 11 + 5) % 251) as u8)
        .collect();
    fs::write(user_b.join("media/clip.bin"), &sample_media).unwrap();
    let upload_payload: Vec<u8> = (0..128 * 1024usize)
        .map(|i| ((i * 5 + 3) % 251) as u8)
        .collect();
    fs::write(user_a.join("seed-upload.bin"), &upload_payload).unwrap();
    let upload_hash = blake3_via_legion_stream(&legion_bin, &home_a, &user_a, "~/seed-upload.bin");

    // 1. Pair with `tower` ONLY (do NOT call `learn()` yet — device has NO direct entry for `pi`).
    let invite_out = run_legion(
        &legion_bin,
        &home_a,
        &user_a,
        &[
            "device.invite",
            "name=pixel",
            "role=owner",
            "allow_owner=true",
        ],
    );
    let invite_text = invite_out["invite"].as_str().unwrap();
    let link = Link::with_options(
        LinkConfig::default(),
        LinkOptions {
            allow_loopback_endpoints: true,
            ..LinkOptions::default()
        },
    );
    link.pair(invite_text, "pixel").await.unwrap();
    assert!(link.config().node("pi").is_none());

    // 2. Relayed ranged `open_content` on `pi` through `tower`.
    let mut relayed_get = link
        .open_content("pi", "~/media/clip.bin", 12_345, Some(40_000))
        .await
        .unwrap();
    assert_eq!(
        relayed_get.route(),
        &Route::Relay {
            via: "tower".to_owned()
        }
    );
    let mut got = Vec::new();
    relayed_get.read_to_end(&mut got).await.unwrap();
    assert_eq!(got, &sample_media[12_345..52_345]);
    assert_eq!(
        link.route_status().get("pi"),
        Some(&MachineRouteStatus::Relay {
            via: "tower".to_owned()
        })
    );

    // 3. Relayed `open_put` with mid-upload cancel, partial check, and resumed completion.
    {
        let mut partial_put = link
            .open_put(
                "pi",
                PutRequest {
                    path: "~/uploads/relayed.bin".to_owned(),
                    size: upload_payload.len() as u64,
                    hash: upload_hash.clone(),
                    mode: None,
                    mtime: None,
                    if_exists: PutIfExists::Replace,
                },
            )
            .await
            .unwrap();
        assert_eq!(
            partial_put.route(),
            &Route::Relay {
                via: "tower".to_owned()
            }
        );
        partial_put
            .write_all(&upload_payload[..48 * 1024])
            .await
            .unwrap();
        partial_put.flush().await.unwrap();
        // Wait until the bytes have crossed sshd -> tower -> pi onto disk before cancelling.
        let part_file = user_b.join("uploads/relayed.bin.legion-part");
        let part_deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < part_deadline {
            if fs::metadata(&part_file).is_ok_and(|m| m.len() >= 48 * 1024) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        partial_put.cancel();
    }
    // Wait briefly for target to flush partial file on cancel, then resume over relay.
    tokio::time::sleep(Duration::from_millis(150)).await;
    {
        let mut resumed_put = link
            .open_put(
                "pi",
                PutRequest {
                    path: "~/uploads/relayed.bin".to_owned(),
                    size: upload_payload.len() as u64,
                    hash: upload_hash.clone(),
                    mode: None,
                    mtime: None,
                    if_exists: PutIfExists::Replace,
                },
            )
            .await
            .unwrap();
        assert_eq!(
            resumed_put.route(),
            &Route::Relay {
                via: "tower".to_owned()
            }
        );
        let resume_at = resumed_put.offset() as usize;
        assert!(
            resume_at > 0 && resume_at <= 48 * 1024,
            "expected partial resume offset after cancel, got {resume_at}"
        );
        resumed_put
            .write_all(&upload_payload[resume_at..])
            .await
            .unwrap();
        let done = resumed_put.finish().await.unwrap();
        assert_eq!(done.size, upload_payload.len() as u64);
        assert_eq!(done.hash, upload_hash);
    }
    assert_eq!(
        fs::read(user_b.join("uploads/relayed.bin")).unwrap(),
        upload_payload
    );

    // 4. Learn `pi` directly -> next `open_content` and `open_put` switch cleanly to `Route::Direct`.
    let learned = link.learn().await.unwrap();
    assert_eq!(learned.added, vec!["pi".to_owned()]);

    let mut direct_get = link
        .open_content("pi", "~/media/clip.bin", 0, Some(16_384))
        .await
        .unwrap();
    assert_eq!(direct_get.route(), &Route::Direct);
    let mut got_direct = Vec::new();
    direct_get.read_to_end(&mut got_direct).await.unwrap();
    assert_eq!(got_direct, &sample_media[..16_384]);
    assert_eq!(
        link.route_status().get("pi"),
        Some(&MachineRouteStatus::Direct)
    );

    let mut direct_put = link
        .open_put(
            "pi",
            PutRequest {
                path: "~/uploads/direct.bin".to_owned(),
                size: upload_payload.len() as u64,
                hash: upload_hash.clone(),
                mode: None,
                mtime: None,
                if_exists: PutIfExists::Replace,
            },
        )
        .await
        .unwrap();
    assert_eq!(direct_put.route(), &Route::Direct);
    direct_put.write_all(&upload_payload).await.unwrap();
    let done_direct = direct_put.finish().await.unwrap();
    assert_eq!(done_direct.size, upload_payload.len() as u64);
    assert_eq!(
        fs::read(user_b.join("uploads/direct.bin")).unwrap(),
        upload_payload
    );

    drop(sshd_a);
    drop(sshd_b);
    link.close().await;
}

#[tokio::test]
async fn real_two_sshd_direct_and_relayed_put_if_exists_fail_refuses_raced_file_atomically() {
    let Some(legion_bin) = real_legion_bin_with_sshd(
        "fleet_routes::real_two_sshd_direct_and_relayed_put_if_exists_fail_refuses_raced_file_atomically",
    ) else {
        return;
    };

    let root = strict_tempdir();
    let user_a = root.path().join("home-a");
    let user_b = root.path().join("home-b");
    let home_a = root.path().join("legion-a");
    let home_b = root.path().join("legion-b");
    let auth_a = user_a.join(".ssh").join("authorized_keys");
    let auth_b = user_b.join(".ssh").join("authorized_keys");

    for dir in [
        home_a.join("config"),
        home_b.join("config"),
        user_a.join(".ssh"),
        user_b.join(".ssh"),
        user_a.join("uploads"),
        user_b.join("uploads"),
    ] {
        fs::create_dir_all(&dir).unwrap();
    }
    #[cfg(unix)]
    for dir in [&user_a, &user_b, &user_a.join(".ssh"), &user_b.join(".ssh")] {
        let _ = fs::set_permissions(dir, fs::Permissions::from_mode(0o700));
    }

    let Some(sshd_a) = ManagedSshd::new(&root.path().join("sshd-a"), &auth_a, &user_a) else {
        return;
    };
    let Some(sshd_b) = ManagedSshd::new(&root.path().join("sshd-b"), &auth_b, &user_b) else {
        return;
    };

    fs::write(
        home_a.join("config/node.toml"),
        format!(
            "name = \"tower\"\n\n[devices]\nhosts = [\"127.0.0.1\"]\nport = {}\nuser = {:?}\nauthorized_keys = {}\nhost_keys = [{}]\n",
            sshd_a.port,
            sshd_a.user,
            toml_literal(&auth_a),
            toml_literal(&sshd_a.host_key_pub),
        ),
    )
    .unwrap();
    fs::write(
        home_b.join("config/node.toml"),
        format!(
            "name = \"pi\"\n\n[devices]\nhosts = [\"127.0.0.1\"]\nport = {}\nuser = {:?}\nauthorized_keys = {}\nhost_keys = [{}]\n",
            sshd_b.port,
            sshd_b.user,
            toml_literal(&auth_b),
            toml_literal(&sshd_b.host_key_pub),
        ),
    )
    .unwrap();
    let policy = "default = \"owner\"\n";
    fs::write(home_a.join("config/policy.toml"), policy).unwrap();
    fs::write(home_b.join("config/policy.toml"), policy).unwrap();

    let node_b_exec = root.path().join("node-b.sh");
    let node_a_exec = root.path().join("node-a.sh");
    write_node_wrapper(&node_b_exec, &legion_bin, &home_b, &user_b);
    write_node_wrapper(&node_a_exec, &legion_bin, &home_a, &user_a);

    fs::write(
        home_a.join("config/fleet.toml"),
        format!(
            "[machines.tower]\nssh = \"127.0.0.1\"\n\n[machines.pi]\ncommand = [{}, \"rpc\"]\n",
            toml_literal(&node_b_exec),
        ),
    )
    .unwrap();
    fs::write(
        home_b.join("config/fleet.toml"),
        format!(
            "[machines.pi]\nssh = \"127.0.0.1\"\n\n[machines.tower]\ncommand = [{}, \"rpc\"]\n",
            toml_literal(&node_a_exec),
        ),
    )
    .unwrap();

    let payload = b"atomic-if-exists-upload-payload-bytes".to_vec();
    fs::write(user_a.join("seed-if-exists.bin"), &payload).unwrap();
    let payload_hash =
        blake3_via_legion_stream(&legion_bin, &home_a, &user_a, "~/seed-if-exists.bin");

    // Pair with `tower` only so `tower` is direct and `pi` is relayed via `tower`.
    let invite_out = run_legion(
        &legion_bin,
        &home_a,
        &user_a,
        &[
            "device.invite",
            "name=pixel",
            "role=owner",
            "allow_owner=true",
        ],
    );
    let invite_text = invite_out["invite"].as_str().unwrap();
    let link = Link::with_options(
        LinkConfig::default(),
        LinkOptions {
            allow_loopback_endpoints: true,
            ..LinkOptions::default()
        },
    );
    link.pair(invite_text, "pixel").await.unwrap();
    assert!(link.config().node("pi").is_none());

    // 1. Direct upload on `tower`: create the colliding file *between `fs.stat` and `put`*.
    //    `fs.stat` sees NotFound, then the file appears on disk before `put`; `"if_exists": "fail"`
    //    must still refuse atomically with `ErrorCode::Exists` and cache `CAP_IF_EXISTS`.
    let stat_direct = link
        .call_machine(
            "tower",
            "fs.stat",
            json!({ "path": "~/uploads/raced-direct.bin" }),
            |_| {},
        )
        .await
        .unwrap_err();
    assert_eq!(stat_direct.code, ErrorCode::NotFound, "{stat_direct}");
    let direct_raced_contents = b"created-between-stat-and-put-direct";
    fs::write(
        user_a.join("uploads/raced-direct.bin"),
        direct_raced_contents,
    )
    .unwrap();

    let err = link
        .open_put(
            "tower",
            PutRequest {
                path: "~/uploads/raced-direct.bin".to_owned(),
                size: payload.len() as u64,
                hash: payload_hash.clone(),
                mode: Some(0o644),
                mtime: None,
                if_exists: PutIfExists::Fail,
            },
        )
        .await
        .err()
        .unwrap();
    assert_eq!(err.code, ErrorCode::Exists, "{err}");
    assert_eq!(err.machine.as_deref(), Some("tower"), "{err}");
    assert_eq!(
        link.stream_supports("tower", CAP_IF_EXISTS),
        Some(true),
        "current node must advertise and cache if_exists even when first put returns Exists"
    );
    assert_eq!(
        fs::read(user_a.join("uploads/raced-direct.bin")).unwrap(),
        direct_raced_contents,
        "direct raced file must remain untouched"
    );

    // Retry onto sibling path succeeds and reports `if_exists_supported() == true`.
    let mut direct_sibling = link
        .open_put(
            "tower",
            PutRequest {
                path: "~/uploads/raced-direct (2).bin".to_owned(),
                size: payload.len() as u64,
                hash: payload_hash.clone(),
                mode: Some(0o644),
                mtime: None,
                if_exists: PutIfExists::Fail,
            },
        )
        .await
        .unwrap();
    assert_eq!(direct_sibling.route(), &Route::Direct);
    assert!(direct_sibling.if_exists_supported());
    direct_sibling.write_all(&payload).await.unwrap();
    let done = direct_sibling.finish().await.unwrap();
    assert_eq!(done.hash, payload_hash);
    assert_eq!(
        fs::read(user_a.join("uploads/raced-direct (2).bin")).unwrap(),
        payload
    );

    // 2. Direct upload commit race on `tower`: file appears *after `PutReady`, before `PutDone`*.
    let mut direct_commit_race = link
        .open_put(
            "tower",
            PutRequest {
                path: "~/uploads/raced-commit-direct.bin".to_owned(),
                size: payload.len() as u64,
                hash: payload_hash.clone(),
                mode: Some(0o644),
                mtime: None,
                if_exists: PutIfExists::Fail,
            },
        )
        .await
        .unwrap();
    let direct_commit_contents = b"created-during-upload-direct";
    fs::write(
        user_a.join("uploads/raced-commit-direct.bin"),
        direct_commit_contents,
    )
    .unwrap();
    direct_commit_race.write_all(&payload).await.unwrap();
    let err = direct_commit_race.finish().await.unwrap_err();
    assert_eq!(err.code, ErrorCode::Exists, "{err}");
    assert_eq!(err.machine.as_deref(), Some("tower"), "{err}");
    assert_eq!(
        fs::read(user_a.join("uploads/raced-commit-direct.bin")).unwrap(),
        direct_commit_contents
    );

    // 3. Relayed upload on `pi` via `tower`: create the colliding file *between `fs.stat` and `put`*.
    let stat_relayed = link
        .call_machine(
            "pi",
            "fs.stat",
            json!({ "path": "~/uploads/raced-relayed.bin" }),
            |_| {},
        )
        .await
        .unwrap_err();
    assert_eq!(stat_relayed.code, ErrorCode::NotFound, "{stat_relayed}");
    let relayed_raced_contents = b"created-between-stat-and-put-relayed";
    fs::write(
        user_b.join("uploads/raced-relayed.bin"),
        relayed_raced_contents,
    )
    .unwrap();

    let err = link
        .open_put(
            "pi",
            PutRequest {
                path: "~/uploads/raced-relayed.bin".to_owned(),
                size: payload.len() as u64,
                hash: payload_hash.clone(),
                mode: Some(0o644),
                mtime: None,
                if_exists: PutIfExists::Fail,
            },
        )
        .await
        .err()
        .unwrap();
    assert_eq!(err.code, ErrorCode::Exists, "{err}");
    assert_eq!(err.machine.as_deref(), Some("pi"), "{err}");
    assert_eq!(
        fs::read(user_b.join("uploads/raced-relayed.bin")).unwrap(),
        relayed_raced_contents,
        "relayed raced file on pi must remain untouched"
    );

    // Retry onto sibling path over relay succeeds.
    let mut relayed_sibling = link
        .open_put(
            "pi",
            PutRequest {
                path: "~/uploads/raced-relayed (2).bin".to_owned(),
                size: payload.len() as u64,
                hash: payload_hash.clone(),
                mode: Some(0o644),
                mtime: None,
                if_exists: PutIfExists::Fail,
            },
        )
        .await
        .unwrap();
    assert_eq!(
        relayed_sibling.route(),
        &Route::Relay {
            via: "tower".to_owned()
        }
    );
    relayed_sibling.write_all(&payload).await.unwrap();
    let done = relayed_sibling.finish().await.unwrap();
    assert_eq!(done.hash, payload_hash);
    assert_eq!(
        fs::read(user_b.join("uploads/raced-relayed (2).bin")).unwrap(),
        payload
    );

    // 4. Relayed upload commit race on `pi`: file appears *after `PutReady`, before `PutDone`*.
    let mut relayed_commit_race = link
        .open_put(
            "pi",
            PutRequest {
                path: "~/uploads/raced-commit-relayed.bin".to_owned(),
                size: payload.len() as u64,
                hash: payload_hash.clone(),
                mode: Some(0o644),
                mtime: None,
                if_exists: PutIfExists::Fail,
            },
        )
        .await
        .unwrap();
    assert_eq!(
        relayed_commit_race.route(),
        &Route::Relay {
            via: "tower".to_owned()
        }
    );
    let relayed_commit_contents = b"created-during-upload-relayed";
    fs::write(
        user_b.join("uploads/raced-commit-relayed.bin"),
        relayed_commit_contents,
    )
    .unwrap();
    relayed_commit_race.write_all(&payload).await.unwrap();
    let err = relayed_commit_race.finish().await.unwrap_err();
    assert_eq!(err.code, ErrorCode::Exists, "{err}");
    assert_eq!(err.machine.as_deref(), Some("pi"), "{err}");
    assert_eq!(
        fs::read(user_b.join("uploads/raced-commit-relayed.bin")).unwrap(),
        relayed_commit_contents
    );

    drop(sshd_a);
    drop(sshd_b);
    link.close().await;
}

#[tokio::test]
async fn real_two_sshd_learn_surfaces_name_mismatch_rejection_from_node() {
    let Some(legion_bin) = real_legion_bin_with_sshd(
        "fleet_routes::real_two_sshd_learn_surfaces_name_mismatch_rejection_from_node",
    ) else {
        return;
    };

    let root = strict_tempdir();
    let user_a = root.path().join("home-a");
    let user_b = root.path().join("home-b");
    let home_a = root.path().join("legion-a");
    let home_b = root.path().join("legion-b");
    let auth_a = user_a.join(".ssh").join("authorized_keys");
    let auth_b = user_b.join(".ssh").join("authorized_keys");

    for dir in [
        home_a.join("config"),
        home_b.join("config"),
        user_a.join(".ssh"),
        user_b.join(".ssh"),
    ] {
        fs::create_dir_all(&dir).unwrap();
    }
    #[cfg(unix)]
    for dir in [&user_a, &user_b, &user_a.join(".ssh"), &user_b.join(".ssh")] {
        let _ = fs::set_permissions(dir, fs::Permissions::from_mode(0o700));
    }

    let Some(sshd_a) = ManagedSshd::new(&root.path().join("sshd-a"), &auth_a, &user_a) else {
        return;
    };
    let Some(sshd_b) = ManagedSshd::new(&root.path().join("sshd-b"), &auth_b, &user_b) else {
        return;
    };

    // Node A is `tower`; Node B explicitly names itself `someones-laptop` while `tower`'s
    // `fleet.toml` addresses it as `studio`. Forwarding `device.endpoint` from `tower` to `studio`
    // must produce main's real `conflict` + `details.kind = "name_mismatch"`, and `Link::learn()`
    // must surface that as a `name_mismatch` rejection rather than `unreachable`.
    fs::write(
        home_a.join("config/node.toml"),
        format!(
            "name = \"tower\"\n\n[devices]\nhosts = [\"127.0.0.1\"]\nport = {}\nuser = {:?}\nauthorized_keys = {}\nhost_keys = [{}]\n",
            sshd_a.port,
            sshd_a.user,
            toml_literal(&auth_a),
            toml_literal(&sshd_a.host_key_pub),
        ),
    )
    .unwrap();
    fs::write(
        home_b.join("config/node.toml"),
        format!(
            "name = \"someones-laptop\"\n\n[devices]\nhosts = [\"127.0.0.1\"]\nport = {}\nuser = {:?}\nauthorized_keys = {}\nhost_keys = [{}]\n",
            sshd_b.port,
            sshd_b.user,
            toml_literal(&auth_b),
            toml_literal(&sshd_b.host_key_pub),
        ),
    )
    .unwrap();
    let policy = "default = \"owner\"\n";
    fs::write(home_a.join("config/policy.toml"), policy).unwrap();
    fs::write(home_b.join("config/policy.toml"), policy).unwrap();

    let node_b_exec = root.path().join("node-b.sh");
    write_node_wrapper(&node_b_exec, &legion_bin, &home_b, &user_b);
    fs::write(
        home_a.join("config/fleet.toml"),
        format!(
            "[machines.tower]\nssh = \"127.0.0.1\"\n\n[machines.studio]\ncommand = [{}, \"rpc\"]\n",
            toml_literal(&node_b_exec),
        ),
    )
    .unwrap();

    let invite_out = run_legion(
        &legion_bin,
        &home_a,
        &user_a,
        &[
            "device.invite",
            "name=pixel",
            "role=owner",
            "allow_owner=true",
        ],
    );
    let invite_text = invite_out["invite"].as_str().unwrap();
    let link = Link::with_options(
        LinkConfig::default(),
        LinkOptions {
            allow_loopback_endpoints: true,
            ..LinkOptions::default()
        },
    );
    link.pair(invite_text, "pixel").await.unwrap();

    let report = link.learn().await.unwrap();
    assert!(
        !report.unreachable.contains(&"studio".to_owned()),
        "real name_mismatch from tower must never be classified as unreachable: {report:?}"
    );
    assert!(
        link.config().node("studio").is_none(),
        "mismatched node must not be learned into LinkConfig"
    );
    let studio_rej = report
        .rejected_details
        .iter()
        .find(|r| r.machine == "studio")
        .unwrap_or_else(|| panic!("expected studio rejection in {report:?}"));
    assert_eq!(studio_rej.code, "name_mismatch", "{studio_rej:?}");
    assert_eq!(studio_rej.reported_by.as_deref(), Some("tower"));
    assert!(
        studio_rej.reason.contains("someones-laptop"),
        "expected node's real mismatch sentence naming someones-laptop: {}",
        studio_rej.reason
    );

    drop(sshd_a);
    drop(sshd_b);
    link.close().await;
}
