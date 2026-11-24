//! In-process SSH pipe using `russh`, with pinned host keys, Ed25519 key authentication,
//! keepalives, and stderr/exit-status reporting into [`Session`].

use std::borrow::Cow;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use legion_proto::{ErrorBody, ErrorCode};
use russh::client::{self, AuthResult};
use russh::keys::ssh_key::{Algorithm, EcdsaCurve, HashAlg, PublicKey};
use russh::keys::{PrivateKeyWithHashAlg, PublicKeyOrCertificate};
use russh::{Channel, ChannelMsg, Disconnect, Preferred, cipher, compression, kex, mac};
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt, DuplexStream};
use tokio::net::TcpStream;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::command::{RemoteOs, Subcommand, remote_command};
use crate::invite::SshTarget;
use crate::key::DeviceKey;
use crate::session::{Session, TransportHandle};
use crate::stderr::StderrTail;

/// Structured `details.reason` for a pinned host key mismatch.
pub const REASON_HOST_KEY_MISMATCH: &str = "host_key_mismatch";
/// Structured `details.reason` for rejected SSH key authentication.
pub const REASON_AUTH_FAILED: &str = "auth_failed";
/// Structured `details.reason` for a connect or handshake timeout.
pub const REASON_TIMEOUT: &str = "timeout";
/// Structured `details.reason` for an unreachable host.
pub const REASON_UNREACHABLE: &str = "unreachable";

/// Internal duplex buffer size between the SSH channel pump tasks and the session streams.
const CHANNEL_BUFFER_BYTES: usize = 64 * 1024;

/// Timeouts and keepalive settings for an in-process SSH connection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SshTimeouts {
    /// Per-host TCP connect + SSH handshake + auth timeout.
    pub connect: Duration,
    /// Timeout for the Legion `hello` handshake once the exec channel is open.
    pub handshake: Duration,
    /// Interval between SSH keepalive probes when the connection is quiet (`None` disables).
    pub keepalive_interval: Option<Duration>,
    /// Number of unanswered keepalives before closing the connection.
    pub keepalive_max: usize,
}

impl Default for SshTimeouts {
    fn default() -> Self {
        SshTimeouts {
            connect: Duration::from_secs(10),
            handshake: Duration::from_secs(10),
            keepalive_interval: Some(Duration::from_secs(15)),
            keepalive_max: 3,
        }
    }
}

/// A live in-process SSH exec channel split into async stdin/stdout streams and a lifecycle handle.
pub struct SshPipe {
    reader: DuplexStream,
    writer: DuplexStream,
    handle: SshPipeHandle,
}

impl SshPipe {
    /// Splits the pipe into its raw reader, writer, and lifecycle handle.
    pub fn into_parts(self) -> (DuplexStream, DuplexStream, SshPipeHandle) {
        (self.reader, self.writer, self.handle)
    }

    /// Borrowed access to the bounded stderr collector.
    pub fn stderr_tail(&self) -> &Arc<StderrTail> {
        &self.handle.stderr
    }

    /// Closes the pipe gracefully within `grace`.
    pub async fn close(self, grace: Duration) {
        drop(self.writer);
        drop(self.reader);
        Box::new(self.handle).close(grace).await;
    }
}

/// Lifecycle handle for an open [`SshPipe`], implementing [`TransportHandle`] so [`Session`]
/// surfaces remote stderr and exit codes in its errors and disconnects cleanly on close.
pub struct SshPipeHandle {
    session: Option<client::Handle<HostKeyHandler>>,
    stderr: Arc<StderrTail>,
    exit_code: Arc<Mutex<Option<i32>>>,
    rx_task: Option<JoinHandle<()>>,
    tx_task: Option<JoinHandle<()>>,
}

impl SshPipeHandle {
    /// Returns the bounded stderr tail attached to this SSH channel.
    pub fn stderr_tail(&self) -> Arc<StderrTail> {
        Arc::clone(&self.stderr)
    }
}

impl TransportHandle for SshPipeHandle {
    fn stderr_tail(&self) -> Option<Arc<StderrTail>> {
        Some(Arc::clone(&self.stderr))
    }

    fn collect_exit_info(
        &mut self,
        wait_limit: Duration,
    ) -> Pin<Box<dyn Future<Output = (Option<i32>, String)> + Send + '_>> {
        Box::pin(async move {
            if let Some(mut task) = self.rx_task.take()
                && tokio::time::timeout(wait_limit, &mut task).await.is_err()
            {
                task.abort();
            }
            let code = *self
                .exit_code
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            (code, self.stderr.text())
        })
    }

    fn close(mut self: Box<Self>, grace: Duration) -> Pin<Box<dyn Future<Output = ()> + Send>> {
        Box::pin(async move {
            if let Some(mut tx) = self.tx_task.take()
                && tokio::time::timeout(grace, &mut tx).await.is_err()
            {
                tx.abort();
            }
            if let Some(mut rx) = self.rx_task.take()
                && tokio::time::timeout(grace, &mut rx).await.is_err()
            {
                rx.abort();
            }
            if let Some(session) = self.session.take() {
                let _ = tokio::time::timeout(
                    grace,
                    session.disconnect(Disconnect::ByApplication, "", "en"),
                )
                .await;
            }
        })
    }
}

impl Drop for SshPipeHandle {
    fn drop(&mut self) {
        if let Some(tx) = self.tx_task.take() {
            tx.abort();
        }
        if let Some(rx) = self.rx_task.take() {
            rx.abort();
        }
    }
}

/// Opens a Legion [`Session`] over an in-process SSH exec channel to `target`.
///
/// Always sends the fixed Unix remote command string for `subcommand`: `legion gate` never runs
/// `SSH_ORIGINAL_COMMAND` through a shell; it compares the string against every shape
/// `remote_command` can produce across both Unix and Windows (see `crates/legion-net/src/gate.rs`),
/// so the Unix `rpc` command is accepted by gated Windows nodes as well as Linux and macOS nodes.
pub async fn open(
    target: &SshTarget,
    key: &DeviceKey,
    subcommand: Subcommand,
    timeouts: SshTimeouts,
    cancel: CancellationToken,
) -> Result<Session, ErrorBody> {
    let pipe = open_raw(target, key, subcommand, timeouts, cancel.clone()).await?;
    let (reader, writer, handle) = pipe.into_parts();
    let label = target.hosts.first().map_or("remote", String::as_str);
    Session::open_transport(
        reader,
        writer,
        Box::new(handle),
        label,
        None,
        timeouts.handshake,
        cancel,
    )
    .await
    .map_err(|err| err.into_error())
}

/// Opens a raw SSH exec channel to `target` for `subcommand`, returning the async reader/writer
/// pair and lifecycle handle without running the Legion `hello` handshake.
///
/// Uses the fixed Unix command shape for `subcommand`, which `legion gate` accepts on both Unix
/// and Windows nodes (see `crates/legion-net/src/gate.rs`).
pub async fn open_raw(
    target: &SshTarget,
    key: &DeviceKey,
    subcommand: Subcommand,
    timeouts: SshTimeouts,
    cancel: CancellationToken,
) -> Result<SshPipe, ErrorBody> {
    let command = remote_command(RemoteOs::Unix, subcommand, None)?;
    open_exec_raw(target, key, &command, timeouts, cancel).await
}

/// Opens a raw SSH exec channel running an exact `command` string on `target`.
pub async fn open_exec_raw(
    target: &SshTarget,
    key: &DeviceKey,
    command: &str,
    timeouts: SshTimeouts,
    cancel: CancellationToken,
) -> Result<SshPipe, ErrorBody> {
    let expected_keys = parse_host_keys(&target.host_keys)?;
    if target.hosts.is_empty() {
        return Err(ErrorBody::new(
            ErrorCode::InvalidArgs,
            "SSH target has no hosts to connect to.",
        )
        .with_hint("Provide at least one host name or IP address."));
    }
    if target.port == 0 {
        return Err(
            ErrorBody::new(ErrorCode::InvalidArgs, "SSH port cannot be zero.")
                .with_hint("Use a TCP port between 1 and 65535 (usually 22)."),
        );
    }
    if target.user.trim().is_empty() {
        return Err(
            ErrorBody::new(ErrorCode::InvalidArgs, "SSH user cannot be empty.")
                .with_hint("Provide the remote account name to authenticate as."),
        );
    }

    let mut attempts: Vec<(String, ErrorBody)> = Vec::new();
    for host in &target.hosts {
        if cancel.is_cancelled() {
            return Err(ErrorBody::new(
                ErrorCode::Cancelled,
                format!("Connecting to {host}:{} was cancelled.", target.port),
            ));
        }
        match connect_one_host(
            host,
            target.port,
            &target.user,
            &expected_keys,
            key,
            command,
            timeouts,
            cancel.clone(),
        )
        .await
        {
            Ok(pipe) => return Ok(pipe),
            Err(HostAttemptError::Fatal(err)) => return Err(err),
            Err(HostAttemptError::TryNext(err)) => {
                attempts.push((host.clone(), err));
            }
        }
    }

    if attempts.len() == 1
        && let Some((_, err)) = attempts.pop()
    {
        return Err(err);
    }

    let all_timeouts = !attempts.is_empty()
        && attempts
            .iter()
            .all(|(_, err)| err.code == ErrorCode::Timeout);
    let summary = attempts
        .iter()
        .map(|(host, err)| format!("{host}: {}", err.message))
        .collect::<Vec<_>>()
        .join("; ");
    let code = if all_timeouts {
        ErrorCode::Timeout
    } else {
        ErrorCode::Unreachable
    };
    let reason = if all_timeouts {
        REASON_TIMEOUT
    } else {
        REASON_UNREACHABLE
    };
    Err(ErrorBody::new(
        code,
        format!("Could not reach any configured host ({summary})."),
    )
    .with_hint("Check that the node is online, sshd is running, and your network can reach one of its addresses.")
    .with_details(json!({ "reason": reason })))
}

enum HostAttemptError {
    /// Do not fall through to other hosts (host key mismatch, auth rejection, cancellation, or
    /// remote exec refusal after connecting).
    Fatal(ErrorBody),
    /// Network/TCP failure or timeout; try the next host in `target.hosts`.
    TryNext(ErrorBody),
}

#[allow(clippy::too_many_arguments)]
async fn connect_one_host(
    host: &str,
    port: u16,
    user: &str,
    expected_keys: &[PublicKey],
    key: &DeviceKey,
    command: &str,
    timeouts: SshTimeouts,
    cancel: CancellationToken,
) -> Result<SshPipe, HostAttemptError> {
    let connect_fut = async {
        let tcp = TcpStream::connect((host, port)).await.map_err(|err| {
            HostAttemptError::TryNext(
                ErrorBody::new(
                    ErrorCode::Unreachable,
                    format!("Could not connect to {host}:{port}: {err}."),
                )
                .with_hint("Check that the node is awake and sshd is listening on that port.")
                .with_details(json!({
                    "reason": REASON_UNREACHABLE,
                    "host": host,
                    "port": port,
                })),
            )
        })?;
        let _ = tcp.set_nodelay(true);

        let offered_mismatch = Arc::new(Mutex::new(None::<String>));
        let handler = HostKeyHandler {
            expected: expected_keys.to_vec(),
            offered_mismatch: Arc::clone(&offered_mismatch),
        };
        let config = Arc::new(build_ssh_config(expected_keys, timeouts));

        let mut session = client::connect_stream(config, tcp, handler)
            .await
            .map_err(|err| {
                map_handshake_error(host, port, err, &offered_mismatch, expected_keys)
            })?;

        let auth = session
            .authenticate_publickey(
                user,
                PrivateKeyWithHashAlg::new(Arc::new(key.private_key().clone()), None),
            )
            .await
            .map_err(|err| {
                HostAttemptError::Fatal(
                    ErrorBody::new(
                        ErrorCode::Unreachable,
                        format!("SSH authentication to {host}:{port} failed: {err}."),
                    )
                    .with_hint("Check that the connection stayed up and retry.")
                    .with_details(json!({
                        "reason": REASON_UNREACHABLE,
                        "host": host,
                        "port": port,
                    })),
                )
            })?;

        if !matches!(auth, AuthResult::Success) {
            let _ = session
                .disconnect(Disconnect::ByApplication, "", "en")
                .await;
            return Err(HostAttemptError::Fatal(
                ErrorBody::new(
                    ErrorCode::Denied,
                    format!("SSH key authentication for {user} on {host}:{port} was rejected."),
                )
                .with_hint("Verify that this device is still paired on the node (`legion device.list`), or pair it again.")
                .with_details(json!({
                    "reason": REASON_AUTH_FAILED,
                    "user": user,
                    "host": host,
                    "port": port,
                })),
            ));
        }

        let channel = session.channel_open_session().await.map_err(|err| {
            HostAttemptError::Fatal(
                ErrorBody::new(
                    ErrorCode::Unreachable,
                    format!("Could not open an SSH session channel on {host}:{port}: {err}."),
                )
                .with_hint("Check sshd MaxSessions and channel permissions on the node.")
                .with_details(json!({
                    "reason": REASON_UNREACHABLE,
                    "host": host,
                    "port": port,
                })),
            )
        })?;

        channel.exec(true, command).await.map_err(|err| {
            HostAttemptError::Fatal(
                ErrorBody::new(
                    ErrorCode::Unreachable,
                    format!("Could not start the remote command on {host}:{port}: {err}."),
                )
                .with_hint("Check that the paired key is allowed to run `legion gate` on the node.")
                .with_details(json!({
                    "reason": REASON_UNREACHABLE,
                    "host": host,
                    "port": port,
                })),
            )
        })?;

        Ok(bridge_channel(session, channel))
    };

    tokio::select! {
        biased;
        () = cancel.cancelled() => Err(HostAttemptError::Fatal(
            ErrorBody::new(
                ErrorCode::Cancelled,
                format!("Connecting to {host}:{port} was cancelled."),
            ),
        )),
        res = tokio::time::timeout(timeouts.connect, connect_fut) => match res {
            Ok(outcome) => outcome,
            Err(_) => Err(HostAttemptError::TryNext(
                ErrorBody::new(
                    ErrorCode::Timeout,
                    format!(
                        "Timed out connecting to {host}:{port} after {}s.",
                        timeouts.connect.as_secs_f64()
                    ),
                )
                .with_hint("Check that the host is awake, reachable over your network, and accepting SSH connections.")
                .with_details(json!({
                    "reason": REASON_TIMEOUT,
                    "host": host,
                    "port": port,
                })),
            )),
        },
    }
}

fn map_handshake_error(
    host: &str,
    port: u16,
    err: russh::Error,
    offered_mismatch: &Arc<Mutex<Option<String>>>,
    expected_keys: &[PublicKey],
) -> HostAttemptError {
    if let Some(offered) = offered_mismatch
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone()
    {
        return HostAttemptError::Fatal(host_key_mismatch_error(
            host,
            port,
            &offered,
            expected_keys,
        ));
    }
    HostAttemptError::TryNext(
        ErrorBody::new(
            ErrorCode::Unreachable,
            format!("SSH handshake with {host}:{port} failed: {err}."),
        )
        .with_hint("Check that an OpenSSH 8+ server is listening on that address and port.")
        .with_details(json!({
            "reason": REASON_UNREACHABLE,
            "host": host,
            "port": port,
        })),
    )
}

fn host_key_mismatch_error(
    host: &str,
    port: u16,
    offered_fingerprint: &str,
    expected_keys: &[PublicKey],
) -> ErrorBody {
    let expected: Vec<String> = expected_keys
        .iter()
        .map(|k| k.fingerprint(HashAlg::Sha256).to_string())
        .collect();
    ErrorBody::new(
        ErrorCode::Denied,
        format!(
            "Host key mismatch for {host}:{port}: the server offered {offered_fingerprint}, which does not match any pinned host key."
        ),
    )
    .with_hint("Verify the node's SSH host key out of band; if the host was reinstalled or rekeyed, pair this device again.")
    .with_details(json!({
        "reason": REASON_HOST_KEY_MISMATCH,
        "host": host,
        "port": port,
        "offered": offered_fingerprint,
        "expected": expected,
    }))
}

struct HostKeyHandler {
    expected: Vec<PublicKey>,
    offered_mismatch: Arc<Mutex<Option<String>>>,
}

impl client::Handler for HostKeyHandler {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        server_public_key: &PublicKeyOrCertificate,
    ) -> Result<bool, Self::Error> {
        let matched = check_pinned_host_key(server_public_key, &self.expected);
        match matched {
            Ok(()) => Ok(true),
            Err(offered_fingerprint) => {
                *self
                    .offered_mismatch
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(offered_fingerprint);
                Ok(false)
            }
        }
    }
}

/// Checks whether the server's presented bare public key matches one of the pinned `expected` keys.
///
/// Certificates are refused because pinning matches exact host keys, never a CA. Returns `Err`
/// with the offered SHA-256 fingerprint on mismatch.
pub(crate) fn check_pinned_host_key(
    presented: &PublicKeyOrCertificate,
    expected: &[PublicKey],
) -> Result<(), String> {
    match presented {
        PublicKeyOrCertificate::PublicKey { key, .. } => {
            if expected.iter().any(|pin| pin.key_data() == key.key_data()) {
                Ok(())
            } else {
                Err(key.fingerprint(HashAlg::Sha256).to_string())
            }
        }
        PublicKeyOrCertificate::Certificate(cert) => {
            let key = PublicKey::new(cert.public_key().clone(), "");
            Err(key.fingerprint(HashAlg::Sha256).to_string())
        }
    }
}

/// Parses OpenSSH public host key lines from [`SshTarget::host_keys`].
pub(crate) fn parse_host_keys(lines: &[String]) -> Result<Vec<PublicKey>, ErrorBody> {
    if lines.is_empty() {
        return Err(ErrorBody::new(
            ErrorCode::InvalidArgs,
            "At least one pinned SSH host key is required.",
        )
        .with_hint(
            "Pass the node's OpenSSH public host key lines (there is no trust on first use).",
        ));
    }
    let mut parsed = Vec::with_capacity(lines.len());
    for (index, line) in lines.iter().enumerate() {
        let key = PublicKey::from_openssh(line.trim()).map_err(|err| {
            ErrorBody::new(
                ErrorCode::InvalidArgs,
                format!("Pinned host key #{index} is not a valid OpenSSH public key: {err}."),
            )
            .with_hint("Use OpenSSH public key lines such as `ssh-ed25519 AAAA...`.")
        })?;
        parsed.push(key);
    }
    Ok(parsed)
}

/// Modern algorithm set accepted by OpenSSH 8+ and Windows OpenSSH by default, filtered to host
/// key algorithms present in `expected_keys` so a multi-key server always offers a pinned key type.
fn build_ssh_config(expected_keys: &[PublicKey], timeouts: SshTimeouts) -> client::Config {
    let mut host_key_algos: Vec<Algorithm> = Vec::new();
    let mut push_unique = |alg: Algorithm| {
        if !host_key_algos.contains(&alg) {
            host_key_algos.push(alg);
        }
    };
    for key in expected_keys {
        match key.algorithm() {
            Algorithm::Ed25519 => push_unique(Algorithm::Ed25519),
            Algorithm::Ecdsa { curve } => push_unique(Algorithm::Ecdsa { curve }),
            Algorithm::Rsa { .. } => {
                push_unique(Algorithm::Rsa {
                    hash: Some(HashAlg::Sha512),
                });
                push_unique(Algorithm::Rsa {
                    hash: Some(HashAlg::Sha256),
                });
            }
            _ => {}
        }
    }
    if host_key_algos.is_empty() {
        host_key_algos = vec![
            Algorithm::Ed25519,
            Algorithm::Ecdsa {
                curve: EcdsaCurve::NistP256,
            },
            Algorithm::Ecdsa {
                curve: EcdsaCurve::NistP384,
            },
            Algorithm::Ecdsa {
                curve: EcdsaCurve::NistP521,
            },
            Algorithm::Rsa {
                hash: Some(HashAlg::Sha512),
            },
            Algorithm::Rsa {
                hash: Some(HashAlg::Sha256),
            },
        ];
    }

    let preferred = Preferred {
        kex: Cow::Borrowed(&[
            kex::MLKEM768X25519_SHA256,
            kex::CURVE25519,
            kex::CURVE25519_PRE_RFC_8731,
            kex::DH_G16_SHA512,
            kex::DH_G14_SHA256,
            kex::EXTENSION_SUPPORT_AS_CLIENT,
            kex::EXTENSION_OPENSSH_STRICT_KEX_AS_CLIENT,
        ]),
        host_key_certificates: Cow::Borrowed(&[]),
        key: Cow::Owned(host_key_algos),
        cipher: Cow::Borrowed(&[
            cipher::CHACHA20_POLY1305,
            cipher::AES_256_GCM,
            cipher::AES_256_CTR,
            cipher::AES_128_CTR,
        ]),
        mac: Cow::Borrowed(&[
            mac::HMAC_SHA512_ETM,
            mac::HMAC_SHA256_ETM,
            mac::HMAC_SHA512,
            mac::HMAC_SHA256,
        ]),
        compression: Cow::Borrowed(&[compression::NONE]),
    };

    client::Config {
        preferred,
        keepalive_interval: timeouts.keepalive_interval,
        keepalive_max: timeouts.keepalive_max,
        nodelay: true,
        ..client::Config::default()
    }
}

fn bridge_channel(
    session: client::Handle<HostKeyHandler>,
    channel: Channel<client::Msg>,
) -> SshPipe {
    let (mut read_half, write_half) = channel.split();
    let (stdout_read, stdout_write) = tokio::io::duplex(CHANNEL_BUFFER_BYTES);
    let (mut stdin_read, stdin_write) = tokio::io::duplex(CHANNEL_BUFFER_BYTES);

    let stderr = Arc::new(StderrTail::default());
    let exit_code = Arc::new(Mutex::new(None));

    let rx_stderr = Arc::clone(&stderr);
    let rx_exit = Arc::clone(&exit_code);
    let rx_task = tokio::spawn(async move {
        let mut stdout_open = Some(stdout_write);
        let mut stderr_buf = Vec::<u8>::new();
        while let Some(msg) = read_half.wait().await {
            match msg {
                ChannelMsg::Data { data } => {
                    if let Some(out) = stdout_open.as_mut()
                        && out.write_all(&data).await.is_err()
                    {
                        stdout_open = None;
                    }
                }
                ChannelMsg::ExtendedData { data, ext: 1 } => {
                    push_stderr_bytes(&rx_stderr, &mut stderr_buf, &data);
                }
                ChannelMsg::ExitStatus { exit_status } => {
                    *rx_exit
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner()) =
                        Some(exit_status as i32);
                }
                ChannelMsg::ExitSignal { signal_name, .. } => {
                    rx_stderr.push_line(&format!("remote terminated by signal {signal_name:?}"));
                }
                ChannelMsg::Eof => {
                    if let Some(mut out) = stdout_open.take() {
                        let _ = out.shutdown().await;
                    }
                }
                ChannelMsg::Close => break,
                _ => {}
            }
        }
        if !stderr_buf.is_empty() {
            let text = String::from_utf8_lossy(&stderr_buf);
            rx_stderr.push_line(&text);
        }
        if let Some(mut out) = stdout_open.take() {
            let _ = out.shutdown().await;
        }
    });

    let tx_task = tokio::spawn(async move {
        let mut buf = vec![0u8; 16 * 1024];
        loop {
            match stdin_read.read(&mut buf).await {
                Ok(0) => {
                    let _ = write_half.eof().await;
                    break;
                }
                Ok(n) => {
                    if write_half.data(&buf[..n]).await.is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });

    SshPipe {
        reader: stdout_read,
        writer: stdin_write,
        handle: SshPipeHandle {
            session: Some(session),
            stderr,
            exit_code,
            rx_task: Some(rx_task),
            tx_task: Some(tx_task),
        },
    }
}

fn push_stderr_bytes(tail: &StderrTail, pending: &mut Vec<u8>, chunk: &[u8]) {
    for &byte in chunk {
        if byte == b'\n' {
            let text = String::from_utf8_lossy(pending);
            tail.push_line(&text);
            pending.clear();
        } else if pending.len() < 4096 {
            pending.push(byte);
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn pinned_host_key_matches_exact_key_and_rejects_mismatch_with_fingerprint() {
        let good = DeviceKey::from_seed([1u8; 32]);
        let other = DeviceKey::from_seed([2u8; 32]);

        let pinned = parse_host_keys(&[good.public_key_line("tower").unwrap()]).unwrap();
        let good_presented = PublicKeyOrCertificate::from(good.private_key().public_key().clone());
        assert!(check_pinned_host_key(&good_presented, &pinned).is_ok());

        let bad_presented = PublicKeyOrCertificate::from(other.private_key().public_key().clone());
        let offered = check_pinned_host_key(&bad_presented, &pinned).unwrap_err();
        assert_eq!(offered, other.fingerprint());

        let err = host_key_mismatch_error("tower.example.net", 22, &offered, &pinned);
        assert_eq!(err.code, ErrorCode::Denied);
        assert!(err.message.contains(&offered), "{}", err.message);
        assert!(err.hint.is_some());
        let details = err.details.unwrap();
        assert_eq!(details["reason"], json!(REASON_HOST_KEY_MISMATCH));
        assert_eq!(details["offered"], json!(offered));
    }

    #[test]
    fn parse_host_keys_requires_at_least_one_valid_openssh_key() {
        assert_eq!(
            parse_host_keys(&[]).unwrap_err().code,
            ErrorCode::InvalidArgs
        );
        assert_eq!(
            parse_host_keys(&["not-a-key".to_owned()]).unwrap_err().code,
            ErrorCode::InvalidArgs
        );
    }
}
