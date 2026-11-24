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

mod catalog;
mod compact;
mod events;

use std::io;
use std::sync::{Arc, Mutex, PoisonError};

use legion_core::policy::Policy;
use legion_core::proto::{ErrorBody, ErrorCode, Target};
use legion_core::{Call, Door, EventSink, Grant, Node, Router, VERSION};
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, Implementation,
    JsonObject, ListToolsResult, PaginatedRequestParams, ServerConfig, Tool,
};
use rmcp::service::{RequestContext, ServerInitializeError};
use rmcp::{ErrorData, RoleServer, ServerHandler};
use serde::Serialize;
use serde_json::{Value, json};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio_util::task::TaskTracker;

use self::catalog::{Catalog, ON, summary};
use self::events::{Forwarder, Level};

/// The hint for a tool a compact session cannot find.
const LIST_COMPACT: &str = "legion_tools lists the tools you may use here.";

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

/// One MCP session over a node.
pub struct McpDoor {
    node: Node,
    router: Arc<dyn Router>,
    options: Options,
    catalog: Catalog,
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
        let catalog = Catalog::new(node.specs(), !options.compact)?;
        Ok(McpDoor {
            node,
            router,
            options,
            catalog,
            log_level: Mutex::new(None),
            calls: TaskTracker::new(),
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
        let calls = self.calls.clone();
        let served = match rmcp::serve_server(self, (input, output)).await {
            Ok(running) => running
                .waiting()
                .await
                .map(drop)
                .map_err(|error| io::Error::other(format!("the session stopped: {error}"))),
            // The input ended before a client said anything, which is a session with nothing in it.
            Err(ServerInitializeError::ConnectionClosed(_)) => Ok(()),
            Err(error) => Err(io::Error::other(error.to_string())),
        };
        calls.close();
        calls.wait().await;
        served
    }

    /// What the session's principal may use, or why that cannot be known: a policy file that
    /// does not parse grants nothing, the same as it does for every call the node runs.
    fn grant(&self) -> Result<Grant, ErrorBody> {
        let here = |error: ErrorBody| match error.machine {
            Some(_) => error,
            None => error.with_machine(self.node.name()),
        };
        let policy = self.node.config().get::<Policy>().map_err(here)?;
        let principal = policy
            .resolve(None, self.options.pinned.as_deref())
            .map_err(here)?;
        Ok(policy.grant(&principal, self.node.paths().home()))
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
    fn listed(&self) -> Vec<Tool> {
        let Ok(grant) = self.grant() else {
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
    ) -> Result<Value, ErrorBody> {
        let session_level = *self
            .log_level
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
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
            door: Door::Mcp,
            pinned: self.options.pinned.clone(),
            // Cancelled when the client cancels the request, and when the session ends.
            cancel: context.ct.clone(),
            events,
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
            Err(ErrorBody::new(
                ErrorCode::Internal,
                format!("The call to {tool} stopped before it replied: {error}."),
            ))
        })
    }

    /// A call to one registry tool by its MCP name.
    async fn call_tool_by_name(
        &self,
        name: &str,
        mut args: JsonObject,
        context: &RequestContext<RoleServer>,
    ) -> Result<Value, ErrorBody> {
        let on = match args.remove(ON) {
            None | Some(Value::Null) => None,
            Some(value) => Some(serde_json::from_value::<Target>(value).map_err(|error| {
                self.door_error(ErrorCode::InvalidArgs, format!("on: {error}."))
                    .with_hint("Give on as a machine's name, a list of names, or \"all\", or leave it out to run on this machine.")
            })?),
        };
        let tool = self.catalog.resolve(name);
        self.run(tool, Value::Object(args), on, context)
            .await
            .map_err(|error| match error.code {
                ErrorCode::UnknownTool => error.with_hint(
                    "tools/list shows the tools here. Their names have an underscore where Legion's have a dot, such as meta_whoami for meta.whoami.",
                ),
                _ => error,
            })
    }

    /// A call to one of the three tools compact mode serves.
    async fn call_compact(
        &self,
        name: &str,
        args: JsonObject,
        context: &RequestContext<RoleServer>,
    ) -> Result<Value, ErrorBody> {
        match name {
            compact::TOOLS => {
                let args: compact::ToolsArgs = compact::parse(name, args)?;
                self.list_compact(args.family.as_deref())
            }
            compact::DESCRIBE => {
                let args: compact::DescribeArgs = compact::parse(name, args)?;
                let grant = self.grant()?;
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
            }
            compact::CALL => {
                let args: compact::CallArgs = compact::parse(name, args)?;
                let tool = self.catalog.resolve(&args.tool);
                let tool_args = Value::Object(args.args.unwrap_or_default());
                self.run(tool, tool_args, args.on, context)
                    .await
                    .map_err(|error| match error.code {
                        ErrorCode::UnknownTool => error.with_hint(LIST_COMPACT),
                        _ => error,
                    })
            }
            other => {
                let hint = match self
                    .grant()
                    .ok()
                    .and_then(|grant| self.visible(&grant, other).map(|entry| entry.spec.name.clone()))
                {
                    Some(tool) => format!("Run it with legion_call, giving tool \"{tool}\"."),
                    None => "This session serves legion_tools, legion_describe and legion_call; legion_call runs every other tool.".to_owned(),
                };
                Err(self
                    .door_error(
                        ErrorCode::UnknownTool,
                        format!("There is no tool called {other} in this session."),
                    )
                    .with_hint(hint))
            }
        }
    }

    /// `legion_tools`: the tools the principal may use, optionally of one family.
    fn list_compact(&self, family: Option<&str>) -> Result<Value, ErrorBody> {
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
        let grant = self.grant()?;
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

    /// An error the door itself finds, before any call reaches the node, naming the node as the
    /// node's own errors do.
    fn door_error(&self, code: ErrorCode, message: String) -> ErrorBody {
        ErrorBody::new(code, message).with_machine(self.node.name())
    }
}

/// A tool's outcome as an MCP result: the JSON object as indented text, which every client shows
/// the model, and as structured content for clients that read that instead. A failure is the
/// error object the same two ways, marked as an error.
fn tool_result(outcome: Result<Value, ErrorBody>) -> CallToolResult {
    let (value, failed) = match outcome {
        Ok(value) => (value, false),
        Err(error) => match serde_json::to_value(&error) {
            Ok(value) => (value, true),
            Err(_) => (json!({"code": "internal", "message": error.message}), true),
        },
    };
    let text = serde_json::to_string_pretty(&value).unwrap_or_else(|_| value.to_string());
    let mut result = if failed {
        CallToolResult::error(vec![ContentBlock::text(text)])
    } else {
        CallToolResult::success(vec![ContentBlock::text(text)])
    };
    result.structured_content = Some(value);
    result
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
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        let tools = if self.options.compact {
            compact::tools()
        } else {
            self.listed()
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
