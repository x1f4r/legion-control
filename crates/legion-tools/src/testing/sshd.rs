//! Throwaway loopback `sshd` harness for real-SSH integration tests.
//!
//! Starts an unprivileged `sshd` on `127.0.0.1` with a freshly generated Ed25519 host key, config,
//! and log file inside a private temporary directory (never touching `~/.ssh` or the system
//! `sshd`). Designed to be reused by both the node pairing tests and client-side SSH tests.

use std::fmt::Write as _;
use std::fs;
use std::io;
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use ed25519_dalek::SigningKey;
use tempfile::TempDir;

/// A running throwaway `sshd` bound to `127.0.0.1` on an ephemeral high port.
///
/// Dropping this handle terminates the `sshd` child process and cleans up its temporary directory.
pub struct TestSshd {
    dir: TempDir,
    child: Child,
    port: u16,
    user: String,
    host_key_pub: PathBuf,
    host_key_line: String,
    authorized_keys: PathBuf,
    known_hosts: PathBuf,
    log_path: PathBuf,
    ssh_bin: PathBuf,
    ssh_keygen_bin: PathBuf,
}

impl TestSshd {
    /// Returns `true` when `sshd`, `ssh`, and `ssh-keygen` are present and an unprivileged
    /// loopback `sshd` can be launched on this platform.
    pub fn available() -> bool {
        cfg!(unix)
            && find_sshd().is_some()
            && find_tool("ssh").is_some()
            && find_tool("ssh-keygen").is_some()
    }

    /// Starts a throwaway loopback `sshd` serving `authorized_keys` and exporting `env` pairs into
    /// each SSH session via `SetEnv`.
    ///
    /// Returns `Ok(None)` when `sshd`, `ssh`, or `ssh-keygen` are absent or when the platform does
    /// not support unprivileged loopback `sshd` (for example Windows, which exercises real `sshd`
    /// through `scripts/legion-e2e.mjs`).
    pub fn start(
        authorized_keys: PathBuf,
        env: &[(&str, &str)],
    ) -> Result<Option<TestSshd>, String> {
        Self::start_with_accept_env(authorized_keys, env, &[])
    }

    /// Starts a throwaway loopback `sshd` serving `authorized_keys`, exporting `env` via `SetEnv`,
    /// and accepting client-sent environment variables listed in `accept_env` via `AcceptEnv`.
    pub fn start_with_accept_env(
        authorized_keys: PathBuf,
        env: &[(&str, &str)],
        accept_env: &[&str],
    ) -> Result<Option<TestSshd>, String> {
        if !cfg!(unix) {
            return Ok(None);
        }
        let Some(sshd_bin) = find_sshd() else {
            return Ok(None);
        };
        let Some(ssh_bin) = find_tool("ssh") else {
            return Ok(None);
        };
        let Some(ssh_keygen_bin) = find_tool("ssh-keygen") else {
            return Ok(None);
        };

        let user = legion_os::user::current_username().map_err(|e| e.to_string())?;
        let dir = if let Some(base) = std::env::var_os("CARGO_TARGET_TMPDIR") {
            let base_path = PathBuf::from(base);
            let _ = fs::create_dir_all(&base_path);
            tempfile::tempdir_in(&base_path).map_err(|e| format!("create sshd tempdir: {e}"))?
        } else {
            TempDir::new().map_err(|e| format!("create sshd tempdir: {e}"))?
        };
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700));
        }

        if let Some(parent) = authorized_keys.parent() {
            fs::create_dir_all(parent)
                .map_err(|e| format!("create authorized_keys parent: {e}"))?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = fs::set_permissions(parent, fs::Permissions::from_mode(0o700));
            }
        }
        if !authorized_keys.exists() {
            fs::write(&authorized_keys, b"")
                .map_err(|e| format!("create empty authorized_keys: {e}"))?;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(&authorized_keys, fs::Permissions::from_mode(0o600));
        }

        let host_key = dir.path().join("host_ed25519");
        let host_key_pub = dir.path().join("host_ed25519.pub");
        let pub_raw = generate_ed25519_keypair(&ssh_keygen_bin, &host_key, "legion-test-host")?;
        let mut pub_parts = pub_raw.split_whitespace();
        let key_type = pub_parts
            .next()
            .ok_or_else(|| "generated host key pub file is empty".to_owned())?;
        let key_b64 = pub_parts
            .next()
            .ok_or_else(|| "generated host key pub file lacks key material".to_owned())?;
        let host_key_line = format!("{key_type} {key_b64}");

        let supports_no_penalties = Command::new(&sshd_bin)
            .args(["-T", "-o", "PerSourcePenalties=no", "-h"])
            .arg(&host_key)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success());

        let mut last_error = String::new();
        for _ in 0..3 {
            let port = pick_free_loopback_port()?;
            let config_path = dir.path().join("sshd_config");
            let log_path = dir.path().join("sshd.log");
            let known_hosts = dir.path().join("known_hosts");

            let config = render_sshd_config(
                port,
                &host_key,
                &authorized_keys,
                &user,
                env,
                accept_env,
                supports_no_penalties,
            );
            fs::write(&config_path, &config).map_err(|e| format!("write sshd_config: {e}"))?;
            write_known_hosts(
                &known_hosts,
                "127.0.0.1",
                port,
                std::slice::from_ref(&host_key_line),
            )
            .map_err(|e| format!("write known_hosts: {e}"))?;

            let log_file =
                fs::File::create(&log_path).map_err(|e| format!("create sshd.log: {e}"))?;
            let mut child = Command::new(&sshd_bin)
                .args(["-D", "-e", "-f"])
                .arg(&config_path)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::from(log_file))
                .spawn()
                .map_err(|e| format!("spawn {}: {e}", sshd_bin.display()))?;

            match wait_for_listen(&mut child, port, Duration::from_secs(5)) {
                Ok(()) => {
                    return Ok(Some(TestSshd {
                        dir,
                        child,
                        port,
                        user,
                        host_key_pub,
                        host_key_line,
                        authorized_keys,
                        known_hosts,
                        log_path,
                        ssh_bin,
                        ssh_keygen_bin,
                    }));
                }
                Err(reason) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    let log = fs::read_to_string(&log_path).unwrap_or_default();
                    last_error = format!("{reason}\n--- sshd log ---\n{log}");
                }
            }
        }

        Err(format!(
            "throwaway sshd failed to start after retries: {last_error}"
        ))
    }

    /// Loopback host address (`127.0.0.1`).
    pub fn host(&self) -> &str {
        "127.0.0.1"
    }

    /// Loopback TCP port this `sshd` listens on.
    pub fn port(&self) -> u16 {
        self.port
    }

    /// OS account name allowed to connect to this `sshd`.
    pub fn user(&self) -> &str {
        &self.user
    }

    /// Path to the throwaway `sshd`'s Ed25519 public host key file.
    pub fn host_key_path(&self) -> &Path {
        &self.host_key_pub
    }

    /// Public host key line (`ssh-ed25519 AAAA...`) without comment.
    pub fn host_key_line(&self) -> &str {
        &self.host_key_line
    }

    /// Path to the `authorized_keys` file served by this `sshd`.
    pub fn authorized_keys_path(&self) -> &Path {
        &self.authorized_keys
    }

    /// Path to a pre-built `known_hosts` file pinning this `sshd`'s host key.
    pub fn known_hosts_path(&self) -> &Path {
        &self.known_hosts
    }

    /// Path to the temporary directory holding this `sshd`'s config, host key, and logs.
    pub fn dir(&self) -> &Path {
        self.dir.path()
    }

    /// Path to the resolved `ssh` client executable.
    pub fn ssh_bin(&self) -> &Path {
        &self.ssh_bin
    }

    /// Path to the resolved `ssh-keygen` executable.
    pub fn ssh_keygen_bin(&self) -> &Path {
        &self.ssh_keygen_bin
    }

    /// Builds a non-interactive `ssh` [`Command`] pinned to `known_hosts` and authenticating only
    /// with `identity_file`.
    pub fn ssh_command(&self, identity_file: &Path, known_hosts: &Path) -> Command {
        let mut cmd = Command::new(&self.ssh_bin);
        cmd.args([
            "-T",
            "-o",
            "BatchMode=yes",
            "-o",
            "IdentitiesOnly=yes",
            "-o",
            "StrictHostKeyChecking=yes",
            "-o",
            "GlobalKnownHostsFile=none",
            "-o",
            "ControlMaster=no",
            "-o",
            "ConnectTimeout=10",
            "-o",
            "PubkeyAuthentication=yes",
            "-o",
            "PasswordAuthentication=no",
            "-o",
            "KbdInteractiveAuthentication=no",
            "-o",
            "LogLevel=ERROR",
        ]);
        cmd.arg("-o")
            .arg(format!("UserKnownHostsFile={}", known_hosts.display()));
        cmd.arg("-p").arg(self.port.to_string());
        cmd.arg("-i").arg(identity_file);
        cmd.arg(format!("{}@{}", self.user, self.host()));
        cmd
    }

    /// Reads the `sshd` stderr log captured so far, useful when asserting or debugging failures.
    pub fn logs(&self) -> String {
        fs::read_to_string(&self.log_path).unwrap_or_default()
    }
}

impl Drop for TestSshd {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Writes an unencrypted OpenSSH Ed25519 private key (`openssh-key-v1`) for the 32-byte `seed`,
/// with mode `0600` on Unix so `ssh -i` accepts it directly.
pub fn write_openssh_ed25519_private_key(
    path: &Path,
    seed: &[u8; 32],
    comment: &str,
) -> io::Result<()> {
    let pem = format_openssh_ed25519_private_key(seed, comment);
    fs::write(path, pem.as_bytes())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

/// Formats an unencrypted OpenSSH `openssh-key-v1` PEM private key from a 32-byte Ed25519 seed.
pub fn format_openssh_ed25519_private_key(seed: &[u8; 32], comment: &str) -> String {
    let signing = SigningKey::from_bytes(seed);
    let pub32 = signing.verifying_key().to_bytes();

    let mut pub_blob = Vec::with_capacity(51);
    push_ssh_bytes(&mut pub_blob, b"ssh-ed25519");
    push_ssh_bytes(&mut pub_blob, &pub32);

    let mut priv_section = Vec::with_capacity(160);
    // Matching checkints verified by OpenSSH after decryption (none cipher).
    let check: [u8; 4] = [0x11, 0x22, 0x33, 0x44];
    priv_section.extend_from_slice(&check);
    priv_section.extend_from_slice(&check);
    push_ssh_bytes(&mut priv_section, b"ssh-ed25519");
    push_ssh_bytes(&mut priv_section, &pub32);
    let mut priv64 = [0u8; 64];
    priv64[..32].copy_from_slice(seed);
    priv64[32..].copy_from_slice(&pub32);
    push_ssh_bytes(&mut priv_section, &priv64);
    push_ssh_bytes(&mut priv_section, comment.as_bytes());
    let mut pad: u8 = 1;
    while priv_section.len() % 8 != 0 {
        priv_section.push(pad);
        pad = pad.wrapping_add(1);
    }

    let mut blob = Vec::with_capacity(64 + pub_blob.len() + priv_section.len());
    blob.extend_from_slice(b"openssh-key-v1\0");
    push_ssh_bytes(&mut blob, b"none");
    push_ssh_bytes(&mut blob, b"none");
    push_ssh_bytes(&mut blob, b"");
    blob.extend_from_slice(&1_u32.to_be_bytes());
    push_ssh_bytes(&mut blob, &pub_blob);
    push_ssh_bytes(&mut blob, &priv_section);

    let b64 = STANDARD.encode(&blob);
    let mut pem = String::from("-----BEGIN OPENSSH PRIVATE KEY-----\n");
    for chunk in b64.as_bytes().chunks(70) {
        if let Ok(line) = std::str::from_utf8(chunk) {
            pem.push_str(line);
            pem.push('\n');
        }
    }
    pem.push_str("-----END OPENSSH PRIVATE KEY-----\n");
    pem
}

/// Writes an OpenSSH `known_hosts` file pinning `host` (and `port` when non-22) to `host_keys`.
pub fn write_known_hosts(
    path: &Path,
    host: &str,
    port: u16,
    host_keys: &[String],
) -> io::Result<()> {
    let entry_host = if port == 22 {
        host.to_owned()
    } else {
        format!("[{host}]:{port}")
    };
    let mut out = String::new();
    for key in host_keys {
        let _ = writeln!(out, "{entry_host} {}", key.trim());
    }
    fs::write(path, out.as_bytes())
}

/// Generates a fresh passphrase-less Ed25519 key pair at `private_path` (and `<private_path>.pub`)
/// using `ssh-keygen`, returning the trimmed `.pub` line.
pub fn generate_ed25519_keypair(
    ssh_keygen: &Path,
    private_path: &Path,
    comment: &str,
) -> Result<String, String> {
    let output = Command::new(ssh_keygen)
        .args(["-q", "-t", "ed25519", "-N", "", "-C", comment, "-f"])
        .arg(private_path)
        .output()
        .map_err(|e| format!("run ssh-keygen: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "ssh-keygen failed ({}): {}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    let pub_path = PathBuf::from(format!("{}.pub", private_path.display()));
    let pub_text =
        fs::read_to_string(&pub_path).map_err(|e| format!("read {}: {e}", pub_path.display()))?;
    Ok(pub_text.trim().to_owned())
}

/// Runs `ssh-keygen -lf <pub_path>` and returns the `SHA256:...` fingerprint token.
pub fn key_fingerprint(ssh_keygen: &Path, pub_path: &Path) -> Result<String, String> {
    let output = Command::new(ssh_keygen)
        .args(["-lf"])
        .arg(pub_path)
        .output()
        .map_err(|e| format!("run ssh-keygen -lf: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "ssh-keygen -lf failed ({}): {}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    stdout
        .split_whitespace()
        .find(|token| token.starts_with("SHA256:"))
        .map(ToOwned::to_owned)
        .ok_or_else(|| format!("unexpected ssh-keygen -lf output: {stdout}"))
}

fn push_ssh_bytes(buf: &mut Vec<u8>, bytes: &[u8]) {
    let len = u32::try_from(bytes.len()).unwrap_or(u32::MAX);
    buf.extend_from_slice(&len.to_be_bytes());
    buf.extend_from_slice(bytes);
}

fn find_sshd() -> Option<PathBuf> {
    for candidate in ["/usr/sbin/sshd", "/usr/bin/sshd", "/sbin/sshd"] {
        let path = PathBuf::from(candidate);
        if path.is_file() {
            return Some(path);
        }
    }
    find_tool("sshd")
}

fn find_tool(name: &str) -> Option<PathBuf> {
    let path_var = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path_var) {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

fn pick_free_loopback_port() -> Result<u16, String> {
    let listener = TcpListener::bind(("127.0.0.1", 0))
        .map_err(|e| format!("bind ephemeral loopback port: {e}"))?;
    let port = listener
        .local_addr()
        .map_err(|e| format!("read ephemeral port: {e}"))?
        .port();
    drop(listener);
    Ok(port)
}

fn render_sshd_config(
    port: u16,
    host_key: &Path,
    authorized_keys: &Path,
    user: &str,
    env: &[(&str, &str)],
    accept_env: &[&str],
    disable_penalties: bool,
) -> String {
    let mut cfg = String::new();
    let _ = writeln!(cfg, "ListenAddress 127.0.0.1");
    let _ = writeln!(cfg, "Port {port}");
    let _ = writeln!(
        cfg,
        "HostKey {}",
        quote_sshd_arg(&host_key.display().to_string())
    );
    let _ = writeln!(
        cfg,
        "AuthorizedKeysFile {}",
        quote_sshd_arg(&authorized_keys.display().to_string())
    );
    let _ = writeln!(cfg, "PidFile none");
    let _ = writeln!(cfg, "UsePAM no");
    let _ = writeln!(cfg, "StrictModes yes");
    let _ = writeln!(cfg, "PubkeyAuthentication yes");
    let _ = writeln!(cfg, "PasswordAuthentication no");
    let _ = writeln!(cfg, "KbdInteractiveAuthentication no");
    if disable_penalties {
        let _ = writeln!(cfg, "PerSourcePenalties no");
    }
    let _ = writeln!(cfg, "AllowUsers {}", quote_sshd_arg(user));
    if !env.is_empty() {
        let pairs: Vec<String> = env
            .iter()
            .map(|(k, v)| quote_sshd_arg(&format!("{k}={v}")))
            .collect();
        let _ = writeln!(cfg, "SetEnv {}", pairs.join(" "));
    }
    if !accept_env.is_empty() {
        let _ = writeln!(cfg, "AcceptEnv {}", accept_env.join(" "));
    }
    let _ = writeln!(cfg, "Subsystem sftp internal-sftp");
    let _ = writeln!(cfg, "LogLevel VERBOSE");
    cfg
}

fn quote_sshd_arg(value: &str) -> String {
    if value.chars().any(char::is_whitespace) {
        format!("\"{value}\"")
    } else {
        value.to_owned()
    }
}

fn wait_for_listen(child: &mut Child, port: u16, timeout: Duration) -> Result<(), String> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if let Some(status) = child
            .try_wait()
            .map_err(|e| format!("poll sshd process: {e}"))?
        {
            return Err(format!("sshd exited early with status {status}"));
        }
        if TcpStream::connect_timeout(
            &std::net::SocketAddr::from(([127, 0, 0, 1], port)),
            Duration::from_millis(100),
        )
        .is_ok()
        {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(30));
    }
    Err(format!(
        "sshd did not listen on 127.0.0.1:{port} within {timeout:?}"
    ))
}
