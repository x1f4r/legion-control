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
    if !cfg!(unix) || find_sshd().is_none() || find_tool("ssh-keygen").is_none() {
        eprintln!(
            "skipping real_sshd_late_enrolment_confirmation_and_retry_recover_cleanly: sshd/ssh-keygen unavailable"
        );
        return;
    }
    let Some(legion_bin) = find_legion_bin() else {
        eprintln!(
            "skipping real_sshd_late_enrolment_confirmation_and_retry_recover_cleanly: built `legion` binary not found"
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

    // Scenario A: Client gives up waiting for confirmation while the node finishes enrolling
    // (`enrol` timeout = 1ms). Either the immediate post-timeout device-key probe confirms
    // enrolment or retrying the same invite once the node finishes commits recovers cleanly.
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
    let invite_a_text = invite_a_res["invite"].as_str().unwrap();

    let key_a = DeviceKey::generate().unwrap();
    let short_opts = legion_link::LinkOptions {
        ssh_timeouts: SshTimeouts {
            enrol: Duration::from_millis(1),
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

    let first_attempt = link_a.pair(invite_a_text, "late-phone").await;
    let enrolled_a = match first_attempt {
        Ok(enrolled) => enrolled,
        Err(err) => {
            assert_eq!(
                err.code,
                ErrorCode::Timeout,
                "unexpected first error: {err}"
            );
            assert!(
                link_a.config().node("tower").is_none(),
                "timed-out pair must not save half-paired node locally"
            );
            // Wait deterministically for the node's in-flight enrolment to finish committing
            // authorized_keys (simulating the node finishing a couple seconds after client timeout).
            let deadline = Instant::now() + Duration::from_secs(5);
            while Instant::now() < deadline {
                if fs::read_to_string(&auth_keys)
                    .unwrap_or_default()
                    .contains("--device late-phone")
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
            assert!(
                fs::read_to_string(&auth_keys)
                    .unwrap_or_default()
                    .contains("--device late-phone"),
                "node did not finish committing late-phone enrolment"
            );
            // Retry with the exact same invite: temp key is already gone, so recovery probes
            // `device_key` + `meta.whoami` and completes pairing cleanly.
            link_a
                .pair(invite_a_text, "late-phone")
                .await
                .unwrap_or_else(|e| {
                    panic!(
                        "retry after late enrolment failed: {e}\n--- sshd log ---\n{}",
                        sshd.log()
                    )
                })
        }
    };
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
    if !cfg!(unix) || find_sshd().is_none() || find_tool("ssh-keygen").is_none() {
        eprintln!(
            "skipping real_sshd_refused_single_use_key_recovers_only_for_device_principals: sshd/ssh-keygen unavailable"
        );
        return;
    }
    let Some(legion_bin) = find_legion_bin() else {
        eprintln!(
            "skipping real_sshd_refused_single_use_key_recovers_only_for_device_principals: built `legion` binary not found"
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
    if !cfg!(unix) || find_sshd().is_none() || find_tool("ssh-keygen").is_none() {
        eprintln!(
            "skipping real_sshd_cancel_before_and_after_send_behaves_honestly: sshd/ssh-keygen unavailable"
        );
        return;
    }
    let Some(legion_bin) = find_legion_bin() else {
        eprintln!(
            "skipping real_sshd_cancel_before_and_after_send_behaves_honestly: built `legion` binary not found"
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

    // 2. Cancel RIGHT AFTER the machine commits `authorized_keys` (reproducing the race where
    //    cancel fires after the request left): pairing MUST succeed and save `tower` instead of
    //    leaving the device half-paired.
    let post_cancel = CancellationToken::new();
    let cancel_trigger = post_cancel.clone();
    let ak_path = auth_keys.clone();
    let watcher = tokio::spawn(async move {
        let before = fs::read_to_string(&ak_path).unwrap_or_default();
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            let now = fs::read_to_string(&ak_path).unwrap_or_default();
            if now != before && now.contains("--device cancel-phone") {
                cancel_trigger.cancel();
                break;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    });

    let enrolled = link
        .pair_cancellable(inv_text, "cancel-phone", post_cancel)
        .await
        .unwrap_or_else(|e| {
            panic!(
                "cancel after send must save completed pairing instead of failing: {e}\n--- sshd log ---\n{}",
                sshd.log()
            )
        });
    let _ = watcher.await;
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
    if !cfg!(unix) || find_sshd().is_none() || find_tool("ssh-keygen").is_none() {
        eprintln!(
            "skipping real_sshd_unconfirmed_lifecycle_confirmed_conflict_restore_and_compat: sshd/ssh-keygen unavailable"
        );
        return;
    }
    let Some(legion_bin) = find_legion_bin() else {
        eprintln!(
            "skipping real_sshd_unconfirmed_lifecycle_confirmed_conflict_restore_and_compat: built `legion` binary not found"
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
            "[machines.slowpeer]\ncommand = [\"/bin/sh\", \"-c\", \"touch '{}'; i=0; while [ ! -f '{}' ] && [ $i -lt 250 ]; do sleep 0.02; i=$((i+1)); done; exit 1\"]\n",
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
            let deadline = Instant::now() + Duration::from_secs(10);
            while !entered.exists() {
                assert!(
                    Instant::now() < deadline,
                    "timed out waiting for enrol to reach fleet push"
                );
                tokio::time::sleep(Duration::from_millis(10)).await;
            }

            // Client vanishes while machine is still finishing before writing its stdout reply.
            drop(writer);
            drop(reader);
            Box::new(handle).close(Duration::from_millis(500)).await;
            tokio::time::sleep(Duration::from_millis(150)).await;
            let _ = fs::write(&release, b"go\n");

            // Wait for `legion gate --enrol` to finish and append its own `tool: "device.enrol"` audit entry.
            let audit_deadline = Instant::now() + Duration::from_secs(10);
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
                    "timed out waiting for device.enrol audit entry"
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
        devices_toml_1.contains("unconfirmed = true"),
        "devices.toml must record unconfirmed = true:\n{devices_toml_1}\n--- audit ---\n{}\n--- sshd ---\n{}",
        fs::read_to_string(&audit_path).unwrap_or_default(),
        sshd.log(),
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

    // 2. Fresh invite presented by the SAME device key replaces the unconfirmed leftover and
    //    succeeds cleanly over real `sshd`.
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
