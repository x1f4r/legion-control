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
use crate::registry::{DocumentInfo, SourceCheck};

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
    /// `legion stream`, the pipe bulk file data moves over, and the ends of a transfer this node
    /// serves itself.
    Stream,
    /// `legion gate --enrol`, a single-use SSH forced command enrolling a paired device.
    Ssh,
}

impl Door {
    /// The door as the audit log spells it.
    pub fn as_str(self) -> &'static str {
        match self {
            Door::Cli => "cli",
            Door::Mcp => "mcp",
            Door::Http => "http",
            Door::Rpc => "rpc",
            Door::Stream => "stream",
            Door::Ssh => "ssh",
        }
    }
}

impl fmt::Display for Door {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One event emitted by a running call: its kind, its content, and the machine that produced it
/// when the call was forwarded or fanned out.
#[derive(Clone, Debug, PartialEq)]
pub struct Event {
    /// What kind of event it is.
    pub kind: EventKind,
    /// Its content.
    pub data: Value,
    /// The machine that produced the event, when set by routing.
    pub machine: Option<String>,
}

/// Where a running call's `event` notifications go: to the rpc client as notifications carrying
/// the call's id, to stderr on a command line, nowhere when nobody listens.
#[derive(Clone, Default)]
pub struct EventSink {
    deliver: Option<Arc<dyn Fn(Event) + Send + Sync>>,
    machine: Option<Arc<str>>,
}

impl EventSink {
    /// A sink that hands each event's kind and content to `deliver`.
    pub fn new(deliver: impl Fn(EventKind, Value) + Send + Sync + 'static) -> EventSink {
        EventSink {
            deliver: Some(Arc::new(move |event| deliver(event.kind, event.data))),
            machine: None,
        }
    }

    /// A sink that receives the full [`Event`], including the machine that produced it.
    pub fn with_events(deliver: impl Fn(Event) + Send + Sync + 'static) -> EventSink {
        EventSink {
            deliver: Some(Arc::new(deliver)),
            machine: None,
        }
    }

    /// A sink that drops every event.
    pub fn discard() -> EventSink {
        EventSink::default()
    }

    /// Returns a sink that stamps emitted events with `machine`, overwriting any label the emitter
    /// supplied so a remote cannot forge another machine's name.
    #[must_use]
    pub fn for_machine(&self, machine: impl Into<Arc<str>>) -> EventSink {
        EventSink {
            deliver: self.deliver.clone(),
            machine: Some(machine.into()),
        }
    }

    /// Delivers one event from this node.
    pub fn emit(&self, kind: EventKind, data: Value) {
        self.emit_event(Event {
            kind,
            data,
            machine: None,
        });
    }

    /// Delivers one [`Event`], overwriting its `machine` with the machine bound to this sink when
    /// one is set.
    pub fn emit_event(&self, mut event: Event) {
        if let Some(deliver) = &self.deliver {
            if let Some(machine) = &self.machine {
                event.machine = Some(machine.to_string());
            }
            deliver(event);
        }
    }
}

impl fmt::Debug for EventSink {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match (&self.deliver, &self.machine) {
            (Some(_), Some(machine)) => write!(f, "EventSink({machine})"),
            (Some(_), None) => f.write_str("EventSink"),
            (None, _) => f.write_str("EventSink(discard)"),
        }
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
    pub(crate) hops: u32,
    pub(crate) credential_expires: Option<jiff::Timestamp>,
}

impl Ctx {
    /// When the credential the call arrived with stops working, when it does: what the call makes
    /// for the caller, such as a token, may not outlive it.
    pub fn credential_expires(&self) -> Option<jiff::Timestamp> {
        self.credential_expires
    }

    /// The node running this call, for tools that authorise or audit forwarded operations against
    /// this node's live policy and audit log.
    pub fn node(&self) -> &Node {
        &self.node
    }

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

    /// An identifier of the machine's current boot, the one `hello` reports; `None` when the
    /// platform will not say.
    pub fn boot_id(&self) -> Option<String> {
        self.node.boot_id()
    }

    /// The principal the call acts as.
    pub fn principal(&self) -> &str {
        self.grant.principal()
    }

    /// The door the call came through.
    pub fn door(&self) -> Door {
        self.door
    }

    /// How many nodes have already forwarded the call this tool is running for; `0` when the call
    /// started on this node.
    pub fn hops(&self) -> u32 {
        self.hops
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

    /// Every tool the node offers, built-ins and reloadable extensions alike.
    pub fn tools(&self) -> Vec<ToolSpec> {
        self.node.specs()
    }

    /// Whether `name` is registered as a built-in tool on this node.
    pub fn has_builtin(&self, name: &str) -> bool {
        self.node.registry().get(name).is_some()
    }

    /// The node's configuration files.
    pub fn config(&self) -> &ConfigStore {
        self.node.config()
    }

    /// The configuration documents the node reads.
    pub fn documents(&self) -> &[DocumentInfo] {
        self.node.registry().documents()
    }

    /// Health checks reported by reloadable tool sources.
    pub fn source_checks(&self) -> Vec<SourceCheck> {
        self.node.source_checks()
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
