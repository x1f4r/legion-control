//! The MCP door: the node's tools, served to AI agents over the Model Context Protocol.
//!
//! It is built on the official SDK, which speaks both revisions in use: the one that opens with an
//! `initialize` handshake, and the newer one where every request carries its own `_meta` and a
//! client may first ask `server/discover`. The door itself does not care which one a client
//! speaks; the SDK shapes each reply for the revision of the request it answers.
//!
//! In full mode every registry tool is an MCP tool of its own, named with an underscore for the
//! dot (`fs.read` is `fs_read`) and taking one more optional argument, `on`, the machine or
//! machines to run on. In compact mode three tools list, describe and call the others. Either way
//! a failed call is a tool result with `isError` set and the error object as its text, so an agent
//! reads the code and the hint instead of a protocol error it cannot see.

pub(crate) mod catalog;
mod compact;
mod events;

use std::io;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use legion_core::proto::{ErrorBody, ErrorCode, Target};
use legion_core::{
    CANCEL_GRACE, Call, CancellationToken, Door, EventSink, Grant, Node, RouteOutcome, Router,
    VERSION,
};
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, Implementation,
    JsonObject, ListToolsResult, PaginatedRequestParams, ServerConfig, Tool,
};
use rmcp::service::{RequestContext, ServerInitializeError};
use rmcp::{ErrorData, RoleServer, ServerHandler};
use serde::Serialize;
use serde_json::{Value, json};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::Semaphore;
use tokio_util::task::TaskTracker;

use crate::Authenticated;

use self::catalog::{Catalog, ON, summary};
use self::events::{Forwarder, Level};

/// The hint for a tool a compact session cannot find.
const LIST_COMPACT: &str = "legion_tools lists the tools you may use here.";

/// Bounded wait for in-flight calls to stop and audit when the door is signalled to shut down.
const SHUTDOWN_GRACE: Duration = CANCEL_GRACE.saturating_add(Duration::from_secs(4));

/// How a session is served.
#[derive(Clone, Debug, Default)]
pub struct Options {
    /// The principal every call in the session acts as, as `legion mcp --as` pins it. A pinned
    /// session cannot be talked into acting as anyone else.
    pub pinned: Option<String>,
    /// Serve `legion_tools`, `legion_describe` and `legion_call` instead of one tool per registry
    /// tool, so an agent carries three schemas rather than every one.
    pub compact: bool,
}

/// What a door serving MCP over HTTP shares with it: how it learns each request's principal, and
/// the server-wide limits every call is held to.
#[derive(Clone)]
pub(crate) struct HttpLink {
    /// Cancelled when the server shuts down, which cancels every call still running.
    pub(crate) shutdown: CancellationToken,
    /// One permit per call allowed to run at once.
    pub(crate) limit: Arc<Semaphore>,
}

/// One MCP session over a node.
pub struct McpDoor {
    node: Node,
    router: Arc<dyn Router>,
    options: Options,
    catalog: Arc<Catalog>,
    /// Set when the door serves HTTP, where each request carries the principal its token stands
    /// for, rather than one principal for the whole session.
    http: Option<HttpLink>,
    /// The level `logging/setLevel` asked for, if a client of the older revision asked.
    log_level: Mutex<Option<Level>>,
    /// Every call still running, so the session can wait for calls it cancels as it ends to reach
    /// the audit log.
    calls: TaskTracker,
}

impl McpDoor {
    /// A door serving `node`'s tools, running each call through `router`.
    ///
    /// Fails in full mode when the registry cannot be served as it is: two tool names that become
    /// the same MCP name, or a tool with an argument of its own called `on`.
    pub fn new(
        node: Node,
        router: Arc<dyn Router>,
        options: Options,
    ) -> Result<McpDoor, ErrorBody> {
        let catalog = Arc::new(Catalog::new(node.specs(), !options.compact)?);
        Ok(McpDoor {
            node,
            router,
            options,
            catalog,
            http: None,
            log_level: Mutex::new(None),
            calls: TaskTracker::new(),
        })
    }

    /// A door for one HTTP session or request, sharing the catalog built once for the server and
    /// the server's call tracker, so shutting the server down waits for every call to be recorded.
    pub(crate) fn for_http(
        node: Node,
        router: Arc<dyn Router>,
        compact: bool,
        catalog: Arc<Catalog>,
        link: HttpLink,
        calls: TaskTracker,
    ) -> McpDoor {
        McpDoor {
            node,
            router,
            options: Options {
                pinned: None,
                compact,
            },
            catalog,
            http: Some(link),
            log_level: Mutex::new(None),
            calls,
        }
    }

    /// The catalog a door in this mode serves, for a server to build once and share.
    pub(crate) fn catalog(node: &Node, compact: bool) -> Result<Arc<Catalog>, ErrorBody> {
        Catalog::new(node.specs(), !compact).map(Arc::new)
    }

    /// The principal a request acts as: the one the session was pinned to, or over HTTP the one
    /// the request's token stands for. An HTTP request that somehow arrives without one is refused
    /// rather than served as the policy's default.
    fn pinned(&self, context: &RequestContext<RoleServer>) -> Result<Option<String>, ErrorBody> {
        if self.http.is_none() {
            return Ok(self.options.pinned.clone());
        }
        context
            .extensions
            .get::<axum::http::request::Parts>()
            .and_then(|parts| parts.extensions.get::<Authenticated>())
            .map(|authenticated| Some(authenticated.0.clone()))
            .ok_or_else(|| {
                self.door_error(
                    ErrorCode::Denied,
                    "The request carries no principal.".to_owned(),
                )
            })
    }

    /// Serves one session until `input` ends, then waits for the calls the end cancelled to finish
    /// and be recorded.
    ///
    /// Nothing but MCP messages is written to `output`: the SDK logs through `tracing`, which goes
    /// nowhere unless a program installs a subscriber, and the node reports trouble on stderr.
    pub async fn serve<R, W>(self, input: R, output: W) -> io::Result<()>
    where
        R: AsyncRead + Send + Unpin + 'static,
        W: AsyncWrite + Send + Unpin + 'static,
    {
        self.serve_until(input, output, CancellationToken::new())
            .await
    }

    /// Serves one session until `input` ends or `shutdown` is cancelled, then cancels running
    /// calls, waits within a bounded grace for them to stop and audit, and shuts down the router.
    pub async fn serve_until<R, W>(
        self,
        input: R,
        output: W,
        shutdown: CancellationToken,
    ) -> io::Result<()>
    where
        R: AsyncRead + Send + Unpin + 'static,
        W: AsyncWrite + Send + Unpin + 'static,
    {
        let calls = self.calls.clone();
        let router = Arc::clone(&self.router);
        let served = tokio::select! {
            biased;
            () = shutdown.cancelled() => Ok(()),
            started = rmcp::serve_server(self, (input, output)) => match started {
                Ok(running) => {
                    let cancel = running.cancellation_token();
                    let waiting = running.waiting();
                    tokio::pin!(waiting);
                    tokio::select! {
                        biased;
                        result = &mut waiting => result
                            .map(drop)
                            .map_err(|error| io::Error::other(format!("the session stopped: {error}"))),
                        () = shutdown.cancelled() => {
                            cancel.cancel();
                            let _ = tokio::time::timeout(SHUTDOWN_GRACE, &mut waiting).await;
                            Ok(())
                        }
                    }
                }
                // The input ended before a client said anything, which is a session with nothing in it.
                Err(ServerInitializeError::ConnectionClosed(_)) => Ok(()),
                Err(error) => Err(io::Error::other(error.to_string())),
            },
        };
        calls.close();
        if shutdown.is_cancelled() {
            let _ = tokio::time::timeout(SHUTDOWN_GRACE, calls.wait()).await;
        } else {
            calls.wait().await;
        }
        router.shutdown().await;
        served
    }

    /// What the session's principal may use, or why that cannot be known: a policy file that
    /// does not parse grants nothing, the same as it does for every call the node runs.
    fn grant(&self, context: &RequestContext<RoleServer>) -> Result<Grant, ErrorBody> {
        crate::grant(&self.node, self.pinned(context)?.as_deref())
    }

    /// The tool a client names, when the principal may use it. A tool beyond its role is treated
    /// exactly like one that does not exist, so a session learns nothing about what it may not
    /// use.
    fn visible<'a>(&'a self, grant: &Grant, name: &str) -> Option<&'a catalog::Entry> {
        self.catalog
            .get(name)
            .filter(|entry| grant.allows(&entry.spec))
    }

    /// The tools listed in full mode: those the principal may use, and none while the policy
    /// cannot be read. Listing is a courtesy that keeps an agent from trying what will be refused;
    /// the node checks every call regardless, and a call is how an agent learns what is wrong with
    /// the policy.
    fn listed(&self, context: &RequestContext<RoleServer>) -> Vec<Tool> {
        let Ok(grant) = self.grant(context) else {
            return Vec::new();
        };
        self.catalog
            .entries()
            .iter()
            .filter(|entry| grant.allows(&entry.spec))
            .map(|entry| entry.tool.clone())
            .collect()
    }

    fn instructions(&self) -> String {
        let (whoami, on) = if self.options.compact {
            (
                "legion_tools lists the tools, legion_describe gives one tool's arguments, and legion_call runs one. Call legion_call with tool meta.whoami first",
                "legion_call takes an optional `on`",
            )
        } else {
            (
                "Call meta_whoami first",
                "Every tool takes an optional `on`",
            )
        };
        format!(
            "Legion gives you the tools of the machine it runs on, and through `on` those of the other machines it can reach. {whoami}: it says who you act as here, your role, the directories file tools are confined to, and the tools you may use. {on}: a machine's name, a list of names, or \"all\"; leave it out to act on this machine. A failed call returns a JSON error object with a `code` to branch on and, usually, a `hint` saying what to do."
        )
    }

    /// Runs one call through the router, forwarding its events while it runs.
    async fn run(
        &self,
        tool: &str,
        args: Value,
        on: Option<Target>,
        context: &RequestContext<RoleServer>,
    ) -> RouteOutcome {
        let session_level = *self
            .log_level
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let pinned = match self.pinned(context) {
            Ok(pinned) => pinned,
            Err(error) => return RouteOutcome::One(Err(error)),
        };
        // Over HTTP every call also ends when the server shuts down, and waits for a permit.
        let (cancel, _permit, _linked) = match &self.http {
            None => (context.ct.clone(), None, None),
            Some(link) => {
                let Ok(permit) = Arc::clone(&link.limit).try_acquire_owned() else {
                    return RouteOutcome::One(Err(crate::too_many_calls(&self.node)));
                };
                let cancel = link.shutdown.child_token();
                let request = context.ct.clone();
                let linked = cancel.clone();
                tokio::spawn(async move {
                    tokio::select! {
                        () = request.cancelled() => linked.cancel(),
                        () = linked.cancelled() => {}
                    }
                });
                // Ends the watcher once the call is over, however it ended.
                let guard = cancel.clone().drop_guard();
                (cancel, Some(permit), Some(guard))
            }
        };
        let forwarder = Forwarder::new(context, tool, session_level);
        let (events, queue) = match &forwarder {
            Some(_) => {
                let (sink, queue) = Forwarder::sink();
                (sink, Some(queue))
            }
            None => (EventSink::discard(), None),
        };
        let call = Call {
            tool: tool.to_owned(),
            args,
            as_principal: None,
            door: if self.http.is_some() {
                Door::Http
            } else {
                Door::Mcp
            },
            pinned,
            // Cancelled when the client cancels the request, and when the session ends.
            cancel,
            events,
            hops: 0,
        };
        let node = self.node.clone();
        let router = Arc::clone(&self.router);
        let running = self
            .calls
            .spawn(async move { router.call(&node, call, on).await });
        let joined = match (forwarder, queue) {
            (Some(forwarder), Some(queue)) => forwarder.forward_during(queue, running).await,
            _ => running.await,
        };
        joined.unwrap_or_else(|error| {
            RouteOutcome::One(Err(ErrorBody::new(
                ErrorCode::Internal,
                format!("The call to {tool} stopped before it replied: {error}."),
            )))
        })
    }

    /// A call to one registry tool by its MCP name.
    async fn call_tool_by_name(
        &self,
        name: &str,
        mut args: JsonObject,
        context: &RequestContext<RoleServer>,
    ) -> RouteOutcome {
        let on = match self.on(args.remove(ON)) {
            Ok(on) => on,
            Err(error) => return RouteOutcome::One(Err(error)),
        };
        let tool = self.catalog.resolve(name);
        self.rewrite_outcome(self.run(tool, Value::Object(args), on, context).await, tool)
    }

    /// A call to one of the three tools compact mode serves.
    async fn call_compact(
        &self,
        name: &str,
        args: JsonObject,
        context: &RequestContext<RoleServer>,
    ) -> RouteOutcome {
        match name {
            compact::TOOLS => RouteOutcome::One(
                compact::parse::<compact::ToolsArgs>(name, args)
                    .and_then(|args| self.list_compact(args.family.as_deref(), context)),
            ),
            compact::DESCRIBE => RouteOutcome::One(
                compact::parse::<compact::DescribeArgs>(name, args).and_then(|args| {
                    let grant = self.grant(context)?;
                    match self.visible(&grant, &args.tool) {
                        Some(entry) => to_object(&entry.spec),
                        None => Err(self
                            .door_error(
                                ErrorCode::UnknownTool,
                                format!(
                                    "There is no tool called {} on {}.",
                                    args.tool,
                                    self.node.name()
                                ),
                            )
                            .with_hint(LIST_COMPACT)),
                    }
                }),
            ),
            compact::CALL => match compact::parse::<compact::CallArgs>(name, args) {
                Ok(args) => {
                    let tool = self.catalog.resolve(&args.tool);
                    let tool_args = Value::Object(args.args.unwrap_or_default());
                    let on = match self.on(args.on) {
                        Ok(on) => on,
                        Err(error) => return RouteOutcome::One(Err(error)),
                    };
                    self.rewrite_outcome(self.run(tool, tool_args, on, context).await, tool)
                }
                Err(error) => RouteOutcome::One(Err(error)),
            },
            other => {
                let hint = match self
                    .grant(context)
                    .ok()
                    .and_then(|grant| self.visible(&grant, other).map(|entry| entry.spec.name.clone()))
                {
                    Some(tool) => format!("Run it with legion_call, giving tool \"{tool}\"."),
                    None => "This session serves legion_tools, legion_describe and legion_call; legion_call runs every other tool.".to_owned(),
                };
                RouteOutcome::One(Err(self
                    .door_error(
                        ErrorCode::UnknownTool,
                        format!("There is no tool called {other} in this session."),
                    )
                    .with_hint(hint)))
            }
        }
    }

    /// `legion_tools`: the tools the principal may use, optionally of one family.
    fn list_compact(
        &self,
        family: Option<&str>,
        context: &RequestContext<RoleServer>,
    ) -> Result<Value, ErrorBody> {
        #[derive(Serialize)]
        struct Listing<'a> {
            tools: Vec<Listed<'a>>,
        }
        #[derive(Serialize)]
        struct Listed<'a> {
            name: &'a str,
            effect: &'a str,
            description: &'a str,
        }
        let grant = self.grant(context)?;
        let allowed: Vec<&catalog::Entry> = self
            .catalog
            .entries()
            .iter()
            .filter(|entry| grant.allows(&entry.spec))
            .collect();
        let chosen: Vec<Listed<'_>> = allowed
            .iter()
            .filter(|entry| {
                family.is_none_or(|family| {
                    compact::in_family(&entry.spec.name, compact::family(family))
                })
            })
            .map(|entry| Listed {
                name: &entry.spec.name,
                effect: entry.spec.effect.as_str(),
                description: summary(&entry.spec.description),
            })
            .collect();
        if let Some(family) = family
            && chosen.is_empty()
        {
            if allowed.is_empty() {
                return Err(self
                    .door_error(
                        ErrorCode::Denied,
                        format!(
                            "{} may use no tools on {}.",
                            grant.principal(),
                            self.node.name()
                        ),
                    )
                    .with_hint(
                        "The owner of this machine decides in policy.toml which tools each principal may use.",
                    ));
            }
            let mut families: Vec<&str> = allowed
                .iter()
                .filter_map(|entry| entry.spec.name.split('.').next())
                .collect();
            families.dedup();
            return Err(self
                .door_error(
                    ErrorCode::NotFound,
                    format!("You may use no tools of a family called {family} here."),
                )
                .with_hint(format!(
                    "The families you may use here are {}. Leave out family to list every tool.",
                    families.join(", ")
                )));
        }
        to_object(&Listing { tools: chosen })
    }

    /// A call's error with any hint written for the command line rewritten to point at what an
    /// MCP client has: `tools/list` and the tool's schema there, or `legion_tools` and
    /// `legion_describe` in compact mode.
    fn for_client(&self, error: ErrorBody, tool: &str) -> ErrorBody {
        let hint = error.hint.as_deref().unwrap_or_default();
        let compact = self.options.compact;
        if error.code == ErrorCode::UnknownTool || hint.contains("`legion tools`") {
            error.with_hint(if compact {
                LIST_COMPACT
            } else {
                "tools/list shows the tools you may use here. Their names have an underscore where Legion's have a dot, such as meta_whoami for meta.whoami."
            })
        } else if hint.contains("`legion help ") {
            error.with_hint(if compact {
                format!("legion_describe with tool \"{tool}\" shows the arguments it takes.")
            } else {
                format!(
                    "The input schema of {} in tools/list shows the arguments it takes.",
                    catalog::mcp_name(tool)
                )
            })
        } else {
            error
        }
    }

    /// Applies [`Self::for_client`] to every error inside a routed outcome.
    fn rewrite_outcome(&self, outcome: RouteOutcome, tool: &str) -> RouteOutcome {
        match outcome {
            RouteOutcome::One(result) => {
                RouteOutcome::One(result.map_err(|error| self.for_client(error, tool)))
            }
            RouteOutcome::Many(results) => RouteOutcome::Many(
                results
                    .into_iter()
                    .map(|(machine, result)| {
                        (
                            machine,
                            result.map_err(|error| self.for_client(error, tool)),
                        )
                    })
                    .collect(),
            ),
        }
    }

    /// The machines an `on` argument names.
    fn on(&self, on: Option<Value>) -> Result<Option<Target>, ErrorBody> {
        catalog::target(on.unwrap_or(Value::Null)).map_err(|error| {
            self.door_error(ErrorCode::InvalidArgs, format!("on: {error}."))
                .with_hint("Give on as a machine's name, several names separated by commas, or \"all\", or leave it out to run on this machine.")
        })
    }

    /// An error the door itself finds, before any call reaches the node, naming the node as the
    /// node's own errors do.
    fn door_error(&self, code: ErrorCode, message: String) -> ErrorBody {
        ErrorBody::new(code, message).with_machine(self.node.name())
    }
}

/// A tool's outcome as an MCP result: the JSON object as one line of text, which every client
/// shows the model. It is not sent as structured content as well: no tool declares an output
/// schema to check that against, and a client that passes on both would cost the model every
/// byte twice. A failure (or a fan-out where every machine failed) is marked as an error.
fn tool_result(outcome: RouteOutcome) -> CallToolResult {
    let failed = outcome.all_failed();
    let text = match outcome.into_wire() {
        Ok(value) => value.to_string(),
        Err(error) => serde_json::to_string(&error)
            .unwrap_or_else(|_| json!({"code": "internal", "message": error.message}).to_string()),
    };
    if failed {
        CallToolResult::error(vec![ContentBlock::text(text)])
    } else {
        CallToolResult::success(vec![ContentBlock::text(text)])
    }
}

fn to_object<T: Serialize>(value: &T) -> Result<Value, ErrorBody> {
    serde_json::to_value(value).map_err(|error| {
        ErrorBody::new(
            ErrorCode::Internal,
            format!("The reply cannot be written as JSON: {error}."),
        )
    })
}

impl ServerHandler for McpDoor {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(events::capabilities())
            .with_server_info(Implementation::new("legion", VERSION).with_title("Legion"))
            .with_instructions(self.instructions())
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        let tools = if self.options.compact {
            compact::tools()
        } else {
            self.listed(&context)
        };
        Ok(ListToolsResult::with_all_items(tools))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        let args = request.arguments.unwrap_or_default();
        let outcome = if self.options.compact {
            self.call_compact(&request.name, args, &context).await
        } else {
            self.call_tool_by_name(&request.name, args, &context).await
        };
        Ok(tool_result(outcome).into())
    }

    #[allow(
        deprecated,
        reason = "logging/setLevel is deprecated in the newest MCP revision, and still sent by the older one"
    )]
    async fn set_level(
        &self,
        request: rmcp::model::SetLevelRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<(), ErrorData> {
        *self
            .log_level
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(Level::of(request.level));
        Ok(())
    }
}
