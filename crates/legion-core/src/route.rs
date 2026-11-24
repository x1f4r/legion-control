//! Where a call runs. Every door hands its calls to a [`Router`], so the fleet milestone replaces
//! [`Local`] with a router that reaches other machines for all of them at once.

use std::collections::BTreeMap;

use legion_proto::{ErrorBody, ErrorCode, FanOut, MachineOutcome, Target};
use serde_json::Value;

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

/// Runs a call on the machine or machines its `on` names.
pub trait Router: Send + Sync + 'static {
    /// Runs `call` where `on` says: this node when `on` is absent.
    fn call<'a>(
        &'a self,
        node: &'a Node,
        call: Call,
        on: Option<Target>,
    ) -> BoxFuture<'a, RouteOutcome>;

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
