//! A Legion node: the tool registry, the context a tool runs in, policy, the audit log,
//! configuration and the directories it all lives in.
//!
//! Every door hands its calls to [`Node::call`], which decides who the call acts as, checks the
//! policy, runs the tool and records the call. A tool family adds its tools to a [`Registry`] with
//! [`Registry::add_fn`]; a tool reads what it needs from its [`Ctx`].

pub mod audit;
pub mod config;
mod ctx;
pub mod exe;
mod glob;
pub mod media;
mod node;
pub mod paths;
pub mod policy;
mod registry;
mod route;
pub mod schema;
pub mod tokens;
mod tool;

pub use config::{ConfigStore, Document};
pub use ctx::{Ctx, Door, Event, EventSink};
pub use exe::{current_exe, record_current_exe, validate_recorded_exe};
pub use node::{BUILD, CANCEL_GRACE, Call, Node, VERSION, format_version_line};
pub use paths::Paths;
pub use policy::{Access, CreateKind, Grant, Place, Policy, ProtectedFilter, SECRET_LOCATIONS};
pub use registry::{DocumentInfo, Registry, SourceCheck, ToolSource, is_tool_name};
pub use route::{Local, MachineEffects, RouteOutcome, Router};
pub use tool::{BoxFuture, Handler, NoArgs, Tool, ToolError, input_schema, parse_args};

/// The wire types, so a tool family needs only this crate.
pub use legion_proto as proto;
pub use tokio_util::sync::CancellationToken;
