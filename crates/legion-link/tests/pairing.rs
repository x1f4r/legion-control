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

use std::env;
use std::fs;
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use legion_link::proto::{ErrorCode, PROTOCOL};
use legion_link::{
    CancellationToken, DeviceKey, Invite, Link, LinkConfig, NodeLink, REASON_AUTH_FAILED,
    SshTimeouts, enrol,
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
            .args(["-T", "-o", "PerSourcePenalties=no", "-h"])
            .arg(&host_key)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success());

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

            let deadline = Instant::now() + Duration::from_secs(5);
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
        }
        panic!("throwaway sshd failed to start");
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
    if !cfg!(unix) || find_sshd().is_none() || find_tool("ssh-keygen").is_none() {
        eprintln!(
            "skipping real_sshd_pairing_with_legion_link_enrol_stream_and_revoke: sshd/ssh-keygen unavailable"
        );
        return;
    }
    let Some(legion_bin) = find_legion_bin() else {
        eprintln!(
            "skipping real_sshd_pairing_with_legion_link_enrol_stream_and_revoke: built `legion` binary not found (build with `cargo build -p legion-cli` or set LEGION_BIN)"
        );
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

    // Verify the installed forced command carries `--as`, `--device`, and `--home`.
    let installed_auth = fs::read_to_string(&auth_keys).unwrap();
    assert!(
        installed_auth.contains("--as phone")
            && installed_auth.contains("--device link-phone")
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
    assert_eq!(whoami["door"], json!("rpc"));

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
    // and fails with Denied / auth_failed.
    let reconnect_err = if first_after.code == ErrorCode::Denied {
        first_after
    } else {
        link.call("tower", "meta.whoami", json!({}), None, |_| {})
            .await
            .expect_err("reconnecting after device.remove unexpectedly succeeded")
    };
    assert_eq!(reconnect_err.code, ErrorCode::Denied, "{reconnect_err}");
    assert_eq!(
        reconnect_err.details.as_ref().unwrap()["reason"],
        json!(REASON_AUTH_FAILED)
    );

    link.close().await;
}
