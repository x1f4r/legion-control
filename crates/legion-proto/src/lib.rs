//! What goes over the wire between a Legion node and whoever talks to it.
//!
//! Every door (the command line, MCP, HTTP and the rpc pipe) speaks in these types, so a reply
//! looks the same whichever way it travelled. Nothing here performs I/O; the shapes are pinned by
//! the tests in `tests/shapes.rs`, and changing one is a protocol change.

use std::time::Duration;

mod error;
mod events;
mod methods;
mod rpc;
mod spec;

pub use error::{ErrorBody, ErrorCode};
pub use events::{Event, EventSink};
pub use methods::{
    CallParams, CancelParams, CancelResult, EventKind, EventParams, FanOut, HelloParams,
    HelloResult, MachineOutcome, Target, ToolsParams, ToolsResult,
};
pub use rpc::{
    INTERNAL_ERROR, INVALID_PARAMS, INVALID_REQUEST, Id, METHOD_NOT_FOUND, Notification, Outcome,
    PARSE_ERROR, Request, Response, RpcError, TOOL_ERROR, Version, method,
};
pub use spec::{Effect, ToolSpec};

/// The protocol this build speaks. A node refuses a `hello` naming another one, so it moves only
/// when an old client could misread a new node; new tools and new optional fields leave it alone.
pub const PROTOCOL: u32 = 1;

/// The version of legion this is; every crate in the workspace shares it.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// How long a cancelled tool has to stop on its own before its future is dropped.
///
/// Dropping a future stops only the part of the work that runs inside it: a blocking thread it
/// started, a child process or a call it forwarded carry on, and would still be changing things
/// after the call was reported as cancelled. So a cancel first cancels the context's token, and
/// whatever the tool returns within this time is the call's result. Long enough to stop a process
/// tree or pass the cancel on, short enough that a tool that never looks does not hold the caller.
pub const CANCEL_GRACE: Duration = Duration::from_secs(5);

/// Maximum number of node-to-node forwards allowed for one call so routing loops cannot spin.
pub const MAX_HOPS: u32 = 3;
