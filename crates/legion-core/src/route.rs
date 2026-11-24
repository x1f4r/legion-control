//! Where a call runs. Every door hands its calls to a [`Router`], so the fleet milestone replaces
//! [`Local`] with a router that reaches other machines for all of them at once.

use std::collections::BTreeMap;

use legion_proto::{Effect, ErrorBody, ErrorCode, FanOut, MachineOutcome, Target};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::node::{Call, Node};
use crate::tool::BoxFuture;

/// The result of a routed call, preserving whether it targeted one machine or fanned out to
/// several so doors can render fan-outs and choose exit/error status without guessing from JSON.
#[derive(Clone, Debug, PartialEq)]
pub enum RouteOutcome {
    /// A call targeting one machine (or this node).
    One(Result<Value, ErrorBody>),
    /// A fan-out across several machines or `all`, keyed by machine name.
    Many(BTreeMap<String, Result<Value, ErrorBody>>),
}

impl RouteOutcome {
    /// Converts this outcome into the wire shape the design specifies: the tool's own object or
    /// error for one machine, and `{"results": {"<machine>": {"ok": …} | {"error": …}}}` for a
    /// fan-out.
    pub fn into_wire(self) -> Result<Value, ErrorBody> {
        match self {
            RouteOutcome::One(result) => result,
            RouteOutcome::Many(results) => {
                let wire = FanOut {
                    results: results
                        .into_iter()
                        .map(|(machine, outcome)| {
                            let outcome = match outcome {
                                Ok(value) => MachineOutcome::Ok(value),
                                Err(error) => MachineOutcome::Error(error),
                            };
                            (machine, outcome)
                        })
                        .collect(),
                };
                serde_json::to_value(wire).map_err(|error| {
                    ErrorBody::new(
                        ErrorCode::Internal,
                        format!("Fan-out result could not be written as JSON: {error}."),
                    )
                })
            }
        }
    }

    /// Whether every machine targeted by the call failed.
    ///
    /// For one machine, this is `true` on `Err`. For a fan-out, this is `true` when at least one
    /// machine was called and every one of them returned `Err` (partial success is `false`, and an
    /// empty fan-out with no machines is `false`).
    pub fn all_failed(&self) -> bool {
        match self {
            RouteOutcome::One(result) => result.is_err(),
            RouteOutcome::Many(results) => {
                !results.is_empty() && results.values().all(Result::is_err)
            }
        }
    }
}

/// Each machine a call would run on, with the effect its tool has there, or `None` where that
/// cannot be established.
pub type MachineEffects = Vec<(String, Option<Effect>)>;

/// Runs a call on the machine or machines its `on` names.
pub trait Router: Send + Sync + 'static {
    /// Runs `call` where `on` says: this node when `on` is absent.
    fn call<'a>(
        &'a self,
        node: &'a Node,
        call: Call,
        on: Option<Target>,
    ) -> BoxFuture<'a, RouteOutcome>;

    /// The effect `tool` has on each machine a call sent where `on` says would run on, learned
    /// the way this router learns it to authorise the call: from this node's spec when the call
    /// runs here or the tool is built in, and otherwise from what the target reports. An effect
    /// is `None` where it cannot be established, such as for a tool that exists only on a
    /// machine this router cannot reach or asks through a relay. Nothing runs.
    ///
    /// Fails with the error the call itself would get when the tool is not on this node and the
    /// call runs here, or when a target cannot be resolved or reached.
    fn effects<'a>(
        &'a self,
        node: &'a Node,
        tool: &'a str,
        on: Option<&'a Target>,
        _cancel: CancellationToken,
    ) -> BoxFuture<'a, Result<MachineEffects, ErrorBody>> {
        Box::pin(async move {
            let here = node.name();
            match on {
                Some(target) if target.single() != Some(here.as_str()) => Ok(vec![(
                    target.to_string(),
                    node.registry().get(tool).map(|tool| tool.spec().effect),
                )]),
                _ => Ok(vec![(here, Some(node.spec(tool)?.effect))]),
            }
        })
    }

    /// Closes any open remote sessions cleanly (EOF, short wait, then kill) before the process
    /// exits.
    fn shutdown(&self) -> BoxFuture<'_, ()> {
        Box::pin(async {})
    }
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
    ) -> BoxFuture<'a, RouteOutcome> {
        Box::pin(async move {
            let here = node.name();
            RouteOutcome::One(match on {
                None => node.call(call).await,
                Some(target) if target.single() == Some(here.as_str()) => node.call(call).await,
                Some(target) => Err(ErrorBody::new(
                    ErrorCode::Unreachable,
                    format!("{target} cannot be reached from {here}: this build of legion runs calls only on the node it runs on."),
                )
                .with_hint(format!(
                    "Routing calls to other machines arrives in a later milestone. Until then, leave out on (--on on the command line), or name this node ({here})."
                ))
                .with_machine(here)),
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn route_outcome_wire_shape_and_all_failed() {
        let ok = RouteOutcome::One(Ok(json!({"v": 1})));
        assert!(!ok.all_failed());
        assert_eq!(ok.into_wire(), Ok(json!({"v": 1})));

        let err = RouteOutcome::One(Err(ErrorBody::new(ErrorCode::Denied, "No.")));
        assert!(err.all_failed());
        assert_eq!(err.into_wire().unwrap_err().code, ErrorCode::Denied);

        let empty = RouteOutcome::Many(BTreeMap::new());
        assert!(!empty.all_failed());
        assert_eq!(empty.into_wire(), Ok(json!({"results": {}})));

        let mut partial = BTreeMap::new();
        partial.insert("pi".to_owned(), Ok(json!({"ok": 1})));
        partial.insert(
            "tower".to_owned(),
            Err(ErrorBody::new(ErrorCode::Unreachable, "Down.")),
        );
        let partial = RouteOutcome::Many(partial);
        assert!(!partial.all_failed());
        assert_eq!(
            partial.into_wire(),
            Ok(json!({
                "results": {
                    "pi": {"ok": {"ok": 1}},
                    "tower": {"error": {"code": "unreachable", "message": "Down."}}
                }
            }))
        );

        let mut failed = BTreeMap::new();
        failed.insert(
            "pi".to_owned(),
            Err(ErrorBody::new(ErrorCode::Unreachable, "Down.")),
        );
        failed.insert(
            "tower".to_owned(),
            Err(ErrorBody::new(ErrorCode::NotFound, "Unknown.")),
        );
        let failed = RouteOutcome::Many(failed);
        assert!(failed.all_failed());
    }
}
