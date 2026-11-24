//! The node: the one place a call is resolved, authorised, run and audited, whichever door it came
//! through.

use std::sync::Arc;
use std::time::{Duration, Instant};

use legion_proto::{ErrorBody, ErrorCode, HelloParams, HelloResult, PROTOCOL, ToolSpec};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::audit::{self, AuditLog, Entry, Outcome};
use crate::config::{ConfigStore, NodeFile, NodeName};
use crate::ctx::{Ctx, Door, EventSink};
use crate::paths::Paths;
use crate::policy::Policy;
use crate::registry::Registry;

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

/// One call, as a door hands it to [`Node::call`].
#[derive(Debug)]
pub struct Call {
    /// The tool's name.
    pub tool: String,
    /// The tool's arguments, as the caller sent them.
    pub args: Value,
    /// The principal the caller asked to act as (`--as`, or `as` in the request).
    pub as_principal: Option<String>,
    /// The door it came through.
    pub door: Door,
    /// The principal the door pinned, when it pinned one: `legion rpc --as`, an ssh key forced to
    /// `legion gate --as`, an HTTP token.
    pub pinned: Option<String>,
    /// Cancelled by the door when the caller cancels.
    pub cancel: CancellationToken,
    /// Where the call's events go.
    pub events: EventSink,
}

impl Call {
    /// A call acting as whoever the door and the policy say, never cancelled, whose events are
    /// dropped; set the other fields to change that.
    pub fn new(door: Door, tool: impl Into<String>, args: Value) -> Call {
        Call {
            tool: tool.into(),
            args,
            as_principal: None,
            door,
            pinned: None,
            cancel: CancellationToken::new(),
            events: EventSink::discard(),
        }
    }
}

/// A running node. Cloning it is cheap and every clone is the same node, so each concurrent call
/// can hold its own.
#[derive(Clone, Debug)]
pub struct Node {
    inner: Arc<Inner>,
}

#[derive(Debug)]
struct Inner {
    paths: Paths,
    registry: Registry,
    config: ConfigStore,
    audit: AuditLog,
    host_name: NodeName,
}

impl Node {
    /// A node over these directories offering these tools.
    ///
    /// The state directory is created now if it can be, so the first call has somewhere to write
    /// its audit line; if it cannot, every call says so on stderr and `meta.doctor` says why.
    pub fn new(paths: Paths, registry: Registry) -> Node {
        let _ = std::fs::create_dir_all(paths.state_dir());
        Node {
            inner: Arc::new(Inner {
                config: ConfigStore::new(paths.config_dir()),
                audit: AuditLog::new(&paths),
                host_name: NodeName::from_host(),
                paths,
                registry,
            }),
        }
    }

    /// Where the node keeps its files.
    pub fn paths(&self) -> &Paths {
        &self.inner.paths
    }

    /// The tools it offers.
    pub fn registry(&self) -> &Registry {
        &self.inner.registry
    }

    /// Its configuration files.
    pub fn config(&self) -> &ConfigStore {
        &self.inner.config
    }

    /// Its audit log.
    pub fn audit_log(&self) -> &AuditLog {
        &self.inner.audit
    }

    /// The node's name: the one `node.toml` gives, or the host's.
    ///
    /// A `node.toml` that does not parse fails the calls that read its other settings, and
    /// `meta.doctor` reports it; the name falls back to the host's meanwhile, because every call
    /// needs a name to record and a wrong one grants nothing.
    pub fn name(&self) -> String {
        match self.inner.config.get::<NodeFile>() {
            Ok(file) => file
                .name
                .as_ref()
                .map_or_else(|| self.inner.host_name.to_string(), NodeName::to_string),
            Err(_) => self.inner.host_name.to_string(),
        }
    }

    /// Every tool's spec, sorted by name.
    pub fn specs(&self) -> Vec<ToolSpec> {
        self.inner.registry.specs()
    }

    /// One tool's spec, or the `unknown_tool` error a call to it would get.
    pub fn spec(&self, tool: &str) -> Result<ToolSpec, ErrorBody> {
        let name = self.name();
        match self.inner.registry.get(tool) {
            Some(found) => Ok(found.spec().clone()),
            None => Err(self.unknown_tool(tool, &name).with_machine(name)),
        }
    }

    /// Answers a `hello`, refusing a client that speaks another protocol.
    pub fn hello(&self, params: &HelloParams) -> Result<HelloResult, ErrorBody> {
        if params.protocol != PROTOCOL {
            return Err(ErrorBody::new(
                ErrorCode::Unsupported,
                format!(
                    "This node speaks protocol {PROTOCOL}, and {} {} asked for protocol {}.",
                    params.client, params.version, params.protocol
                ),
            )
            .with_hint("Update legion on whichever side is older, so both speak the same protocol.")
            .with_machine(self.name())
            .with_details(serde_json::json!({"supported": [PROTOCOL]})));
        }
        Ok(HelloResult {
            node: self.name(),
            version: VERSION.to_owned(),
            protocol: PROTOCOL,
            os: std::env::consts::OS.to_owned(),
            arch: std::env::consts::ARCH.to_owned(),
            systems: Vec::new(),
        })
    }

    /// Runs one call on this node: decides who it acts as, checks that the policy allows it, runs
    /// the tool, and appends the call to the audit log, whatever came of it.
    ///
    /// `policy.toml` is read for every call, so an edit applies to the next one. A file that does
    /// not parse fails the call; nothing falls back to a default that could allow more.
    ///
    /// Cancelling `call.cancel` cancels the tool's context, and the call ends with what the tool
    /// returns within [`CANCEL_GRACE`], or as `cancelled` when it has not returned by then.
    pub async fn call(&self, call: Call) -> Result<Value, ErrorBody> {
        let started = Instant::now();
        let name = self.name();
        let recorded_args = audit::redact(&call.args);
        let tool = call.tool.clone();
        let door = call.door;

        let (principal, result) = self.run(&name, call).await;
        let result = result.map_err(|error| match error.machine {
            Some(_) => error,
            None => error.with_machine(name.clone()),
        });

        let entry = Entry {
            time: audit::now(),
            principal,
            door,
            tool,
            target: name,
            outcome: match &result {
                Ok(_) => Outcome::Ok,
                Err(error) => Outcome::Error(error.code),
            },
            duration: (started.elapsed().as_secs_f64() * 1e6).round() / 1e6,
            args: recorded_args,
            message: result
                .as_ref()
                .err()
                .map(|error| audit::redact_message(&error.message)),
        };
        let log = self.inner.audit.clone();
        let written = tokio::task::spawn_blocking(move || log.append(&entry)).await;
        // stderr is for people; a call whose audit line is lost still happened, so it is reported
        // there rather than turned into a failure of the call.
        let problem = match written {
            Ok(Ok(())) => None,
            Ok(Err(error)) => Some(format!(
                "legion: the call ran, but it could not be written to the audit log {}: {error}\n",
                self.inner.audit.path().display()
            )),
            Err(error) => Some(format!("legion: writing the audit log failed: {error}\n")),
        };
        if let Some(problem) = problem {
            let _ = std::io::Write::write_all(&mut std::io::stderr(), problem.as_bytes());
        }
        result
    }

    /// Everything [`Node::call`] does but the audit: the principal, once known, and the outcome.
    async fn run(&self, name: &str, call: Call) -> (Option<String>, Result<Value, ErrorBody>) {
        let policy = match self.inner.config.get::<Policy>() {
            Ok(policy) => policy,
            Err(error) => return (call.pinned, Err(error)),
        };
        let principal = match policy.resolve(call.as_principal.as_deref(), call.pinned.as_deref()) {
            Ok(principal) => principal,
            // Recorded as the caller it is, not as whoever it asked to be.
            Err(error) => {
                let caller = call
                    .pinned
                    .unwrap_or_else(|| policy.default_principal().to_owned());
                return (Some(caller), Err(error));
            }
        };
        let Some(tool) = self.inner.registry.get(&call.tool) else {
            return (Some(principal), Err(self.unknown_tool(&call.tool, name)));
        };
        let grant = match policy.authorize(&principal, tool.spec(), &self.inner.paths) {
            Ok(grant) => grant,
            Err(error) => return (Some(principal), Err(error)),
        };
        if call.cancel.is_cancelled() {
            return (Some(principal), Err(cancelled(&call.tool)));
        }

        let ctx = Ctx {
            node: self.clone(),
            name: name.to_owned(),
            door: call.door,
            grant,
            cancel: call.cancel.clone(),
            events: call.events,
        };
        let work = tool.call(&ctx, call.args);
        tokio::pin!(work);
        let result = tokio::select! {
            result = &mut work => result,
            () = call.cancel.cancelled() => {
                // The tool sees the same token through its context. A result it had ready when the
                // cancel came, or returns after cleaning up, is the call's result.
                match tokio::time::timeout(CANCEL_GRACE, &mut work).await {
                    Ok(result) => result,
                    Err(_) => Err(cancelled(&call.tool)),
                }
            }
        };
        (Some(principal), result)
    }

    fn unknown_tool(&self, tool: &str, name: &str) -> ErrorBody {
        let family = tool.split('.').next().unwrap_or_default();
        let siblings: Vec<&str> = self
            .inner
            .registry
            .names()
            .filter(|known| known.split('.').next() == Some(family))
            .collect();
        let hint = if siblings.is_empty() {
            "`legion tools` lists the tools there are.".to_owned()
        } else {
            format!("The {family} tools here are {}.", siblings.join(", "))
        };
        ErrorBody::new(
            ErrorCode::UnknownTool,
            format!("There is no tool called {tool} on {name}."),
        )
        .with_hint(hint)
    }
}

fn cancelled(tool: &str) -> ErrorBody {
    ErrorBody::new(
        ErrorCode::Cancelled,
        format!("The call to {tool} was cancelled before it finished."),
    )
}
