//! The doors programs come in through, as libraries over a [`Node`](legion_core::Node) and a
//! [`Router`](legion_core::Router).
//!
//! A door only translates: it turns what a client sends into a [`Call`](legion_core::Call), hands
//! it to the router, and turns the answer back into what the client expects. Policy, auditing and
//! the tools themselves stay in the node, so every door enforces the same rules. The `legion`
//! binary wires a door to stdin and stdout or to a socket and nothing more, which keeps each door
//! testable over an in-memory pipe.

pub mod http;
pub mod mcp;

use legion_core::policy::Policy;
use legion_core::proto::{ErrorBody, ErrorCode};
use legion_core::{Grant, Node};

/// Who an HTTP request's token says it is, put in the request's extensions once the token is
/// verified. Every door behind the HTTP listener takes the principal from here and nowhere else.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Authenticated {
    /// The principal the token stands for.
    pub(crate) principal: String,
    /// The token's name in `tokens.toml`.
    pub(crate) token: String,
    /// The token's stored hash, which tells a token apart from a later one given the same name.
    pub(crate) hash: String,
    /// When the token stops working, if it does.
    pub(crate) expires: Option<jiff::Timestamp>,
}

/// What a caller pinned to `pinned` (or the policy's default when nothing pinned it) may use, or
/// why that cannot be known. A policy file that does not parse grants nothing, the same as it does
/// for every call the node runs, so every door that lists tools fails closed through here.
pub(crate) fn grant(node: &Node, pinned: Option<&str>) -> Result<Grant, ErrorBody> {
    let here = |error: ErrorBody| match error.machine {
        Some(_) => error,
        None => error.with_machine(node.name()),
    };
    let policy = node.config().get::<Policy>().map_err(here)?;
    let principal = policy.resolve(None, pinned).map_err(here)?;
    Ok(policy.grant(&principal, node.paths()))
}

/// The error for a call refused because the server already runs as many as it allows.
pub(crate) fn too_many_calls(node: &Node) -> ErrorBody {
    ErrorBody::new(
        ErrorCode::Busy,
        format!(
            "{} is already running as many calls as it takes at once.",
            node.name()
        ),
    )
    .with_hint("Wait for a call to finish, then try again.")
    .with_machine(node.name())
}
