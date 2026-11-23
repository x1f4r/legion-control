//! JSON-RPC 2.0 envelopes, one per line of a byte stream.

use std::fmt;

use serde::de::{self, Deserializer};
use serde::{Deserialize, Serialize, Serializer};
use serde_json::Value;

use crate::ErrorBody;

/// The method names a node answers, and the one notification it sends.
pub mod method {
    /// Introduces the client and agrees on the protocol.
    pub const HELLO: &str = "hello";
    /// Lists the tools the node offers.
    pub const TOOLS: &str = "tools";
    /// Runs one tool.
    pub const CALL: &str = "call";
    /// Cancels a call that is still running.
    pub const CANCEL: &str = "cancel";
    /// Sent by the node while a call runs: output, progress, log lines.
    pub const EVENT: &str = "event";
}

/// The line was not JSON.
pub const PARSE_ERROR: i64 = -32700;
/// The line was JSON but not a JSON-RPC request.
pub const INVALID_REQUEST: i64 = -32600;
/// The node does not answer that method.
pub const METHOD_NOT_FOUND: i64 = -32601;
/// The method's params did not have the shape it takes.
pub const INVALID_PARAMS: i64 = -32602;
/// The node failed in a way that is its own fault.
pub const INTERNAL_ERROR: i64 = -32603;
/// A tool call failed; `data.code` says how. JSON-RPC leaves -32000 to -32099 to servers, and one
/// number for every tool failure keeps agents branching on `data.code`, which every door shares.
pub const TOOL_ERROR: i64 = -32000;

/// The `"jsonrpc": "2.0"` every message carries. Anything else is refused when read, so a peer
/// speaking another protocol is noticed on its first line.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Version;

impl Serialize for Version {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str("2.0")
    }
}

impl<'de> Deserialize<'de> for Version {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        if text == "2.0" {
            Ok(Version)
        } else {
            Err(de::Error::custom(format!(
                "jsonrpc must be \"2.0\", not {text:?}"
            )))
        }
    }
}

/// A request id. JSON-RPC allows numbers and strings; fractions are allowed there too but are
/// refused here, since an id that does not round-trip exactly cannot be matched to its reply.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Id {
    /// A whole number.
    Number(i64),
    /// A string.
    String(String),
}

impl fmt::Display for Id {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Id::Number(number) => write!(f, "{number}"),
            Id::String(text) => write!(f, "{text:?}"),
        }
    }
}

impl From<i64> for Id {
    fn from(number: i64) -> Self {
        Id::Number(number)
    }
}

impl From<&str> for Id {
    fn from(text: &str) -> Self {
        Id::String(text.to_owned())
    }
}

/// A request, or a notification when it has no id.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Request {
    /// Always `"2.0"`.
    pub jsonrpc: Version,
    /// Absent for a notification, which gets no reply.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<Id>,
    /// One of [`method`].
    pub method: String,
    /// The method's params; absent is the same as `{}`.
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub params: Value,
}

impl Request {
    /// A request expecting a reply.
    pub fn new(id: impl Into<Id>, method: &str, params: Value) -> Self {
        Request {
            jsonrpc: Version,
            id: Some(id.into()),
            method: method.to_owned(),
            params,
        }
    }
}

/// A notification: a message that gets no reply, such as an `event` while a call runs.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Notification {
    /// Always `"2.0"`.
    pub jsonrpc: Version,
    /// What kind of notification it is.
    pub method: String,
    /// Its content.
    pub params: Value,
}

impl Notification {
    /// A notification with the given method and params.
    pub fn new(method: &str, params: Value) -> Self {
        Notification {
            jsonrpc: Version,
            method: method.to_owned(),
            params,
        }
    }
}

/// The reply to a request: its result or its error, never both.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Response {
    /// Always `"2.0"`.
    pub jsonrpc: Version,
    /// The id of the request this answers; `null` when the request was too broken to have one.
    pub id: Option<Id>,
    /// What the request came to.
    #[serde(flatten)]
    pub outcome: Outcome,
}

/// Either half of a [`Response`], written as its `result` or its `error` member.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Outcome {
    /// The method's result.
    Result(Value),
    /// Why there is none.
    Error(RpcError),
}

impl Response {
    /// A successful reply.
    pub fn ok(id: Option<Id>, result: Value) -> Self {
        Response {
            jsonrpc: Version,
            id,
            outcome: Outcome::Result(result),
        }
    }

    /// A failed reply.
    pub fn error(id: Option<Id>, error: RpcError) -> Self {
        Response {
            jsonrpc: Version,
            id,
            outcome: Outcome::Error(error),
        }
    }
}

/// A JSON-RPC error. `data` is always Legion's [`ErrorBody`], even for the protocol's own errors,
/// so a client reads one shape whatever failed.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RpcError {
    /// One of the constants in this module.
    pub code: i64,
    /// The same sentence as `data.message`, for clients that only read JSON-RPC.
    pub message: String,
    /// What Legion has to say about it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<ErrorBody>,
}

impl RpcError {
    /// Wraps an [`ErrorBody`] under a JSON-RPC code, copying its message up.
    pub fn new(code: i64, body: ErrorBody) -> Self {
        RpcError {
            code,
            message: body.message.clone(),
            data: Some(body),
        }
    }
}
