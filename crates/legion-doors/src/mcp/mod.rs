//! The MCP door: the node's tools, served to AI agents over the Model Context Protocol.
//!
//! It is built on the official SDK, which speaks both revisions in use: the one that opens with an
//! `initialize` handshake, and the newer one where every request carries its own `_meta` and a
//! client may first ask `server/discover`. The door itself does not care which one a client
//! speaks; the SDK shapes each reply for the revision of the request it answers.
//!
//! Every registry tool is an MCP tool of its own, named with an underscore for the dot (`fs.read`
//! is `fs_read`) and taking one more optional argument, `on`, the machine or machines to run on. A
//! failed call is a tool result with `isError` set and the error object as its text, so an agent
//! reads the code and the hint instead of a protocol error it cannot see.

mod catalog;

use std::io;
use std::sync::Arc;

use legion_core::policy::Policy;
use legion_core::proto::{ErrorBody, ErrorCode, Target};
use legion_core::{Call, Door, EventSink, Grant, Node, Router, VERSION};
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, Implementation,
    JsonObject, ListToolsResult, PaginatedRequestParams, ServerCapabilities, ServerConfig, Tool,
};
use rmcp::service::{RequestContext, ServerInitializeError};
use rmcp::{ErrorData, RoleServer, ServerHandler};
use serde_json::{Value, json};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio_util::task::TaskTracker;

use self::catalog::{Catalog, ON};

/// How a session is served.
#[derive(Clone, Debug, Default)]
pub struct Options {
    /// The principal every call in the session acts as, as `legion mcp --as` pins it. A pinned
    /// session cannot be talked into acting as anyone else.
    pub pinned: Option<String>,
}

/// One MCP session over a node.
pub struct McpDoor {
    node: Node,
    router: Arc<dyn Router>,
    options: Options,
    catalog: Catalog,
    /// Every call still running, so the session can wait for calls it cancels as it ends to reach
    /// the audit log.
    calls: TaskTracker,
}

impl McpDoor {
    /// A door serving `node`'s tools, running each call through `router`.
    ///
    /// Fails when the registry cannot be served as it is: two tool names that become the same MCP
    /// name, or a tool with an argument of its own called `on`.
    pub fn new(
        node: Node,
        router: Arc<dyn Router>,
        options: Options,
    ) -> Result<McpDoor, ErrorBody> {
        let catalog = Catalog::new(node.specs())?;
        Ok(McpDoor {
            node,
            router,
            options,
            catalog,
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

    /// What the session's principal may use, or `None` when the policy cannot be read: then every
    /// tool is listed, and calling one says what is wrong with the file.
    fn grant(&self) -> Option<Grant> {
        let policy = self.node.config().get::<Policy>().ok()?;
        let principal = policy.resolve(None, self.options.pinned.as_deref()).ok()?;
        Some(policy.grant(&principal, self.node.paths().home()))
    }

    /// The tools listed: those the principal may use. Listing is a courtesy that keeps an agent
    /// from trying what will be refused; the node checks every call regardless.
    fn listed(&self) -> Vec<Tool> {
        let grant = self.grant();
        self.catalog
            .entries()
            .iter()
            .filter(|entry| grant.as_ref().is_none_or(|grant| grant.allows(&entry.spec)))
            .map(|entry| entry.tool.clone())
            .collect()
    }

    fn instructions(&self) -> String {
        "Legion gives you the tools of the machine it runs on, and through `on` those of the other machines it can reach. Call meta_whoami first: it says who you act as here, your role, the directories file tools are confined to, and the tools you may use. Every tool takes an optional `on`: a machine's name, a list of names, or \"all\"; leave it out to act on this machine. A failed call returns a JSON error object with a `code` to branch on and, usually, a `hint` saying what to do.".to_owned()
    }

    /// Runs one call through the router.
    async fn run(
        &self,
        tool: &str,
        args: Value,
        on: Option<Target>,
        context: &RequestContext<RoleServer>,
    ) -> Result<Value, ErrorBody> {
        let call = Call {
            tool: tool.to_owned(),
            args,
            as_principal: None,
            door: Door::Mcp,
            pinned: self.options.pinned.clone(),
            // Cancelled when the client cancels the request, and when the session ends.
            cancel: context.ct.clone(),
            events: EventSink::discard(),
        };
        let node = self.node.clone();
        let router = Arc::clone(&self.router);
        let running = self
            .calls
            .spawn(async move { router.call(&node, call, on).await });
        running.await.unwrap_or_else(|error| {
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

impl ServerHandler for McpDoor {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("legion", VERSION).with_title("Legion"))
            .with_instructions(self.instructions())
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        Ok(ListToolsResult::with_all_items(self.listed()))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        let args = request.arguments.unwrap_or_default();
        let outcome = self.call_tool_by_name(&request.name, args, &context).await;
        Ok(tool_result(outcome).into())
    }
}
