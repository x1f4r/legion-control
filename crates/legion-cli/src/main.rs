//! `legion`: one binary on every machine, giving people and agents the same tools.
//!
//! Exit codes: 0 when the call succeeded, 1 when it failed (the error object is on stdout as JSON
//! unless stdout is a terminal, where a readable message goes to stderr instead), 2 when the
//! command line itself is wrong.

mod args;
mod render;
mod route;
mod rpc;

use std::io::{self, IsTerminal, Write};
use std::process::ExitCode;
use std::sync::Arc;

use legion_core::proto::{ErrorBody, EventKind, ToolsResult};
use legion_core::{Call, Door, EventSink, Node, Paths, VERSION};
use serde::Serialize;
use serde_json::Value;
use tokio::io::AsyncReadExt;

use crate::args::{ArgsSource, CallCommand, Command, USAGE, UsageError};
use crate::route::{Local, Router};

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
        command => match tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
        {
            Ok(runtime) => runtime.block_on(run(command)),
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
        Command::Tools { json } | Command::Help { json, .. } => *json,
        Command::Rpc { .. } | Command::Version => false,
    };
    let node = match Paths::from_env() {
        Ok(paths) => Node::new(paths, legion_tools::registry()),
        Err(error) => return failed(&error, json),
    };
    match command {
        Command::Call(call) => call_tool(&node, call).await,
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
        Command::Rpc { pinned } => {
            let router: Arc<dyn Router> = Arc::new(Local);
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
        Command::Help { tool: None, .. } | Command::Version => {
            out(&format!("{USAGE}\n"));
            ExitCode::SUCCESS
        }
    }
}

async fn call_tool(node: &Node, command: CallCommand) -> ExitCode {
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

    let mut call = Call::new(Door::Cli, command.tool, args);
    call.as_principal = command.as_principal;
    if io::stderr().is_terminal() {
        call.events = EventSink::new(print_event);
    }
    let cancel = call.cancel.clone();
    let running = Local.call(node, call, command.on);
    tokio::pin!(running);
    // Ctrl-C cancels the call rather than killing the process, so the call still ends in the
    // audit log, as cancelled.
    let result = tokio::select! {
        result = &mut running => result,
        Ok(()) = tokio::signal::ctrl_c() => {
            cancel.cancel();
            running.await
        }
    };
    match result {
        Ok(value) if wants_json(command.json) => {
            print_json(&value, command.json);
            ExitCode::SUCCESS
        }
        Ok(value) => {
            out(&render::value(&value));
            ExitCode::SUCCESS
        }
        Err(error) => failed(&error, command.json),
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

/// A call's events, for a person watching stderr.
fn print_event(kind: EventKind, data: Value) {
    let text = match (kind, data) {
        (EventKind::Stdout | EventKind::Stderr, Value::String(text)) => text,
        (_, Value::String(text)) => format!("{text}\n"),
        (kind, data) => format!("{}: {data}\n", kind.as_str()),
    };
    err(&text);
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
