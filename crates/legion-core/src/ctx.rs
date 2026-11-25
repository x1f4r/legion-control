//! What a tool knows about the call it is running for.

use std::fmt;

pub use legion_proto::{Event, EventSink};
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
    /// `legion gate --as` and `legion gate --enrol`, calls arriving through a restricted SSH
    /// forced command (paired devices and hand-written restricted SSH keys).
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

/// The context a tool runs in: which node, for whom, through which door, what it is allowed, and
/// how to notice it was cancelled or tell the caller how it is getting on.
pub struct Ctx {
    pub(crate) node: Node,
    pub(crate) name: String,
    pub(crate) door: Door,
    pub(crate) device: Option<String>,
    pub(crate) pairing_id: Option<String>,
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

    /// Whether this node's name is explicitly set in `node.toml` (`Some(true)`), host-derived
    /// (`Some(false)`), or unknown because `node.toml` failed to parse (`None`).
    pub fn node_name_explicit(&self) -> Option<bool> {
        self.node.name_explicit()
    }

    /// Persistent identifier for this node's state directory, or `None` if unavailable.
    pub fn node_id(&self) -> Option<String> {
        self.node.node_id()
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

    /// The paired device name when the call arrived through `legion gate --as ... --device <name>`.
    pub fn device(&self) -> Option<&str> {
        self.device.as_deref()
    }

    /// The paired device's 32-hex pairing identifier when the call arrived through
    /// `legion gate --as ... --pairing <id>`.
    pub fn pairing_id(&self) -> Option<&str> {
        self.pairing_id.as_deref()
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
