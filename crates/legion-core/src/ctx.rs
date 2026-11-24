//! What a tool knows about the call it is running for.

use std::fmt;
use std::sync::Arc;

use legion_proto::{EventKind, ToolSpec};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::config::ConfigStore;
use crate::node::Node;
use crate::paths::Paths;
use crate::policy::Grant;
use crate::registry::DocumentInfo;

/// How a call arrived. Recorded in the audit log, and the reason a principal can be trusted as far
/// as it is: a local shell is the owner, a pinned ssh key or token is a wall.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Door {
    /// `legion <tool>` on a command line.
    Cli,
    /// `legion mcp`, for AI agents.
    Mcp,
    /// `legion serve`.
    Http,
    /// `legion rpc`, the pipe other nodes and clients use.
    Rpc,
}

impl Door {
    /// The door as the audit log spells it.
    pub fn as_str(self) -> &'static str {
        match self {
            Door::Cli => "cli",
            Door::Mcp => "mcp",
            Door::Http => "http",
            Door::Rpc => "rpc",
        }
    }
}

impl fmt::Display for Door {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Where a running call's `event` notifications go: to the rpc client as notifications carrying
/// the call's id, to stderr on a command line, nowhere when nobody listens.
#[derive(Clone, Default)]
pub struct EventSink(Option<Arc<dyn Fn(EventKind, Value) + Send + Sync>>);

impl EventSink {
    /// A sink that hands each event to `deliver`.
    pub fn new(deliver: impl Fn(EventKind, Value) + Send + Sync + 'static) -> EventSink {
        EventSink(Some(Arc::new(deliver)))
    }

    /// A sink that drops every event.
    pub fn discard() -> EventSink {
        EventSink(None)
    }

    /// Delivers one event.
    pub fn emit(&self, kind: EventKind, data: Value) {
        if let Some(deliver) = &self.0 {
            deliver(kind, data);
        }
    }
}

impl fmt::Debug for EventSink {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(if self.0.is_some() {
            "EventSink"
        } else {
            "EventSink(discard)"
        })
    }
}

/// The context a tool runs in: which node, for whom, through which door, what it is allowed, and
/// how to notice it was cancelled or tell the caller how it is getting on.
pub struct Ctx {
    pub(crate) node: Node,
    pub(crate) name: String,
    pub(crate) door: Door,
    pub(crate) grant: Grant,
    pub(crate) cancel: CancellationToken,
    pub(crate) events: EventSink,
}

impl Ctx {
    /// The name of the node the tool runs on.
    pub fn node_name(&self) -> &str {
        &self.name
    }

    /// The operating system: `linux`, `macos` or `windows`.
    pub fn os(&self) -> &'static str {
        std::env::consts::OS
    }

    /// The CPU architecture, such as `x86_64` or `aarch64`.
    pub fn arch(&self) -> &'static str {
        std::env::consts::ARCH
    }

    /// Where the node keeps its files.
    pub fn paths(&self) -> &Paths {
        self.node.paths()
    }

    /// The principal the call acts as.
    pub fn principal(&self) -> &str {
        self.grant.principal()
    }

    /// The door the call came through.
    pub fn door(&self) -> Door {
        self.door
    }

    /// What the call is allowed. File tools check every path with
    /// [`Grant::check_path`](crate::Grant::check_path) before touching it.
    pub fn grant(&self) -> &Grant {
        &self.grant
    }

    /// Whether the caller has cancelled the call. Once it has, the tool has
    /// [`CANCEL_GRACE`](crate::CANCEL_GRACE) to clean up and return before it is dropped.
    pub fn is_cancelled(&self) -> bool {
        self.cancel.is_cancelled()
    }

    /// Completes when the caller cancels the call. A long operation waits on this alongside its
    /// work (`tokio::select!`), so it can stop what it started and return within
    /// [`CANCEL_GRACE`](crate::CANCEL_GRACE).
    pub async fn cancelled(&self) {
        self.cancel.cancelled().await;
    }

    /// The token that is cancelled with the call. Work handed to `spawn_blocking` cannot be dropped
    /// and keeps going after a cancel unless it checks a clone of this between steps.
    pub fn cancel_token(&self) -> &CancellationToken {
        &self.cancel
    }

    /// Tells the caller something before the call replies: output as it is produced, progress,
    /// a log line. A caller that does not listen loses nothing it needs; the result must still
    /// say everything.
    pub fn emit(&self, kind: EventKind, data: Value) {
        self.events.emit(kind, data);
    }

    /// Every tool the node offers.
    pub fn tools(&self) -> Vec<ToolSpec> {
        self.node.registry().specs()
    }

    /// The node's configuration files.
    pub fn config(&self) -> &ConfigStore {
        self.node.config()
    }

    /// The configuration documents the node reads.
    pub fn documents(&self) -> &[DocumentInfo] {
        self.node.registry().documents()
    }
}

impl fmt::Debug for Ctx {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Ctx")
            .field("node", &self.name)
            .field("door", &self.door)
            .field("grant", &self.grant)
            .finish_non_exhaustive()
    }
}
