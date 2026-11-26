//! Local SSH loopback tunnel (`op: "tunnel"`) for end-to-end encrypted ("sealed") sessions.
//!
//! When a controller app cannot reach a fleet target directly, it asks a relay to open
//! `{"protocol":1,"op":"tunnel"}` on the target. The target connects strictly to its own `sshd`
//! (`127.0.0.1:<port>` first, falling back to its own advertised addresses from `node.toml` /
//! Tailscale / hostname — never any address from the request), replies `{"tunnel":true}\n`, and
//! copies raw bytes bidirectionally with bounded buffers, live revocation checks, and no idle
//! timeout. Everything inside the tunnel is authenticated by the target's `sshd` against the
//! device's own key and forced command.

use std::collections::HashSet;
use std::io::{BufReader, BufWriter, Read, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::time::{Duration, Instant};

use legion_core::config::{ConfigStore, NodeFile};
use legion_core::policy::{NOBODY, Policy};
use legion_core::{CancellationToken, Door};
use legion_proto::{ErrorBody, ErrorCode};
use serde_json::{Map, Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpStream, lookup_host};
use tokio::sync::mpsc as async_mpsc;

use crate::stream::{
    Host, OUTPUT_BUFFER, QUEUED_CHUNKS, StreamServeOptions, check_pairing_not_revoked, gone, line,
    machine_or, record, reply_error,
};
use crate::stream_relay::{
    RelayAuthCheck, RelayAuthTarget, RelayRevokeWatchdog, RuntimeChoice, client_gone_error,
    relay_shutdown_error,
};

/// Timeout for connecting to the node's own loopback `127.0.0.1:<port>` SSH server.
const SELF_LOOPBACK_CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
/// Timeout per fallback address when `sshd` is not listening on `127.0.0.1`.
const SELF_ADDR_CONNECT_TIMEOUT: Duration = Duration::from_secs(3);
/// Maximum fallback socket addresses attempted after `127.0.0.1` fails.
const MAX_SELF_FALLBACK_ADDRS: usize = 16;
/// Grace period waiting for the tunnel writer task to finish after the read side ends.
const TUNNEL_WRITER_JOIN_GRACE: Duration = Duration::from_millis(200);

/// Refuses any destination or unknown field in a `tunnel` header.
///
/// Only `protocol`, `op`, and (when present) `as` are accepted; `host`, `port`, `path`, or any
/// other field is rejected with `invalid_args`.
pub(crate) fn validate_tunnel_fields(fields: &Map<String, Value>) -> Result<(), ErrorBody> {
    let mut unexpected: Vec<&str> = fields
        .keys()
        .map(String::as_str)
        .filter(|k| !matches!(*k, "protocol" | "op" | "as"))
        .collect();
    if unexpected.is_empty() {
        return Ok(());
    }
    unexpected.sort_unstable();
    Err(ErrorBody::new(
        ErrorCode::InvalidArgs,
        format!(
            "The tunnel operation accepts no destination or extra fields (unexpected: {}).",
            unexpected.join(", ")
        ),
    )
    .with_hint(
        "Send {\"protocol\":1,\"op\":\"tunnel\"} with no host, port, path, or other fields.",
    ))
}

/// Checks whether a principal may open or relay a `tunnel`: pairing must not be revoked and the
/// resolved principal's role must not be `nobody`.
pub(crate) fn authorise_tunnel_principal(
    host: &Host,
    config: &ConfigStore,
    pinned: Option<&str>,
    pairing_id: Option<&str>,
    as_principal: Option<&str>,
) -> Result<String, (Option<String>, ErrorBody)> {
    if let Some(pid) = pairing_id
        && let Err(error) = check_pairing_not_revoked(config, pid)
    {
        return Err((pinned.map(str::to_owned), machine_or(error, host.name())));
    }
    let policy = config
        .get::<Policy>()
        .map_err(|error| (pinned.map(str::to_owned), machine_or(error, host.name())))?;
    let principal = policy.resolve(as_principal, pinned).map_err(|error| {
        let caller = pinned.map_or_else(|| policy.default_principal().to_owned(), str::to_owned);
        (Some(caller), machine_or(error, host.name()))
    })?;
    let (role_name, _) = policy.role_of(&principal);
    if role_name == NOBODY {
        return Err((
            Some(principal.clone()),
            ErrorBody::new(
                ErrorCode::Denied,
                format!("{principal} has the {NOBODY} role and may not open a stream tunnel."),
            )
            .with_hint("Assign a role other than nobody in policy.toml to allow sealed sessions.")
            .with_machine(host.name()),
        ));
    }
    Ok(principal)
}

/// Serves a local `op: "tunnel"` operation: connects to this node's own `sshd`, writes
/// `{"tunnel":true}\n`, and copies raw bytes in both directions until either side closes or access
/// is revoked.
#[allow(clippy::too_many_arguments)]
pub(crate) fn serve_tunnel<R: Read + Send + 'static, W: Write>(
    host: &Host,
    pinned: Option<&str>,
    door: Door,
    device: Option<&str>,
    options: &StreamServeOptions,
    as_principal: Option<String>,
    recorded: Value,
    started: Instant,
    mut input: BufReader<R>,
    mut output: BufWriter<W>,
) -> Result<(), ErrorBody> {
    let config = ConfigStore::new(host.paths().config_dir());
    let principal = match authorise_tunnel_principal(
        host,
        &config,
        pinned,
        options.pairing_id.as_deref(),
        as_principal.as_deref(),
    ) {
        Ok(principal) => principal,
        Err((principal, error)) => {
            let _ = reply_error(&mut output, &error);
            record(
                host,
                principal,
                door,
                device,
                "stream.tunnel",
                recorded,
                started,
                &Err(error.clone()),
            );
            return Err(error);
        }
    };

    let rt = match RuntimeChoice::acquire(options, host) {
        Ok(rt) => rt,
        Err(error) => {
            let _ = reply_error(&mut output, &error);
            record(
                host,
                Some(principal),
                door,
                device,
                "stream.tunnel",
                recorded,
                started,
                &Err(error.clone()),
            );
            return Err(error);
        }
    };

    let auth_check = RelayAuthCheck::new(
        host,
        pinned,
        as_principal.as_deref(),
        options,
        RelayAuthTarget::Tunnel,
    );
    let _watchdog = RelayRevokeWatchdog::start(auth_check.clone());

    let handle = rt.handle().clone();
    let outcome = handle.block_on(async move {
        let tcp = match connect_self_sshd(host, &config, options, &auth_check).await {
            Ok(tcp) => tcp,
            Err(error) => {
                let error = machine_or(error, host.name());
                let _ = reply_error(&mut output, &error);
                return Err(error);
            }
        };

        if let Err(err) =
            line(&mut output, &json!({ "tunnel": true })).and_then(|()| output.flush())
        {
            drop(tcp);
            return Err(machine_or(gone(&err), host.name()));
        }

        let (mut tcp_read, mut tcp_write) = tcp.into_split();
        let (chunk_tx, mut chunk_rx) = async_mpsc::channel::<Vec<u8>>(QUEUED_CHUNKS);
        let stop_tunnel = CancellationToken::new();
        let reader_stop = stop_tunnel.clone();
        let reader_revoke = auth_check.revoke_token().clone();

        // Read client bytes on a dedicated OS thread from the existing `BufReader` so any bytes
        // already buffered after the header line are preserved verbatim, and returning early on
        // target EOF or revocation never blocks on client stdin.
        std::thread::spawn(move || {
            let mut buf = vec![0u8; OUTPUT_BUFFER];
            while !reader_stop.is_cancelled() && !reader_revoke.is_cancelled() {
                match input.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if reader_stop.is_cancelled()
                            || reader_revoke.is_cancelled()
                            || chunk_tx.blocking_send(buf[..n].to_vec()).is_err()
                        {
                            break;
                        }
                    }
                }
            }
        });

        let writer_stop = stop_tunnel.clone();
        let writer_shutdown = options.shutdown.clone();
        let writer_client_gone = options.client_gone.clone();
        let writer_auth = auth_check.clone();
        let writer_revoke = auth_check.revoke_token().clone();
        let mut writer_task = tokio::spawn(async move {
            let mut since_check = 0u64;
            let mut clean_eof = false;
            loop {
                if writer_auth
                    .maybe_recheck_async(&mut since_check, Some(&writer_stop))
                    .await
                    .is_err()
                {
                    break;
                }
                let next = tokio::select! {
                    biased;
                    () = writer_stop.cancelled() => break,
                    () = async {
                        match writer_shutdown.as_ref() {
                            Some(tok) => tok.cancelled().await,
                            None => std::future::pending().await,
                        }
                    } => break,
                    () = writer_revoke.cancelled() => break,
                    () = async {
                        match writer_client_gone.as_ref() {
                            Some(tok) => tok.cancelled().await,
                            None => std::future::pending().await,
                        }
                    } => break,
                    chunk = chunk_rx.recv() => chunk,
                };
                match next {
                    Some(bytes) => {
                        let chunk_len = bytes.len() as u64;
                        let write_ok = tokio::select! {
                            biased;
                            () = writer_stop.cancelled() => false,
                            () = async {
                                match writer_shutdown.as_ref() {
                                    Some(tok) => tok.cancelled().await,
                                    None => std::future::pending().await,
                                }
                            } => false,
                            () = writer_revoke.cancelled() => false,
                            () = async {
                                match writer_client_gone.as_ref() {
                                    Some(tok) => tok.cancelled().await,
                                    None => std::future::pending().await,
                                }
                            } => false,
                            res = async {
                                tcp_write.write_all(&bytes).await?;
                                tcp_write.flush().await
                            } => res.is_ok(),
                        };
                        if !write_ok {
                            break;
                        }
                        since_check = since_check.saturating_add(chunk_len);
                    }
                    None => {
                        clean_eof = true;
                        break;
                    }
                }
            }
            if clean_eof && !writer_stop.is_cancelled() && !writer_revoke.is_cancelled() {
                let _ = tcp_write.shutdown().await;
            }
            drop(tcp_write);
        });

        let mut buf = vec![0u8; OUTPUT_BUFFER];
        let mut since_check = 0u64;
        let read_outcome: Result<(), ErrorBody> = loop {
            if let Err(err) = auth_check.maybe_recheck_async(&mut since_check, None).await {
                break Err(err);
            }
            enum ReadEvent {
                Data(std::io::Result<usize>),
                Shutdown,
                ClientGone,
                Revoked,
            }
            let event = tokio::select! {
                biased;
                () = async {
                    match options.shutdown.as_ref() {
                        Some(tok) => tok.cancelled().await,
                        None => std::future::pending().await,
                    }
                } => ReadEvent::Shutdown,
                () = auth_check.revoke_token().cancelled() => ReadEvent::Revoked,
                () = async {
                    match options.client_gone.as_ref() {
                        Some(tok) => tok.cancelled().await,
                        None => std::future::pending().await,
                    }
                } => ReadEvent::ClientGone,
                res = tcp_read.read(&mut buf) => ReadEvent::Data(res),
            };
            match event {
                ReadEvent::Shutdown => break Err(relay_shutdown_error(host.name())),
                ReadEvent::Revoked => break Err(auth_check.denied_or_generic()),
                ReadEvent::ClientGone => break Err(client_gone_error(host.name())),
                ReadEvent::Data(Ok(0)) => break Ok(()),
                ReadEvent::Data(Ok(n)) => {
                    if let Err(err) = output.write_all(&buf[..n]).and_then(|()| output.flush()) {
                        break Err(machine_or(gone(&err), host.name()));
                    }
                    since_check = since_check.saturating_add(n as u64);
                }
                ReadEvent::Data(Err(err)) => {
                    break Err(ErrorBody::new(
                        ErrorCode::Unreachable,
                        format!("Reading from local SSH server failed: {err}."),
                    )
                    .with_machine(host.name()));
                }
            }
        };

        stop_tunnel.cancel();
        drop(tcp_read);
        if tokio::time::timeout(TUNNEL_WRITER_JOIN_GRACE, &mut writer_task)
            .await
            .is_err()
        {
            writer_task.abort();
            let _ = writer_task.await;
        }
        match (read_outcome, auth_check.denied_error()) {
            (Err(_), Some(denied)) => Err(denied),
            (other, _) => other,
        }
    });

    record(
        host,
        Some(principal),
        door,
        device,
        "stream.tunnel",
        recorded,
        started,
        &outcome,
    );
    outcome
}

async fn connect_self_sshd(
    host: &Host,
    config: &ConfigStore,
    options: &StreamServeOptions,
    auth_check: &RelayAuthCheck,
) -> Result<TcpStream, ErrorBody> {
    let node_file = config
        .get::<NodeFile>()
        .map_err(|err| machine_or(err, host.name()))?;
    let port = legion_tools::device::self_ssh_port(&node_file)
        .map_err(|err| machine_or(err, host.name()))?;

    let loopback = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port);
    let mut errors: Vec<String> = Vec::new();

    match connect_one_self_addr(
        loopback,
        SELF_LOOPBACK_CONNECT_TIMEOUT,
        host,
        options,
        auth_check,
    )
    .await
    {
        Ok(tcp) => return Ok(tcp),
        Err(SelfConnectErr::Terminal(err)) => return Err(err),
        Err(SelfConnectErr::Failed(msg)) => errors.push(format!("{loopback}: {msg}")),
    }

    let resolve_cancel = CancellationToken::new();
    let advertised = tokio::select! {
        biased;
        () = async {
            match options.shutdown.as_ref() {
                Some(tok) => tok.cancelled().await,
                None => std::future::pending().await,
            }
        } => {
            resolve_cancel.cancel();
            return Err(relay_shutdown_error(host.name()));
        }
        () = auth_check.revoke_token().cancelled() => {
            resolve_cancel.cancel();
            return Err(auth_check.denied_or_generic());
        }
        () = async {
            match options.client_gone.as_ref() {
                Some(tok) => tok.cancelled().await,
                None => std::future::pending().await,
            }
        } => {
            resolve_cancel.cancel();
            return Err(client_gone_error(host.name()));
        }
        res = legion_tools::device::resolve_self_ssh_hosts(&node_file, resolve_cancel.clone()) => res,
    };

    let mut tried = HashSet::new();
    tried.insert(loopback);

    if let Ok(hosts) = advertised {
        let mut fallback_addrs = Vec::new();
        for name in hosts {
            let trimmed = name
                .strip_prefix('[')
                .and_then(|s| s.strip_suffix(']'))
                .unwrap_or(&name);
            let looked_up = tokio::select! {
                biased;
                () = async {
                    match options.shutdown.as_ref() {
                        Some(tok) => tok.cancelled().await,
                        None => std::future::pending().await,
                    }
                } => return Err(relay_shutdown_error(host.name())),
                () = auth_check.revoke_token().cancelled() => return Err(auth_check.denied_or_generic()),
                () = async {
                    match options.client_gone.as_ref() {
                        Some(tok) => tok.cancelled().await,
                        None => std::future::pending().await,
                    }
                } => return Err(client_gone_error(host.name())),
                res = tokio::time::timeout(SELF_ADDR_CONNECT_TIMEOUT, lookup_host((trimmed, port))) => res,
            };
            match looked_up {
                Ok(Ok(iter)) => {
                    for addr in iter {
                        if tried.insert(addr) {
                            fallback_addrs.push(addr);
                            if fallback_addrs.len() >= MAX_SELF_FALLBACK_ADDRS {
                                break;
                            }
                        }
                    }
                }
                Ok(Err(err)) => errors.push(format!("{name}:{port}: {err}")),
                Err(_) => errors.push(format!("{name}:{port}: timed out resolving")),
            }
            if fallback_addrs.len() >= MAX_SELF_FALLBACK_ADDRS {
                break;
            }
        }

        for addr in fallback_addrs {
            match connect_one_self_addr(addr, SELF_ADDR_CONNECT_TIMEOUT, host, options, auth_check)
                .await
            {
                Ok(tcp) => return Ok(tcp),
                Err(SelfConnectErr::Terminal(err)) => return Err(err),
                Err(SelfConnectErr::Failed(msg)) => errors.push(format!("{addr}: {msg}")),
            }
        }
    }

    Err(ErrorBody::new(
        ErrorCode::Unreachable,
        format!(
            "{} could not reach its own SSH server on port {port} ({}).",
            host.name(),
            errors.join("; ")
        ),
    )
    .with_hint(
        "Check that sshd is running on this machine and matches `[devices] port` / `[devices] hosts` in node.toml.",
    )
    .with_machine(host.name()))
}

enum SelfConnectErr {
    Terminal(ErrorBody),
    Failed(String),
}

async fn connect_one_self_addr(
    addr: SocketAddr,
    timeout: Duration,
    host: &Host,
    options: &StreamServeOptions,
    auth_check: &RelayAuthCheck,
) -> Result<TcpStream, SelfConnectErr> {
    tokio::select! {
        biased;
        () = async {
            match options.shutdown.as_ref() {
                Some(tok) => tok.cancelled().await,
                None => std::future::pending().await,
            }
        } => Err(SelfConnectErr::Terminal(relay_shutdown_error(host.name()))),
        () = auth_check.revoke_token().cancelled() => {
            Err(SelfConnectErr::Terminal(auth_check.denied_or_generic()))
        }
        () = async {
            match options.client_gone.as_ref() {
                Some(tok) => tok.cancelled().await,
                None => std::future::pending().await,
            }
        } => Err(SelfConnectErr::Terminal(client_gone_error(host.name()))),
        res = tokio::time::timeout(timeout, TcpStream::connect(addr)) => match res {
            Ok(Ok(tcp)) => {
                let _ = tcp.set_nodelay(true);
                Ok(tcp)
            }
            Ok(Err(err)) => Err(SelfConnectErr::Failed(err.to_string())),
            Err(_) => Err(SelfConnectErr::Failed("timed out".to_owned())),
        },
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::io::{BufRead, BufReader, Cursor, Read, Write};
    use std::net::{TcpListener, TcpStream as StdTcpStream};
    use std::thread;
    use std::time::Duration;

    use legion_core::{Door, Paths};
    use legion_proto::ErrorCode;
    use serde_json::{Value, json};

    use super::*;
    use crate::stream::{
        CAP_STREAM_RELAY_TUNNEL, CAP_STREAM_TUNNEL, Host, STREAM_PROTOCOL, StreamServeOptions,
        serve_with_options,
    };

    fn make_host(dir: &std::path::Path, name: &str, port: u16, hosts: &[&str]) -> Host {
        let paths = Paths::from_explicit_home(dir).unwrap();
        fs::create_dir_all(paths.config_dir()).unwrap();
        fs::create_dir_all(paths.state_dir()).unwrap();
        let hosts_toml: Vec<String> = hosts.iter().map(|h| format!("{h:?}")).collect();
        fs::write(
            paths.config_file("node.toml"),
            format!(
                "name = {name:?}\n\n[devices]\nport = {port}\nhosts = [{}]\n",
                hosts_toml.join(", ")
            ),
        )
        .unwrap();
        fs::write(
            paths.config_file("policy.toml"),
            "default = \"owner\"\n\n\
             [principals.minimal]\nrole = \"meta_only\"\n\n\
             [roles.meta_only]\nallow = [\"meta.whoami\"]\n\n\
             [principals.blocked]\nrole = \"nobody\"\n",
        )
        .unwrap();
        Host::new(paths, name)
    }

    fn spawn_echo_server(bind: &str) -> (u16, thread::JoinHandle<()>) {
        let listener = TcpListener::bind((bind, 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle = thread::spawn(move || {
            if let Ok((mut conn, _)) = listener.accept() {
                let _ = conn.set_nodelay(true);
                let mut buf = [0u8; 4096];
                while let Ok(n) = conn.read(&mut buf) {
                    if n == 0 {
                        break;
                    }
                    if conn.write_all(&buf[..n]).is_err() {
                        break;
                    }
                }
            }
        });
        (port, handle)
    }

    fn tcp_pair() -> (StdTcpStream, StdTcpStream) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let client = StdTcpStream::connect(addr).unwrap();
        let (server, _) = listener.accept().unwrap();
        let _ = client.set_nodelay(true);
        let _ = server.set_nodelay(true);
        (client, server)
    }

    #[test]
    fn validate_tunnel_fields_rejects_destination_and_extra_fields() {
        let ok_min = json!({"protocol": 1, "op": "tunnel"})
            .as_object()
            .unwrap()
            .clone();
        assert!(validate_tunnel_fields(&ok_min).is_ok());

        let ok_as = json!({"protocol": 1, "op": "tunnel", "as": "owner"})
            .as_object()
            .unwrap()
            .clone();
        assert!(validate_tunnel_fields(&ok_as).is_ok());

        for field in ["host", "port", "path", "addr", "on", "target", "keep_open"] {
            let mut obj = ok_min.clone();
            obj.insert(field.to_owned(), json!("127.0.0.1"));
            let err = validate_tunnel_fields(&obj).unwrap_err();
            assert_eq!(err.code, ErrorCode::InvalidArgs, "field {field}");
            assert!(err.message.contains(field), "{err}");
            assert!(err.hint.is_some());
        }
    }

    #[test]
    fn serve_tunnel_checks_auth_extra_fields_and_preserves_pipelined_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let (port, echo_thread) = spawn_echo_server("127.0.0.1");
        let host = make_host(dir.path(), "studio", port, &["127.0.0.1"]);

        // 1. Extra destination field refused with `invalid_args`.
        let mut out = Vec::new();
        let req = format!(
            "{}\n",
            json!({"protocol": STREAM_PROTOCOL, "op": "tunnel", "host": "127.0.0.1"})
        );
        let err = serve_with_options(
            &host,
            Some("minimal"),
            Door::Stream,
            None,
            StreamServeOptions::default(),
            Cursor::new(req.into_bytes()),
            &mut out,
        )
        .unwrap_err();
        assert_eq!(err.code, ErrorCode::InvalidArgs);
        let lines: Vec<Value> = String::from_utf8(out)
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        let caps = lines[0]["capabilities"].as_array().unwrap();
        assert!(caps.iter().any(|c| c == CAP_STREAM_TUNNEL));
        assert!(caps.iter().any(|c| c == CAP_STREAM_RELAY_TUNNEL));
        assert_eq!(lines[1]["error"]["code"], json!("invalid_args"));

        // 2. Principal with role `nobody` is denied.
        let mut out = Vec::new();
        let req = format!("{}\n", json!({"protocol": STREAM_PROTOCOL, "op": "tunnel"}));
        let err = serve_with_options(
            &host,
            Some("blocked"),
            Door::Stream,
            None,
            StreamServeOptions::default(),
            Cursor::new(req.clone().into_bytes()),
            &mut out,
        )
        .unwrap_err();
        assert_eq!(err.code, ErrorCode::Denied);

        // 3. Revoked/absent pairing id is denied even for an allowed principal.
        let pairing_id = "0123456789abcdef0123456789abcdef";
        fs::write(host.paths().config_file("devices.toml"), "version = 1\n").unwrap();
        let mut out = Vec::new();
        let err = serve_with_options(
            &host,
            Some("minimal"),
            Door::Stream,
            Some("old"),
            StreamServeOptions {
                pairing_id: Some(pairing_id.to_owned()),
                ..StreamServeOptions::default()
            },
            Cursor::new(req.into_bytes()),
            &mut out,
        )
        .unwrap_err();
        assert_eq!(err.code, ErrorCode::Denied);
        let _ = fs::remove_file(host.paths().config_file("devices.toml"));

        // 4. Non-`nobody` role with NO `fs.*` permissions succeeds and preserves bytes pipelined
        //    right after the header line inside the same read buffer.
        let mut pipelined =
            format!("{}\n", json!({"protocol": STREAM_PROTOCOL, "op": "tunnel"})).into_bytes();
        pipelined.extend_from_slice(b"SSH-2.0-pipelined-bytes\r\n");
        let mut out = Vec::new();
        serve_with_options(
            &host,
            Some("minimal"),
            Door::Stream,
            None,
            StreamServeOptions::default(),
            Cursor::new(pipelined),
            &mut out,
        )
        .unwrap();
        let mut reader = BufReader::new(Cursor::new(out));
        let mut greeting = String::new();
        reader.read_line(&mut greeting).unwrap();
        let mut ready = String::new();
        reader.read_line(&mut ready).unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&ready).unwrap(),
            json!({"tunnel": true})
        );
        let mut echoed = Vec::new();
        reader.read_to_end(&mut echoed).unwrap();
        assert_eq!(echoed, b"SSH-2.0-pipelined-bytes\r\n");
        echo_thread.join().unwrap();
    }

    #[test]
    fn serve_tunnel_falls_back_to_advertised_host_and_reports_unreachable() {
        let dir = tempfile::tempdir().unwrap();
        // Bind strictly on 127.0.0.2 (if available on this OS) so 127.0.0.1:<port> fails and
        // `connect_self_sshd` must fall back to `[devices] hosts`.
        let Ok(listener) = TcpListener::bind(("127.0.0.2", 0)) else {
            return;
        };
        let port = listener.local_addr().unwrap().port();
        if StdTcpStream::connect(("127.0.0.1", port)).is_ok() {
            return;
        }
        let echo_thread = thread::spawn(move || {
            if let Ok((mut conn, _)) = listener.accept() {
                let mut buf = [0u8; 256];
                if let Ok(n) = conn.read(&mut buf)
                    && n > 0
                {
                    let _ = conn.write_all(&buf[..n]);
                }
            }
        });

        let host = make_host(dir.path(), "studio", port, &["127.0.0.2"]);
        let mut req =
            format!("{}\n", json!({"protocol": STREAM_PROTOCOL, "op": "tunnel"})).into_bytes();
        req.extend_from_slice(b"via-fallback");
        let mut out = Vec::new();
        serve_with_options(
            &host,
            Some("minimal"),
            Door::Stream,
            None,
            StreamServeOptions::default(),
            Cursor::new(req),
            &mut out,
        )
        .unwrap();
        let mut reader = BufReader::new(Cursor::new(out));
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        line.clear();
        reader.read_line(&mut line).unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&line).unwrap(),
            json!({"tunnel": true})
        );
        let mut echoed = Vec::new();
        reader.read_to_end(&mut echoed).unwrap();
        assert_eq!(echoed, b"via-fallback");
        echo_thread.join().unwrap();

        // With the listener now closed on both 127.0.0.1 and 127.0.0.2, tunnel returns `unreachable`.
        let mut out = Vec::new();
        let err = serve_with_options(
            &host,
            Some("minimal"),
            Door::Stream,
            None,
            StreamServeOptions::default(),
            Cursor::new(
                format!("{}\n", json!({"protocol": STREAM_PROTOCOL, "op": "tunnel"})).into_bytes(),
            ),
            &mut out,
        )
        .unwrap_err();
        assert_eq!(err.code, ErrorCode::Unreachable);
        let lines: Vec<Value> = String::from_utf8(out)
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(lines[1]["error"]["code"], json!("unreachable"));
    }

    #[test]
    fn serve_tunnel_survives_idle_timeouts_and_cuts_on_live_revocation() {
        let dir = tempfile::tempdir().unwrap();
        let (port, _echo_thread) = spawn_echo_server("127.0.0.1");
        let host = make_host(dir.path(), "studio", port, &["127.0.0.1"]);

        let (mut client_w, server_r) = tcp_pair();
        let client_r = client_w.try_clone().unwrap();
        let server_w = server_r.try_clone().unwrap();
        let host_bg = host.clone();
        let serve_thread = thread::spawn(move || {
            serve_with_options(
                &host_bg,
                Some("minimal"),
                Door::Stream,
                None,
                StreamServeOptions {
                    keep_open_idle_timeout: Duration::from_millis(60),
                    downstream_read_timeout: Duration::from_millis(60),
                    ..StreamServeOptions::default()
                },
                server_r,
                server_w,
            )
        });

        let mut reader = BufReader::new(client_r);
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        writeln!(
            client_w,
            "{}",
            json!({"protocol": STREAM_PROTOCOL, "op": "tunnel"})
        )
        .unwrap();
        client_w.flush().unwrap();
        line.clear();
        reader.read_line(&mut line).unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&line).unwrap(),
            json!({"tunnel": true})
        );

        // Stay completely idle well past both 60 ms timeouts: tunnel must stay open!
        thread::sleep(Duration::from_millis(250));
        client_w.write_all(b"after-idle").unwrap();
        client_w.flush().unwrap();
        let mut got = [0u8; 10];
        reader.read_exact(&mut got).unwrap();
        assert_eq!(&got, b"after-idle");

        // Now demote `minimal` to `nobody` while completely idle: watchdog must cut the tunnel.
        fs::write(
            host.paths().config_file("policy.toml"),
            "default = \"owner\"\n\n[principals.minimal]\nrole = \"nobody\"\n",
        )
        .unwrap();
        let cut_started = Instant::now();
        let outcome = serve_thread.join().unwrap();
        assert!(
            cut_started.elapsed() < Duration::from_secs(6),
            "revocation took too long: {:?}",
            cut_started.elapsed()
        );
        let err = outcome.unwrap_err();
        assert_eq!(err.code, ErrorCode::Denied);
    }
}
