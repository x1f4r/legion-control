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

use std::env;
use std::fs;
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use legion_link::proto::ErrorCode;
use legion_link::{
    Link, LinkConfig, LinkOptions, MachineRouteStatus, NodeOrigin,
    REASON_DEVICE_REMOVED_OR_UNKNOWN, Route,
};
use serde_json::{Value, json};
use tempfile::TempDir;

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

fn find_tool(name: &str) -> Option<PathBuf> {
    let path_var = env::var_os("PATH")?;
    for dir in env::split_paths(&path_var) {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

fn find_sshd() -> Option<PathBuf> {
    if let Some(p) = find_tool("sshd") {
        return Some(p);
    }
    for fallback in ["/usr/sbin/sshd", "/sbin/sshd", "/usr/local/sbin/sshd"] {
        let p = PathBuf::from(fallback);
        if p.is_file() {
            return Some(p);
        }
    }
    None
}

fn find_legion_bin() -> Option<PathBuf> {
    if let Some(explicit) = env::var_os("LEGION_BIN") {
        let p = PathBuf::from(explicit);
        if p.is_file() {
            return Some(p);
        }
    }
    let exe_name = format!("legion{}", env::consts::EXE_SUFFIX);
    let mut target_dirs = Vec::new();
    if let Some(tmpdir) = option_env!("CARGO_TARGET_TMPDIR")
        && let Some(parent) = Path::new(tmpdir).parent()
    {
        target_dirs.push(parent.to_path_buf());
    }
    if let Some(repo_root) = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
    {
        target_dirs.push(repo_root.join("target"));
    }
    for target_dir in target_dirs {
        for profile in ["debug", "release"] {
            let candidate = target_dir.join(profile).join(&exe_name);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

fn current_user() -> Option<String> {
    let output = Command::new("id")
        .arg("-un")
        .stdin(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let name = String::from_utf8(output.stdout).ok()?.trim().to_owned();
    (!name.is_empty()).then_some(name)
}

fn strict_tempdir() -> TempDir {
    let dir = if let Some(base) = option_env!("CARGO_TARGET_TMPDIR") {
        let base_path = PathBuf::from(base);
        fs::create_dir_all(&base_path).unwrap();
        tempfile::tempdir_in(&base_path).unwrap()
    } else {
        tempfile::tempdir().unwrap()
    };
    #[cfg(unix)]
    {
        let _ = fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700));
    }
    dir
}

fn toml_literal(path: &Path) -> String {
    format!("'{}'", path.display().to_string().replace('\'', "''"))
}

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
    if !cfg!(unix) || find_sshd().is_none() || find_tool("ssh-keygen").is_none() {
        eprintln!("skipping two-sshd fleet test: sshd/ssh-keygen unavailable");
        return;
    }
    let Some(legion_bin) = find_legion_bin() else {
        panic!(
            "built `legion` binary not found; run `cargo build -p legion-cli` (or set LEGION_BIN) before running fleet_routes"
        );
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
