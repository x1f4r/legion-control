//! Where a call runs. The fleet milestone replaces [`Local`] with a router that reaches other
//! machines; the command line only ever sees the [`Router`] trait.

use legion_core::proto::{ErrorBody, ErrorCode, Target};
use legion_core::{BoxFuture, Call, Node};
use serde_json::Value;

/// Runs a call on the machine or machines its `on` names.
pub trait Router: Send + Sync + 'static {
    /// Runs `call` where `on` says: this node when `on` is absent.
    fn call<'a>(
        &'a self,
        node: &'a Node,
        call: Call,
        on: Option<Target>,
    ) -> BoxFuture<'a, Result<Value, ErrorBody>>;
}

/// Runs calls on this node, and refuses any other machine as unreachable, since this build cannot
/// reach one.
pub struct Local;

impl Router for Local {
    fn call<'a>(
        &'a self,
        node: &'a Node,
        call: Call,
        on: Option<Target>,
    ) -> BoxFuture<'a, Result<Value, ErrorBody>> {
        Box::pin(async move {
            let here = node.name();
            match on {
                None => node.call(call).await,
                Some(target) if target.single() == Some(here.as_str()) => node.call(call).await,
                Some(target) => Err(ErrorBody::new(
                    ErrorCode::Unreachable,
                    format!("{target} cannot be reached from {here}: this build of legion runs calls only on the node it runs on."),
                )
                .with_hint(format!(
                    "Routing calls to other machines arrives in a later milestone. Until then, leave out --on, or name this node ({here})."
                ))
                .with_machine(here)),
            }
        })
    }
}
