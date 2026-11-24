//! What goes over the wire between a Legion node and whoever talks to it.
//!
//! Every door (the command line, MCP, HTTP and the rpc pipe) speaks in these types, so a reply
//! looks the same whichever way it travelled. Nothing here performs I/O; the shapes are pinned by
//! the tests in `tests/shapes.rs`, and changing one is a protocol change.

mod error;
mod methods;
mod rpc;
mod spec;

pub use error::{ErrorBody, ErrorCode};
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
