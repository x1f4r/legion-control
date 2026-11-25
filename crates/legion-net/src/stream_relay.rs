//! Relayed `legion stream` operations (`op: "relay"`).
//!
//! When a device is paired with one node (`tower`) and needs bulk file data on another fleet node
//! (`studio`), the device sends `{"protocol": 1, "op": "relay", "on": "studio", "request": {...}}`
//! to `tower`. This module authorises the inner operation against `tower`'s own live `policy.toml`
//! (without checking paths, which belong to `studio`), enforces [`MAX_HOPS`], opens a downstream
//! `legion stream` pipe via [`Fleet::open_relay_stream_pipe`], attaches `"as": "<principal>"` so
//! `studio` authorises the same caller principal, and streams bytes in both directions with fixed
//! bounded buffers and backpressure.

use std::io::{self, BufRead, BufReader, BufWriter, Read, Write};
use std::sync::Arc;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::{Duration, Instant};

use legion_core::config::ConfigStore;
use legion_core::policy::Policy;
use legion_core::{CancellationToken, Door};
use legion_proto::{Effect, ErrorBody, ErrorCode};
use serde_json::{Map, Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader as AsyncBufReader};
use tokio::process::{ChildStdin, ChildStdout};
use tokio::sync::{Notify, mpsc as async_mpsc};

use crate::config::validate_machine_name;
use crate::connector::{PipeHandle, StderrTail};
use crate::pool::{Fleet, MAX_HOPS, RelayPipe};
use crate::stream::{
    CAP_STREAM_RELAY, GetAuditBucket, GetReady, Header, Host, NextHeader, OUTPUT_BUFFER, Op,
    PutReady, QUEUED_CHUNKS, STREAM_PROTOCOL, StreamServeOptions, gone, machine_or, op_tool,
    read_next_header, record_for_target, reply_error, unsupported,
};

/// Maximum time to wait for a downstream node's opening `{"protocol": 1}` greeting line.
const DOWNSTREAM_GREETING_TIMEOUT: Duration = Duration::from_secs(30);
/// Maximum non-protocol login banner lines skipped before downstream's `{"protocol": 1}` greeting.
const MAX_NOISE_LINES: usize = 64;
/// Maximum cumulative login banner bytes skipped before giving up.
const MAX_NOISE_BYTES: usize = 64 * 1024;
/// Maximum reply line length forwarded from downstream (e.g. `manifest` lines, `GetReady`, errors).
const MAX_REPLY_LINE: usize = 1024 * 1024;
/// Maximum `{"done": true}` trailer line length on a `keep_open` `get`.
const MAX_DONE_LINE: usize = 1024;
/// Bounded grace period when tearing down a downstream stream process after normal completion.
const DOWNSTREAM_CLOSE_GRACE: Duration = Duration::from_millis(500);
/// Short grace period when tearing down a timed-out, cancelled, or abandoned downstream process.
const DOWNSTREAM_ABORT_GRACE: Duration = Duration::from_millis(200);
/// Poll slice used to notice `shutdown` or `client_gone` while awaiting downstream I/O.
const SHUTDOWN_POLL_SLICE: Duration = Duration::from_millis(50);

enum DownstreamWait<T> {
    Ready(T),
    Shutdown,
    ClientGone,
    TimedOut,
}

fn format_timeout(duration: Duration) -> String {
    if duration.as_secs() > 0 && duration.subsec_millis() == 0 {
        format!("{}s", duration.as_secs())
    } else {
        format!("{}ms", duration.as_millis().max(1))
    }
}

fn downstream_timeout_error(target: &str, timeout: Duration) -> ErrorBody {
    ErrorBody::new(
        ErrorCode::Timeout,
        format!(
            "{target} stopped responding on the relayed stream for {}.",
            format_timeout(timeout)
        ),
    )
    .with_machine(target)
}

fn client_gone_error(relay: &str) -> ErrorBody {
    ErrorBody::new(
        ErrorCode::Cancelled,
        "The client closed the relayed stream.",
    )
    .with_machine(relay)
}

fn relay_shutdown_error(relay: &str) -> ErrorBody {
    ErrorBody::new(
        ErrorCode::Cancelled,
        "The stream stopped because the node is shutting down.",
    )
    .with_machine(relay)
}

async fn wait_downstream<F, T>(
    fut: F,
    timeout: Duration,
    shutdown: Option<&CancellationToken>,
    client_gone: Option<&CancellationToken>,
    progress: Option<&Notify>,
    external_timeout: Option<&CancellationToken>,
) -> DownstreamWait<T>
where
    F: std::future::Future<Output = T>,
{
    tokio::pin!(fut);
    let sleep = tokio::time::sleep(timeout);
    tokio::pin!(sleep);
    loop {
        tokio::select! {
            biased;
            () = async {
                match shutdown {
                    Some(tok) => tok.cancelled().await,
                    None => std::future::pending().await,
                }
            } => return DownstreamWait::Shutdown,
            () = async {
                match client_gone {
                    Some(tok) => tok.cancelled().await,
                    None => std::future::pending().await,
                }
            } => return DownstreamWait::ClientGone,
            () = async {
                match external_timeout {
                    Some(tok) => tok.cancelled().await,
                    None => std::future::pending().await,
                }
            } => return DownstreamWait::TimedOut,
            res = &mut fut => return DownstreamWait::Ready(res),
            () = async {
                match progress {
                    Some(notify) => notify.notified().await,
                    None => std::future::pending().await,
                }
            } => {
                sleep.as_mut().reset(tokio::time::Instant::now() + timeout);
            }
            () = &mut sleep => return DownstreamWait::TimedOut,
        }
    }
}

/// Validates the target machine name and inner `request` of a `relay` header.
///
/// Any `"as"` field inside `request` is stripped so a caller can never escalate identity inside
/// the inner payload; the relaying node always supplies the principal resolved from the outer
/// header and `pinned`.
pub(crate) fn validate_relay_header(
    host: &Host,
    on: &str,
    request: &Value,
) -> Result<(Op, Map<String, Value>), ErrorBody> {
    if let Err(reason) = validate_machine_name(on) {
        return Err(ErrorBody::new(ErrorCode::InvalidArgs, format!("{reason}."))
            .with_hint("Set `on` to a valid fleet machine name.")
            .with_machine(host.name()));
    }
    parse_relay_inner(host, request)
}

pub(crate) fn parse_relay_inner(
    host: &Host,
    request: &Value,
) -> Result<(Op, Map<String, Value>), ErrorBody> {
    let Value::Object(fields) = request else {
        return Err(ErrorBody::new(
            ErrorCode::InvalidArgs,
            "A relay stream's `request` must be a JSON object.",
        )
        .with_hint("Pass the inner stream operation header inside `request`.")
        .with_machine(host.name()));
    };
    match fields.get("protocol") {
        Some(Value::Number(number)) if number.as_u64() == Some(u64::from(STREAM_PROTOCOL)) => {}
        Some(Value::Number(number)) => {
            return Err(unsupported(format!(
                "The relayed request asks for stream protocol {number}, and this node speaks stream protocol {STREAM_PROTOCOL}."
            ))
            .with_machine(host.name()));
        }
        _ => {
            return Err(unsupported(format!(
                "The relayed request names no protocol, and this node speaks stream protocol {STREAM_PROTOCOL}."
            ))
            .with_machine(host.name()));
        }
    }
    match fields.get("op").and_then(Value::as_str) {
        Some("relay") => {
            return Err(ErrorBody::new(
                ErrorCode::InvalidArgs,
                "A relay request cannot wrap another relay operation.",
            )
            .with_hint("Set `on` to the final target machine instead of nesting relay headers.")
            .with_machine(host.name()));
        }
        Some(op) if op_tool(op).is_some() => {}
        Some(op) => {
            return Err(unsupported(format!(
                "This node cannot relay the unknown stream operation {op}."
            ))
            .with_machine(host.name()));
        }
        None => {
            return Err(
                unsupported("The relayed request names no stream operation.".to_owned())
                    .with_machine(host.name()),
            );
        }
    }
    let inner_op: Op = serde_json::from_value(request.clone()).map_err(|error| {
        ErrorBody::new(
            ErrorCode::InvalidArgs,
            format!("The relayed request is not a valid stream operation: {error}."),
        )
        .with_machine(host.name())
    })?;
    let mut sanitized = fields.clone();
    sanitized.remove("as");
    Ok((inner_op, sanitized))
}

fn authorise_relay(
    host: &Host,
    config: &ConfigStore,
    pinned: Option<&str>,
    as_principal: Option<&str>,
    inner_op: &Op,
) -> Result<String, (Option<String>, ErrorBody)> {
    let policy = config
        .get::<Policy>()
        .map_err(|error| (pinned.map(str::to_owned), machine_or(error, host.name())))?;
    let principal = policy.resolve(as_principal, pinned).map_err(|error| {
        let caller = pinned.map_or_else(|| policy.default_principal().to_owned(), str::to_owned);
        (Some(caller), machine_or(error, host.name()))
    })?;
    let (tool, effect) = inner_op.authorised_as();
    policy
        .authorize_name(&principal, tool, Some(effect), host.paths())
        .map_err(|error| (Some(principal.clone()), machine_or(error, host.name())))?;
    Ok(principal)
}

fn recheck_relay_access(
    host: &Host,
    config: &ConfigStore,
    pinned: Option<&str>,
    as_principal: Option<&str>,
    tool: &str,
    effect: Effect,
) -> Result<(), ErrorBody> {
    let policy = config
        .get::<Policy>()
        .map_err(|err| machine_or(err, host.name()))?;
    let principal = policy
        .resolve(as_principal, pinned)
        .map_err(|err| machine_or(err, host.name()))?;
    let _ = policy
        .authorize_name(&principal, tool, Some(effect), host.paths())
        .map_err(|err| machine_or(err, host.name()))?;
    Ok(())
}

fn build_downstream_header(
    inner_map: &Map<String, Value>,
    principal: &str,
    target: &str,
    chained: bool,
    next_hops: u32,
) -> Vec<u8> {
    let payload = if chained {
        json!({
            "protocol": STREAM_PROTOCOL,
            "op": "relay",
            "on": target,
            "hops": next_hops,
            "as": principal,
            "request": Value::Object(inner_map.clone()),
        })
    } else {
        let mut direct = inner_map.clone();
        direct.insert("as".to_owned(), Value::String(principal.to_owned()));
        Value::Object(direct)
    };
    let mut bytes = payload.to_string().into_bytes();
    bytes.push(b'\n');
    bytes
}

enum RuntimeChoice {
    Borrowed(tokio::runtime::Handle),
    Owned(tokio::runtime::Runtime),
}

impl RuntimeChoice {
    fn acquire(options: &StreamServeOptions, host: &Host) -> Result<Self, ErrorBody> {
        if let Some(handle) = options
            .runtime
            .clone()
            .or_else(|| tokio::runtime::Handle::try_current().ok())
        {
            return Ok(RuntimeChoice::Borrowed(handle));
        }
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .map(RuntimeChoice::Owned)
            .map_err(|error| {
                ErrorBody::new(
                    ErrorCode::Internal,
                    format!("Could not start relay runtime: {error}."),
                )
                .with_machine(host.name())
            })
    }

    fn handle(&self) -> &tokio::runtime::Handle {
        match self {
            RuntimeChoice::Borrowed(handle) => handle,
            RuntimeChoice::Owned(runtime) => runtime.handle(),
        }
    }
}

struct RelayFleet {
    fleet: Fleet,
    owned: bool,
}

impl RelayFleet {
    fn acquire(host: &Host, options: &StreamServeOptions) -> Self {
        match options.fleet.clone() {
            Some(fleet) => Self {
                fleet,
                owned: false,
            },
            None => Self {
                fleet: Fleet::new(host.paths()),
                owned: true,
            },
        }
    }

    async fn finish(self) {
        if self.owned {
            self.fleet.close().await;
        }
    }
}

/// Serves a remote `op: "relay"` stream when `input` is `Send + 'static` (`serve_with_options`,
/// used by `legion stream` and `legion gate`).
#[allow(clippy::too_many_arguments)]
pub(crate) fn serve_relay_remote_spawned<R: Read + Send + 'static, W: Write>(
    host: &Host,
    pinned: Option<&str>,
    door: Door,
    device: Option<&str>,
    options: &StreamServeOptions,
    as_principal: Option<String>,
    on: String,
    request: Value,
    hops: u32,
    recorded: Value,
    started: Instant,
    mut input: BufReader<R>,
    mut output: BufWriter<W>,
) -> Result<(), ErrorBody> {
    let config = ConfigStore::new(host.paths().config_dir());
    let (inner_op, inner_map) = match validate_relay_header(host, &on, &request) {
        Ok(parsed) => parsed,
        Err(error) => {
            let _ = reply_error(&mut output, &error);
            record_for_target(
                host,
                &on,
                pinned.map(str::to_owned),
                door,
                device,
                "stream.relay",
                recorded,
                started,
                &Err(error.clone()),
            );
            return Err(error);
        }
    };
    let audit_tool = format!("stream.{}", inner_op.name());
    let next_hops = hops.saturating_add(1);
    if next_hops > MAX_HOPS {
        let error = ErrorBody::new(
            ErrorCode::Failed,
            format!(
                "Relaying stream.{} to {on} exceeded the maximum of {MAX_HOPS} hops (possible routing loop).",
                inner_op.name()
            ),
        )
        .with_hint(
            "Check fleet.toml and node.toml on the machines along the path so they do not forward to each other in a cycle.",
        )
        .with_machine(host.name());
        let _ = reply_error(&mut output, &error);
        record_for_target(
            host,
            &on,
            pinned.map(str::to_owned),
            door,
            device,
            &audit_tool,
            recorded,
            started,
            &Err(error.clone()),
        );
        return Err(error);
    }

    let principal = match authorise_relay(host, &config, pinned, as_principal.as_deref(), &inner_op)
    {
        Ok(principal) => principal,
        Err((principal, error)) => {
            let _ = reply_error(&mut output, &error);
            record_for_target(
                host,
                &on,
                principal,
                door,
                device,
                &audit_tool,
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
            record_for_target(
                host,
                &on,
                Some(principal),
                door,
                device,
                &audit_tool,
                recorded,
                started,
                &Err(error.clone()),
            );
            return Err(error);
        }
    };
    let relay_fleet = RelayFleet::acquire(host, options);
    let handle = rt.handle().clone();

    if matches!(
        inner_op,
        Op::Get {
            keep_open: true,
            ..
        }
    ) {
        let (tx, rx) = mpsc::sync_channel::<Result<Option<(Header, Value)>, ErrorBody>>(1);
        std::thread::spawn(move || {
            loop {
                let next = read_next_header(&mut input);
                let stop = matches!(next, Ok(None) | Err(_));
                if tx.send(next).is_err() || stop {
                    break;
                }
            }
        });
        return handle.block_on(async move {
            let res = relay_keep_open_gets(
                host,
                &config,
                &relay_fleet.fleet,
                pinned,
                door,
                device,
                options,
                &on,
                as_principal,
                principal,
                inner_op,
                inner_map,
                next_hops,
                recorded,
                started,
                &mut output,
                |bucket| {
                    let idle_since = Instant::now();
                    loop {
                        if options
                            .shutdown
                            .as_ref()
                            .is_some_and(CancellationToken::is_cancelled)
                            || options
                                .client_gone
                                .as_ref()
                                .is_some_and(CancellationToken::is_cancelled)
                        {
                            return NextHeader::IdleExit;
                        }
                        if let Some(pending) = bucket.as_ref()
                            && pending.started.elapsed() >= options.audit_coalesce_window
                            && let Some(finished) = bucket.take()
                        {
                            finished.flush(host, door, device);
                        }
                        let elapsed_idle = idle_since.elapsed();
                        if elapsed_idle >= options.keep_open_idle_timeout {
                            return NextHeader::IdleExit;
                        }
                        let until_idle =
                            options.keep_open_idle_timeout.saturating_sub(elapsed_idle);
                        let until_flush = bucket
                            .as_ref()
                            .map(|b| {
                                options
                                    .audit_coalesce_window
                                    .saturating_sub(b.started.elapsed())
                            })
                            .unwrap_or(until_idle);
                        let mut wait = until_idle.min(until_flush);
                        if options.shutdown.is_some() || options.client_gone.is_some() {
                            wait = wait.min(SHUTDOWN_POLL_SLICE);
                        }
                        match rx.recv_timeout(wait) {
                            Ok(Ok(Some(pair))) => return NextHeader::Header(pair),
                            Ok(Ok(None)) | Err(RecvTimeoutError::Disconnected) => {
                                return NextHeader::Eof;
                            }
                            Ok(Err(err)) => return NextHeader::Error(err),
                            Err(RecvTimeoutError::Timeout) => {}
                        }
                    }
                },
            )
            .await;
            relay_fleet.finish().await;
            res
        });
    }

    handle.block_on(async move {
        let res = match &inner_op {
            Op::Put { size, .. } => {
                let size = *size;
                relay_write_spawned(
                    host,
                    &config,
                    &relay_fleet.fleet,
                    door,
                    device,
                    options,
                    &on,
                    principal,
                    &audit_tool,
                    Some(size),
                    inner_map,
                    next_hops,
                    recorded,
                    started,
                    input,
                    &mut output,
                )
                .await
            }
            Op::Unpack { .. } => {
                relay_write_spawned(
                    host,
                    &config,
                    &relay_fleet.fleet,
                    door,
                    device,
                    options,
                    &on,
                    principal,
                    &audit_tool,
                    None,
                    inner_map,
                    next_hops,
                    recorded,
                    started,
                    input,
                    &mut output,
                )
                .await
            }
            _ => {
                relay_read_or_control(
                    host,
                    &config,
                    &relay_fleet.fleet,
                    pinned,
                    as_principal.as_deref(),
                    door,
                    device,
                    options,
                    &on,
                    principal,
                    inner_op,
                    inner_map,
                    next_hops,
                    recorded,
                    started,
                    &mut output,
                )
                .await
            }
        };
        relay_fleet.finish().await;
        res
    })
}

/// Serves a remote `op: "relay"` stream when `input` is not `Send + 'static` (`serve` and
/// `serve_with_door`).
#[allow(clippy::too_many_arguments)]
pub(crate) fn serve_relay_remote_scoped<R: BufRead, W: Write>(
    host: &Host,
    pinned: Option<&str>,
    door: Door,
    device: Option<&str>,
    options: &StreamServeOptions,
    as_principal: Option<String>,
    on: String,
    request: Value,
    hops: u32,
    recorded: Value,
    started: Instant,
    input: &mut R,
    mut output: BufWriter<W>,
) -> Result<(), ErrorBody> {
    let config = ConfigStore::new(host.paths().config_dir());
    let (inner_op, inner_map) = match validate_relay_header(host, &on, &request) {
        Ok(parsed) => parsed,
        Err(error) => {
            let _ = reply_error(&mut output, &error);
            record_for_target(
                host,
                &on,
                pinned.map(str::to_owned),
                door,
                device,
                "stream.relay",
                recorded,
                started,
                &Err(error.clone()),
            );
            return Err(error);
        }
    };
    let audit_tool = format!("stream.{}", inner_op.name());
    let next_hops = hops.saturating_add(1);
    if next_hops > MAX_HOPS {
        let error = ErrorBody::new(
            ErrorCode::Failed,
            format!(
                "Relaying stream.{} to {on} exceeded the maximum of {MAX_HOPS} hops (possible routing loop).",
                inner_op.name()
            ),
        )
        .with_hint(
            "Check fleet.toml and node.toml on the machines along the path so they do not forward to each other in a cycle.",
        )
        .with_machine(host.name());
        let _ = reply_error(&mut output, &error);
        record_for_target(
            host,
            &on,
            pinned.map(str::to_owned),
            door,
            device,
            &audit_tool,
            recorded,
            started,
            &Err(error.clone()),
        );
        return Err(error);
    }

    let principal = match authorise_relay(host, &config, pinned, as_principal.as_deref(), &inner_op)
    {
        Ok(principal) => principal,
        Err((principal, error)) => {
            let _ = reply_error(&mut output, &error);
            record_for_target(
                host,
                &on,
                principal,
                door,
                device,
                &audit_tool,
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
            record_for_target(
                host,
                &on,
                Some(principal),
                door,
                device,
                &audit_tool,
                recorded,
                started,
                &Err(error.clone()),
            );
            return Err(error);
        }
    };
    let relay_fleet = RelayFleet::acquire(host, options);
    let handle = rt.handle().clone();

    if matches!(
        inner_op,
        Op::Get {
            keep_open: true,
            ..
        }
    ) {
        return handle.block_on(async move {
            let res = relay_keep_open_gets(
                host,
                &config,
                &relay_fleet.fleet,
                pinned,
                door,
                device,
                options,
                &on,
                as_principal,
                principal,
                inner_op,
                inner_map,
                next_hops,
                recorded,
                started,
                &mut output,
                |_| match read_next_header(input) {
                    Ok(Some(pair)) => NextHeader::Header(pair),
                    Ok(None) => NextHeader::Eof,
                    Err(err) => NextHeader::Error(err),
                },
            )
            .await;
            relay_fleet.finish().await;
            res
        });
    }

    handle.block_on(async move {
        let res = match &inner_op {
            Op::Put { size, .. } => {
                let size = *size;
                relay_write_scoped(
                    host,
                    &config,
                    &relay_fleet.fleet,
                    door,
                    device,
                    options,
                    &on,
                    principal,
                    &audit_tool,
                    Some(size),
                    inner_map,
                    next_hops,
                    recorded,
                    started,
                    input,
                    &mut output,
                )
                .await
            }
            Op::Unpack { .. } => {
                relay_write_scoped(
                    host,
                    &config,
                    &relay_fleet.fleet,
                    door,
                    device,
                    options,
                    &on,
                    principal,
                    &audit_tool,
                    None,
                    inner_map,
                    next_hops,
                    recorded,
                    started,
                    input,
                    &mut output,
                )
                .await
            }
            _ => {
                relay_read_or_control(
                    host,
                    &config,
                    &relay_fleet.fleet,
                    pinned,
                    as_principal.as_deref(),
                    door,
                    device,
                    options,
                    &on,
                    principal,
                    inner_op,
                    inner_map,
                    next_hops,
                    recorded,
                    started,
                    &mut output,
                )
                .await
            }
        };
        relay_fleet.finish().await;
        res
    })
}

struct OpenedDownstream {
    stdin: ChildStdin,
    stdout: AsyncBufReader<ChildStdout>,
    handle: PipeHandle,
    stderr: std::sync::Arc<StderrTail>,
    next_hop: String,
    chained: bool,
}

async fn open_and_greet_downstream(
    fleet: &Fleet,
    config: &ConfigStore,
    host: &Host,
    target: &str,
    options: &StreamServeOptions,
) -> Result<OpenedDownstream, ErrorBody> {
    let pipe_cancel = CancellationToken::new();
    let opened_pipe = tokio::select! {
        biased;
        () = async {
            match options.shutdown.as_ref() {
                Some(tok) => tok.cancelled().await,
                None => std::future::pending().await,
            }
        } => {
            pipe_cancel.cancel();
            return Err(relay_shutdown_error(host.name()));
        }
        () = async {
            match options.client_gone.as_ref() {
                Some(tok) => tok.cancelled().await,
                None => std::future::pending().await,
            }
        } => {
            pipe_cancel.cancel();
            return Err(client_gone_error(host.name()));
        }
        res = fleet.open_relay_stream_pipe(config, host.name(), target, pipe_cancel.clone()) => res?,
    };
    let RelayPipe {
        pipe,
        next_hop,
        chained,
    } = opened_pipe;
    let stderr = pipe.stderr_tail().clone();
    let (stdin, stdout, handle) = pipe.into_parts();
    let mut stdout = AsyncBufReader::with_capacity(OUTPUT_BUFFER, stdout);

    let greet_timeout = DOWNSTREAM_GREETING_TIMEOUT.min(options.downstream_read_timeout);
    let greet_res = match wait_downstream(
        read_downstream_greeting(&mut stdout, &next_hop, target, chained, &stderr),
        greet_timeout,
        options.shutdown.as_ref(),
        options.client_gone.as_ref(),
        None,
        None,
    )
    .await
    {
        DownstreamWait::Ready(inner) => inner,
        DownstreamWait::Shutdown => Err(relay_shutdown_error(host.name())),
        DownstreamWait::ClientGone => Err(client_gone_error(host.name())),
        DownstreamWait::TimedOut => {
            let mut err = ErrorBody::new(
                ErrorCode::Timeout,
                format!(
                    "Timed out waiting for {next_hop} to start legion stream after {}.",
                    format_timeout(greet_timeout)
                ),
            )
            .with_machine(&next_hop);
            let text = stderr.text();
            if !text.is_empty() {
                err = err.with_hint(format!("Remote stderr: {text}"));
            }
            Err(err)
        }
    };

    match greet_res {
        Ok(()) => Ok(OpenedDownstream {
            stdin,
            stdout,
            handle,
            stderr,
            next_hop,
            chained,
        }),
        Err(err) => {
            drop(stdin);
            drop(stdout);
            handle.close(DOWNSTREAM_ABORT_GRACE).await;
            Err(err)
        }
    }
}

async fn read_downstream_greeting(
    stdout: &mut AsyncBufReader<ChildStdout>,
    next_hop: &str,
    target: &str,
    chained: bool,
    stderr: &std::sync::Arc<StderrTail>,
) -> Result<(), ErrorBody> {
    let mut noise_lines = 0usize;
    let mut noise_bytes = 0usize;
    let mut buf = Vec::new();
    loop {
        buf.clear();
        let n = read_bounded_async_line(stdout, &mut buf, MAX_REPLY_LINE)
            .await
            .map_err(|err| {
                ErrorBody::new(
                    ErrorCode::Unreachable,
                    format!("Reading stream greeting from {next_hop} failed: {err}."),
                )
                .with_machine(next_hop)
            })?;
        if n == 0 {
            let mut err = ErrorBody::new(
                ErrorCode::Unreachable,
                format!("The stream to {next_hop} closed before answering."),
            )
            .with_machine(next_hop);
            let text = stderr.text();
            if !text.is_empty() {
                err = err.with_hint(format!("Remote stderr: {text}"));
            }
            return Err(err);
        }
        if let Ok(Value::Object(fields)) = serde_json::from_slice::<Value>(&buf)
            && let Some(spoken) = fields.get("protocol")
        {
            if spoken.as_u64() != Some(u64::from(STREAM_PROTOCOL)) {
                return Err(ErrorBody::new(
                    ErrorCode::Unsupported,
                    format!(
                        "{next_hop} speaks stream protocol {spoken}, and this node speaks stream protocol {STREAM_PROTOCOL}."
                    ),
                )
                .with_hint("Update legion on whichever machine is older.")
                .with_machine(next_hop));
            }
            if chained {
                let supports_relay = fields
                    .get("capabilities")
                    .and_then(Value::as_array)
                    .is_some_and(|arr| {
                        arr.iter()
                            .filter_map(Value::as_str)
                            .any(|c| c == CAP_STREAM_RELAY)
                    });
                if !supports_relay {
                    return Err(ErrorBody::new(
                        ErrorCode::Unsupported,
                        format!(
                            "{next_hop} needs an update before it can pass files along from {target}."
                        ),
                    )
                    .with_hint(format!(
                        "Update legion on {next_hop} so it supports relayed streams."
                    ))
                    .with_machine(next_hop)
                    .with_details(json!({
                        "reason": "relay_too_old",
                        "relay": next_hop,
                        "target": target,
                    })));
                }
            }
            return Ok(());
        }
        noise_lines += 1;
        noise_bytes += buf.len();
        if noise_lines > MAX_NOISE_LINES || noise_bytes > MAX_NOISE_BYTES {
            return Err(ErrorBody::new(
                ErrorCode::Unreachable,
                format!(
                    "{next_hop} sent too much non-protocol output before starting legion stream."
                ),
            )
            .with_machine(next_hop));
        }
    }
}

enum ReplyLine {
    /// A non-busy, non-error JSON reply line (raw bytes ending in `\n`) and its parsed object.
    Json(Vec<u8>, Map<String, Value>),
    /// Downstream returned `{"error": ...}` (with `machine` ensured).
    RemoteError(ErrorBody),
    /// Downstream closed stdout cleanly before sending a reply.
    Eof,
}

/// Reads downstream JSON lines, forwarding any `{"busy": true}` heartbeats verbatim to `output`
/// until the first non-busy reply line, remote error, or EOF arrives.
async fn read_downstream_reply_forwarding_busy<W: Write>(
    stdout: &mut AsyncBufReader<ChildStdout>,
    host: &Host,
    target: &str,
    options: &StreamServeOptions,
    progress: Option<&Notify>,
    external_timeout: Option<&CancellationToken>,
    output: &mut BufWriter<W>,
) -> Result<ReplyLine, ErrorBody> {
    let mut buf = Vec::new();
    loop {
        buf.clear();
        let read_res = match wait_downstream(
            read_bounded_async_line(stdout, &mut buf, MAX_REPLY_LINE),
            options.downstream_read_timeout,
            options.shutdown.as_ref(),
            options.client_gone.as_ref(),
            progress,
            external_timeout,
        )
        .await
        {
            DownstreamWait::Ready(res) => res,
            DownstreamWait::Shutdown => {
                return Err(relay_shutdown_error(host.name()));
            }
            DownstreamWait::ClientGone => {
                return Err(client_gone_error(host.name()));
            }
            DownstreamWait::TimedOut => {
                return Err(downstream_timeout_error(
                    target,
                    options.downstream_read_timeout,
                ));
            }
        };
        let n = read_res.map_err(|err| {
            ErrorBody::new(
                ErrorCode::Failed,
                format!("Reading stream reply from {target} failed: {err}."),
            )
            .with_machine(target)
        })?;
        if n == 0 {
            return Ok(ReplyLine::Eof);
        }
        let Value::Object(fields) = serde_json::from_slice::<Value>(&buf).map_err(|_| {
            ErrorBody::new(
                ErrorCode::Failed,
                format!("{target} answered the relayed stream with non-JSON output."),
            )
            .with_machine(target)
        })?
        else {
            return Err(ErrorBody::new(
                ErrorCode::Failed,
                format!("{target} answered the relayed stream with a non-object JSON line."),
            )
            .with_machine(target));
        };
        if fields.len() == 1 && fields.get("busy") == Some(&Value::Bool(true)) {
            output
                .write_all(&buf)
                .and_then(|()| output.flush())
                .map_err(|err| machine_or(gone(&err), host.name()))?;
            continue;
        }
        if let Some(err_val) = fields.get("error") {
            let body = match serde_json::from_value::<ErrorBody>(err_val.clone()) {
                Ok(body) => machine_or(body, target),
                Err(_) => {
                    let message = err_val
                        .get("message")
                        .and_then(Value::as_str)
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .map(str::to_owned)
                        .unwrap_or_else(|| {
                            format!(
                                "{target} answered the relayed stream with an unrecognised error."
                            )
                        });
                    let machine = err_val
                        .get("machine")
                        .and_then(Value::as_str)
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .unwrap_or(target);
                    let mut fallback =
                        ErrorBody::new(ErrorCode::Failed, message).with_machine(machine);
                    if let Some(hint) = err_val
                        .get("hint")
                        .and_then(Value::as_str)
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                    {
                        fallback = fallback.with_hint(hint);
                    }
                    fallback
                }
            };
            return Ok(ReplyLine::RemoteError(body));
        }
        return Ok(ReplyLine::Json(buf, fields));
    }
}

#[allow(clippy::too_many_arguments)]
async fn relay_read_or_control<W: Write>(
    host: &Host,
    config: &ConfigStore,
    fleet: &Fleet,
    pinned: Option<&str>,
    as_principal: Option<&str>,
    door: Door,
    device: Option<&str>,
    options: &StreamServeOptions,
    target: &str,
    principal: String,
    inner_op: Op,
    inner_map: Map<String, Value>,
    next_hops: u32,
    recorded: Value,
    started: Instant,
    output: &mut BufWriter<W>,
) -> Result<(), ErrorBody> {
    let audit_tool = format!("stream.{}", inner_op.name());
    let (tool_name, tool_effect) = inner_op.authorised_as();
    let opened = match open_and_greet_downstream(fleet, config, host, target, options).await {
        Ok(opened) => opened,
        Err(error) => {
            let error = machine_or(error, host.name());
            let _ = reply_error(output, &error);
            record_for_target(
                host,
                target,
                Some(principal),
                door,
                device,
                &audit_tool,
                recorded,
                started,
                &Err(error.clone()),
            );
            return Err(error);
        }
    };

    let OpenedDownstream {
        mut stdin,
        mut stdout,
        handle,
        stderr,
        next_hop,
        chained,
    } = opened;

    let header_bytes = build_downstream_header(&inner_map, &principal, target, chained, next_hops);
    let write_wait = wait_downstream(
        async {
            stdin.write_all(&header_bytes).await?;
            stdin.flush().await
        },
        options.downstream_read_timeout,
        options.shutdown.as_ref(),
        options.client_gone.as_ref(),
        None,
        None,
    )
    .await;
    // Read/control operations carry no request body; half-close downstream stdin immediately.
    drop(stdin);

    let write_err = match write_wait {
        DownstreamWait::Ready(Ok(())) => None,
        DownstreamWait::Ready(Err(err)) => Some(
            ErrorBody::new(
                ErrorCode::Unreachable,
                format!("Could not send stream request to {next_hop}: {err}."),
            )
            .with_machine(&next_hop),
        ),
        DownstreamWait::Shutdown => Some(relay_shutdown_error(host.name())),
        DownstreamWait::ClientGone => Some(client_gone_error(host.name())),
        DownstreamWait::TimedOut => Some(downstream_timeout_error(
            target,
            options.downstream_read_timeout,
        )),
    };
    if let Some(error) = write_err {
        drop(stdout);
        handle.close(DOWNSTREAM_ABORT_GRACE).await;
        let _ = reply_error(output, &error);
        record_for_target(
            host,
            target,
            Some(principal),
            door,
            device,
            &audit_tool,
            recorded,
            started,
            &Err(error.clone()),
        );
        return Err(error);
    }

    let first_reply = read_downstream_reply_forwarding_busy(
        &mut stdout,
        host,
        target,
        options,
        None,
        None,
        output,
    )
    .await;

    let (first_raw, first_fields) = match first_reply {
        Ok(ReplyLine::Json(raw, fields)) => (raw, fields),
        Ok(ReplyLine::RemoteError(error)) => {
            drop(stdout);
            handle.close(DOWNSTREAM_CLOSE_GRACE).await;
            let _ = reply_error(output, &error);
            record_for_target(
                host,
                target,
                Some(principal),
                door,
                device,
                &audit_tool,
                recorded,
                started,
                &Err(error.clone()),
            );
            return Err(error);
        }
        Ok(ReplyLine::Eof) => {
            drop(stdout);
            handle.close(DOWNSTREAM_ABORT_GRACE).await;
            let mut error = ErrorBody::new(
                ErrorCode::Unreachable,
                format!("The stream to {target} closed before answering."),
            )
            .with_machine(target);
            let text = stderr.text();
            if !text.is_empty() {
                error = error.with_hint(format!("Remote stderr: {text}"));
            }
            let _ = reply_error(output, &error);
            record_for_target(
                host,
                target,
                Some(principal),
                door,
                device,
                &audit_tool,
                recorded,
                started,
                &Err(error.clone()),
            );
            return Err(error);
        }
        Err(error) => {
            drop(stdout);
            handle.close(DOWNSTREAM_ABORT_GRACE).await;
            let error = machine_or(error, host.name());
            let _ = reply_error(output, &error);
            record_for_target(
                host,
                target,
                Some(principal),
                door,
                device,
                &audit_tool,
                recorded,
                started,
                &Err(error.clone()),
            );
            return Err(error);
        }
    };

    let outcome: Result<(), ErrorBody> = match inner_op {
        Op::Stat { .. } | Op::Delete { .. } => output
            .write_all(&first_raw)
            .and_then(|()| output.flush())
            .map_err(|err| machine_or(gone(&err), host.name())),
        Op::Manifest { .. } => {
            let first_done = first_fields.get("done") == Some(&Value::Bool(true));
            let res = output
                .write_all(&first_raw)
                .and_then(|()| output.flush())
                .map_err(|err| machine_or(gone(&err), host.name()));
            if res.is_err() || first_done {
                res
            } else {
                pump_manifest_lines(&mut stdout, host, target, options, output).await
            }
        }
        Op::Get { .. } => {
            let ready: Result<GetReady, _> = serde_json::from_value(Value::Object(first_fields));
            match ready {
                Ok(ready) => {
                    if let Err(err) = output.write_all(&first_raw).and_then(|()| output.flush()) {
                        Err(machine_or(gone(&err), host.name()))
                    } else {
                        pump_exact_bytes(
                            &mut stdout,
                            ready.length,
                            host,
                            config,
                            pinned,
                            as_principal,
                            tool_name,
                            tool_effect,
                            options,
                            target,
                            output,
                        )
                        .await
                    }
                }
                Err(_) => {
                    let error = ErrorBody::new(
                        ErrorCode::Failed,
                        format!("{target} answered relayed get with an unexpected reply."),
                    )
                    .with_machine(target);
                    let _ = reply_error(output, &error);
                    Err(error)
                }
            }
        }
        Op::Pack { .. } => {
            if let Err(err) = output.write_all(&first_raw).and_then(|()| output.flush()) {
                Err(machine_or(gone(&err), host.name()))
            } else {
                pump_bytes_to_eof(&mut stdout, host, target, options, output).await
            }
        }
        Op::Put { .. } | Op::Unpack { .. } | Op::Relay { .. } => unreachable!(),
    };

    drop(stdout);
    let close_grace = if outcome.is_ok() {
        DOWNSTREAM_CLOSE_GRACE
    } else {
        DOWNSTREAM_ABORT_GRACE
    };
    handle.close(close_grace).await;

    record_for_target(
        host,
        target,
        Some(principal),
        door,
        device,
        &audit_tool,
        recorded,
        started,
        &outcome,
    );
    outcome
}

async fn pump_manifest_lines<W: Write>(
    stdout: &mut AsyncBufReader<ChildStdout>,
    host: &Host,
    target: &str,
    options: &StreamServeOptions,
    output: &mut BufWriter<W>,
) -> Result<(), ErrorBody> {
    let mut buf = Vec::new();
    loop {
        buf.clear();
        let read_res = match wait_downstream(
            read_bounded_async_line(stdout, &mut buf, MAX_REPLY_LINE),
            options.downstream_read_timeout,
            options.shutdown.as_ref(),
            options.client_gone.as_ref(),
            None,
            None,
        )
        .await
        {
            DownstreamWait::Ready(res) => res,
            DownstreamWait::Shutdown => {
                let err = relay_shutdown_error(host.name());
                let _ = reply_error(output, &err);
                return Err(err);
            }
            DownstreamWait::ClientGone => {
                return Err(client_gone_error(host.name()));
            }
            DownstreamWait::TimedOut => {
                let err = downstream_timeout_error(target, options.downstream_read_timeout);
                let _ = reply_error(output, &err);
                return Err(err);
            }
        };
        let n = match read_res {
            Ok(n) => n,
            Err(err) => {
                let error = ErrorBody::new(
                    ErrorCode::Failed,
                    format!("Reading manifest stream from {target} failed: {err}."),
                )
                .with_machine(target);
                let _ = reply_error(output, &error);
                return Err(error);
            }
        };
        if n == 0 {
            let error = ErrorBody::new(
                ErrorCode::Failed,
                format!("The manifest stream from {target} ended before finishing."),
            )
            .with_machine(target);
            let _ = reply_error(output, &error);
            return Err(error);
        }
        output
            .write_all(&buf)
            .and_then(|()| output.flush())
            .map_err(|err| machine_or(gone(&err), host.name()))?;
        if let Ok(Value::Object(fields)) = serde_json::from_slice::<Value>(&buf) {
            if fields.get("done") == Some(&Value::Bool(true)) {
                return Ok(());
            }
            if let Some(err_val) = fields.get("error")
                && let Ok(body) = serde_json::from_value::<ErrorBody>(err_val.clone())
            {
                return Err(machine_or(body, target));
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn pump_exact_bytes<W: Write>(
    stdout: &mut AsyncBufReader<ChildStdout>,
    expected: u64,
    host: &Host,
    config: &ConfigStore,
    pinned: Option<&str>,
    as_principal: Option<&str>,
    tool: &str,
    effect: Effect,
    options: &StreamServeOptions,
    target: &str,
    output: &mut BufWriter<W>,
) -> Result<(), ErrorBody> {
    let mut remaining = expected;
    let mut written = 0u64;
    let mut last_checked_bytes = 0u64;
    let mut last_checked_at = Instant::now();
    let recheck_bytes = options.recheck_bytes.max(1);
    let recheck_interval = options.recheck_interval;
    let mut buf = vec![0u8; OUTPUT_BUFFER];

    while remaining > 0 {
        if options
            .shutdown
            .as_ref()
            .is_some_and(CancellationToken::is_cancelled)
        {
            return Err(ErrorBody::new(
                ErrorCode::Cancelled,
                "The stream stopped because the node is shutting down.",
            )
            .with_machine(host.name()));
        }
        if options
            .client_gone
            .as_ref()
            .is_some_and(CancellationToken::is_cancelled)
        {
            return Err(client_gone_error(host.name()));
        }
        let want = usize::try_from(remaining)
            .unwrap_or(usize::MAX)
            .min(buf.len());
        let read_res = match wait_downstream(
            stdout.read(&mut buf[..want]),
            options.downstream_read_timeout,
            options.shutdown.as_ref(),
            options.client_gone.as_ref(),
            None,
            None,
        )
        .await
        {
            DownstreamWait::Ready(res) => res,
            DownstreamWait::Shutdown => {
                return Err(ErrorBody::new(
                    ErrorCode::Cancelled,
                    "The stream stopped because the node is shutting down.",
                )
                .with_machine(host.name()));
            }
            DownstreamWait::ClientGone => {
                return Err(client_gone_error(host.name()));
            }
            DownstreamWait::TimedOut => {
                return Err(downstream_timeout_error(
                    target,
                    options.downstream_read_timeout,
                ));
            }
        };
        let n = read_res.map_err(|err| {
            ErrorBody::new(
                ErrorCode::Failed,
                format!("Reading relayed bytes from {target} failed: {err}."),
            )
            .with_machine(target)
        })?;
        if n == 0 {
            return Err(ErrorBody::new(
                ErrorCode::Failed,
                format!("The relayed stream from {target} ended with {remaining} bytes remaining."),
            )
            .with_machine(target));
        }
        // Flush per fixed chunk so backpressure stays tight and a client disappearing mid-get is
        // noticed within one bounded buffer rather than buffered silently.
        output
            .write_all(&buf[..n])
            .and_then(|()| output.flush())
            .map_err(|err| machine_or(gone(&err), host.name()))?;
        remaining -= n as u64;
        written = written.saturating_add(n as u64);

        if written.saturating_sub(last_checked_bytes) >= recheck_bytes
            || last_checked_at.elapsed() >= recheck_interval
        {
            last_checked_bytes = written;
            last_checked_at = Instant::now();
            recheck_relay_access(host, config, pinned, as_principal, tool, effect)?;
        }
    }
    Ok(())
}

async fn pump_bytes_to_eof<W: Write>(
    stdout: &mut AsyncBufReader<ChildStdout>,
    host: &Host,
    target: &str,
    options: &StreamServeOptions,
    output: &mut BufWriter<W>,
) -> Result<(), ErrorBody> {
    let mut buf = vec![0u8; OUTPUT_BUFFER];
    loop {
        let read_res = match wait_downstream(
            stdout.read(&mut buf),
            options.downstream_read_timeout,
            options.shutdown.as_ref(),
            options.client_gone.as_ref(),
            None,
            None,
        )
        .await
        {
            DownstreamWait::Ready(res) => res,
            DownstreamWait::Shutdown => {
                return Err(relay_shutdown_error(host.name()));
            }
            DownstreamWait::ClientGone => {
                return Err(client_gone_error(host.name()));
            }
            DownstreamWait::TimedOut => {
                return Err(downstream_timeout_error(
                    target,
                    options.downstream_read_timeout,
                ));
            }
        };
        let n = read_res.map_err(|err| {
            ErrorBody::new(
                ErrorCode::Failed,
                format!("Reading relayed archive from {target} failed: {err}."),
            )
            .with_machine(target)
        })?;
        if n == 0 {
            return Ok(());
        }
        output
            .write_all(&buf[..n])
            .and_then(|()| output.flush())
            .map_err(|err| machine_or(gone(&err), host.name()))?;
    }
}

#[allow(clippy::too_many_arguments)]
async fn relay_keep_open_gets<W: Write>(
    host: &Host,
    config: &ConfigStore,
    fleet: &Fleet,
    pinned: Option<&str>,
    door: Door,
    device: Option<&str>,
    options: &StreamServeOptions,
    target: &str,
    first_as_principal: Option<String>,
    first_principal: String,
    first_op: Op,
    first_inner_map: Map<String, Value>,
    first_next_hops: u32,
    first_recorded: Value,
    first_started: Instant,
    output: &mut BufWriter<W>,
    mut next_header: impl FnMut(&mut Option<GetAuditBucket>) -> NextHeader,
) -> Result<(), ErrorBody> {
    let opened = match open_and_greet_downstream(fleet, config, host, target, options).await {
        Ok(opened) => opened,
        Err(error) => {
            let error = machine_or(error, host.name());
            let _ = reply_error(output, &error);
            record_for_target(
                host,
                target,
                Some(first_principal),
                door,
                device,
                "stream.get",
                first_recorded,
                first_started,
                &Err(error.clone()),
            );
            return Err(error);
        }
    };

    let OpenedDownstream {
        mut stdin,
        mut stdout,
        handle,
        stderr,
        next_hop,
        chained,
    } = opened;

    struct PendingRelayedGet {
        as_principal: Option<String>,
        principal: String,
        path: String,
        inner_map: Map<String, Value>,
        next_hops: u32,
        recorded: Value,
        started: Instant,
    }

    let first_path = match &first_op {
        Op::Get { path, .. } => path.clone(),
        _ => String::new(),
    };
    let mut current = Some(PendingRelayedGet {
        as_principal: first_as_principal,
        principal: first_principal,
        path: first_path,
        inner_map: first_inner_map,
        next_hops: first_next_hops,
        recorded: first_recorded,
        started: first_started,
    });
    let mut bucket: Option<GetAuditBucket> = None;

    let res: Result<(), ErrorBody> = loop {
        let req = match current.take() {
            Some(req) => req,
            None => match next_header(&mut bucket) {
                NextHeader::Header((hdr, rec)) => {
                    let started = Instant::now();
                    let Op::Relay {
                        on: next_on,
                        request: next_req,
                        hops: next_hops_in,
                    } = hdr.op
                    else {
                        let err = ErrorBody::new(
                            ErrorCode::InvalidArgs,
                            "A persistent relayed keep_open stream accepts only relay get operations.",
                        )
                        .with_machine(host.name());
                        if reply_error(output, &err).is_err() {
                            break Err(err);
                        }
                        record_for_target(
                            host,
                            target,
                            pinned.map(str::to_owned),
                            door,
                            device,
                            &format!("stream.{}", hdr.op.name()),
                            rec,
                            started,
                            &Err(err),
                        );
                        continue;
                    };
                    if next_on != target {
                        let err = ErrorBody::new(
                            ErrorCode::InvalidArgs,
                            format!(
                                "A persistent relayed stream to {target} cannot switch to {next_on}."
                            ),
                        )
                        .with_machine(host.name());
                        if reply_error(output, &err).is_err() {
                            break Err(err);
                        }
                        record_for_target(
                            host,
                            &next_on,
                            pinned.map(str::to_owned),
                            door,
                            device,
                            "stream.relay",
                            rec,
                            started,
                            &Err(err),
                        );
                        continue;
                    }
                    let (inner_op, inner_map) = match parse_relay_inner(host, &next_req) {
                        Ok(parsed) => parsed,
                        Err(err) => {
                            if reply_error(output, &err).is_err() {
                                break Err(err);
                            }
                            record_for_target(
                                host,
                                target,
                                pinned.map(str::to_owned),
                                door,
                                device,
                                "stream.relay",
                                rec,
                                started,
                                &Err(err),
                            );
                            continue;
                        }
                    };
                    let Op::Get { path, .. } = &inner_op else {
                        let err = ErrorBody::new(
                            ErrorCode::InvalidArgs,
                            "A persistent keep_open stream accepts only get operations.",
                        )
                        .with_machine(host.name());
                        if reply_error(output, &err).is_err() {
                            break Err(err);
                        }
                        record_for_target(
                            host,
                            target,
                            pinned.map(str::to_owned),
                            door,
                            device,
                            &format!("stream.{}", inner_op.name()),
                            rec,
                            started,
                            &Err(err),
                        );
                        continue;
                    };
                    let path = path.clone();
                    let next_hops = next_hops_in.saturating_add(1);
                    if next_hops > MAX_HOPS {
                        let err = ErrorBody::new(
                            ErrorCode::Failed,
                            format!(
                                "Relaying stream.get to {target} exceeded the maximum of {MAX_HOPS} hops (possible routing loop)."
                            ),
                        )
                        .with_machine(host.name());
                        if reply_error(output, &err).is_err() {
                            break Err(err);
                        }
                        record_for_target(
                            host,
                            target,
                            pinned.map(str::to_owned),
                            door,
                            device,
                            "stream.get",
                            rec,
                            started,
                            &Err(err),
                        );
                        continue;
                    }
                    let principal = match authorise_relay(
                        host,
                        config,
                        pinned,
                        hdr.as_principal.as_deref(),
                        &inner_op,
                    ) {
                        Ok(p) => p,
                        Err((principal, error)) => {
                            if let Some(pending) = bucket.take() {
                                pending.flush(host, door, device);
                            }
                            if reply_error(output, &error).is_err() {
                                break Err(error);
                            }
                            record_for_target(
                                host,
                                target,
                                principal,
                                door,
                                device,
                                "stream.get",
                                rec,
                                started,
                                &Err(error),
                            );
                            continue;
                        }
                    };
                    PendingRelayedGet {
                        as_principal: hdr.as_principal,
                        principal,
                        path,
                        inner_map,
                        next_hops,
                        recorded: rec,
                        started,
                    }
                }
                NextHeader::Eof | NextHeader::IdleExit => break Ok(()),
                NextHeader::Error(error) => {
                    let error = machine_or(error, host.name());
                    let _ = reply_error(output, &error);
                    break Err(error);
                }
            },
        };

        let header_bytes = build_downstream_header(
            &req.inner_map,
            &req.principal,
            target,
            chained,
            req.next_hops,
        );
        let write_err = match wait_downstream(
            async {
                stdin.write_all(&header_bytes).await?;
                stdin.flush().await
            },
            options.downstream_read_timeout,
            options.shutdown.as_ref(),
            options.client_gone.as_ref(),
            None,
            None,
        )
        .await
        {
            DownstreamWait::Ready(Ok(())) => None,
            DownstreamWait::Ready(Err(err)) => Some(
                ErrorBody::new(
                    ErrorCode::Unreachable,
                    format!("Could not send stream get to {next_hop}: {err}."),
                )
                .with_machine(&next_hop),
            ),
            DownstreamWait::Shutdown => Some(
                ErrorBody::new(
                    ErrorCode::Cancelled,
                    "The stream stopped because the node is shutting down.",
                )
                .with_machine(host.name()),
            ),
            DownstreamWait::ClientGone => Some(client_gone_error(host.name())),
            DownstreamWait::TimedOut => Some(downstream_timeout_error(
                target,
                options.downstream_read_timeout,
            )),
        };
        if let Some(error) = write_err {
            let _ = reply_error(output, &error);
            record_for_target(
                host,
                target,
                Some(req.principal),
                door,
                device,
                "stream.get",
                req.recorded,
                req.started,
                &Err(error.clone()),
            );
            break Err(error);
        }

        let reply = read_downstream_reply_forwarding_busy(
            &mut stdout,
            host,
            target,
            options,
            None,
            None,
            output,
        )
        .await;
        let (raw_ready, ready_fields) = match reply {
            Ok(ReplyLine::Json(raw, fields)) => (raw, fields),
            Ok(ReplyLine::RemoteError(error)) => {
                if error.code == ErrorCode::Denied
                    && let Some(pending) = bucket.take()
                {
                    pending.flush(host, door, device);
                }
                if reply_error(output, &error).is_err() {
                    break Err(error);
                }
                record_for_target(
                    host,
                    target,
                    Some(req.principal),
                    door,
                    device,
                    "stream.get",
                    req.recorded,
                    req.started,
                    &Err(error),
                );
                continue;
            }
            Ok(ReplyLine::Eof) => {
                let mut error = ErrorBody::new(
                    ErrorCode::Unreachable,
                    format!("The stream to {target} closed before answering."),
                )
                .with_machine(target);
                let text = stderr.text();
                if !text.is_empty() {
                    error = error.with_hint(format!("Remote stderr: {text}"));
                }
                let _ = reply_error(output, &error);
                record_for_target(
                    host,
                    target,
                    Some(req.principal),
                    door,
                    device,
                    "stream.get",
                    req.recorded,
                    req.started,
                    &Err(error.clone()),
                );
                break Err(error);
            }
            Err(error) => {
                let error = machine_or(error, host.name());
                let _ = reply_error(output, &error);
                record_for_target(
                    host,
                    target,
                    Some(req.principal),
                    door,
                    device,
                    "stream.get",
                    req.recorded,
                    req.started,
                    &Err(error.clone()),
                );
                break Err(error);
            }
        };

        let ready: GetReady = match serde_json::from_value(Value::Object(ready_fields)) {
            Ok(ready) => ready,
            Err(_) => {
                let error = ErrorBody::new(
                    ErrorCode::Failed,
                    format!("{target} answered relayed get with an unexpected reply."),
                )
                .with_machine(target);
                let _ = reply_error(output, &error);
                record_for_target(
                    host,
                    target,
                    Some(req.principal),
                    door,
                    device,
                    "stream.get",
                    req.recorded,
                    req.started,
                    &Err(error.clone()),
                );
                break Err(error);
            }
        };

        if let Err(err) = output.write_all(&raw_ready).and_then(|()| output.flush()) {
            let error = machine_or(gone(&err), host.name());
            record_for_target(
                host,
                target,
                Some(req.principal),
                door,
                device,
                "stream.get",
                req.recorded,
                req.started,
                &Err(error.clone()),
            );
            break Err(error);
        }

        if let Err(error) = pump_exact_bytes(
            &mut stdout,
            ready.length,
            host,
            config,
            pinned,
            req.as_principal.as_deref(),
            "fs.read",
            Effect::Read,
            options,
            target,
            output,
        )
        .await
        {
            record_for_target(
                host,
                target,
                Some(req.principal),
                door,
                device,
                "stream.get",
                req.recorded,
                req.started,
                &Err(error.clone()),
            );
            break Err(error);
        }

        let target_keep_open = ready.keep_open == Some(true);
        if target_keep_open {
            let mut trailer = Vec::with_capacity(32);
            let trailer_wait = wait_downstream(
                read_bounded_async_line(&mut stdout, &mut trailer, MAX_DONE_LINE),
                options.downstream_read_timeout,
                options.shutdown.as_ref(),
                options.client_gone.as_ref(),
                None,
                None,
            )
            .await;
            let trailer_err = match trailer_wait {
                DownstreamWait::Ready(Ok(n))
                    if n > 0
                        && serde_json::from_slice::<Value>(&trailer)
                            .ok()
                            .and_then(|v| v.as_object().cloned())
                            .is_some_and(|m| {
                                m.len() == 1 && m.get("done") == Some(&Value::Bool(true))
                            }) =>
                {
                    None
                }
                DownstreamWait::Shutdown => Some(
                    ErrorBody::new(
                        ErrorCode::Cancelled,
                        "The stream stopped because the node is shutting down.",
                    )
                    .with_machine(host.name()),
                ),
                DownstreamWait::ClientGone => Some(client_gone_error(host.name())),
                DownstreamWait::TimedOut => Some(downstream_timeout_error(
                    target,
                    options.downstream_read_timeout,
                )),
                DownstreamWait::Ready(_) => Some(
                    ErrorBody::new(
                        ErrorCode::Failed,
                        format!(
                            "The relayed keep_open stream from {target} ended without its frame terminator."
                        ),
                    )
                    .with_machine(target),
                ),
            };
            if let Some(error) = trailer_err {
                record_for_target(
                    host,
                    target,
                    Some(req.principal),
                    door,
                    device,
                    "stream.get",
                    req.recorded,
                    req.started,
                    &Err(error.clone()),
                );
                break Err(error);
            }
            if let Err(err) = output.write_all(&trailer).and_then(|()| output.flush()) {
                let error = machine_or(gone(&err), host.name());
                record_for_target(
                    host,
                    target,
                    Some(req.principal),
                    door,
                    device,
                    "stream.get",
                    req.recorded,
                    req.started,
                    &Err(error.clone()),
                );
                break Err(error);
            }
        }

        let matches_existing = bucket
            .as_ref()
            .is_some_and(|b| b.principal == req.principal && b.path == req.path);
        if matches_existing {
            if let Some(b) = bucket.as_mut() {
                b.requests = b.requests.saturating_add(1);
                b.bytes = b.bytes.saturating_add(ready.length);
                if b.started.elapsed() >= options.audit_coalesce_window
                    && let Some(finished) = bucket.take()
                {
                    finished.flush(host, door, device);
                }
            }
        } else {
            if let Some(prev) = bucket.take() {
                prev.flush(host, door, device);
            }
            bucket = Some(GetAuditBucket {
                target: target.to_owned(),
                principal: req.principal,
                path: req.path,
                requests: 1,
                bytes: ready.length,
                started: req.started,
            });
        }

        if !target_keep_open {
            break Ok(());
        }
    };

    if let Some(pending) = bucket.take() {
        pending.flush(host, door, device);
    }
    let _ = output.flush();
    drop(stdin);
    drop(stdout);
    let close_grace = if res.is_ok() {
        DOWNSTREAM_CLOSE_GRACE
    } else {
        DOWNSTREAM_ABORT_GRACE
    };
    handle.close(close_grace).await;
    res
}

/// Relays a `put` or `unpack` when `input: Read + Send + 'static`: reads client bytes on a
/// dedicated OS thread into a bounded channel so target death mid-upload closes the client
/// immediately without blocking on stdin.
#[allow(clippy::too_many_arguments)]
async fn relay_write_spawned<R: Read + Send + 'static, W: Write>(
    host: &Host,
    config: &ConfigStore,
    fleet: &Fleet,
    door: Door,
    device: Option<&str>,
    options: &StreamServeOptions,
    target: &str,
    principal: String,
    audit_tool: &str,
    declared_size: Option<u64>,
    inner_map: Map<String, Value>,
    next_hops: u32,
    recorded: Value,
    started: Instant,
    mut input: BufReader<R>,
    output: &mut BufWriter<W>,
) -> Result<(), ErrorBody> {
    let opened = match open_and_greet_downstream(fleet, config, host, target, options).await {
        Ok(opened) => opened,
        Err(error) => {
            let error = machine_or(error, host.name());
            let _ = reply_error(output, &error);
            record_for_target(
                host,
                target,
                Some(principal),
                door,
                device,
                audit_tool,
                recorded,
                started,
                &Err(error.clone()),
            );
            return Err(error);
        }
    };

    let OpenedDownstream {
        mut stdin,
        mut stdout,
        handle,
        stderr,
        next_hop,
        chained,
    } = opened;

    let header_bytes = build_downstream_header(&inner_map, &principal, target, chained, next_hops);
    let header_err = match wait_downstream(
        async {
            stdin.write_all(&header_bytes).await?;
            stdin.flush().await
        },
        options.downstream_read_timeout,
        options.shutdown.as_ref(),
        options.client_gone.as_ref(),
        None,
        None,
    )
    .await
    {
        DownstreamWait::Ready(Ok(())) => None,
        DownstreamWait::Ready(Err(err)) => Some(
            ErrorBody::new(
                ErrorCode::Unreachable,
                format!("Could not send stream request to {next_hop}: {err}."),
            )
            .with_machine(&next_hop),
        ),
        DownstreamWait::Shutdown => Some(relay_shutdown_error(host.name())),
        DownstreamWait::ClientGone => Some(client_gone_error(host.name())),
        DownstreamWait::TimedOut => Some(downstream_timeout_error(
            target,
            options.downstream_read_timeout,
        )),
    };
    if let Some(error) = header_err {
        drop(stdin);
        drop(stdout);
        handle.close(DOWNSTREAM_ABORT_GRACE).await;
        let _ = reply_error(output, &error);
        record_for_target(
            host,
            target,
            Some(principal),
            door,
            device,
            audit_tool,
            recorded,
            started,
            &Err(error.clone()),
        );
        return Err(error);
    }

    let first_reply = read_downstream_reply_forwarding_busy(
        &mut stdout,
        host,
        target,
        options,
        None,
        None,
        output,
    )
    .await;
    let (ready_raw, ready_fields) = match first_reply {
        Ok(ReplyLine::Json(raw, fields)) => (raw, fields),
        Ok(ReplyLine::RemoteError(error)) => {
            drop(stdin);
            drop(stdout);
            handle.close(DOWNSTREAM_CLOSE_GRACE).await;
            let _ = reply_error(output, &error);
            record_for_target(
                host,
                target,
                Some(principal),
                door,
                device,
                audit_tool,
                recorded,
                started,
                &Err(error.clone()),
            );
            return Err(error);
        }
        Ok(ReplyLine::Eof) => {
            drop(stdin);
            drop(stdout);
            handle.close(DOWNSTREAM_CLOSE_GRACE).await;
            let mut error = ErrorBody::new(
                ErrorCode::Unreachable,
                format!("The stream to {target} closed before answering."),
            )
            .with_machine(target);
            let text = stderr.text();
            if !text.is_empty() {
                error = error.with_hint(format!("Remote stderr: {text}"));
            }
            let _ = reply_error(output, &error);
            record_for_target(
                host,
                target,
                Some(principal),
                door,
                device,
                audit_tool,
                recorded,
                started,
                &Err(error.clone()),
            );
            return Err(error);
        }
        Err(error) => {
            drop(stdin);
            drop(stdout);
            handle.close(DOWNSTREAM_CLOSE_GRACE).await;
            let error = machine_or(error, host.name());
            let _ = reply_error(output, &error);
            record_for_target(
                host,
                target,
                Some(principal),
                door,
                device,
                audit_tool,
                recorded,
                started,
                &Err(error.clone()),
            );
            return Err(error);
        }
    };

    let body_limit = match declared_size {
        Some(size) => {
            let ready = match serde_json::from_value::<PutReady>(Value::Object(ready_fields)) {
                Ok(ready) if ready.offset <= size => ready,
                _ => {
                    drop(stdin);
                    drop(stdout);
                    handle.close(DOWNSTREAM_CLOSE_GRACE).await;
                    let error = ErrorBody::new(
                        ErrorCode::Failed,
                        format!("{target} answered relayed put with an invalid resume offset."),
                    )
                    .with_machine(target);
                    let _ = reply_error(output, &error);
                    record_for_target(
                        host,
                        target,
                        Some(principal),
                        door,
                        device,
                        audit_tool,
                        recorded,
                        started,
                        &Err(error.clone()),
                    );
                    return Err(error);
                }
            };
            Some(size - ready.offset)
        }
        None => None,
    };

    if let Err(err) = output.write_all(&ready_raw).and_then(|()| output.flush()) {
        drop(stdin);
        drop(stdout);
        handle.close(DOWNSTREAM_CLOSE_GRACE).await;
        let error = machine_or(gone(&err), host.name());
        record_for_target(
            host,
            target,
            Some(principal),
            door,
            device,
            audit_tool,
            recorded,
            started,
            &Err(error.clone()),
        );
        return Err(error);
    }

    let (chunk_tx, mut chunk_rx) = async_mpsc::channel::<Vec<u8>>(QUEUED_CHUNKS);
    let stop_writing = CancellationToken::new();
    let writer_stop = stop_writing.clone();
    let writer_shutdown = options.shutdown.clone();
    let writer_client_gone = options.client_gone.clone();
    let write_timeout = options.downstream_read_timeout;
    let write_progress = Arc::new(Notify::new());
    let writer_progress = Arc::clone(&write_progress);
    let write_timed_out = CancellationToken::new();
    let writer_timed_out = write_timed_out.clone();

    // Dedicated OS thread feeds bounded chunks from client stdin so returning early when the
    // target dies mid-put never blocks on an unread client stdin.
    std::thread::spawn(move || {
        let mut left = body_limit.unwrap_or(u64::MAX);
        let mut buf = vec![0u8; OUTPUT_BUFFER];
        while left > 0 {
            let want = usize::try_from(left).unwrap_or(usize::MAX).min(buf.len());
            match input.read(&mut buf[..want]) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if chunk_tx.blocking_send(buf[..n].to_vec()).is_err() {
                        break;
                    }
                    left -= n as u64;
                }
            }
        }
    });

    let writer_task = tokio::spawn(async move {
        loop {
            let next = tokio::select! {
                biased;
                () = writer_stop.cancelled() => None,
                () = async {
                    match writer_shutdown.as_ref() {
                        Some(tok) => tok.cancelled().await,
                        None => std::future::pending().await,
                    }
                } => None,
                () = async {
                    match writer_client_gone.as_ref() {
                        Some(tok) => tok.cancelled().await,
                        None => std::future::pending().await,
                    }
                } => None,
                chunk = chunk_rx.recv() => chunk,
            };
            match next {
                Some(bytes) => {
                    match wait_downstream(
                        stdin.write_all(&bytes),
                        write_timeout,
                        writer_shutdown.as_ref(),
                        writer_client_gone.as_ref(),
                        None,
                        Some(&writer_stop),
                    )
                    .await
                    {
                        DownstreamWait::Ready(Ok(())) => {
                            writer_progress.notify_one();
                        }
                        DownstreamWait::TimedOut => {
                            if !writer_stop.is_cancelled() {
                                writer_timed_out.cancel();
                            }
                            break;
                        }
                        DownstreamWait::Ready(Err(_))
                        | DownstreamWait::Shutdown
                        | DownstreamWait::ClientGone => {
                            break;
                        }
                    }
                }
                None => {
                    let _ = tokio::time::timeout(DOWNSTREAM_ABORT_GRACE, stdin.flush()).await;
                    break;
                }
            }
        }
        // Dropping `stdin` delivers EOF to the target so it either finishes verifying or leaves
        // its partial file intact (identical to a direct cancelled put).
        drop(stdin);
    });

    let final_reply = read_downstream_reply_forwarding_busy(
        &mut stdout,
        host,
        target,
        options,
        Some(write_progress.as_ref()),
        Some(&write_timed_out),
        output,
    )
    .await;

    stop_writing.cancel();
    let _ = tokio::time::timeout(DOWNSTREAM_ABORT_GRACE, writer_task).await;

    let outcome = match final_reply {
        Ok(ReplyLine::Json(raw, _fields)) => output
            .write_all(&raw)
            .and_then(|()| output.flush())
            .map_err(|err| machine_or(gone(&err), host.name())),
        Ok(ReplyLine::RemoteError(error)) => {
            let _ = reply_error(output, &error);
            Err(error)
        }
        Ok(ReplyLine::Eof) => {
            let mut error = ErrorBody::new(
                ErrorCode::Failed,
                format!("The upload stream to {target} ended before finishing."),
            )
            .with_machine(target);
            let text = stderr.text();
            if !text.is_empty() {
                error = error.with_hint(format!("Remote stderr: {text}"));
            }
            let _ = reply_error(output, &error);
            Err(error)
        }
        Err(error) => {
            let error = machine_or(error, host.name());
            let _ = reply_error(output, &error);
            Err(error)
        }
    };

    drop(stdout);
    let close_grace = if outcome.is_ok() {
        DOWNSTREAM_CLOSE_GRACE
    } else {
        DOWNSTREAM_ABORT_GRACE
    };
    handle.close(close_grace).await;
    record_for_target(
        host,
        target,
        Some(principal),
        door,
        device,
        audit_tool,
        recorded,
        started,
        &outcome,
    );
    outcome
}

/// Relays a `put` or `unpack` when `input` is not `Send + 'static` (`serve` and `serve_with_door`).
#[allow(clippy::too_many_arguments)]
async fn relay_write_scoped<R: BufRead, W: Write>(
    host: &Host,
    config: &ConfigStore,
    fleet: &Fleet,
    door: Door,
    device: Option<&str>,
    options: &StreamServeOptions,
    target: &str,
    principal: String,
    audit_tool: &str,
    declared_size: Option<u64>,
    inner_map: Map<String, Value>,
    next_hops: u32,
    recorded: Value,
    started: Instant,
    input: &mut R,
    output: &mut BufWriter<W>,
) -> Result<(), ErrorBody> {
    let opened = match open_and_greet_downstream(fleet, config, host, target, options).await {
        Ok(opened) => opened,
        Err(error) => {
            let error = machine_or(error, host.name());
            let _ = reply_error(output, &error);
            record_for_target(
                host,
                target,
                Some(principal),
                door,
                device,
                audit_tool,
                recorded,
                started,
                &Err(error.clone()),
            );
            return Err(error);
        }
    };

    let OpenedDownstream {
        mut stdin,
        mut stdout,
        handle,
        stderr,
        next_hop,
        chained,
    } = opened;

    let header_bytes = build_downstream_header(&inner_map, &principal, target, chained, next_hops);
    let header_err = match wait_downstream(
        async {
            stdin.write_all(&header_bytes).await?;
            stdin.flush().await
        },
        options.downstream_read_timeout,
        options.shutdown.as_ref(),
        options.client_gone.as_ref(),
        None,
        None,
    )
    .await
    {
        DownstreamWait::Ready(Ok(())) => None,
        DownstreamWait::Ready(Err(err)) => Some(
            ErrorBody::new(
                ErrorCode::Unreachable,
                format!("Could not send stream request to {next_hop}: {err}."),
            )
            .with_machine(&next_hop),
        ),
        DownstreamWait::Shutdown => Some(relay_shutdown_error(host.name())),
        DownstreamWait::ClientGone => Some(client_gone_error(host.name())),
        DownstreamWait::TimedOut => Some(downstream_timeout_error(
            target,
            options.downstream_read_timeout,
        )),
    };
    if let Some(error) = header_err {
        drop(stdin);
        drop(stdout);
        handle.close(DOWNSTREAM_ABORT_GRACE).await;
        let _ = reply_error(output, &error);
        record_for_target(
            host,
            target,
            Some(principal),
            door,
            device,
            audit_tool,
            recorded,
            started,
            &Err(error.clone()),
        );
        return Err(error);
    }

    let first_reply = read_downstream_reply_forwarding_busy(
        &mut stdout,
        host,
        target,
        options,
        None,
        None,
        output,
    )
    .await;
    let (ready_raw, ready_fields) = match first_reply {
        Ok(ReplyLine::Json(raw, fields)) => (raw, fields),
        Ok(ReplyLine::RemoteError(error)) => {
            drop(stdin);
            drop(stdout);
            handle.close(DOWNSTREAM_CLOSE_GRACE).await;
            let _ = reply_error(output, &error);
            record_for_target(
                host,
                target,
                Some(principal),
                door,
                device,
                audit_tool,
                recorded,
                started,
                &Err(error.clone()),
            );
            return Err(error);
        }
        Ok(ReplyLine::Eof) => {
            drop(stdin);
            drop(stdout);
            handle.close(DOWNSTREAM_CLOSE_GRACE).await;
            let error = ErrorBody::new(
                ErrorCode::Unreachable,
                format!("The stream to {target} closed before answering."),
            )
            .with_machine(target);
            let _ = reply_error(output, &error);
            record_for_target(
                host,
                target,
                Some(principal),
                door,
                device,
                audit_tool,
                recorded,
                started,
                &Err(error.clone()),
            );
            return Err(error);
        }
        Err(error) => {
            drop(stdin);
            drop(stdout);
            handle.close(DOWNSTREAM_CLOSE_GRACE).await;
            let error = machine_or(error, host.name());
            let _ = reply_error(output, &error);
            record_for_target(
                host,
                target,
                Some(principal),
                door,
                device,
                audit_tool,
                recorded,
                started,
                &Err(error.clone()),
            );
            return Err(error);
        }
    };

    let body_limit = match declared_size {
        Some(size) => {
            let ready = match serde_json::from_value::<PutReady>(Value::Object(ready_fields)) {
                Ok(ready) if ready.offset <= size => ready,
                _ => {
                    drop(stdin);
                    drop(stdout);
                    handle.close(DOWNSTREAM_CLOSE_GRACE).await;
                    let error = ErrorBody::new(
                        ErrorCode::Failed,
                        format!("{target} answered relayed put with an invalid resume offset."),
                    )
                    .with_machine(target);
                    let _ = reply_error(output, &error);
                    record_for_target(
                        host,
                        target,
                        Some(principal),
                        door,
                        device,
                        audit_tool,
                        recorded,
                        started,
                        &Err(error.clone()),
                    );
                    return Err(error);
                }
            };
            Some(size - ready.offset)
        }
        None => None,
    };

    if let Err(err) = output.write_all(&ready_raw).and_then(|()| output.flush()) {
        drop(stdin);
        drop(stdout);
        handle.close(DOWNSTREAM_CLOSE_GRACE).await;
        let error = machine_or(gone(&err), host.name());
        record_for_target(
            host,
            target,
            Some(principal),
            door,
            device,
            audit_tool,
            recorded,
            started,
            &Err(error.clone()),
        );
        return Err(error);
    }

    let mut left = body_limit.unwrap_or(u64::MAX);
    let mut buf = vec![0u8; OUTPUT_BUFFER];
    let mut body_abort: Option<ErrorBody> = None;
    while left > 0 {
        let want = usize::try_from(left).unwrap_or(usize::MAX).min(buf.len());
        match input.read(&mut buf[..want]) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                match wait_downstream(
                    stdin.write_all(&buf[..n]),
                    options.downstream_read_timeout,
                    options.shutdown.as_ref(),
                    options.client_gone.as_ref(),
                    None,
                    None,
                )
                .await
                {
                    DownstreamWait::Ready(Ok(())) => {
                        left -= n as u64;
                    }
                    DownstreamWait::Ready(Err(_)) => break,
                    DownstreamWait::Shutdown => {
                        body_abort = Some(relay_shutdown_error(host.name()));
                        break;
                    }
                    DownstreamWait::ClientGone => {
                        body_abort = Some(client_gone_error(host.name()));
                        break;
                    }
                    DownstreamWait::TimedOut => {
                        body_abort = Some(downstream_timeout_error(
                            target,
                            options.downstream_read_timeout,
                        ));
                        break;
                    }
                }
            }
        }
    }
    if body_abort.is_none() {
        let _ = tokio::time::timeout(DOWNSTREAM_ABORT_GRACE, stdin.flush()).await;
    }
    drop(stdin);

    let final_reply = match body_abort {
        Some(err) => Err(err),
        None => {
            read_downstream_reply_forwarding_busy(
                &mut stdout,
                host,
                target,
                options,
                None,
                None,
                output,
            )
            .await
        }
    };

    let outcome = match final_reply {
        Ok(ReplyLine::Json(raw, _)) => output
            .write_all(&raw)
            .and_then(|()| output.flush())
            .map_err(|err| machine_or(gone(&err), host.name())),
        Ok(ReplyLine::RemoteError(error)) => {
            let _ = reply_error(output, &error);
            Err(error)
        }
        Ok(ReplyLine::Eof) => {
            let mut error = ErrorBody::new(
                ErrorCode::Failed,
                format!("The upload stream to {target} ended before finishing."),
            )
            .with_machine(target);
            let text = stderr.text();
            if !text.is_empty() {
                error = error.with_hint(format!("Remote stderr: {text}"));
            }
            let _ = reply_error(output, &error);
            Err(error)
        }
        Err(error) => {
            let error = machine_or(error, host.name());
            let _ = reply_error(output, &error);
            Err(error)
        }
    };

    drop(stdout);
    let close_grace = if outcome.is_ok() {
        DOWNSTREAM_CLOSE_GRACE
    } else {
        DOWNSTREAM_ABORT_GRACE
    };
    handle.close(close_grace).await;
    record_for_target(
        host,
        target,
        Some(principal),
        door,
        device,
        audit_tool,
        recorded,
        started,
        &outcome,
    );
    outcome
}

async fn read_bounded_async_line(
    reader: &mut AsyncBufReader<ChildStdout>,
    out: &mut Vec<u8>,
    limit: usize,
) -> io::Result<usize> {
    let mut total = 0usize;
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            return Ok(total);
        }
        if let Some(pos) = available.iter().position(|&b| b == b'\n') {
            let take = pos + 1;
            if total + take > limit {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "downstream stream reply line exceeded maximum length",
                ));
            }
            out.extend_from_slice(&available[..take]);
            reader.consume(take);
            return Ok(total + take);
        }
        let n = available.len();
        if total + n > limit {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "downstream stream reply line exceeded maximum length",
            ));
        }
        out.extend_from_slice(available);
        reader.consume(n);
        total += n;
    }
}

/// Unwraps a subsequent header on a local `on == host.name` `keep_open` relayed stream.
pub(crate) fn unwrap_local_relay_next(host: &Host, next: NextHeader) -> NextHeader {
    match next {
        NextHeader::Header((hdr, rec)) => match hdr.op {
            Op::Relay { on, request, .. } => {
                if on != host.name() {
                    return NextHeader::Error(
                        ErrorBody::new(
                            ErrorCode::InvalidArgs,
                            format!(
                                "A persistent relayed stream to {} cannot switch to {on}.",
                                host.name()
                            ),
                        )
                        .with_machine(host.name()),
                    );
                }
                match parse_relay_inner(host, &request) {
                    Ok((inner_op, _)) => NextHeader::Header((
                        Header {
                            as_principal: hdr.as_principal,
                            op: inner_op,
                        },
                        rec,
                    )),
                    Err(err) => NextHeader::Error(err),
                }
            }
            _ => NextHeader::Error(
                ErrorBody::new(
                    ErrorCode::InvalidArgs,
                    "A persistent relayed keep_open stream accepts only relay get operations.",
                )
                .with_machine(host.name()),
            ),
        },
        other => other,
    }
}
