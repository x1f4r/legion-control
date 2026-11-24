//! `legion serve`: the HTTP door on a loopback address until a signal says stop.

use std::io::{self, Write};
use std::process::ExitCode;
use std::sync::Arc;

use legion_core::{CancellationToken, Node, Router};
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

/// Serves until `shutdown` fires, then stops accepting, cancels running calls, waits for them to
/// be recorded, and shuts down the router.
pub async fn run(
    node: Node,
    router: Arc<dyn Router>,
    listen: Option<String>,
    help: bool,
    shutdown: CancellationToken,
) -> ExitCode {
    if help {
        let _ = io::stdout()
            .lock()
            .write_all(format!("{HELP}\n").as_bytes());
        router.shutdown().await;
        return ExitCode::SUCCESS;
    }
    let failed = |text: String| {
        let _ = io::stderr().lock().write_all(text.as_bytes());
        ExitCode::from(1)
    };
    let address = match http::listen_address(listen.as_deref().unwrap_or(http::DEFAULT_LISTEN)) {
        Ok(address) => address,
        Err(error) => {
            router.shutdown().await;
            return failed(render::error(&error));
        }
    };
    let door = match HttpDoor::new(node, Arc::clone(&router)) {
        Ok(door) => door,
        Err(error) => {
            router.shutdown().await;
            return failed(render::error(&error));
        }
    };
    let listener = match tokio::net::TcpListener::bind(address).await {
        Ok(listener) => listener,
        Err(error) => {
            router.shutdown().await;
            return failed(format!(
                "legion serve: {address} cannot be listened on: {error}\n"
            ));
        }
    };
    let bound = listener.local_addr().unwrap_or(address);
    let _ = io::stderr()
        .lock()
        .write_all(format!("legion serve: listening on http://{bound}\n").as_bytes());
    match door.serve(listener, shutdown.cancelled()).await {
        Ok(()) => {
            let _ = io::stderr().lock().write_all(b"legion serve: stopped\n");
            ExitCode::SUCCESS
        }
        Err(error) => failed(format!("legion serve: {error}\n")),
    }
}
