//! `legion`: one binary on every machine, giving people and agents the same tools.
//!
//! Exit codes: 0 when the call succeeded, 1 when it failed (the error object is on stdout as JSON
//! unless stdout is a terminal, where a readable message goes to stderr instead), 2 when the
//! command line itself is wrong.

mod args;
mod cp;
mod pair;
mod render;
mod rpc;
mod serve;

use std::io::{self, IsTerminal, Write};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use legion_core::proto::{ErrorBody, ErrorCode, Target, ToolsResult};
use legion_core::{
    BUILD, CANCEL_GRACE, Call, CancellationToken, Door, EventSink, Node, Paths, RouteOutcome,
    Router, VERSION, format_version_line,
};
use legion_doors::mcp::{self, McpDoor};
use legion_net::stream::Host;
use legion_net::{Fleet, Subcommand};
use legion_tools::device::enrol::EnrolReply;
use serde::Serialize;
use serde_json::Value;
use tokio::io::AsyncReadExt;

use crate::args::{
    ArgsSource, CallCommand, Command, GateCommand, GateHome, USAGE, UsageError, command_help,
};

const FAILED: u8 = 1;
const USAGE_ERROR: u8 = 2;
/// Bounded wait for a cancelled call to stop and audit after a termination signal.
const SHUTDOWN_GRACE: Duration = CANCEL_GRACE.saturating_add(Duration::from_secs(4));

fn main() -> ExitCode {
    legion_core::record_current_exe();
    if let Some(code) = legion_tools::job::try_supervise_cli(std::env::args_os().skip(1)) {
        return code;
    }
    if let Some(code) = legion_tools::power::try_delay_cli(std::env::args_os().skip(1)) {
        return code;
    }
    legion_tools::update::cleanup_old_binary();
    let command = match args::parse(std::env::args_os().skip(1)) {
        Ok(command) => command,
        Err(error) => return usage_error(&error),
    };
    match command {
        Command::Version => {
            out(&format!("{}\n", format_version_line(VERSION, BUILD)));
            ExitCode::SUCCESS
        }
        Command::Help { tool: None, .. } => {
            out(&format!("{USAGE}\n"));
            ExitCode::SUCCESS
        }
        Command::Help {
            tool: Some(name), ..
        } if command_help(&name).is_some() => {
            out(&format!("{}\n", command_help(&name).unwrap_or_default()));
            ExitCode::SUCCESS
        }
        command => match tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
        {
            Ok(runtime) => {
                let code = runtime.block_on(run(command));
                // A read of stdin cannot be cancelled, and would keep the runtime from shutting
                // down until the input ends; nothing else is left running by now.
                runtime.shutdown_background();
                code
            }
            Err(error) => {
                err(&format!(
                    "legion: the async runtime could not start: {error}\n"
                ));
                ExitCode::from(FAILED)
            }
        },
    }
}

async fn run(command: Command) -> ExitCode {
    let json = match &command {
        Command::Call(call) => call.json,
        Command::Cp(cp) => cp.json,
        Command::Pair(pair) => pair.json,
        Command::Tools { json } | Command::Help { json, .. } => *json,
        _ => false,
    };
    let paths_result = match &command {
        Command::Gate(GateCommand::As { home, .. } | GateCommand::Enrol { home, .. }) => match home
        {
            GateHome::Root(root) => Paths::from_explicit_home(root),
            GateHome::Split {
                config_dir,
                state_dir,
            } => Paths::from_explicit_dirs(config_dir, state_dir),
            GateHome::Default => Paths::from_gate_default(),
        },
        _ => Paths::from_env(),
    };
    let (node, router) = match paths_result {
        Ok(paths) => {
            let fleet = Fleet::new(&paths);
            let mut registry = legion_tools::registry();
            legion_net::register(&mut registry, fleet.clone());
            (
                Node::new(paths, registry),
                Arc::new(fleet) as Arc<dyn Router>,
            )
        }
        // The stdout of `legion rpc`, `legion mcp`, `legion stream` and `legion gate` carries
        // protocol messages and nothing else; stream and gate --enrol report startup errors as
        // their one JSON reply line.
        Err(error)
            if matches!(
                command,
                Command::Rpc { .. }
                    | Command::Mcp { .. }
                    | Command::Stream { .. }
                    | Command::Gate(_)
            ) =>
        {
            match command {
                Command::Stream { .. } => {
                    out(&format!("{}\n", serde_json::json!({ "error": error })));
                }
                Command::Gate(GateCommand::Enrol { .. }) => {
                    let reply = EnrolReply::from_error(error.clone());
                    if let Ok(line) = serde_json::to_string(&reply) {
                        out(&format!("{line}\n"));
                    }
                }
                _ => {}
            }
            err(&render::error(&error));
            return ExitCode::from(FAILED);
        }
        Err(error) => return failed(&error, json),
    };
    match command {
        Command::Call(call) => {
            let shutdown = install_shutdown_token();
            let code = call_tool(&node, router.as_ref(), call, shutdown).await;
            router.shutdown().await;
            code
        }
        Command::Tools { json } => {
            let specs = node.specs();
            if wants_json(json) {
                print_json(&ToolsResult { tools: specs }, json);
            } else {
                out(&render::tools(&specs));
            }
            ExitCode::SUCCESS
        }
        Command::Help {
            tool: Some(tool),
            json,
        } => match node.spec(&tool) {
            Ok(spec) if wants_json(json) => {
                print_json(&spec, json);
                ExitCode::SUCCESS
            }
            Ok(spec) => {
                out(&render::help(&spec));
                ExitCode::SUCCESS
            }
            Err(error) => failed(&error, json),
        },
        Command::Cp(command) => {
            let shutdown = install_shutdown_token();
            let code = cp::run(&node, router.as_ref(), command, shutdown).await;
            router.shutdown().await;
            code
        }
        Command::Rpc { pinned } => {
            let shutdown = install_shutdown_token();
            serve_rpc(node, router, pinned, shutdown).await
        }
        Command::Mcp { pinned, compact } => {
            let shutdown = install_shutdown_token();
            serve_mcp(node, router, pinned, compact, shutdown).await
        }
        Command::Stream { pinned } => {
            let shutdown = install_shutdown_token();
            let code = serve_stream(&node, pinned, shutdown).await;
            router.shutdown().await;
            code
        }
        Command::Gate(GateCommand::As {
            pinned,
            device,
            account,
            ..
        }) => {
            let shutdown = install_shutdown_token();
            gate(node, router, pinned, device, account, shutdown).await
        }
        Command::Gate(GateCommand::Enrol { id, account, .. }) => {
            let shutdown = install_shutdown_token();
            enrol(node, router, id, account, shutdown).await
        }
        Command::Pair(command) => {
            let shutdown = install_shutdown_token();
            let code = pair::run(&node, router.as_ref(), command, shutdown).await;
            router.shutdown().await;
            code
        }
        Command::Serve { listen, help } => {
            let shutdown = install_shutdown_token();
            serve::run(node, router, listen, help, shutdown).await
        }
        Command::Help { tool: None, .. } | Command::Version => {
            out(&format!("{USAGE}\n"));
            ExitCode::SUCCESS
        }
    }
}

/// `legion rpc`: JSON-RPC on stdin and stdout until stdin ends or a termination signal fires.
async fn serve_rpc(
    node: Node,
    router: Arc<dyn Router>,
    pinned: Option<String>,
    shutdown: CancellationToken,
) -> ExitCode {
    match rpc::serve_until(
        tokio::io::stdin(),
        tokio::io::stdout(),
        node,
        router,
        pinned,
        shutdown,
    )
    .await
    {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            err(&format!("legion rpc: the session ended: {error}\n"));
            ExitCode::from(FAILED)
        }
    }
}

/// `legion stream`: one bulk-data operation on stdin and stdout. Its reason for failing, when it
/// fails, goes to stderr whether or not it could still be said on stdout.
async fn serve_stream(
    node: &Node,
    pinned: Option<String>,
    shutdown: CancellationToken,
) -> ExitCode {
    let host = Host::new(node.paths().clone(), node.name());
    let mut served = tokio::task::spawn_blocking(move || {
        legion_net::stream::serve(
            &host,
            pinned.as_deref(),
            io::stdin().lock(),
            io::stdout().lock(),
        )
    });
    let outcome = tokio::select! {
        result = &mut served => Some(result),
        () = shutdown.cancelled() => {
            // Give an operation that is already finishing a short window to audit cleanly
            // without hanging if stdin stays open.
            tokio::time::timeout(Duration::from_millis(500), served)
                .await
                .ok()
        }
    };
    match outcome {
        Some(Ok(Ok(()))) => ExitCode::SUCCESS,
        Some(Ok(Err(error))) => {
            err(&format!("legion stream: {}\n", error.message));
            ExitCode::from(FAILED)
        }
        Some(Err(error)) => {
            err(&format!("legion stream: the operation stopped: {error}\n"));
            ExitCode::from(FAILED)
        }
        None => {
            err("legion stream: stopped by signal\n");
            ExitCode::from(FAILED)
        }
    }
}

/// `legion gate`: serves what a key forced to it asked for, when that is exactly `legion rpc` or
/// `legion stream` as the ssh connector sends them, pinned to the key's principal. The request is
/// only compared, never run. When `--device` is passed (as on paired-device keys), registers the
/// live process under `state/device-sessions/` so `device.remove` ends it immediately. When
/// `--account` is passed (on Windows's shared `administrators_authorized_keys`), refuses any other
/// OS account.
async fn gate(
    node: Node,
    router: Arc<dyn Router>,
    pinned: String,
    device: Option<String>,
    account: Option<String>,
    shutdown: CancellationToken,
) -> ExitCode {
    if let Err(error) = legion_tools::device::authkeys::verify_gate_account(account.as_deref()) {
        router.shutdown().await;
        err(&format!("legion gate: {}\n", error.message));
        return ExitCode::from(FAILED);
    }
    let requested = std::env::var("SSH_ORIGINAL_COMMAND")
        .ok()
        .and_then(|command| legion_net::gate::requested(&command));
    let Some(subcommand) = requested else {
        router.shutdown().await;
        err(&format!("{}\n", legion_net::gate::REFUSAL));
        return ExitCode::from(FAILED);
    };
    let _session = if let Some(device_name) = device.as_deref() {
        match legion_tools::device::sessions::DeviceSessionGuard::register(
            node.paths(),
            device_name,
            &pinned,
        ) {
            Ok(guard) => Some(guard),
            Err(error) => {
                router.shutdown().await;
                err(&render::error(&error));
                return ExitCode::from(FAILED);
            }
        }
    } else {
        None
    };
    match subcommand {
        Subcommand::Rpc => serve_rpc(node, router, Some(pinned), shutdown).await,
        Subcommand::Stream => {
            let code = serve_stream(&node, Some(pinned), shutdown).await;
            router.shutdown().await;
            code
        }
    }
}

/// `legion gate --enrol <id>`: single-use enrolment forced command for a temporary invite key.
/// Ignores `SSH_ORIGINAL_COMMAND` completely, reads one JSON line from stdin, and writes one JSON
/// reply line on stdout.
async fn enrol(
    node: Node,
    router: Arc<dyn Router>,
    id: String,
    account: Option<String>,
    shutdown: CancellationToken,
) -> ExitCode {
    let reply = if let Err(error) =
        legion_tools::device::authkeys::verify_gate_account(account.as_deref())
    {
        EnrolReply::from_error(error)
    } else {
        tokio::select! {
            reply = legion_tools::device::enrol::run_enrol(&node, &id, tokio::io::stdin()) => reply,
            () = shutdown.cancelled() => EnrolReply::from_error(ErrorBody::new(
                ErrorCode::Cancelled,
                "Device enrolment was cancelled by signal.",
            )),
        }
    };
    router.shutdown().await;
    match serde_json::to_string(&reply) {
        Ok(line) => out(&format!("{line}\n")),
        Err(error) => {
            let fallback = EnrolReply::from_error(ErrorBody::new(
                ErrorCode::Internal,
                format!("The enrolment reply cannot be written as JSON: {error}."),
            ));
            if let Ok(line) = serde_json::to_string(&fallback) {
                out(&format!("{line}\n"));
            }
            return ExitCode::from(FAILED);
        }
    }
    if reply.is_ok() {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(FAILED)
    }
}

/// `legion mcp`: the node's tools for an AI agent, over MCP on stdin and stdout until stdin ends.
async fn serve_mcp(
    node: Node,
    router: Arc<dyn Router>,
    pinned: Option<String>,
    compact: bool,
    shutdown: CancellationToken,
) -> ExitCode {
    let door = match McpDoor::new(node, router, mcp::Options { pinned, compact }) {
        Ok(door) => door,
        Err(error) => {
            err(&render::error(&error));
            return ExitCode::from(FAILED);
        }
    };
    match door
        .serve_until(tokio::io::stdin(), tokio::io::stdout(), shutdown)
        .await
    {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            err(&format!("legion mcp: the session ended: {error}\n"));
            ExitCode::from(FAILED)
        }
    }
}

/// Installs OS termination-signal handlers synchronously on the runtime and returns a token that
/// cancels on SIGINT, SIGTERM or SIGHUP on Unix, and Ctrl-C, Ctrl-Break, Ctrl-Close or
/// Ctrl-Shutdown on Windows.
fn install_shutdown_token() -> CancellationToken {
    let token = CancellationToken::new();
    let fire = token.clone();
    #[cfg(unix)]
    {
        use tokio::signal::unix::{Signal, SignalKind, signal};
        let mut term = signal(SignalKind::terminate()).ok();
        let mut hup = signal(SignalKind::hangup()).ok();
        async fn recv(sig: Option<&mut Signal>) {
            match sig {
                Some(sig) => {
                    let _ = sig.recv().await;
                }
                None => std::future::pending::<()>().await,
            }
        }
        tokio::spawn(async move {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {}
                () = recv(term.as_mut()) => {}
                () = recv(hup.as_mut()) => {}
            }
            fire.cancel();
        });
    }
    #[cfg(windows)]
    {
        use tokio::signal::windows::{ctrl_break, ctrl_close, ctrl_shutdown};
        let mut brk = ctrl_break().ok();
        let mut close = ctrl_close().ok();
        let mut down = ctrl_shutdown().ok();
        tokio::spawn(async move {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {}
                () = async {
                    match brk.as_mut() {
                        Some(sig) => {
                            let _ = sig.recv().await;
                        }
                        None => std::future::pending::<()>().await,
                    }
                } => {}
                () = async {
                    match close.as_mut() {
                        Some(sig) => {
                            let _ = sig.recv().await;
                        }
                        None => std::future::pending::<()>().await,
                    }
                } => {}
                () = async {
                    match down.as_mut() {
                        Some(sig) => {
                            let _ = sig.recv().await;
                        }
                        None => std::future::pending::<()>().await,
                    }
                } => {}
            }
            fire.cancel();
        });
    }
    #[cfg(not(any(unix, windows)))]
    {
        tokio::spawn(async move {
            let _ = tokio::signal::ctrl_c().await;
            fire.cancel();
        });
    }
    token
}

async fn call_tool(
    node: &Node,
    router: &dyn Router,
    command: CallCommand,
    shutdown: CancellationToken,
) -> ExitCode {
    let args = match command.args {
        ArgsSource::Pairs(pairs) => Value::Object(pairs),
        ArgsSource::Json(text) => match args::json_object(&text, "--args") {
            Ok(args) => args,
            Err(error) => return usage_error(&error),
        },
        ArgsSource::Stdin => {
            let mut text = String::new();
            if let Err(error) = tokio::io::stdin().read_to_string(&mut text).await {
                return usage_error(&UsageError(format!(
                    "--args -: stdin cannot be read: {error}."
                )));
            }
            match args::json_object(&text, "The JSON on stdin") {
                Ok(args) => args,
                Err(error) => return usage_error(&error),
            }
        }
    };

    let multi = match &command.on {
        Some(Target::All) => true,
        Some(Target::Many(names)) => {
            names.iter().collect::<std::collections::HashSet<_>>().len() > 1
        }
        None | Some(Target::One(_)) => false,
    };
    let mut call = Call::new(Door::Cli, command.tool, args);
    call.as_principal = command.as_principal;
    if io::stderr().is_terminal() {
        call.events = EventSink::with_events(move |event| err(&render::event(&event, multi)));
    }
    let outcome = run_call(node, router, call, command.on, shutdown).await;
    print_outcome(outcome, command.json)
}

/// Runs a call until it ends. Termination signals (Ctrl-C, SIGTERM, SIGHUP; Ctrl-Break / console
/// close on Windows) cancel the call and wait within a bounded grace so remote work stops and the
/// call reaches the audit log before `router.shutdown()` closes sessions.
async fn run_call(
    node: &Node,
    router: &dyn Router,
    call: Call,
    on: Option<Target>,
    shutdown: CancellationToken,
) -> RouteOutcome {
    let tool_name = call.tool.clone();
    let cancel = call.cancel.clone();
    let running = router.call(node, call, on);
    tokio::pin!(running);
    tokio::select! {
        outcome = &mut running => outcome,
        () = shutdown.cancelled() => {
            cancel.cancel();
            match tokio::time::timeout(SHUTDOWN_GRACE, &mut running).await {
                Ok(outcome) => outcome,
                Err(_) => RouteOutcome::One(Err(ErrorBody::new(
                    ErrorCode::Cancelled,
                    format!("The call to {tool_name} was cancelled by signal."),
                )
                .with_machine(node.name()))),
            }
        }
    }
}

/// Prints what a call came to and returns the exit code it means.
fn print_outcome(outcome: RouteOutcome, json: bool) -> ExitCode {
    let exit_code = if outcome.all_failed() {
        ExitCode::from(FAILED)
    } else {
        ExitCode::SUCCESS
    };
    match outcome {
        RouteOutcome::One(Ok(value)) if wants_json(json) => {
            print_json(&value, json);
            exit_code
        }
        RouteOutcome::One(Ok(value)) => {
            out(&render::value(&value));
            exit_code
        }
        RouteOutcome::One(Err(error)) => failed(&error, json),
        RouteOutcome::Many(results) if wants_json(json) => {
            match RouteOutcome::Many(results).into_wire() {
                Ok(wire) => {
                    print_json(&wire, json);
                    exit_code
                }
                Err(error) => failed(&error, json),
            }
        }
        RouteOutcome::Many(results) => {
            out(&render::fan_out(&results));
            exit_code
        }
    }
}

/// Whether to print JSON: when asked to, and whenever stdout is not a person.
fn wants_json(asked: bool) -> bool {
    asked || !io::stdout().is_terminal()
}

/// Prints JSON on stdout: indented for a person who asked for JSON, one line for a program.
fn print_json<T: Serialize>(value: &T, asked: bool) {
    let text = if asked && io::stdout().is_terminal() {
        serde_json::to_string_pretty(value)
    } else {
        serde_json::to_string(value)
    };
    match text {
        Ok(text) => out(&format!("{text}\n")),
        Err(error) => err(&format!(
            "legion: the result cannot be written as JSON: {error}\n"
        )),
    }
}

/// Reports a failed call and returns its exit code: the error object on stdout for a program, a
/// readable message on stderr for a person.
fn failed(error: &ErrorBody, json: bool) -> ExitCode {
    if wants_json(json) {
        print_json(error, json);
    } else {
        err(&render::error(error));
    }
    ExitCode::from(FAILED)
}

fn usage_error(error: &UsageError) -> ExitCode {
    err(&format!(
        "legion: {error}\nRun `legion help` to see how it is used.\n"
    ));
    ExitCode::from(USAGE_ERROR)
}

// Written by hand rather than with println!, which panics when the reader has gone away, as it
// has when the output is piped into `head`.
fn out(text: &str) {
    let mut stdout = io::stdout().lock();
    let _ = stdout.write_all(text.as_bytes());
    let _ = stdout.flush();
}

fn err(text: &str) {
    let _ = io::stderr().lock().write_all(text.as_bytes());
}
