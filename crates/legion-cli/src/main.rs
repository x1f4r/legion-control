//! `legion`: one binary on every machine, giving people and agents the same tools.
//!
//! Exit codes: 0 when the call succeeded, 1 when it failed (the error object is on stdout as JSON
//! unless stdout is a terminal, where a readable message goes to stderr instead), 2 when the
//! command line itself is wrong.

mod args;
mod cp;
mod render;
mod rpc;

use std::io::{self, IsTerminal, Write};
use std::process::ExitCode;
use std::sync::Arc;

use legion_core::proto::{ErrorBody, Target, ToolsResult};
use legion_core::{Call, Door, EventSink, Node, Paths, RouteOutcome, Router, VERSION};
use legion_doors::mcp::{self, McpDoor};
use legion_net::stream::Host;
use legion_net::{Fleet, Subcommand};
use serde::Serialize;
use serde_json::Value;
use tokio::io::AsyncReadExt;

use crate::args::{ArgsSource, CallCommand, Command, USAGE, UsageError, command_help};

const FAILED: u8 = 1;
const USAGE_ERROR: u8 = 2;

fn main() -> ExitCode {
    if let Some(code) = legion_tools::job::try_supervise_cli(std::env::args_os().skip(1)) {
        return code;
    }
    let command = match args::parse(std::env::args_os().skip(1)) {
        Ok(command) => command,
        Err(error) => return usage_error(&error),
    };
    match command {
        Command::Version => {
            out(&format!("legion {VERSION}\n"));
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
        Command::Tools { json } | Command::Help { json, .. } => *json,
        Command::Rpc { .. }
        | Command::Mcp { .. }
        | Command::Stream { .. }
        | Command::Gate { .. }
        | Command::Version => false,
    };
    let (node, router) = match Paths::from_env() {
        Ok(paths) => {
            let fleet = Fleet::new(&paths);
            let mut registry = legion_tools::registry();
            legion_net::register(&mut registry, fleet.clone());
            (
                Node::new(paths, registry),
                Arc::new(fleet) as Arc<dyn Router>,
            )
        }
        // The stdout of `legion rpc`, `legion mcp` and `legion stream` carries protocol messages
        // and nothing else; a stream says what went wrong as its one reply.
        Err(error)
            if matches!(
                command,
                Command::Rpc { .. }
                    | Command::Mcp { .. }
                    | Command::Stream { .. }
                    | Command::Gate { .. }
            ) =>
        {
            if matches!(command, Command::Stream { .. }) {
                out(&format!("{}\n", serde_json::json!({ "error": error })));
            }
            err(&render::error(&error));
            return ExitCode::from(FAILED);
        }
        Err(error) => return failed(&error, json),
    };
    match command {
        Command::Call(call) => {
            let code = call_tool(&node, router.as_ref(), call).await;
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
            let code = cp::run(&node, router.as_ref(), command).await;
            router.shutdown().await;
            code
        }
        Command::Rpc { pinned } => serve_rpc(node, router, pinned).await,
        Command::Mcp { pinned, compact } => serve_mcp(node, router, pinned, compact).await,
        Command::Stream { pinned } => serve_stream(&node, pinned).await,
        Command::Gate { pinned } => gate(node, router, pinned).await,
        Command::Help { tool: None, .. } | Command::Version => {
            out(&format!("{USAGE}\n"));
            ExitCode::SUCCESS
        }
    }
}

/// `legion rpc`: JSON-RPC on stdin and stdout until stdin ends.
async fn serve_rpc(node: Node, router: Arc<dyn Router>, pinned: Option<String>) -> ExitCode {
    match rpc::serve(
        tokio::io::stdin(),
        tokio::io::stdout(),
        node,
        router,
        pinned,
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
async fn serve_stream(node: &Node, pinned: Option<String>) -> ExitCode {
    let host = Host::new(node.paths().clone(), node.name());
    let served = tokio::task::spawn_blocking(move || {
        legion_net::stream::serve(
            &host,
            pinned.as_deref(),
            io::stdin().lock(),
            io::stdout().lock(),
        )
    })
    .await;
    match served {
        Ok(Ok(())) => ExitCode::SUCCESS,
        Ok(Err(error)) => {
            err(&format!("legion stream: {}\n", error.message));
            ExitCode::from(FAILED)
        }
        Err(error) => {
            err(&format!("legion stream: the operation stopped: {error}\n"));
            ExitCode::from(FAILED)
        }
    }
}

/// `legion gate`: serves what a key forced to it asked for, when that is exactly `legion rpc` or
/// `legion stream` as the ssh connector sends them, pinned to the key's principal. The request is
/// only compared, never run.
async fn gate(node: Node, router: Arc<dyn Router>, pinned: String) -> ExitCode {
    let requested = std::env::var("SSH_ORIGINAL_COMMAND")
        .ok()
        .and_then(|command| legion_net::gate::requested(&command));
    match requested {
        Some(Subcommand::Rpc) => serve_rpc(node, router, Some(pinned)).await,
        Some(Subcommand::Stream) => serve_stream(&node, Some(pinned)).await,
        None => {
            err(&format!("{}\n", legion_net::gate::REFUSAL));
            ExitCode::from(FAILED)
        }
    }
}

/// `legion mcp`: the node's tools for an AI agent, over MCP on stdin and stdout until stdin ends.
async fn serve_mcp(
    node: Node,
    router: Arc<dyn Router>,
    pinned: Option<String>,
    compact: bool,
) -> ExitCode {
    let door = match McpDoor::new(node, router, mcp::Options { pinned, compact }) {
        Ok(door) => door,
        Err(error) => {
            err(&render::error(&error));
            return ExitCode::from(FAILED);
        }
    };
    match door.serve(tokio::io::stdin(), tokio::io::stdout()).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            err(&format!("legion mcp: the session ended: {error}\n"));
            ExitCode::from(FAILED)
        }
    }
}

async fn call_tool(node: &Node, router: &dyn Router, command: CallCommand) -> ExitCode {
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

    let multi = matches!(&command.on, Some(Target::All))
        || matches!(&command.on, Some(Target::Many(names)) if names.len() > 1);
    let mut call = Call::new(Door::Cli, command.tool, args);
    call.as_principal = command.as_principal;
    if io::stderr().is_terminal() {
        call.events = EventSink::with_events(move |event| err(&render::event(&event, multi)));
    }
    let outcome = run_call(node, router, call, command.on).await;
    print_outcome(outcome, command.json)
}

/// Runs a call until it ends. Ctrl-C cancels the call rather than killing the process, so the
/// call still ends in the audit log, as cancelled.
async fn run_call(
    node: &Node,
    router: &dyn Router,
    call: Call,
    on: Option<Target>,
) -> RouteOutcome {
    let cancel = call.cancel.clone();
    let running = router.call(node, call, on);
    tokio::pin!(running);
    tokio::select! {
        outcome = &mut running => outcome,
        Ok(()) = tokio::signal::ctrl_c() => {
            cancel.cancel();
            running.await
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
