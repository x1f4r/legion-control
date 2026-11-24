//! The params and results of each method.
//!
//! None of these refuse fields they do not know: the protocol grows by adding optional fields, and
//! an older node has to accept a newer client's call that carries one.

use std::collections::BTreeMap;
use std::fmt;

use serde::de::{self, Deserializer};
use serde::{Deserialize, Serialize, Serializer};
use serde_json::Value;

use crate::{ErrorBody, Id, ToolSpec};

/// `hello` params: who is calling and which protocol it speaks.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct HelloParams {
    /// The client's name, such as `legion` or `legion-control`, for diagnostics.
    pub client: String,
    /// The client's own version, for diagnostics.
    pub version: String,
    /// The protocol the client speaks; a node refuses one it does not.
    pub protocol: u32,
}

/// `hello` result: which node answered and what it runs on.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct HelloResult {
    /// The node's name.
    pub node: String,
    /// The version of legion the node runs.
    pub version: String,
    /// The source revision of legion the node runs, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub build: Option<String>,
    /// The protocol the node speaks.
    pub protocol: u32,
    /// The operating system: `linux`, `macos` or `windows`.
    pub os: String,
    /// The CPU architecture, such as `x86_64` or `aarch64`.
    pub arch: String,
    /// The systems this machine can boot into, as `node.toml` names them; empty until it does.
    pub systems: Vec<String>,
    /// Which system of the machine answered, as its `node.toml` names it; absent until it does.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system: Option<String>,
    /// An identifier of the machine's current boot, which changes on every restart, so a caller
    /// can tell a machine that came back from one that never went down; absent when the node
    /// cannot tell.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub boot_id: Option<String>,
}

/// `tools` params: none yet.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ToolsParams {}

/// `tools` result.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolsResult {
    /// Every tool the node offers, sorted by name.
    pub tools: Vec<ToolSpec>,
}

/// `call` params: which tool, with what, as whom and where.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CallParams {
    /// The tool's name.
    pub tool: String,
    /// The one object the tool takes; absent is `{}`.
    #[serde(default = "empty_object")]
    pub args: Value,
    /// The principal to act as. A door that pinned one ignores a different one here.
    #[serde(rename = "as", default, skip_serializing_if = "Option::is_none")]
    pub as_principal: Option<String>,
    /// The machine or machines to run on; absent is the node that was asked.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub on: Option<Target>,
    /// How many nodes have already forwarded this call; absent is 0.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub hops: u32,
}

fn empty_object() -> Value {
    Value::Object(serde_json::Map::new())
}

fn is_zero(value: &u32) -> bool {
    *value == 0
}

/// `cancel` params.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CancelParams {
    /// The id of the `call` request to cancel.
    pub id: Id,
}

/// `cancel` result, `{}`: cancelling is asked for, and the call's own reply says how it ended.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct CancelResult {}

/// What an `event` notification carries.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventKind {
    /// Output a program wrote to its standard output.
    Stdout,
    /// Output a program wrote to its standard error.
    Stderr,
    /// How far along the call is.
    Progress,
    /// A line a person may want to read.
    Log,
}

impl EventKind {
    /// The kind as the wire spells it.
    pub fn as_str(self) -> &'static str {
        match self {
            EventKind::Stdout => "stdout",
            EventKind::Stderr => "stderr",
            EventKind::Progress => "progress",
            EventKind::Log => "log",
        }
    }
}

/// `event` params: something a running call has to say before it replies.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct EventParams {
    /// The id of the call this belongs to.
    pub id: Id,
    /// What kind of event it is.
    pub kind: EventKind,
    /// Its content.
    pub data: Value,
    /// The machine that produced the event, set on forwarded calls and fan-outs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub machine: Option<String>,
}

/// Where a call runs: one machine, several, or every machine in the fleet.
///
/// On the wire it is a string (`"tower"`, or `"all"`) or a list of strings, so `all` cannot be a
/// machine's name.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Target {
    /// Every machine in the fleet.
    All,
    /// One machine.
    One(String),
    /// Several machines, each answering on its own.
    Many(Vec<String>),
}

impl Target {
    /// Reads a target the way the command line writes one: `all`, a name, or names separated by
    /// commas.
    pub fn parse(text: &str) -> Result<Target, String> {
        if text == "all" {
            return Ok(Target::All);
        }
        let names: Vec<String> = text.split(',').map(|name| name.trim().to_owned()).collect();
        for name in &names {
            check_machine_name(name)?;
        }
        if let [name] = names.as_slice() {
            return Ok(Target::One(name.clone()));
        }
        Ok(Target::Many(names))
    }

    /// The one machine this names, when it names exactly one.
    pub fn single(&self) -> Option<&str> {
        match self {
            Target::One(name) => Some(name),
            Target::Many(names) if names.len() == 1 => names.first().map(String::as_str),
            _ => None,
        }
    }
}

impl fmt::Display for Target {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Target::All => f.write_str("all"),
            Target::One(name) => f.write_str(name),
            Target::Many(names) => f.write_str(&names.join(",")),
        }
    }
}

fn check_machine_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("a machine name cannot be empty".to_owned());
    }
    if name == "all" {
        return Err("`all` means every machine and cannot be listed beside others".to_owned());
    }
    if name
        .chars()
        .any(|c| c.is_whitespace() || c.is_control() || c == ',')
    {
        return Err(format!(
            "{name:?} is not a machine name: names hold no spaces or commas"
        ));
    }
    Ok(())
}

impl Serialize for Target {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Target::All => serializer.serialize_str("all"),
            Target::One(name) => serializer.serialize_str(name),
            Target::Many(names) => names.serialize(serializer),
        }
    }
}

impl<'de> Deserialize<'de> for Target {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Raw {
            One(String),
            Many(Vec<String>),
        }
        let raw = Raw::deserialize(deserializer).map_err(|_| {
            de::Error::custom("`on` must be a machine name, a list of them, or \"all\"")
        })?;
        match raw {
            Raw::One(name) if name == "all" => Ok(Target::All),
            Raw::One(name) => {
                check_machine_name(&name).map_err(de::Error::custom)?;
                Ok(Target::One(name))
            }
            Raw::Many(names) => {
                if names.is_empty() {
                    return Err(de::Error::custom("`on` cannot be an empty list"));
                }
                for name in &names {
                    check_machine_name(name).map_err(de::Error::custom)?;
                }
                Ok(Target::Many(names))
            }
        }
    }
}

/// The result of a call whose `on` named more than one machine: each machine's own outcome, so one
/// failing never hides what the others did.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FanOut {
    /// Each machine's outcome, by machine name.
    pub results: BTreeMap<String, MachineOutcome>,
}

/// How a call went on one machine of a fan-out: `{"ok": …}` or `{"error": …}`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MachineOutcome {
    /// The tool's result.
    Ok(Value),
    /// Why there is none.
    Error(ErrorBody),
}
