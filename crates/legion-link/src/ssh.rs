//! In-process SSH pipe using `russh`, with pinned host keys, Ed25519 key authentication,
//! keepalives, and stderr/exit-status reporting into [`Session`].

use std::borrow::Cow;
use std::collections::VecDeque;
use std::future::Future;
use std::io;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};
use std::time::Duration;

use legion_proto::{ErrorBody, ErrorCode};
use russh::client::{self, AuthResult};
use russh::keys::ssh_key::{Algorithm, EcdsaCurve, HashAlg, PublicKey};
use russh::keys::{PrivateKeyWithHashAlg, PublicKeyOrCertificate};
use russh::{
    AlgorithmKind, Channel, ChannelMsg, Disconnect, Preferred, cipher, compression, kex, mac,
};
use serde_json::json;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt, DuplexStream, ReadBuf};
use tokio::net::{TcpStream, lookup_host};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::command::{RemoteOs, Subcommand, remote_command};
use crate::content::{RawStream, StreamOpenError, StreamOpener, WindowPolicy};
use crate::invite::SshTarget;
pub use crate::invite::SshTimeouts;
use crate::key::DeviceKey;
use crate::session::{Session, TransportHandle};
use crate::stderr::StderrTail;

pub use crate::{REASON_AUTH_FAILED, REASON_HOST_KEY_MISMATCH, REASON_TIMEOUT, REASON_UNREACHABLE};

/// Internal duplex buffer size between the RPC SSH channel pump tasks and the session streams.
const CHANNEL_BUFFER_BYTES: usize = 64 * 1024;

/// Maximum concurrent content stream channels allowed on one SSH connection (strictly below
/// OpenSSH's default `MaxSessions = 10`, leaving room for the multiplexed RPC channel).
const MAX_CONCURRENT_SSH_STREAMS: usize = 8;

/// Initial pull-based window requested on an SSH `keep_open` `legion stream get` channel for fast
/// time to first byte and minimal buffering on an unread stream.
const SSH_STREAM_INITIAL_WINDOW: u64 = 256 * 1024;

/// Maximum adaptive pull-based window requested on an SSH `keep_open` `legion stream get` channel.
const SSH_STREAM_MAX_WINDOW: u64 = 4 * 1024 * 1024;

/// Hard memory cap for one SSH stream channel's non-blocking receive buffer (current window plus
/// one pipelined read-ahead window plus framing headroom).
const SSH_STREAM_BUFFER_CAP: usize = (2 * SSH_STREAM_MAX_WINDOW as usize) + 256 * 1024;

/// Per-channel SSH receive window size (4 MiB) so high-RTT links keep the pipe full.
const SSH_CHANNEL_WINDOW_SIZE: u32 = 4 * 1024 * 1024;

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
    session: Option<Arc<client::Handle<HostKeyHandler>>>,
    stream_permits: Arc<Semaphore>,
    connect_timeout: Duration,
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

    fn stream_opener(&self) -> Option<Arc<dyn StreamOpener>> {
        self.session.as_ref().map(|session| {
            Arc::new(SshStreamOpener {
                session: Arc::clone(session),
                stream_permits: Arc::clone(&self.stream_permits),
                connect_timeout: self.connect_timeout,
            }) as Arc<dyn StreamOpener>
        })
    }

    fn collect_exit_info(
        &mut self,
        wait_limit: Duration,
    ) -> Pin<Box<dyn Future<Output = (Option<i32>, String)> + Send + '_>> {
        Box::pin(async move {
            if !wait_limit.is_zero()
                && let Some(mut task) = self.rx_task.take()
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
            let deadline = tokio::time::Instant::now() + grace;
            if let Some(mut tx) = self.tx_task.take()
                && tokio::time::timeout_at(deadline, &mut tx).await.is_err()
            {
                tx.abort();
            }
            if let Some(mut rx) = self.rx_task.take()
                && tokio::time::timeout_at(deadline, &mut rx).await.is_err()
            {
                rx.abort();
            }
            if let Some(session) = self.session.take() {
                let _ = tokio::time::timeout_at(
                    deadline,
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

/// Policy applied to resolved socket addresses before opening a TCP connection.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct ConnectPolicy {
    /// When `true` (used for learned entries), refuse connecting to loopback (unless
    /// `allow_loopback`), unspecified, link-local, multicast, and broadcast addresses even if a
    /// hostname resolves to them at connect time.
    pub restrict_resolved_addrs: bool,
    /// When `true`, allow loopback addresses under `restrict_resolved_addrs`.
    pub allow_loopback: bool,
}

/// Opens a Legion [`Session`] over an in-process SSH exec channel to `target`.
///
/// Always sends the fixed Unix remote command string for `subcommand`: `legion gate` never runs
/// `SSH_ORIGINAL_COMMAND` through a shell; it compares the string against every shape
/// `remote_command` can produce across both Unix and Windows (see `crates/legion-net/src/gate.rs`),
/// so the Unix `rpc` command is accepted by gated Windows nodes as well as Linux and macOS nodes.
pub async fn open(
    machine: &str,
    target: &SshTarget,
    key: &DeviceKey,
    subcommand: Subcommand,
    timeouts: SshTimeouts,
    cancel: CancellationToken,
) -> Result<Session, ErrorBody> {
    open_with_policy(
        machine,
        target,
        key,
        subcommand,
        timeouts,
        ConnectPolicy::default(),
        cancel,
    )
    .await
}

pub(crate) async fn open_with_policy(
    machine: &str,
    target: &SshTarget,
    key: &DeviceKey,
    subcommand: Subcommand,
    timeouts: SshTimeouts,
    policy: ConnectPolicy,
    cancel: CancellationToken,
) -> Result<Session, ErrorBody> {
    let command = remote_command(RemoteOs::Unix, subcommand, None)?;
    let pipe = open_exec_raw_with_policy(target, key, &command, timeouts, policy, cancel.clone())
        .await
        .map_err(|err| err.with_machine(machine))?;
    let (reader, writer, handle) = pipe.into_parts();
    Session::open_transport(
        reader,
        writer,
        Box::new(handle),
        machine,
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
    open_exec_raw_with_policy(
        target,
        key,
        &command,
        timeouts,
        ConnectPolicy::default(),
        cancel,
    )
    .await
}

/// Opens a raw SSH exec channel running an exact `command` string on `target`.
pub async fn open_exec_raw(
    target: &SshTarget,
    key: &DeviceKey,
    command: &str,
    timeouts: SshTimeouts,
    cancel: CancellationToken,
) -> Result<SshPipe, ErrorBody> {
    open_exec_raw_with_policy(
        target,
        key,
        command,
        timeouts,
        ConnectPolicy::default(),
        cancel,
    )
    .await
}

async fn open_exec_raw_with_policy(
    target: &SshTarget,
    key: &DeviceKey,
    command: &str,
    timeouts: SshTimeouts,
    policy: ConnectPolicy,
    cancel: CancellationToken,
) -> Result<SshPipe, ErrorBody> {
    let expected_keys = parse_host_keys(&target.host_keys)?;
    // Fail fast if none of the pinned host keys use a supported algorithm.
    let _ = build_ssh_config(&expected_keys, None, timeouts)?;
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
            policy,
            cancel.clone(),
        )
        .await
        {
            Ok(pipe) => return Ok(pipe),
            Err(HostAttemptError::Cancelled(err)) => return Err(err),
            Err(HostAttemptError::Failed(err)) => {
                attempts.push((host.clone(), err));
            }
        }
    }

    Err(combine_failures(attempts))
}

fn error_reason(err: &ErrorBody) -> Option<&str> {
    err.details
        .as_ref()
        .and_then(|d| d.get("reason"))
        .and_then(serde_json::Value::as_str)
}

/// Combines per-host or per-address connection failures, giving priority to `host_key_mismatch`
/// (and then `auth_failed`) so a real cryptographic/auth failure is never hidden behind a later
/// unreachable host.
fn combine_failures(mut attempts: Vec<(String, ErrorBody)>) -> ErrorBody {
    if attempts.len() == 1
        && let Some((_, err)) = attempts.pop()
    {
        return err;
    }

    let summary = attempts
        .iter()
        .map(|(host, err)| format!("{host}: {}", err.message))
        .collect::<Vec<_>>()
        .join("; ");

    if let Some((_, mismatch)) = attempts
        .iter()
        .find(|(_, err)| error_reason(err) == Some(REASON_HOST_KEY_MISMATCH))
    {
        let mut details = mismatch
            .details
            .clone()
            .unwrap_or_else(|| json!({ "reason": REASON_HOST_KEY_MISMATCH }));
        if let Some(obj) = details.as_object_mut() {
            obj.insert("reason".to_owned(), json!(REASON_HOST_KEY_MISMATCH));
        }
        let mut combined = ErrorBody::new(
            ErrorCode::Denied,
            format!("Host key mismatch while connecting ({summary})."),
        )
        .with_details(details);
        if let Some(hint) = &mismatch.hint {
            combined = combined.with_hint(hint.clone());
        }
        return combined;
    }

    if let Some((_, auth)) = attempts
        .iter()
        .find(|(_, err)| error_reason(err) == Some(REASON_AUTH_FAILED))
    {
        let mut combined = ErrorBody::new(
            ErrorCode::Denied,
            format!("SSH authentication failed ({summary})."),
        )
        .with_details(
            auth.details
                .clone()
                .unwrap_or_else(|| json!({ "reason": REASON_AUTH_FAILED })),
        );
        if let Some(hint) = &auth.hint {
            combined = combined.with_hint(hint.clone());
        }
        return combined;
    }

    if let Some((_, disallowed)) = attempts
        .iter()
        .find(|(_, err)| error_reason(err) == Some("disallowed_address"))
    {
        let mut combined = ErrorBody::new(
            ErrorCode::Denied,
            format!("Refused connecting to disallowed address ({summary})."),
        )
        .with_details(
            disallowed
                .details
                .clone()
                .unwrap_or_else(|| json!({ "reason": "disallowed_address" })),
        );
        if let Some(hint) = &disallowed.hint {
            combined = combined.with_hint(hint.clone());
        }
        return combined;
    }

    let all_timeouts = !attempts.is_empty()
        && attempts
            .iter()
            .all(|(_, err)| err.code == ErrorCode::Timeout);
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
    ErrorBody::new(
        code,
        format!("Could not reach any configured host ({summary})."),
    )
    .with_hint("Check that the node is online, sshd is running, and your network can reach one of its addresses.")
    .with_details(json!({ "reason": reason }))
}

/// Splits a host's connect timeout across multiple resolved addresses so one blackholed address
/// (such as an unreachable IPv6 record) cannot consume the entire budget.
fn per_addr_connect_timeout(total: Duration, count: usize) -> Duration {
    if count <= 1 {
        return total;
    }
    let divisor = u32::try_from(count).unwrap_or(u32::MAX).max(1);
    (total / divisor).max(Duration::from_millis(500).min(total))
}

#[derive(Debug)]
enum HostAttemptError {
    /// Caller cancelled connecting; abort remaining hosts immediately.
    Cancelled(ErrorBody),
    /// This host failed; record the error and continue trying remaining hosts.
    Failed(ErrorBody),
}

fn filter_resolved_addrs(
    host: &str,
    port: u16,
    resolved: Vec<SocketAddr>,
    allow_loopback: bool,
) -> Result<Vec<SocketAddr>, HostAttemptError> {
    let allowed: Vec<SocketAddr> = resolved
        .into_iter()
        .filter(|addr| crate::link::is_allowed_ip(addr.ip(), allow_loopback))
        .collect();
    if allowed.is_empty() {
        return Err(HostAttemptError::Failed(
            ErrorBody::new(
                ErrorCode::Denied,
                format!(
                    "Refused connecting to learned host {host}:{port}: resolved only to disallowed address(es)."
                ),
            )
            .with_hint(
                "Learned machines may not resolve to loopback, link-local, unspecified, multicast, or broadcast addresses.",
            )
            .with_details(json!({
                "reason": "disallowed_address",
                "host": host,
                "port": port,
            })),
        ));
    }
    Ok(allowed)
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
    policy: ConnectPolicy,
    cancel: CancellationToken,
) -> Result<SshPipe, HostAttemptError> {
    let resolved: Vec<SocketAddr> = tokio::select! {
        biased;
        () = cancel.cancelled() => {
            return Err(HostAttemptError::Cancelled(ErrorBody::new(
                ErrorCode::Cancelled,
                format!("Connecting to {host}:{port} was cancelled."),
            )));
        }
        res = tokio::time::timeout(timeouts.connect, lookup_host((host, port))) => match res {
            Ok(Ok(iter)) => iter.collect(),
            Ok(Err(err)) => {
                return Err(HostAttemptError::Failed(
                    ErrorBody::new(
                        ErrorCode::Unreachable,
                        format!("Could not resolve {host}:{port}: {err}."),
                    )
                    .with_hint("Check the host name and network connectivity.")
                    .with_details(json!({
                        "reason": REASON_UNREACHABLE,
                        "host": host,
                        "port": port,
                    })),
                ));
            }
            Err(_) => {
                return Err(HostAttemptError::Failed(
                    ErrorBody::new(
                        ErrorCode::Timeout,
                        format!(
                            "Timed out resolving {host}:{port} after {}s.",
                            timeouts.connect.as_secs_f64()
                        ),
                    )
                    .with_hint("Check DNS and network connectivity.")
                    .with_details(json!({
                        "reason": REASON_TIMEOUT,
                        "host": host,
                        "port": port,
                    })),
                ));
            }
        }
    };

    if resolved.is_empty() {
        return Err(HostAttemptError::Failed(
            ErrorBody::new(
                ErrorCode::Unreachable,
                format!("Host {host}:{port} resolved to no addresses."),
            )
            .with_hint("Check the host name and DNS configuration.")
            .with_details(json!({
                "reason": REASON_UNREACHABLE,
                "host": host,
                "port": port,
            })),
        ));
    }

    let addrs = if policy.restrict_resolved_addrs {
        filter_resolved_addrs(host, port, resolved, policy.allow_loopback)?
    } else {
        resolved
    };

    let tcp_timeout = per_addr_connect_timeout(timeouts.connect, addrs.len());
    let mut addr_failures: Vec<(String, ErrorBody)> = Vec::new();
    for addr in addrs {
        match connect_one_addr(
            host,
            port,
            addr,
            user,
            expected_keys,
            key,
            command,
            tcp_timeout,
            timeouts,
            cancel.clone(),
        )
        .await
        {
            Ok(pipe) => return Ok(pipe),
            Err(HostAttemptError::Cancelled(err)) => return Err(HostAttemptError::Cancelled(err)),
            Err(HostAttemptError::Failed(err)) => {
                addr_failures.push((addr.to_string(), err));
            }
        }
    }

    Err(HostAttemptError::Failed(combine_failures(addr_failures)))
}

#[allow(clippy::too_many_arguments)]
async fn connect_one_addr(
    host: &str,
    port: u16,
    addr: SocketAddr,
    user: &str,
    expected_keys: &[PublicKey],
    key: &DeviceKey,
    command: &str,
    tcp_timeout: Duration,
    timeouts: SshTimeouts,
    cancel: CancellationToken,
) -> Result<SshPipe, HostAttemptError> {
    let tcp = tokio::select! {
        biased;
        () = cancel.cancelled() => {
            return Err(HostAttemptError::Cancelled(ErrorBody::new(
                ErrorCode::Cancelled,
                format!("Connecting to {host}:{port} was cancelled."),
            )));
        }
        res = tokio::time::timeout(tcp_timeout, TcpStream::connect(addr)) => match res {
            Ok(Ok(stream)) => stream,
            Ok(Err(err)) => {
                return Err(HostAttemptError::Failed(
                    ErrorBody::new(
                        ErrorCode::Unreachable,
                        format!("Could not connect to {host}:{port} ({addr}): {err}."),
                    )
                    .with_hint("Check that the node is awake and sshd is listening on that port.")
                    .with_details(json!({
                        "reason": REASON_UNREACHABLE,
                        "host": host,
                        "port": port,
                    })),
                ));
            }
            Err(_) => {
                return Err(HostAttemptError::Failed(
                    ErrorBody::new(
                        ErrorCode::Timeout,
                        format!(
                            "Timed out connecting to {host}:{port} ({addr}) after {}s.",
                            tcp_timeout.as_secs_f64()
                        ),
                    )
                    .with_hint("Check that the host is awake, reachable over your network, and accepting SSH connections.")
                    .with_details(json!({
                        "reason": REASON_TIMEOUT,
                        "host": host,
                        "port": port,
                    })),
                ));
            }
        }
    };
    let _ = tcp.set_nodelay(true);

    let ssh_fut = async {
        let mut session = handshake_with_host_key_retry(
            host,
            port,
            addr,
            tcp,
            expected_keys,
            tcp_timeout,
            timeouts,
        )
        .await?;

        let auth = session
            .authenticate_publickey(
                user,
                PrivateKeyWithHashAlg::new(Arc::new(key.private_key().clone()), None),
            )
            .await
            .map_err(|err| {
                HostAttemptError::Failed(
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
            return Err(HostAttemptError::Failed(
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
            HostAttemptError::Failed(
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
            HostAttemptError::Failed(
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

        Ok(bridge_channel(session, channel, timeouts.connect))
    };

    tokio::select! {
        biased;
        () = cancel.cancelled() => Err(HostAttemptError::Cancelled(
            ErrorBody::new(
                ErrorCode::Cancelled,
                format!("Connecting to {host}:{port} was cancelled."),
            ),
        )),
        res = tokio::time::timeout(timeouts.connect, ssh_fut) => match res {
            Ok(outcome) => outcome,
            Err(_) => Err(HostAttemptError::Failed(
                ErrorBody::new(
                    ErrorCode::Timeout,
                    format!(
                        "Timed out during SSH handshake with {host}:{port} after {}s.",
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

async fn handshake_with_host_key_retry(
    host: &str,
    port: u16,
    addr: SocketAddr,
    tcp: TcpStream,
    expected_keys: &[PublicKey],
    tcp_timeout: Duration,
    timeouts: SshTimeouts,
) -> Result<client::Handle<HostKeyHandler>, HostAttemptError> {
    let first = handshake_once(host, port, tcp, expected_keys, None, timeouts).await;
    match first {
        Ok(session) => Ok(session),
        Err(HandshakeAttemptError::HostKeyMismatch {
            error: first_err,
            offered_family: Some(fam),
        }) if !host_key_algorithms(expected_keys, Some(fam)).is_empty() => {
            // The server offered a key of family `fam` that did not match our pin (for instance a
            // rotated ed25519 key while our ecdsa pin still matches). Retry once without `fam`.
            let tcp2 = match tokio::time::timeout(tcp_timeout, TcpStream::connect(addr)).await {
                Ok(Ok(stream)) => {
                    let _ = stream.set_nodelay(true);
                    stream
                }
                _ => return Err(HostAttemptError::Failed(first_err)),
            };
            match handshake_once(host, port, tcp2, expected_keys, Some(fam), timeouts).await {
                Ok(session) => Ok(session),
                Err(HandshakeAttemptError::HostKeyMismatch { error, .. }) => {
                    Err(HostAttemptError::Failed(error))
                }
                // If the server has no key of the fallback family (NoCommonAlgo) or fails on retry,
                // keep the original mismatch error carrying the fingerprint it actually offered.
                Err(HandshakeAttemptError::Other(_)) => Err(HostAttemptError::Failed(first_err)),
            }
        }
        Err(HandshakeAttemptError::HostKeyMismatch { error, .. }) => {
            Err(HostAttemptError::Failed(error))
        }
        Err(HandshakeAttemptError::Other(err)) => Err(err),
    }
}

enum HandshakeAttemptError {
    HostKeyMismatch {
        error: ErrorBody,
        offered_family: Option<HostKeyFamily>,
    },
    Other(HostAttemptError),
}

async fn handshake_once(
    host: &str,
    port: u16,
    tcp: TcpStream,
    expected_keys: &[PublicKey],
    exclude: Option<HostKeyFamily>,
    timeouts: SshTimeouts,
) -> Result<client::Handle<HostKeyHandler>, HandshakeAttemptError> {
    let offered_mismatch = Arc::new(Mutex::new(None::<OfferedMismatch>));
    let handler = HostKeyHandler {
        expected: expected_keys.to_vec(),
        offered_mismatch: Arc::clone(&offered_mismatch),
    };
    let config = build_ssh_config(expected_keys, exclude, timeouts)
        .map_err(|err| HandshakeAttemptError::Other(HostAttemptError::Failed(err)))?;
    client::connect_stream(Arc::new(config), tcp, handler)
        .await
        .map_err(|err| map_handshake_error(host, port, err, &offered_mismatch, expected_keys))
}

fn map_handshake_error(
    host: &str,
    port: u16,
    err: russh::Error,
    offered_mismatch: &Arc<Mutex<Option<OfferedMismatch>>>,
    expected_keys: &[PublicKey],
) -> HandshakeAttemptError {
    if let Some(mismatch) = offered_mismatch
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone()
    {
        return HandshakeAttemptError::HostKeyMismatch {
            error: host_key_mismatch_error(host, port, &mismatch.fingerprint, expected_keys),
            offered_family: mismatch.family,
        };
    }
    if let russh::Error::NoCommonAlgo {
        kind: AlgorithmKind::Key,
        ours,
        theirs,
    } = err
    {
        return HandshakeAttemptError::Other(HostAttemptError::Failed(
            ErrorBody::new(
                ErrorCode::Denied,
                format!(
                    "Host key mismatch for {host}:{port}: the server offers only {} while this target pins {}.",
                    if theirs.is_empty() {
                        "no host key algorithms".to_owned()
                    } else {
                        theirs.join(", ")
                    },
                    ours.join(", "),
                ),
            )
            .with_hint(
                "None of the server's host key types match the pinned host keys. Verify the node's host keys and pair again if they changed.",
            )
            .with_details(json!({
                "reason": REASON_HOST_KEY_MISMATCH,
                "host": host,
                "port": port,
                "offered_algorithms": theirs,
                "pinned_algorithms": ours,
            })),
        ));
    }
    HandshakeAttemptError::Other(HostAttemptError::Failed(
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
    ))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum HostKeyFamily {
    Ed25519,
    Ecdsa(EcdsaCurve),
    Rsa,
}

impl HostKeyFamily {
    fn from_algorithm(alg: Algorithm) -> Option<Self> {
        match alg {
            Algorithm::Ed25519 => Some(HostKeyFamily::Ed25519),
            Algorithm::Ecdsa { curve } => Some(HostKeyFamily::Ecdsa(curve)),
            Algorithm::Rsa { .. } => Some(HostKeyFamily::Rsa),
            _ => None,
        }
    }
}

#[derive(Clone, Debug)]
struct OfferedMismatch {
    fingerprint: String,
    family: Option<HostKeyFamily>,
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
    offered_mismatch: Arc<Mutex<Option<OfferedMismatch>>>,
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
                let family =
                    HostKeyFamily::from_algorithm(server_public_key.public_key().algorithm());
                *self
                    .offered_mismatch
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(OfferedMismatch {
                    fingerprint: offered_fingerprint,
                    family,
                });
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

/// Returns the supported host-key algorithms present in `expected_keys` in canonical preference
/// order (`ed25519`, `ecdsa`, `rsa`), regardless of the order the pins were listed in, optionally
/// excluding one family after a mismatch.
fn host_key_algorithms(
    expected_keys: &[PublicKey],
    exclude: Option<HostKeyFamily>,
) -> Vec<Algorithm> {
    let has = |fam: HostKeyFamily| -> bool {
        if exclude == Some(fam) {
            return false;
        }
        expected_keys
            .iter()
            .any(|k| HostKeyFamily::from_algorithm(k.algorithm()) == Some(fam))
    };

    let mut algos = Vec::new();
    if has(HostKeyFamily::Ed25519) {
        algos.push(Algorithm::Ed25519);
    }
    for curve in [
        EcdsaCurve::NistP256,
        EcdsaCurve::NistP384,
        EcdsaCurve::NistP521,
    ] {
        if has(HostKeyFamily::Ecdsa(curve)) {
            algos.push(Algorithm::Ecdsa { curve });
        }
    }
    if has(HostKeyFamily::Rsa) {
        algos.push(Algorithm::Rsa {
            hash: Some(HashAlg::Sha512),
        });
        algos.push(Algorithm::Rsa {
            hash: Some(HashAlg::Sha256),
        });
    }
    algos
}

/// Modern algorithm set accepted by OpenSSH 8+ and Windows OpenSSH by default, filtered to host
/// key algorithms present in `expected_keys` in canonical order (`ed25519`, `ecdsa`, `rsa`).
fn build_ssh_config(
    expected_keys: &[PublicKey],
    exclude: Option<HostKeyFamily>,
    timeouts: SshTimeouts,
) -> Result<client::Config, ErrorBody> {
    let host_key_algos = host_key_algorithms(expected_keys, exclude);
    if host_key_algos.is_empty() {
        return Err(ErrorBody::new(
            ErrorCode::Unsupported,
            "None of the pinned SSH host keys use a supported key algorithm (ed25519, ecdsa, rsa).",
        )
        .with_hint("Pin at least one ssh-ed25519, ecdsa-sha2-nistp*, or ssh-rsa host key."));
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

    Ok(client::Config {
        preferred,
        keepalive_interval: timeouts.keepalive_interval,
        keepalive_max: timeouts.keepalive_max,
        window_size: SSH_CHANNEL_WINDOW_SIZE,
        nodelay: true,
        ..client::Config::default()
    })
}

fn bridge_channel(
    session: client::Handle<HostKeyHandler>,
    channel: Channel<client::Msg>,
    connect_timeout: Duration,
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
            session: Some(Arc::new(session)),
            stream_permits: Arc::new(Semaphore::new(MAX_CONCURRENT_SSH_STREAMS)),
            connect_timeout,
            stderr,
            exit_code,
            rx_task: Some(rx_task),
            tx_task: Some(tx_task),
        },
    }
}

struct SshStreamOpener {
    session: Arc<client::Handle<HostKeyHandler>>,
    stream_permits: Arc<Semaphore>,
    connect_timeout: Duration,
}

impl StreamOpener for SshStreamOpener {
    fn window_policy(&self) -> Option<WindowPolicy> {
        Some(WindowPolicy {
            initial_bytes: SSH_STREAM_INITIAL_WINDOW,
            max_bytes: SSH_STREAM_MAX_WINDOW,
        })
    }

    fn open_stream(
        &self,
        cancel: CancellationToken,
    ) -> Pin<Box<dyn Future<Output = Result<RawStream, StreamOpenError>> + Send + '_>> {
        Box::pin(async move {
            let command = remote_command(RemoteOs::Unix, Subcommand::Stream, None)
                .map_err(StreamOpenError::Failed)?;

            let permit = tokio::select! {
                biased;
                () = cancel.cancelled() => {
                    return Err(StreamOpenError::Failed(ErrorBody::new(
                        ErrorCode::Cancelled,
                        "Opening the SSH stream channel was cancelled.",
                    )));
                }
                res = tokio::time::timeout(
                    self.connect_timeout,
                    Arc::clone(&self.stream_permits).acquire_owned(),
                ) => match res {
                    Ok(Ok(permit)) => permit,
                    Ok(Err(_)) => {
                        return Err(StreamOpenError::DeadSession(
                            ErrorBody::new(
                                ErrorCode::Unreachable,
                                "The SSH session closed while waiting for a stream slot.",
                            )
                            .with_details(json!({ "reason": REASON_UNREACHABLE })),
                        ));
                    }
                    Err(_) => {
                        return Err(StreamOpenError::ChannelRefused(
                            ErrorBody::new(
                                ErrorCode::Busy,
                                "Too many concurrent content streams are open on this SSH connection.",
                            )
                            .with_hint("Retry once an existing stream finishes.")
                            .with_details(json!({
                                "reason": "channel_limit",
                                "retryable": true,
                            })),
                        ));
                    }
                }
            };

            let open_fut = async {
                let channel = open_channel_with_retry(|| async {
                    self.session
                        .channel_open_session()
                        .await
                        .map_err(classify_channel_open_error)
                })
                .await?;
                channel.exec(true, command).await.map_err(|err| {
                    let _ = err;
                    StreamOpenError::DeadSession(
                        ErrorBody::new(
                            ErrorCode::Unreachable,
                            "Could not start `legion stream` on the SSH channel.",
                        )
                        .with_hint(
                            "Check that the paired key is allowed to run `legion gate` on the node.",
                        )
                        .with_details(json!({ "reason": REASON_UNREACHABLE })),
                    )
                })?;
                Ok(bridge_stream_channel(channel, permit))
            };

            tokio::select! {
                biased;
                () = cancel.cancelled() => Err(StreamOpenError::Failed(ErrorBody::new(
                    ErrorCode::Cancelled,
                    "Opening the SSH stream channel was cancelled.",
                ))),
                res = tokio::time::timeout(self.connect_timeout, open_fut) => match res {
                    Ok(outcome) => outcome,
                    Err(_) => Err(StreamOpenError::DeadSession(
                        ErrorBody::new(
                            ErrorCode::Timeout,
                            format!(
                                "Timed out opening an SSH stream channel after {}s.",
                                self.connect_timeout.as_secs_f64()
                            ),
                        )
                        .with_details(json!({ "reason": REASON_TIMEOUT })),
                    )),
                },
            }
        })
    }
}

/// OpenSSH can briefly return `ChannelOpenFailure` while a just-closed channel is still being
/// reaped in `server_loop2` even though our local permit count stays strictly below default
/// `MaxSessions = 10`. Retry briefly before surfacing `Busy`.
const CHANNEL_OPEN_RETRY_DELAYS_MS: [u64; 5] = [10, 30, 80, 200, 400];

async fn open_channel_with_retry<T, F, Fut>(mut try_open: F) -> Result<T, StreamOpenError>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, StreamOpenError>>,
{
    let mut attempt = 0usize;
    loop {
        match try_open().await {
            Ok(channel) => return Ok(channel),
            Err(StreamOpenError::ChannelRefused(body)) => {
                let Some(&delay_ms) = CHANNEL_OPEN_RETRY_DELAYS_MS.get(attempt) else {
                    return Err(StreamOpenError::ChannelRefused(body));
                };
                attempt += 1;
                tokio::time::sleep(Duration::from_millis(delay_ms)).await;
            }
            Err(other) => return Err(other),
        }
    }
}

/// Distinguishes a server-side channel refusal (such as sshd `MaxSessions`) from a dead SSH
/// transport so a transient channel refusal never tears down a healthy shared session.
fn classify_channel_open_error(err: russh::Error) -> StreamOpenError {
    match err {
        russh::Error::ChannelOpenFailure(reason) => StreamOpenError::ChannelRefused(
            ErrorBody::new(
                ErrorCode::Busy,
                format!("The SSH server refused to open another stream channel ({reason:?})."),
            )
            .with_hint(
                "The server may be at its sshd MaxSessions limit; retry once a stream closes.",
            )
            .with_details(json!({
                "reason": "channel_open_failure",
                "permits_free": true,
                "retryable": true,
            })),
        ),
        other => StreamOpenError::DeadSession(
            ErrorBody::new(
                ErrorCode::Unreachable,
                format!("Could not open an SSH stream channel: {other}."),
            )
            .with_hint("Check sshd MaxSessions and that the connection is still up.")
            .with_details(json!({ "reason": REASON_UNREACHABLE })),
        ),
    }
}

struct SshStreamGuard {
    cancel: CancellationToken,
    park: CancellationToken,
    rx_finished: CancellationToken,
}

impl Drop for SshStreamGuard {
    fn drop(&mut self) {
        if !self.rx_finished.is_cancelled() && !self.park.is_cancelled() {
            self.cancel.cancel();
        }
    }
}

#[derive(Default)]
struct ChannelBufState {
    queue: VecDeque<Box<dyn AsRef<[u8]> + Send>>,
    head_offset: usize,
    buffered: usize,
    eof: bool,
    reader_dropped: bool,
    error: Option<io::Error>,
    waker: Option<Waker>,
}

struct BoundedChannelReader {
    state: Arc<Mutex<ChannelBufState>>,
    cancel: CancellationToken,
}

impl Drop for BoundedChannelReader {
    fn drop(&mut self) {
        let should_cancel = {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let incomplete = !state.eof;
            state.reader_dropped = true;
            state.queue = VecDeque::new();
            state.buffered = 0;
            incomplete
        };
        if should_cancel {
            self.cancel.cancel();
        }
    }
}

impl AsyncRead for BoundedChannelReader {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        if buf.remaining() == 0 {
            return Poll::Ready(Ok(()));
        }
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut copied = 0usize;
        while buf.remaining() > 0 {
            let Some(front) = state.queue.front() else {
                break;
            };
            let slice = (**front).as_ref();
            let front_len = slice.len();
            let avail = &slice[state.head_offset..];
            let take = avail.len().min(buf.remaining());
            buf.put_slice(&avail[..take]);
            state.head_offset += take;
            state.buffered -= take;
            copied += take;
            if state.head_offset >= front_len {
                state.queue.pop_front();
                state.head_offset = 0;
            }
        }
        if copied > 0 {
            return Poll::Ready(Ok(()));
        }
        if let Some(err) = state.error.take() {
            return Poll::Ready(Err(err));
        }
        if state.eof {
            return Poll::Ready(Ok(()));
        }
        state.waker = Some(cx.waker().clone());
        Poll::Pending
    }
}

/// Bridges an independent SSH exec channel for `legion stream` without ever blocking russh's
/// single session reader loop: incoming packets are queued directly into a bounded per-channel
/// buffer (`SSH_STREAM_BUFFER_CAP`), and the SSH channel and its concurrency permit close when
/// the remote finishes or the reader is cancelled/dropped.
fn bridge_stream_channel(channel: Channel<client::Msg>, permit: OwnedSemaphorePermit) -> RawStream {
    let (mut read_half, write_half) = channel.split();
    let (mut stdin_read, stdin_write) = tokio::io::duplex(CHANNEL_BUFFER_BYTES);

    let buf_state = Arc::new(Mutex::new(ChannelBufState::default()));
    let stderr = Arc::new(StderrTail::default());
    let channel_cancel = CancellationToken::new();
    let park_cancel = CancellationToken::new();
    let rx_finished = CancellationToken::new();
    let remote_closed_tok = CancellationToken::new();
    let force_close_tok = CancellationToken::new();

    let rx_state = Arc::clone(&buf_state);
    let rx_stderr = Arc::clone(&stderr);
    let rx_cancel = channel_cancel.clone();
    let rx_park = park_cancel.clone();
    let rx_done = rx_finished.clone();
    let rx_remote_closed = remote_closed_tok.clone();
    let rx_force_close = force_close_tok.clone();
    tokio::spawn(async move {
        // Hold the concurrency permit until `sshd` has sent `ChannelMsg::Close` (or the channel
        // terminates / drain timeout elapses) so we never open a replacement channel while OpenSSH
        // still counts the closing channel against `MaxSessions`.
        let _permit = permit;
        let mut stderr_buf = Vec::<u8>::new();
        let mut eof_marked = false;
        let mut remote_closed = false;
        let mark_eof = |eof_marked: &mut bool| {
            if !*eof_marked {
                *eof_marked = true;
                let mut state = rx_state
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                state.eof = true;
                if let Some(waker) = state.waker.take() {
                    waker.wake();
                }
            }
        };
        loop {
            let msg = tokio::select! {
                biased;
                () = rx_cancel.cancelled() => break,
                () = rx_park.cancelled() => break,
                msg = read_half.wait() => msg,
            };
            let Some(msg) = msg else {
                remote_closed = true;
                break;
            };
            match msg {
                ChannelMsg::Data { data } => {
                    if data.is_empty() {
                        continue;
                    }
                    let mut state = rx_state
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                    if state.reader_dropped {
                        drop(state);
                        rx_cancel.cancel();
                        break;
                    }
                    if state.buffered.saturating_add(data.len()) > SSH_STREAM_BUFFER_CAP {
                        state.error = Some(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "remote stream exceeded bounded window buffer",
                        ));
                        state.eof = true;
                        eof_marked = true;
                        if let Some(waker) = state.waker.take() {
                            waker.wake();
                        }
                        drop(state);
                        rx_cancel.cancel();
                        break;
                    }
                    state.buffered += data.len();
                    state.queue.push_back(Box::new(data));
                    if let Some(waker) = state.waker.take() {
                        waker.wake();
                    }
                }
                ChannelMsg::ExtendedData { data, ext: 1 } => {
                    push_stderr_bytes(&rx_stderr, &mut stderr_buf, &data);
                }
                ChannelMsg::Eof => {
                    mark_eof(&mut eof_marked);
                    rx_done.cancel();
                }
                ChannelMsg::Close => {
                    remote_closed = true;
                    break;
                }
                _ => {}
            }
        }
        if !stderr_buf.is_empty() {
            let text = String::from_utf8_lossy(&stderr_buf);
            rx_stderr.push_line(&text);
        }
        mark_eof(&mut eof_marked);
        rx_done.cancel();
        if !remote_closed {
            // Wait for `sshd` to send `SSH_MSG_CHANNEL_CLOSE` after `tx_task` sends `EOF`:
            // `russh` automatically replies with `CHANNEL_CLOSE` via `close_from_peer` and
            // delivers `ChannelMsg::Close` here as soon as OpenSSH has freed its `MaxSessions`
            // slot. Only force a local `close()` if a misbehaving remote ignores stdin EOF.
            let closed_cleanly = tokio::time::timeout(Duration::from_secs(2), async {
                while let Some(msg) = read_half.wait().await {
                    if matches!(msg, ChannelMsg::Close) {
                        return true;
                    }
                }
                true
            })
            .await
            .unwrap_or(false);
            if !closed_cleanly {
                rx_force_close.cancel();
            }
        }
        rx_remote_closed.cancel();
    });

    let tx_cancel = channel_cancel.clone();
    let tx_park = park_cancel.clone();
    let tx_remote_closed = remote_closed_tok;
    let tx_force_close = force_close_tok;
    tokio::spawn(async move {
        let mut buf = vec![0u8; 16 * 1024];
        let mut stdin_open = true;
        loop {
            if !stdin_open {
                tokio::select! {
                    biased;
                    () = tx_remote_closed.cancelled() => break,
                    () = tx_force_close.cancelled() => {
                        let _ = write_half.close().await;
                        break;
                    }
                }
            }
            tokio::select! {
                biased;
                () = tx_remote_closed.cancelled() => break,
                () = tx_force_close.cancelled() => {
                    let _ = write_half.close().await;
                    break;
                }
                () = tx_cancel.cancelled() => {
                    let _ = write_half.eof().await;
                    stdin_open = false;
                }
                () = tx_park.cancelled() => {
                    let _ = write_half.eof().await;
                    stdin_open = false;
                }
                read = stdin_read.read(&mut buf) => match read {
                    Ok(0) => {
                        let _ = write_half.eof().await;
                        stdin_open = false;
                    }
                    Ok(n) => {
                        if write_half.data(&buf[..n]).await.is_err() {
                            break;
                        }
                    }
                    Err(_) => {
                        let _ = write_half.eof().await;
                        stdin_open = false;
                    }
                }
            }
        }
    });

    RawStream {
        reader: Box::new(BoundedChannelReader {
            state: buf_state,
            cancel: channel_cancel.clone(),
        }),
        writer: Box::new(stdin_write),
        stderr: Some(stderr),
        cancel: channel_cancel.clone(),
        park: Some(park_cancel.clone()),
        guard: Some(Box::new(SshStreamGuard {
            cancel: channel_cancel,
            park: park_cancel,
            rx_finished,
        })),
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

    #[test]
    fn host_key_algorithms_follow_canonical_order_and_support_exclusion() {
        // Valid NIST P-256 ECDSA public key (generator point) + Ed25519 public key.
        let ed = DeviceKey::from_seed([5u8; 32])
            .private_key()
            .public_key()
            .clone();
        let ecdsa = PublicKey::from_openssh(
            "ecdsa-sha2-nistp256 AAAAE2VjZHNhLXNoYTItbmlzdHAyNTYAAAAIbmlzdHAyNTYAAABBBGvC1z7tL0kP4c0L6K0d6O9u1nL2y2M9N5Z2G3m5e2d5M9Q2G4H7j8K1l2M3n4O5p6Q7r8S9t0U1v2W3x4Y5z6A=",
        );
        // Generate an actual valid ECDSA key via ssh_key::PrivateKey if needed, or test with
        // russh's own ed25519 + ecdsa test key.
        let ecdsa_key = match ecdsa {
            Ok(k) => k,
            Err(_) => {
                // Build a valid P-256 public key directly from the SEC1 generator point.
                let sec1 = [
                    0x04u8, 0x6b, 0x17, 0xd1, 0xf2, 0xe1, 0x2c, 0x42, 0x47, 0xf8, 0xbc, 0xe6, 0xe5,
                    0x63, 0xa4, 0x40, 0xf2, 0x77, 0x03, 0x7d, 0x81, 0x2d, 0xeb, 0x33, 0xa0, 0xf4,
                    0xa1, 0x39, 0x45, 0xd8, 0x98, 0xc2, 0x96, 0x4f, 0xe3, 0x42, 0xe2, 0xfe, 0x1a,
                    0x7f, 0x9b, 0x8e, 0xe7, 0xeb, 0x4a, 0x7c, 0x0f, 0x9e, 0x16, 0x2b, 0xce, 0x33,
                    0x57, 0x6b, 0x31, 0x5e, 0xce, 0xcb, 0xb6, 0x40, 0x68, 0x37, 0xbf, 0x51, 0xf5,
                ];
                let ecdsa_pub =
                    russh::keys::ssh_key::public::EcdsaPublicKey::from_sec1_bytes(&sec1).unwrap();
                PublicKey::from(russh::keys::ssh_key::public::KeyData::Ecdsa(ecdsa_pub))
            }
        };

        // Even when ECDSA is listed first in pins, Ed25519 is offered first.
        let algos = host_key_algorithms(&[ecdsa_key.clone(), ed.clone()], None);
        assert_eq!(
            algos,
            vec![
                Algorithm::Ed25519,
                Algorithm::Ecdsa {
                    curve: EcdsaCurve::NistP256
                }
            ]
        );

        // Excluding Ed25519 falls back to ECDSA only.
        let fallback = host_key_algorithms(&[ecdsa_key, ed.clone()], Some(HostKeyFamily::Ed25519));
        assert_eq!(
            fallback,
            vec![Algorithm::Ecdsa {
                curve: EcdsaCurve::NistP256
            }]
        );

        // No remaining supported algorithms fails cleanly with Unsupported.
        let err = build_ssh_config(&[ed], Some(HostKeyFamily::Ed25519), SshTimeouts::default())
            .err()
            .unwrap();
        assert_eq!(err.code, ErrorCode::Unsupported);
    }

    #[test]
    fn no_common_key_algorithm_maps_to_denied_host_key_mismatch() {
        let ed = DeviceKey::from_seed([6u8; 32])
            .private_key()
            .public_key()
            .clone();
        let mismatch = Arc::new(Mutex::new(None));
        let outcome = map_handshake_error(
            "tower.example.net",
            22,
            russh::Error::NoCommonAlgo {
                kind: AlgorithmKind::Key,
                ours: vec!["ssh-ed25519".to_owned()],
                theirs: vec!["ecdsa-sha2-nistp256".to_owned()],
            },
            &mismatch,
            &[ed],
        );
        let HandshakeAttemptError::Other(HostAttemptError::Failed(err)) = outcome else {
            panic!("expected denied error");
        };
        assert_eq!(err.code, ErrorCode::Denied);
        assert!(
            err.message.contains("ecdsa-sha2-nistp256"),
            "{}",
            err.message
        );
        assert!(err.message.contains("ssh-ed25519"), "{}", err.message);
        assert!(
            err.hint.as_deref().unwrap().contains("pinned host keys"),
            "{:?}",
            err.hint
        );
        assert_eq!(
            err.details.unwrap()["reason"],
            json!(REASON_HOST_KEY_MISMATCH)
        );
    }

    #[test]
    fn combine_failures_prioritizes_host_key_mismatch_and_splits_addr_timeout() {
        let ed = DeviceKey::from_seed([7u8; 32])
            .private_key()
            .public_key()
            .clone();
        let mismatch = host_key_mismatch_error("192.0.2.1", 22, "SHA256:badkey", &[ed]);
        let unreachable = ErrorBody::new(ErrorCode::Unreachable, "connection refused")
            .with_details(json!({"reason": REASON_UNREACHABLE}));

        let combined = combine_failures(vec![
            ("192.0.2.1".to_owned(), mismatch),
            ("198.51.100.1".to_owned(), unreachable),
        ]);
        assert_eq!(combined.code, ErrorCode::Denied);
        assert!(
            combined.message.contains("SHA256:badkey"),
            "{}",
            combined.message
        );
        let details = combined.details.unwrap();
        assert_eq!(details["reason"], json!(REASON_HOST_KEY_MISMATCH));
        assert_eq!(details["offered"], json!("SHA256:badkey"));

        assert_eq!(
            per_addr_connect_timeout(Duration::from_secs(10), 1),
            Duration::from_secs(10)
        );
        assert_eq!(
            per_addr_connect_timeout(Duration::from_secs(10), 2),
            Duration::from_secs(5)
        );
        assert_eq!(
            per_addr_connect_timeout(Duration::from_secs(10), 4),
            Duration::from_millis(2500)
        );
    }

    #[tokio::test]
    async fn learned_connect_policy_refuses_disallowed_resolved_addresses_before_tcp() {
        use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
        use tokio::net::TcpListener;

        let bad_ips = [
            IpAddr::V4(Ipv4Addr::LOCALHOST),
            IpAddr::V4(Ipv4Addr::UNSPECIFIED),
            IpAddr::V4(Ipv4Addr::new(169, 254, 169, 254)),
            IpAddr::V4(Ipv4Addr::new(224, 0, 0, 1)),
            IpAddr::V4(Ipv4Addr::BROADCAST),
            IpAddr::V6(Ipv6Addr::LOCALHOST),
            IpAddr::V6(Ipv6Addr::UNSPECIFIED),
            IpAddr::V6("fe80::1".parse().unwrap()),
            IpAddr::V6("ff02::1".parse().unwrap()),
            IpAddr::V6("::127.0.0.1".parse().unwrap()),
            IpAddr::V6("::ffff:127.0.0.1".parse().unwrap()),
            IpAddr::V6("::169.254.169.254".parse().unwrap()),
        ];
        for ip in bad_ips {
            let err = filter_resolved_addrs(
                "rebind.example.net",
                22,
                vec![SocketAddr::new(ip, 22)],
                false,
            )
            .unwrap_err();
            let HostAttemptError::Failed(body) = err else {
                panic!("expected Failed for {ip}");
            };
            assert_eq!(body.code, ErrorCode::Denied, "expected Denied for {ip}");
            assert_eq!(body.details.unwrap()["reason"], json!("disallowed_address"));
        }

        // Mixed resolved list drops disallowed addresses and keeps allowed ones.
        let kept = filter_resolved_addrs(
            "dual.example.net",
            22,
            vec![
                SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 22),
                SocketAddr::new(IpAddr::V4(Ipv4Addr::new(192, 0, 2, 10)), 22),
            ],
            false,
        )
        .unwrap();
        assert_eq!(
            kept,
            vec![SocketAddr::new(
                IpAddr::V4(Ipv4Addr::new(192, 0, 2, 10)),
                22
            )]
        );

        // Live pre-connect check: bind a loopback listener and verify learned connect refuses
        // `localhost` before accepting any TCP connection when allow_loopback is false.
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let accepted = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let accepted_bg = Arc::clone(&accepted);
        let accept_task = tokio::spawn(async move {
            if listener.accept().await.is_ok() {
                accepted_bg.store(true, std::sync::atomic::Ordering::SeqCst);
            }
        });

        let key = DeviceKey::from_seed([9u8; 32]);
        let host_key = DeviceKey::from_seed([10u8; 32])
            .public_key_line("h")
            .unwrap();
        let target = SshTarget {
            hosts: vec!["localhost".to_owned()],
            port,
            user: "me".to_owned(),
            host_keys: vec![host_key],
        };
        let Err(err) = open_with_policy(
            "rebind",
            &target,
            &key,
            Subcommand::Rpc,
            SshTimeouts::default(),
            ConnectPolicy {
                restrict_resolved_addrs: true,
                allow_loopback: false,
            },
            CancellationToken::new(),
        )
        .await
        else {
            panic!("expected learned loopback connect to be refused");
        };
        assert_eq!(err.code, ErrorCode::Denied, "{err}");
        assert_eq!(err.details.unwrap()["reason"], json!("disallowed_address"));
        assert!(
            !accepted.load(std::sync::atomic::Ordering::SeqCst),
            "TCP connection must never be opened to a disallowed resolved address"
        );
        accept_task.abort();
    }

    #[tokio::test]
    async fn channel_open_retry_rides_out_sshd_reap_delay_past_160ms() {
        let started = std::time::Instant::now();
        let attempts = std::sync::atomic::AtomicUsize::new(0);
        let res = open_channel_with_retry(|| {
            let n = attempts.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            async move {
                // Simulate sshd taking 250 ms to finish reaping a closed session slot.
                if started.elapsed() < Duration::from_millis(250) {
                    Err(StreamOpenError::ChannelRefused(ErrorBody::new(
                        ErrorCode::Busy,
                        format!("channel refused on attempt {n}"),
                    )))
                } else {
                    Ok("opened")
                }
            }
        })
        .await;
        assert_eq!(
            res.ok(),
            Some("opened"),
            "channel-open retries should outlast a 250 ms sshd reap window"
        );
    }
}
