//! `legion serve`: the HTTP door on a loopback address until a signal says stop.

use std::io::{self, Write};
use std::process::ExitCode;
use std::sync::Arc;

use legion_core::{Node, Router};
use legion_doors::http::{self, HttpDoor};

use crate::render;

/// What `legion serve --help` prints.
pub const HELP: &str = "\
Usage: legion serve [--listen ADDRESS]

Serves this machine's tools over HTTP to anything holding a token from `legion token add`: \
GET /v1/tools, POST /v1/call and MCP at /mcp.
It listens on a loopback address only (127.0.0.1:7117 unless --listen says otherwise), and \
refuses any other, so nothing beyond this machine can reach it on its own.
To reach it from your other devices, publish it on your tailnet with Tailscale Serve: \
tailscale serve --bg --https=8443 http://127.0.0.1:7117";

/// Serves until Ctrl-C, SIGTERM or SIGHUP, then stops accepting, cancels running calls and waits
/// for them to be recorded.
pub async fn run(
    node: Node,
    router: Arc<dyn Router>,
    listen: Option<String>,
    help: bool,
) -> ExitCode {
    if help {
        let _ = io::stdout()
            .lock()
            .write_all(format!("{HELP}\n").as_bytes());
        return ExitCode::SUCCESS;
    }
    let failed = |text: String| {
        let _ = io::stderr().lock().write_all(text.as_bytes());
        ExitCode::from(1)
    };
    let address = match http::listen_address(listen.as_deref().unwrap_or(http::DEFAULT_LISTEN)) {
        Ok(address) => address,
        Err(error) => return failed(render::error(&error)),
    };
    let door = match HttpDoor::new(node, router) {
        Ok(door) => door,
        Err(error) => return failed(render::error(&error)),
    };
    let listener = match tokio::net::TcpListener::bind(address).await {
        Ok(listener) => listener,
        Err(error) => {
            return failed(format!(
                "legion serve: {address} cannot be listened on: {error}\n"
            ));
        }
    };
    let bound = listener.local_addr().unwrap_or(address);
    let _ = io::stderr()
        .lock()
        .write_all(format!("legion serve: listening on http://{bound}\n").as_bytes());
    match door.serve(listener, stop()).await {
        Ok(()) => {
            let _ = io::stderr().lock().write_all(b"legion serve: stopped\n");
            ExitCode::SUCCESS
        }
        Err(error) => failed(format!("legion serve: {error}\n")),
    }
}

/// Completes on the first of the signals that ask a server to stop.
#[cfg(unix)]
async fn stop() {
    use tokio::signal::unix::{SignalKind, signal};
    let (Ok(mut terminate), Ok(mut hangup)) = (
        signal(SignalKind::terminate()),
        signal(SignalKind::hangup()),
    ) else {
        let _ = tokio::signal::ctrl_c().await;
        return;
    };
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = terminate.recv() => {}
        _ = hangup.recv() => {}
    }
}

/// Completes on Ctrl-C, Ctrl-Break, or the console closing.
#[cfg(windows)]
async fn stop() {
    use tokio::signal::windows::{ctrl_break, ctrl_close};
    let (Ok(mut breaks), Ok(mut close)) = (ctrl_break(), ctrl_close()) else {
        let _ = tokio::signal::ctrl_c().await;
        return;
    };
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = breaks.recv() => {}
        _ = close.recv() => {}
    }
}
