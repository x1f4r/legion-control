//! End-to-end integration tests for sealed relayed sessions (`op: "tunnel"` + `russh` inside
//! `legion stream` + per-node ratchet):
//!
//! 1. Real two-sshd sealed session over real node-to-node SSH: RPC, `tools_machine`, concurrent
//!    `open_content`, `open_put` (`if_exists_supported()`), concurrent relay RPC, single-tunnel
//!    reuse, transparent reconnect after target sshd restart, host-key MITM refusal without
//!    downgrade, and clean teardown with zero leftover processes.
//! 2. Dual-boot `systems` failover, 2-hop chained `via`, and non-`127.0.0.1` `ListenAddress`
//!    (`127.0.0.2`) advertised-address fallback.
//! 3. Ratchet & interoperability against older relays and targets (always tested via real binaries
//!    plus optional `LEGION_MAIN_BIN` built from `origin/main` for both directions without hanging).
//! 4. Live relay revocation watchdog cutting a live sealed session, and OpenSSH `PerSourcePenalties`
//!    protection on loopback (tested without `PerSourcePenalties no`).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::env;
use std::fs;
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use common::{
    current_user, find_sshd, find_tool, real_legion_bin_with_sshd, strict_tempdir, toml_literal,
};
use legion_link::proto::ErrorCode;
use legion_link::{
    Link, LinkConfig, LinkOptions, PutIfExists, PutRequest, REASON_HOST_KEY_MISMATCH,
    REASON_SEAL_REQUIRED, Route,
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
    listen_addr: String,
    port: u16,
    user: String,
    child: Option<Child>,
}

impl ManagedSshd {
    fn new(
        dir: &Path,
        authorized_keys: &Path,
        home_dir: &Path,
        listen_addr: &str,
        disable_penalties: bool,
    ) -> Option<Self> {
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
            .args(["-q", "-t", "ed25519", "-N", "", "-C", "legion-sealed-test"])
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

        let supports_no_penalties = disable_penalties
            && Command::new(&sshd_bin)
                .args(["-T", "-f", "/dev/null", "-o", "PerSourcePenalties=no", "-h"])
                .arg(&host_key)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .is_ok_and(|s| s.success());

        let listener = TcpListener::bind((listen_addr, 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);

        let ssh_bin = find_tool("ssh").unwrap_or_else(|| PathBuf::from("/usr/bin/ssh"));
        let bin_dir = dir.join("bin");
        fs::create_dir_all(&bin_dir).unwrap();
        let ssh_shim = bin_dir.join("ssh");
        fs::write(
            &ssh_shim,
            format!(
                "#!/bin/sh\nif [ -n \"$HOME\" ] && [ -f \"$HOME/.ssh/config\" ]; then\n  exec \"{}\" -F \"$HOME/.ssh/config\" \"$@\"\nfi\nexec \"{}\" \"$@\"\n",
                ssh_bin.display(),
                ssh_bin.display(),
            ),
        )
        .unwrap();
        #[cfg(unix)]
        {
            let _ = fs::set_permissions(&ssh_shim, fs::Permissions::from_mode(0o755));
        }
        let existing_path = std::env::var("PATH").unwrap_or_else(|_| "/usr/bin:/bin".to_owned());
        let session_path = format!("{}:{existing_path}", bin_dir.display());

        let config_path = dir.join("sshd_config");
        let log_path = dir.join("sshd.log");
        let mut config = format!(
            "ListenAddress {listen_addr}\n\
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
             SetEnv HOME={home} PATH={session_path}\n",
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
            listen_addr: listen_addr.to_owned(),
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
            if TcpStream::connect((self.listen_addr.as_str(), self.port)).is_ok() {
                self.child = Some(child);
                return;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        let _ = child.kill();
        let _ = child.wait();
        panic!(
            "sshd failed to listen on {}:{}",
            self.listen_addr, self.port
        );
    }

    fn stop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline {
            if TcpStream::connect((self.listen_addr.as_str(), self.port)).is_err() {
                break;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
    }

    fn log_text(&self) -> String {
        fs::read_to_string(&self.log_path).unwrap_or_default()
    }
}

impl Drop for ManagedSshd {
    fn drop(&mut self) {
        self.stop();
    }
}

struct NodeDirs {
    user_home: PathBuf,
    legion_home: PathBuf,
    auth_keys: PathBuf,
    sshd: ManagedSshd,
}

impl NodeDirs {
    fn new(
        root: &Path,
        label: &str,
        node_name: &str,
        listen_addr: &str,
        disable_penalties: bool,
    ) -> Option<Self> {
        let user_home = root.join(format!("home-{label}"));
        let legion_home = root.join(format!("legion-{label}"));
        let auth_keys = user_home.join(".ssh").join("authorized_keys");
        for dir in [
            legion_home.join("config"),
            user_home.join(".ssh"),
            user_home.join("files"),
        ] {
            fs::create_dir_all(&dir).unwrap();
        }
        #[cfg(unix)]
        for dir in [&user_home, &user_home.join(".ssh")] {
            let _ = fs::set_permissions(dir, fs::Permissions::from_mode(0o700));
        }
        let sshd = ManagedSshd::new(
            &root.join(format!("sshd-{label}")),
            &auth_keys,
            &user_home,
            listen_addr,
            disable_penalties,
        )?;
        fs::write(
            legion_home.join("config/node.toml"),
            format!(
                "name = {node_name:?}\n\n[devices]\nhosts = [{listen_addr:?}]\nport = {}\nuser = {:?}\nauthorized_keys = {}\nhost_keys = [{}]\n",
                sshd.port,
                sshd.user,
                toml_literal(&auth_keys),
                toml_literal(&sshd.host_key_pub),
            ),
        )
        .unwrap();
        fs::write(
            legion_home.join("config/policy.toml"),
            "default = \"owner\"\n",
        )
        .unwrap();
        Some(Self {
            user_home,
            legion_home,
            auth_keys,
            sshd,
        })
    }
}

fn run_legion(legion_bin: &Path, node: &NodeDirs, args: &[&str]) -> Value {
    let ssh_shim_dir = node.sshd.config_path.parent().unwrap().join("bin");
    let existing_path = std::env::var("PATH").unwrap_or_else(|_| "/usr/bin:/bin".to_owned());
    let path_env = format!("{}:{existing_path}", ssh_shim_dir.display());
    let output = Command::new(legion_bin)
        .args(args)
        .arg("--json")
        .env("LEGION_HOME", &node.legion_home)
        .env("HOME", &node.user_home)
        .env("PATH", path_env)
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

fn write_node_wrapper(path: &Path, legion_bin: &Path, node: &NodeDirs) {
    let script = format!(
        "#!/bin/sh\nexport LEGION_HOME='{}'\nexport HOME='{}'\nunset XDG_CONFIG_HOME XDG_STATE_HOME\nexec '{}' \"$@\"\n",
        node.legion_home.display(),
        node.user_home.display(),
        legion_bin.display(),
    );
    fs::write(path, script).unwrap();
    #[cfg(unix)]
    {
        let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o700));
    }
}

fn unused_loopback_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    port
}

fn break_direct_port(link: &Link, machine: &str) {
    let mut entry = link
        .config()
        .node(machine)
        .unwrap_or_else(|| panic!("missing node {machine}"))
        .clone();
    entry.target.as_mut().unwrap().port = unused_loopback_port();
    link.upsert_node(entry);
}

fn blake3_of_file(legion_bin: &Path, node: &NodeDirs, path_in_home: &str) -> String {
    use std::io::Write;
    let mut child = Command::new(legion_bin)
        .arg("stream")
        .env("LEGION_HOME", &node.legion_home)
        .env("HOME", &node.user_home)
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
    let reply_line = stdout.lines().nth(1).expect("greeting + stat reply");
    let reply: Value = serde_json::from_str(reply_line).unwrap();
    reply["hash"].as_str().unwrap().to_owned()
}

fn audit_entries(node: &NodeDirs) -> Vec<Value> {
    let path = node.legion_home.join("state/audit.jsonl");
    let Ok(text) = fs::read_to_string(path) else {
        return Vec::new();
    };
    text.lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .collect()
}

/// Wires `from`'s `~/.ssh/config` and `known_hosts` so `ssh <alias>` connects over real OpenSSH
/// to `to`'s throwaway `sshd` using a generated key installed with `legion gate --as owner` on
/// `to` (setting `LEGION_HOME` and `HOME` on `to` via `--home` and `SetEnv`).
fn wire_real_ssh_hop(
    root: &Path,
    from: &NodeDirs,
    alias: &str,
    to: &NodeDirs,
    to_legion_bin: &Path,
) {
    let keygen = find_tool("ssh-keygen").unwrap();
    let key_path = root.join(format!("hop-key-{alias}-{}", from.sshd.port));
    if !key_path.exists() {
        let out = Command::new(&keygen)
            .args(["-q", "-t", "ed25519", "-N", "", "-C", "fleet-hop", "-f"])
            .arg(&key_path)
            .stdin(Stdio::null())
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "ssh-keygen failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        #[cfg(unix)]
        {
            let _ = fs::set_permissions(&key_path, fs::Permissions::from_mode(0o600));
        }
    }
    let pub_key = fs::read_to_string(key_path.with_extension("pub")).unwrap();
    let pub_key = pub_key.trim();
    // Authorize the relay key on `to` forced to `to`'s `legion gate --as owner --home <to.legion_home>`
    // so `ssh <alias> legion rpc` and `ssh <alias> legion stream` hit `to`'s isolated LEGION_HOME.
    let mut existing_auth = fs::read_to_string(&to.auth_keys).unwrap_or_default();
    let gate_line = format!(
        "restrict,command=\"{} gate --as owner --home {}\" {}\n",
        to_legion_bin.display(),
        to.legion_home.display(),
        pub_key
    );
    if !existing_auth.contains(&gate_line) {
        existing_auth.push_str(&gate_line);
        fs::write(&to.auth_keys, existing_auth).unwrap();
    }

    let host_key_line = fs::read_to_string(&to.sshd.host_key_pub).unwrap();
    let known_hosts = from.user_home.join(".ssh/known_hosts");
    let mut kh = fs::read_to_string(&known_hosts).unwrap_or_default();
    kh.push_str(&format!(
        "[{}]:{} {}\n",
        to.sshd.listen_addr,
        to.sshd.port,
        host_key_line.trim()
    ));
    fs::write(&known_hosts, kh).unwrap();

    let ssh_config = from.user_home.join(".ssh/config");
    let mut cfg = fs::read_to_string(&ssh_config).unwrap_or_default();
    // Remove any prior block for this alias if re-wiring (e.g. MITM swap).
    let marker_start = format!("# BEGIN {alias}\n");
    let marker_end = format!("# END {alias}\n");
    if let (Some(s), Some(e)) = (cfg.find(&marker_start), cfg.find(&marker_end)) {
        cfg.replace_range(s..e + marker_end.len(), "");
    }
    cfg.push_str(&format!(
        "{marker_start}Host {alias}\n  HostName {}\n  Port {}\n  User {}\n  IdentityFile {}\n  IdentitiesOnly yes\n  UserKnownHostsFile {}\n  StrictHostKeyChecking yes\n{marker_end}",
        to.sshd.listen_addr,
        to.sshd.port,
        to.sshd.user,
        key_path.display(),
        known_hosts.display(),
    ));
    fs::write(&ssh_config, cfg).unwrap();
    #[cfg(unix)]
    {
        let _ = fs::set_permissions(&ssh_config, fs::Permissions::from_mode(0o600));
        let _ = fs::set_permissions(&known_hosts, fs::Permissions::from_mode(0o600));
    }
}

/// Counts live processes on Linux whose `/proc/<pid>/cmdline` references `needle`.
#[cfg(target_os = "linux")]
fn count_processes_matching(needle: &str) -> usize {
    let Ok(entries) = fs::read_dir("/proc") else {
        return 0;
    };
    let mut count = 0usize;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(pid_str) = name.to_str() else {
            continue;
        };
        if !pid_str.bytes().all(|b| b.is_ascii_digit()) {
            continue;
        }
        let Ok(raw) = fs::read(entry.path().join("cmdline")) else {
            continue;
        };
        let cmd = String::from_utf8_lossy(&raw).replace('\0', " ");
        if cmd.contains(needle) {
            count += 1;
        }
    }
    count
}

#[tokio::test]
async fn real_sealed_session_end_to_end_reuse_concurrency_reconnect_mitm_and_teardown() {
    let Some(legion_bin) = real_legion_bin_with_sshd(
        "sealed_sessions::real_sealed_session_end_to_end_reuse_concurrency_reconnect_mitm_and_teardown",
    ) else {
        return;
    };

    let root = strict_tempdir();
    let Some(tower) = NodeDirs::new(root.path(), "tower", "tower", "127.0.0.1", true) else {
        return;
    };
    let Some(mut studio) = NodeDirs::new(root.path(), "studio", "studio", "127.0.0.1", true) else {
        return;
    };
    let Some(imposter) = NodeDirs::new(root.path(), "imposter", "studio", "127.0.0.1", true) else {
        return;
    };

    // Wire tower -> studio over REAL OpenSSH (exercising `ssh = "studio"` in fleet.toml).
    wire_real_ssh_hop(root.path(), &tower, "studio", &studio, &legion_bin);
    fs::write(
        tower.legion_home.join("config/fleet.toml"),
        "[machines.tower]\nssh = \"127.0.0.1\"\n\n[machines.studio]\nssh = \"studio\"\n",
    )
    .unwrap();
    fs::write(
        studio.legion_home.join("config/fleet.toml"),
        "[machines.studio]\nssh = \"127.0.0.1\"\n",
    )
    .unwrap();

    // Seed sample file on studio and upload payload on tower.
    let sample_bytes: Vec<u8> = (0..512 * 1024usize)
        .map(|i| ((i * 31 + 7) % 251) as u8)
        .collect();
    fs::write(studio.user_home.join("files/blob.bin"), &sample_bytes).unwrap();
    let upload_bytes: Vec<u8> = (0..196 * 1024usize)
        .map(|i| ((i * 17 + 3) % 251) as u8)
        .collect();
    fs::write(tower.user_home.join("files/up.bin"), &upload_bytes).unwrap();
    let upload_hash = blake3_of_file(&legion_bin, &tower, "~/files/up.bin");

    // Pair with tower; enrolment pushes the device key to studio over real SSH.
    let invite_out = run_legion(
        &legion_bin,
        &tower,
        &[
            "device.invite",
            "name=handset",
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
    let config_changes = Arc::new(AtomicUsize::new(0));
    let changes_bg = Arc::clone(&config_changes);
    link.set_on_config_change(Arc::new(move |cfg| {
        if cfg.node("studio").is_some_and(|n| n.sealed) {
            changes_bg.fetch_add(1, Ordering::SeqCst);
        }
    }));

    let enrolled = link.pair(invite_text, "handset").await.unwrap();
    assert_eq!(
        enrolled.fleet.get("studio").map(|o| o.status.as_str()),
        Some("synced"),
        "expected studio synced: {enrolled:?}"
    );
    let learned = link.learn().await.unwrap();
    assert_eq!(learned.added, vec!["studio".to_owned()]);
    assert!(!link.config().node("studio").unwrap().sealed);

    // Force studio to be unreachable directly so traffic MUST relay through tower.
    break_direct_port(&link, "studio");

    // 1. Sealed RPC: studio must see the device's own SSH door and principal (never `door: rpc`).
    let whoami = link
        .call_machine("studio", "meta.whoami", json!({}), |_| {})
        .await
        .unwrap();
    assert_eq!(
        whoami.route,
        Route::Relay {
            via: "tower".to_owned()
        }
    );
    assert_eq!(whoami.value["node"], json!("studio"));
    assert_eq!(whoami.value["door"], json!("ssh"), "{}", whoami.value);
    assert_eq!(
        whoami.value["principal"],
        json!("device:handset"),
        "{}",
        whoami.value
    );
    assert!(
        link.config().node("studio").unwrap().sealed,
        "ratchet must flip sealed=true after first sealed session"
    );
    assert_eq!(
        config_changes.load(Ordering::SeqCst),
        1,
        "config-change hook must fire once when sealed flips false->true"
    );

    // 2. Reuse the SINGLE sealed tunnel across tools_machine, 2 concurrent open_content reads,
    //    open_put (verifying `if_exists_supported() == true`), and a concurrent RPC to tower.
    let tools = link.tools_machine("studio").await.unwrap();
    assert_eq!(
        tools.route,
        Route::Relay {
            via: "tower".to_owned()
        }
    );
    assert!(
        tools.value["tools"]
            .as_array()
            .is_some_and(|a| !a.is_empty())
    );

    let (r1, r2, tower_rpc) = tokio::join!(
        async {
            let mut s = link
                .open_content("studio", "~/files/blob.bin", 0, None)
                .await
                .unwrap();
            assert_eq!(
                s.route(),
                &Route::Relay {
                    via: "tower".to_owned()
                }
            );
            let mut buf = Vec::new();
            s.read_to_end(&mut buf).await.unwrap();
            buf
        },
        async {
            let mut s = link
                .open_content("studio", "~/files/blob.bin", 65_536, Some(128 * 1024))
                .await
                .unwrap();
            assert_eq!(
                s.route(),
                &Route::Relay {
                    via: "tower".to_owned()
                }
            );
            let mut buf = Vec::new();
            s.read_to_end(&mut buf).await.unwrap();
            buf
        },
        async {
            link.call_machine("tower", "meta.whoami", json!({}), |_| {})
                .await
                .unwrap()
        }
    );
    assert_eq!(r1, sample_bytes);
    assert_eq!(r2, &sample_bytes[65_536..65_536 + 128 * 1024]);
    assert_eq!(tower_rpc.route, Route::Direct);
    assert_eq!(tower_rpc.value["node"], json!("tower"));

    let mut put = link
        .open_put(
            "studio",
            PutRequest {
                path: "~/files/uploaded-sealed.bin".to_owned(),
                size: upload_bytes.len() as u64,
                hash: upload_hash.clone(),
                mode: Some(0o644),
                mtime: None,
                if_exists: PutIfExists::Fail,
            },
        )
        .await
        .unwrap();
    assert_eq!(
        put.route(),
        &Route::Relay {
            via: "tower".to_owned()
        }
    );
    assert!(
        put.if_exists_supported(),
        "sealed put receives target's own greeting so if_exists_supported must be true"
    );
    put.write_all(&upload_bytes).await.unwrap();
    let done = put.finish().await.unwrap();
    assert_eq!(done.hash, upload_hash);
    assert_eq!(
        fs::read(studio.user_home.join("files/uploaded-sealed.bin")).unwrap(),
        upload_bytes
    );

    // Count how many times studio's sshd accepted the device key (`handset`): all of the above
    // (RPC + tools + 2 concurrent content reads + put) must have reused ONE tunnel!
    let studio_log = studio.sshd.log_text();
    let device_accepts = studio_log
        .lines()
        .filter(|l| l.contains("Accepted publickey"))
        .count();
    // 1 accept from tower's fleet-hop during device sync + 1 during learn + 1 for the tunnel hop
    // + 1 for handset's inner sealed SSH session. Verify `handset` only had ONE ssh session by
    // checking studio's audit log for distinct door="ssh" calls and tower's audit log after close.
    assert!(device_accepts >= 2, "expected sshd accepts in {studio_log}");
    let studio_audit = audit_entries(&studio);
    assert!(
        studio_audit.iter().any(|e| {
            e["tool"] == json!("meta.whoami")
                && e["door"] == json!("ssh")
                && e["device"] == json!("handset")
        }),
        "studio audit must record handset over door=ssh: {studio_audit:?}"
    );

    // Close the link and verify tower logged exactly ONE `stream.tunnel` session for all of those
    // multiplexed operations, AND that zero legion stream / ssh child processes remain behind.
    link.close().await;
    let deadline = Instant::now() + Duration::from_secs(4);
    let mut tunnel_audits = 0usize;
    while Instant::now() < deadline {
        tunnel_audits = audit_entries(&tower)
            .iter()
            .filter(|e| e["tool"] == json!("stream.tunnel") && e["target"] == json!("studio"))
            .count();
        if tunnel_audits >= 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(
        tunnel_audits, 1,
        "all multiplexed calls must have reused exactly one sealed tunnel"
    );

    #[cfg(target_os = "linux")]
    {
        let studio_marker = studio.legion_home.display().to_string();
        let tower_marker = tower.legion_home.display().to_string();
        let proc_deadline = Instant::now() + Duration::from_secs(4);
        while Instant::now() < proc_deadline {
            if count_processes_matching(&studio_marker) == 0
                && count_processes_matching(&tower_marker) == 0
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert_eq!(
            count_processes_matching(&studio_marker),
            0,
            "leftover studio legion processes after close"
        );
        assert_eq!(
            count_processes_matching(&tower_marker),
            0,
            "leftover tower legion processes after close"
        );
    }

    // 3. Reconnect after target sshd restarts while the sealed session is cached.
    let before_restart = link
        .call_machine("studio", "meta.whoami", json!({}), |_| {})
        .await
        .unwrap();
    assert_eq!(before_restart.value["node"], json!("studio"));
    studio.sshd.stop();
    studio.sshd.start();
    let after_restart = link
        .call_machine("studio", "meta.whoami", json!({}), |_| {})
        .await
        .unwrap();
    assert_eq!(
        after_restart.route,
        Route::Relay {
            via: "tower".to_owned()
        }
    );
    assert_eq!(after_restart.value["node"], json!("studio"));

    // 4. Compromised relay redirecting tunnel to an imposter sshd with a different host key:
    //    must fail with Denied + host_key_mismatch and NEVER fall back to unsealed relay.
    link.close().await;
    fs::copy(
        studio.legion_home.join("config/policy.toml"),
        imposter.legion_home.join("config/policy.toml"),
    )
    .unwrap();
    wire_real_ssh_hop(root.path(), &tower, "studio", &imposter, &legion_bin);
    let mitm_err = link
        .call_machine("studio", "meta.whoami", json!({}), |_| {})
        .await
        .unwrap_err();
    assert_eq!(mitm_err.code, ErrorCode::Denied, "{mitm_err}");
    assert_eq!(
        mitm_err.details.as_ref().unwrap()["reason"],
        json!(REASON_HOST_KEY_MISMATCH),
        "{mitm_err}"
    );

    link.close().await;
}

#[tokio::test]
async fn real_sealed_session_systems_chained_via_and_non_loopback_listen_address() {
    let Some(legion_bin) = real_legion_bin_with_sshd(
        "sealed_sessions::real_sealed_session_systems_chained_via_and_non_loopback_listen_address",
    ) else {
        return;
    };

    let root = strict_tempdir();
    let Some(hop1) = NodeDirs::new(root.path(), "hop1", "hop1", "127.0.0.1", true) else {
        return;
    };
    let Some(hop2) = NodeDirs::new(root.path(), "hop2", "hop2", "127.0.0.1", true) else {
        return;
    };
    // Studio listens ONLY on 127.0.0.2 (NOT 127.0.0.1!), testing advertised-address fallback.
    let Some(studio) = NodeDirs::new(root.path(), "studio", "studio", "127.0.0.2", true) else {
        return;
    };

    let hop2_exec = root.path().join("hop2-exec.sh");
    let studio_exec = root.path().join("studio-exec.sh");
    let dead_exec = root.path().join("dead-exec.sh");
    write_node_wrapper(&hop2_exec, &legion_bin, &hop2);
    write_node_wrapper(&studio_exec, &legion_bin, &studio);
    fs::write(&dead_exec, "#!/bin/sh\nexit 1\n").unwrap();
    #[cfg(unix)]
    {
        let _ = fs::set_permissions(&dead_exec, fs::Permissions::from_mode(0o700));
    }

    // hop1 reaches hop2 via command and studio via `via = "hop2"`.
    // hop2 reaches studio via dual-boot `systems` where system 1 (`offline`) fails and system 2
    // (`linux`) succeeds — exercising BOTH chained `via` AND dual-boot `systems` AND `command`!
    fs::write(
        hop1.legion_home.join("config/fleet.toml"),
        format!(
            "[machines.hop1]\nssh = \"127.0.0.1\"\n\n\
             [machines.hop2]\ncommand = [{}, \"rpc\"]\n\n\
             [machines.studio]\nvia = \"hop2\"\n",
            toml_literal(&hop2_exec),
        ),
    )
    .unwrap();
    fs::write(
        hop2.legion_home.join("config/fleet.toml"),
        format!(
            "[machines.hop2]\nssh = \"127.0.0.1\"\n\n\
             [[machines.studio.systems]]\nname = \"offline\"\ncommand = [{}, \"rpc\"]\n\n\
             [[machines.studio.systems]]\nname = \"linux\"\ncommand = [{}, \"rpc\"]\n",
            toml_literal(&dead_exec),
            toml_literal(&studio_exec),
        ),
    )
    .unwrap();
    fs::write(
        studio.legion_home.join("config/fleet.toml"),
        "[machines.studio]\nssh = \"127.0.0.2\"\n",
    )
    .unwrap();

    fs::write(
        studio.user_home.join("files/hello.txt"),
        b"chained-non-loopback-sealed",
    )
    .unwrap();

    // Pair with hop1; push device to hop2 and studio (via hop2).
    let invite_out = run_legion(
        &legion_bin,
        &hop1,
        &[
            "device.invite",
            "name=handset",
            "role=owner",
            "allow_owner=true",
        ],
    );
    let link = Link::with_options(
        LinkConfig::default(),
        LinkOptions {
            allow_loopback_endpoints: true,
            ..LinkOptions::default()
        },
    );
    let enrolled = link
        .pair(invite_out["invite"].as_str().unwrap(), "handset")
        .await
        .unwrap();
    assert_eq!(
        enrolled.fleet.get("studio").map(|o| o.status.as_str()),
        Some("synced"),
        "{enrolled:?}"
    );
    let learned = link.learn().await.unwrap();
    assert!(learned.added.contains(&"studio".to_owned()), "{learned:?}");

    // Break direct ports for BOTH hop2 and studio so studio MUST go through hop1 -> hop2 -> studio
    // (where studio's sshd only listens on 127.0.0.2!).
    break_direct_port(&link, "hop2");
    break_direct_port(&link, "studio");

    let whoami = link
        .call_machine("studio", "meta.whoami", json!({}), |_| {})
        .await
        .unwrap();
    assert_eq!(
        whoami.route,
        Route::Relay {
            via: "hop1".to_owned()
        }
    );
    assert_eq!(whoami.value["node"], json!("studio"));
    assert_eq!(whoami.value["door"], json!("ssh"));
    assert!(link.config().node("studio").unwrap().sealed);

    let mut content = link
        .open_content("studio", "~/files/hello.txt", 0, None)
        .await
        .unwrap();
    let mut bytes = Vec::new();
    content.read_to_end(&mut bytes).await.unwrap();
    assert_eq!(bytes, b"chained-non-loopback-sealed");

    link.close().await;
}

/// Writes a wrapper script around `legion_bin` that acts like an older node lacking
/// `stream.relay.tunnel` (for old-relay tests) or lacking `stream.tunnel` (for old-target tests)
/// by stripping the specified capability from `legion stream`'s greeting line and rejecting
/// `op: "tunnel"` with `unsupported` — OR runs a real `origin/main` binary verbatim when given.
fn write_old_cap_wrapper(path: &Path, legion_bin: &Path, node: &NodeDirs, strip_cap: &str) {
    let script = format!(
        r#"#!/usr/bin/env python3
import json, os, subprocess, sys, threading
env = os.environ.copy()
env['LEGION_HOME'] = {legion_home:?}
env['HOME'] = {user_home:?}
env.pop('XDG_CONFIG_HOME', None)
env.pop('XDG_STATE_HOME', None)
args = sys.argv[1:]
if args and args[0] == 'stream':
    p = subprocess.Popen([{bin:?}] + args, stdin=subprocess.PIPE, stdout=subprocess.PIPE, env=env)
    greet = p.stdout.readline()
    obj = json.loads(greet.decode())
    caps = [c for c in obj.get('capabilities', []) if c != {strip_cap:?}]
    obj['capabilities'] = caps
    sys.stdout.buffer.write((json.dumps(obj) + '\n').encode())
    sys.stdout.buffer.flush()
    def c_in():
        while True:
            b = sys.stdin.buffer.read1(65536)
            if not b:
                break
            p.stdin.write(b)
            p.stdin.flush()
        p.stdin.close()
    t = threading.Thread(target=c_in, daemon=True)
    t.start()
    while True:
        b = p.stdout.read1(65536)
        if not b:
            break
        sys.stdout.buffer.write(b)
        sys.stdout.buffer.flush()
    sys.exit(p.wait())
else:
    os.execve({bin:?}, [{bin:?}] + args, env)
"#,
        legion_home = node.legion_home.display().to_string(),
        user_home = node.user_home.display().to_string(),
        bin = legion_bin.display().to_string(),
        strip_cap = strip_cap,
    );
    fs::write(path, script).unwrap();
    #[cfg(unix)]
    {
        let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o700));
    }
}

/// Runs both interoperability directions (new app + old relay, new app + old target) plus the
/// ratchet (refusal to downgrade when sealed=true, failover to a second capable relay).
async fn run_old_peer_compat_and_ratchet(new_bin: &Path, old_bin_override: Option<&Path>) {
    let root = strict_tempdir();
    let Some(relay_old) = NodeDirs::new(root.path(), "relay-old", "relay-old", "127.0.0.1", true)
    else {
        return;
    };
    let Some(relay_new) = NodeDirs::new(root.path(), "relay-new", "relay-new", "127.0.0.1", true)
    else {
        return;
    };
    let Some(studio) = NodeDirs::new(root.path(), "studio", "studio", "127.0.0.1", true) else {
        return;
    };

    let studio_new_exec = root.path().join("studio-new.sh");
    let studio_old_exec = root.path().join("studio-old.sh");
    let relay_new_exec = root.path().join("relay-new.sh");
    write_node_wrapper(&studio_new_exec, new_bin, &studio);
    write_node_wrapper(&relay_new_exec, new_bin, &relay_new);
    if let Some(old_bin) = old_bin_override {
        write_node_wrapper(&studio_old_exec, old_bin, &studio);
    } else {
        write_old_cap_wrapper(&studio_old_exec, new_bin, &studio, "stream.tunnel");
    }

    // When testing without an external `origin/main` binary, simulate an older relay on the hop
    // `relay_old -> old_mid -> studio` (where `old_mid` omits `stream.relay.tunnel` so `relay_old`
    // returns `unsupported` with `reason: "relay_too_old"` without altering `authorized_keys`).
    let old_mid_exec = root.path().join("old-mid.sh");
    let old_mid = if old_bin_override.is_none() {
        let m = NodeDirs::new(root.path(), "old-mid", "old-mid", "127.0.0.1", true).unwrap();
        fs::write(
            m.legion_home.join("config/fleet.toml"),
            format!(
                "[machines.old-mid]\nssh = \"127.0.0.1\"\n\n[machines.studio]\ncommand = [{}, \"rpc\"]\n",
                toml_literal(&studio_new_exec),
            ),
        )
        .unwrap();
        write_old_cap_wrapper(&old_mid_exec, new_bin, &m, "stream.relay.tunnel");
        Some(m)
    } else {
        None
    };

    if old_mid.is_some() {
        fs::write(
            relay_old.legion_home.join("config/fleet.toml"),
            format!(
                "[machines.relay-old]\nssh = \"127.0.0.1\"\n\n[machines.old-mid]\ncommand = [{}, \"rpc\"]\n\n[machines.studio]\nvia = \"old-mid\"\n\n[machines.relay-new]\ncommand = [{}, \"rpc\"]\n",
                toml_literal(&old_mid_exec),
                toml_literal(&relay_new_exec),
            ),
        )
        .unwrap();
    } else {
        fs::write(
            relay_old.legion_home.join("config/fleet.toml"),
            format!(
                "[machines.relay-old]\nssh = \"127.0.0.1\"\n\n[machines.studio]\ncommand = [{}, \"rpc\"]\n\n[machines.relay-new]\ncommand = [{}, \"rpc\"]\n",
                toml_literal(&studio_new_exec),
                toml_literal(&relay_new_exec),
            ),
        )
        .unwrap();
    }
    fs::write(
        relay_new.legion_home.join("config/fleet.toml"),
        format!(
            "[machines.relay-new]\nssh = \"127.0.0.1\"\n\n[machines.studio]\ncommand = [{}, \"rpc\"]\n",
            toml_literal(&studio_new_exec),
        ),
    )
    .unwrap();
    fs::write(
        studio.legion_home.join("config/fleet.toml"),
        "[machines.studio]\nssh = \"127.0.0.1\"\n",
    )
    .unwrap();
    fs::write(studio.user_home.join("files/compat.txt"), b"compat-payload").unwrap();

    // Pair with relay_old (using old_bin if given, else new_bin fronting `old-mid` via `via`).
    // `relay_old` also pushes the device key to `relay_new` and `studio` (and `old_mid`).
    let invite_old = run_legion(
        old_bin_override.unwrap_or(new_bin),
        &relay_old,
        &[
            "device.invite",
            "name=handset-old",
            "role=owner",
            "allow_owner=true",
        ],
    );
    let link = Link::with_options(
        LinkConfig::default(),
        LinkOptions {
            allow_loopback_endpoints: true,
            ..LinkOptions::default()
        },
    );
    let enrolled_old = link
        .pair(invite_old["invite"].as_str().unwrap(), "handset-old")
        .await
        .unwrap();
    assert_eq!(
        enrolled_old.fleet.get("studio").map(|o| o.status.as_str()),
        Some("synced"),
        "expected relay_old to sync handset-old to studio: {enrolled_old:?}"
    );
    assert_eq!(
        enrolled_old
            .fleet
            .get("relay-new")
            .map(|o| o.status.as_str()),
        Some("synced"),
        "expected relay_old to sync handset-old to relay-new: {enrolled_old:?}"
    );
    link.learn().await.unwrap();
    // Save relay-new's learned NodeLink for Ratchet check C, and remove relay-new (and old-mid)
    // from `link` during checks 1 and B so `relay-old` is the ONLY available relay.
    let saved_relay_new = link.config().node("relay-new").unwrap().clone();
    link.forget("relay-new").await;
    if link.config().node("old-mid").is_some() {
        link.forget("old-mid").await;
    }
    assert!(!link.config().node("studio").unwrap().sealed);
    break_direct_port(&link, "studio");

    // Direction 1 (pre-ratchet): new app + old relay -> falls back cleanly to legacy relay, no hang.
    let res_old_relay = tokio::time::timeout(
        Duration::from_secs(10),
        link.call_machine("studio", "meta.whoami", json!({}), |_| {}),
    )
    .await
    .expect("old relay fallback must not hang")
    .unwrap();
    assert_eq!(
        res_old_relay.route,
        Route::Relay {
            via: "relay-old".to_owned()
        }
    );
    assert_eq!(res_old_relay.value["node"], json!("studio"));
    assert_eq!(res_old_relay.value["door"], json!("rpc"));
    assert!(
        !link.config().node("studio").unwrap().sealed,
        "legacy fallback must NOT flip sealed=true"
    );

    let mut content_old_relay = tokio::time::timeout(
        Duration::from_secs(10),
        link.open_content("studio", "~/files/compat.txt", 0, None),
    )
    .await
    .expect("old relay content fallback must not hang")
    .unwrap();
    let mut got = Vec::new();
    content_old_relay.read_to_end(&mut got).await.unwrap();
    assert_eq!(got, b"compat-payload");

    // Direction 2 (pre-ratchet): new app + new relay + old target -> falls back cleanly, no hang.
    fs::write(
        relay_new.legion_home.join("config/fleet.toml"),
        format!(
            "[machines.relay-new]\nssh = \"127.0.0.1\"\n\n[machines.studio]\ncommand = [{}, \"rpc\"]\n",
            toml_literal(&studio_old_exec),
        ),
    )
    .unwrap();
    let invite_new = run_legion(
        new_bin,
        &relay_new,
        &[
            "device.invite",
            "name=handset-new",
            "role=owner",
            "allow_owner=true",
        ],
    );
    let link2 = Link::with_options(
        LinkConfig::default(),
        LinkOptions {
            allow_loopback_endpoints: true,
            ..LinkOptions::default()
        },
    );
    let enrolled_new = link2
        .pair(invite_new["invite"].as_str().unwrap(), "handset-new")
        .await
        .unwrap();
    assert_eq!(
        enrolled_new.fleet.get("studio").map(|o| o.status.as_str()),
        Some("synced"),
        "expected relay_new to sync handset-new to studio: {enrolled_new:?}"
    );
    link2.learn().await.unwrap();
    break_direct_port(&link2, "studio");

    let res_old_target = tokio::time::timeout(
        Duration::from_secs(10),
        link2.call_machine("studio", "meta.whoami", json!({}), |_| {}),
    )
    .await
    .expect("old target fallback must not hang")
    .unwrap();
    assert_eq!(
        res_old_target.route,
        Route::Relay {
            via: "relay-new".to_owned()
        }
    );
    assert_eq!(res_old_target.value["node"], json!("studio"));
    assert_eq!(res_old_target.value["door"], json!("rpc"));
    assert!(!link2.config().node("studio").unwrap().sealed);

    // Now upgrade studio behind relay_new to new_bin so studio completes ONE sealed session!
    fs::write(
        relay_new.legion_home.join("config/fleet.toml"),
        format!(
            "[machines.relay-new]\nssh = \"127.0.0.1\"\n\n[machines.studio]\ncommand = [{}, \"rpc\"]\n",
            toml_literal(&studio_new_exec),
        ),
    )
    .unwrap();
    let res_sealed = link2
        .call_machine("studio", "meta.whoami", json!({}), |_| {})
        .await
        .unwrap();
    assert_eq!(res_sealed.value["door"], json!("ssh"));
    assert!(link2.config().node("studio").unwrap().sealed);
    link2.close().await;

    // Ratchet check A: studio is now downgraded to old target -> sealed=true MUST refuse fallback!
    fs::write(
        relay_new.legion_home.join("config/fleet.toml"),
        format!(
            "[machines.relay-new]\nssh = \"127.0.0.1\"\n\n[machines.studio]\ncommand = [{}, \"rpc\"]\n",
            toml_literal(&studio_old_exec),
        ),
    )
    .unwrap();
    let err_target_downgrade = tokio::time::timeout(
        Duration::from_secs(10),
        link2.call_machine("studio", "meta.whoami", json!({}), |_| {}),
    )
    .await
    .expect("ratchet refusal must not hang")
    .unwrap_err();
    assert_eq!(
        err_target_downgrade.code,
        ErrorCode::Unsupported,
        "{err_target_downgrade}"
    );
    assert_eq!(
        err_target_downgrade.details.as_ref().unwrap()["reason"],
        json!(REASON_SEAL_REQUIRED),
        "{err_target_downgrade}"
    );
    assert!(err_target_downgrade.hint.is_some());

    // Ratchet check B: studio has sealed=true, ONLY relay_old is available -> refuses downgrade for
    // call_machine, tools_machine, open_content, and open_put!
    let mut sealed_studio = link.config().node("studio").unwrap().clone();
    sealed_studio.sealed = true;
    link.upsert_node(sealed_studio);
    link.close().await;

    for (op, err) in [
        (
            "call_machine",
            link.call_machine("studio", "meta.whoami", json!({}), |_| {})
                .await
                .unwrap_err(),
        ),
        (
            "tools_machine",
            link.tools_machine("studio").await.unwrap_err(),
        ),
        (
            "open_content",
            link.open_content("studio", "~/files/compat.txt", 0, None)
                .await
                .err()
                .unwrap(),
        ),
        (
            "open_put",
            link.open_put(
                "studio",
                PutRequest {
                    path: "~/files/refused.bin".to_owned(),
                    size: 5,
                    hash: format!("blake3:{}", "0".repeat(64)),
                    mode: None,
                    mtime: None,
                    if_exists: PutIfExists::Replace,
                },
            )
            .await
            .err()
            .unwrap(),
        ),
    ] {
        assert_eq!(
            err.code,
            ErrorCode::Unsupported,
            "{op} must fail with Unsupported when ratchet engaged: {err}"
        );
        assert_eq!(
            err.details.as_ref().unwrap()["reason"],
            json!(REASON_SEAL_REQUIRED),
            "{op}: {err}"
        );
        assert!(err.hint.is_some(), "{op} must carry an actionable hint");
    }

    // Ratchet check C: add relay_new (backed by new studio, already synced with handset-old) as a
    // second relay on `link` while `relay_old` stays first in relay_candidates (introducer of
    // studio) -> app tries relay_old, refuses to downgrade, moves on to relay_new, and seals!
    fs::write(
        relay_new.legion_home.join("config/fleet.toml"),
        format!(
            "[machines.relay-new]\nssh = \"127.0.0.1\"\n\n[machines.studio]\ncommand = [{}, \"rpc\"]\n",
            toml_literal(&studio_new_exec),
        ),
    )
    .unwrap();
    link.upsert_node(saved_relay_new);
    let res_failover = link
        .call_machine("studio", "meta.whoami", json!({}), |_| {})
        .await
        .unwrap();
    assert_eq!(
        res_failover.route,
        Route::Relay {
            via: "relay-new".to_owned()
        }
    );
    assert_eq!(res_failover.value["door"], json!("ssh"));
    drop(old_mid);

    link.close().await;
    link2.close().await;
}

#[tokio::test]
async fn real_sealed_ratchet_and_old_peer_compat() {
    let Some(legion_bin) =
        real_legion_bin_with_sshd("sealed_sessions::real_sealed_ratchet_and_old_peer_compat")
    else {
        return;
    };
    run_old_peer_compat_and_ratchet(&legion_bin, None).await;
}

#[tokio::test]
async fn real_sealed_origin_main_interop_both_directions() {
    let Some(legion_bin) = real_legion_bin_with_sshd(
        "sealed_sessions::real_sealed_origin_main_interop_both_directions",
    ) else {
        return;
    };
    let Some(main_bin) = env::var_os("LEGION_MAIN_BIN")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
    else {
        eprintln!(
            "[real-binary] sealed_sessions::real_sealed_origin_main_interop_both_directions: skipped (LEGION_MAIN_BIN not set)"
        );
        return;
    };
    assert!(
        main_bin.is_file(),
        "LEGION_MAIN_BIN={} does not exist",
        main_bin.display()
    );
    run_old_peer_compat_and_ratchet(&legion_bin, Some(&main_bin)).await;
}

#[tokio::test]
async fn real_sealed_relay_revocation_cuts_live_session_and_per_source_penalties() {
    let Some(legion_bin) = real_legion_bin_with_sshd(
        "sealed_sessions::real_sealed_relay_revocation_cuts_live_session_and_per_source_penalties",
    ) else {
        return;
    };

    let root = strict_tempdir();
    let Some(tower) = NodeDirs::new(root.path(), "tower", "tower", "127.0.0.1", true) else {
        return;
    };
    // Studio runs WITHOUT `PerSourcePenalties no` so OpenSSH's default loopback penalties are live!
    let Some(studio) = NodeDirs::new(root.path(), "studio", "studio", "127.0.0.1", false) else {
        return;
    };
    let studio_exec = root.path().join("studio-exec.sh");
    write_node_wrapper(&studio_exec, &legion_bin, &studio);
    fs::write(
        tower.legion_home.join("config/fleet.toml"),
        format!(
            "[machines.tower]\nssh = \"127.0.0.1\"\n\n[machines.studio]\ncommand = [{}, \"rpc\"]\n",
            toml_literal(&studio_exec),
        ),
    )
    .unwrap();
    fs::write(
        studio.legion_home.join("config/fleet.toml"),
        "[machines.studio]\nssh = \"127.0.0.1\"\n",
    )
    .unwrap();

    // Pair device 1 (`good`) and device 2 (`revoked`).
    let inv_good = run_legion(
        &legion_bin,
        &tower,
        &[
            "device.invite",
            "name=good",
            "role=owner",
            "allow_owner=true",
        ],
    );
    let inv_rev = run_legion(
        &legion_bin,
        &tower,
        &[
            "device.invite",
            "name=revoked",
            "role=owner",
            "allow_owner=true",
        ],
    );

    let link_good = Link::with_options(
        LinkConfig::default(),
        LinkOptions {
            allow_loopback_endpoints: true,
            ..LinkOptions::default()
        },
    );
    link_good
        .pair(inv_good["invite"].as_str().unwrap(), "good")
        .await
        .unwrap();
    link_good.learn().await.unwrap();
    break_direct_port(&link_good, "studio");

    let link_rev = Link::with_options(
        LinkConfig::default(),
        LinkOptions {
            allow_loopback_endpoints: true,
            ..LinkOptions::default()
        },
    );
    link_rev
        .pair(inv_rev["invite"].as_str().unwrap(), "revoked")
        .await
        .unwrap();
    link_rev.learn().await.unwrap();
    break_direct_port(&link_rev, "studio");

    // Establish live sealed session for `revoked`.
    let live_before = link_rev
        .call_machine("studio", "meta.whoami", json!({}), |_| {})
        .await
        .unwrap();
    assert_eq!(live_before.value["door"], json!("ssh"));

    // Remove `revoked`'s pairing in tower's `devices.toml` (and policy) while leaving sshd running:
    // the relay revocation watchdog must cut the live tunnel within the recheck window!
    run_legion(&legion_bin, &tower, &["device.remove", "name=revoked"]);

    let cut_deadline = Instant::now() + Duration::from_secs(6);
    let mut cut = false;
    while Instant::now() < cut_deadline {
        if link_rev
            .call_machine("studio", "meta.whoami", json!({}), |_| {})
            .await
            .is_err()
        {
            cut = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    assert!(
        cut,
        "relay revocation watchdog must cut a live sealed tunnel promptly"
    );

    // Repeated attempts by the revoked device must be blocked before touching studio's loopback
    // sshd so OpenSSH PerSourcePenalties on 127.0.0.1 are never triggered for valid devices.
    for _ in 0..6 {
        let _ = link_rev
            .call_machine("studio", "meta.whoami", json!({}), |_| {})
            .await;
    }

    // Valid device `good` must still seal cleanly to studio with zero PerSourcePenalties drops!
    let good_res = link_good
        .call_machine("studio", "meta.whoami", json!({}), |_| {})
        .await
        .unwrap();
    assert_eq!(good_res.value["node"], json!("studio"));
    assert_eq!(good_res.value["door"], json!("ssh"));

    link_good.close().await;
    link_rev.close().await;
}
