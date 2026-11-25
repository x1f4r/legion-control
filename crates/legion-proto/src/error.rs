use std::fmt;
use std::str::FromStr;

use serde::de::{self, Deserializer};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// What went wrong, from a set closed on purpose: an agent branches on it, so every door uses the
/// same thirteen words and a new failure picks the nearest one rather than inventing another.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    /// The arguments do not fit the tool's schema, or a value in them makes no sense.
    InvalidArgs,
    /// No tool has that name on the node that was asked.
    UnknownTool,
    /// The thing the call names does not exist.
    NotFound,
    /// The thing the call would create is already there.
    Exists,
    /// The thing changed since the caller last looked, so the call would overwrite someone.
    Conflict,
    /// Policy does not let this principal do this.
    Denied,
    /// The machine the call is for cannot be reached.
    Unreachable,
    /// The call took longer than it was allowed to.
    Timeout,
    /// The caller cancelled the call before it finished.
    Cancelled,
    /// The machine is busy and the call would interrupt what keeps it busy.
    Busy,
    /// This node, platform or protocol cannot do what was asked.
    Unsupported,
    /// The operation was attempted and did not succeed.
    Failed,
    /// Legion itself went wrong; the message is for whoever maintains it.
    Internal,
}

impl ErrorCode {
    /// Every code, in the order the design lists them.
    pub const ALL: [ErrorCode; 13] = [
        ErrorCode::InvalidArgs,
        ErrorCode::UnknownTool,
        ErrorCode::NotFound,
        ErrorCode::Exists,
        ErrorCode::Conflict,
        ErrorCode::Denied,
        ErrorCode::Unreachable,
        ErrorCode::Timeout,
        ErrorCode::Cancelled,
        ErrorCode::Busy,
        ErrorCode::Unsupported,
        ErrorCode::Failed,
        ErrorCode::Internal,
    ];

    /// The code as it is written on the wire, in the audit log and in policy.
    pub fn as_str(self) -> &'static str {
        match self {
            ErrorCode::InvalidArgs => "invalid_args",
            ErrorCode::UnknownTool => "unknown_tool",
            ErrorCode::NotFound => "not_found",
            ErrorCode::Exists => "exists",
            ErrorCode::Conflict => "conflict",
            ErrorCode::Denied => "denied",
            ErrorCode::Unreachable => "unreachable",
            ErrorCode::Timeout => "timeout",
            ErrorCode::Cancelled => "cancelled",
            ErrorCode::Busy => "busy",
            ErrorCode::Unsupported => "unsupported",
            ErrorCode::Failed => "failed",
            ErrorCode::Internal => "internal",
        }
    }
}

impl fmt::Display for ErrorCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for ErrorCode {
    type Err = String;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        ErrorCode::ALL
            .into_iter()
            .find(|code| code.as_str() == text)
            .ok_or_else(|| format!("`{text}` is not an error code"))
    }
}

/// The error object every door returns: in the `data` of a JSON-RPC error, as the text of a failed
/// MCP result, and on stdout of a failed CLI call that is not on a terminal.
///
/// `message` says what happened in one sentence and `hint`, when there is something to do, says
/// what. Both are written for someone who has never read the source.
///
/// Deserialization is forward-tolerant: an unrecognized `code` string from a newer peer decodes as
/// [`ErrorCode::Failed`] while preserving the original string under `details.unknown_code`.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ErrorBody {
    /// The code an agent branches on.
    pub code: ErrorCode,
    /// What happened, in one sentence.
    pub message: String,
    /// What to do about it, when there is something to do.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
    /// The machine the error happened on, so a fan-out or a forwarded call says where.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub machine: Option<String>,
    /// Anything structured a caller may want to act on, such as the line a file failed to parse.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<Value>,
}

impl<'de> Deserialize<'de> for ErrorBody {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        struct RawErrorBody {
            code: String,
            message: String,
            #[serde(default)]
            hint: Option<String>,
            #[serde(default)]
            machine: Option<String>,
            #[serde(default)]
            details: Option<Value>,
        }

        let raw = RawErrorBody::deserialize(deserializer)?;
        let (code, details) = match raw.code.parse::<ErrorCode>() {
            Ok(known) => (known, raw.details),
            Err(_) => {
                if raw.code.is_empty() {
                    return Err(de::Error::custom("error code cannot be empty"));
                }
                let merged = match raw.details {
                    None => {
                        let mut map = Map::new();
                        map.insert("unknown_code".to_owned(), Value::String(raw.code));
                        Value::Object(map)
                    }
                    Some(Value::Object(mut map)) => {
                        map.entry("unknown_code".to_owned())
                            .or_insert_with(|| Value::String(raw.code));
                        Value::Object(map)
                    }
                    Some(other) => {
                        let mut map = Map::new();
                        map.insert("unknown_code".to_owned(), Value::String(raw.code));
                        map.insert("original_details".to_owned(), other);
                        Value::Object(map)
                    }
                };
                (ErrorCode::Failed, Some(merged))
            }
        };
        Ok(ErrorBody {
            code,
            message: raw.message,
            hint: raw.hint,
            machine: raw.machine,
            details,
        })
    }
}

impl ErrorBody {
    /// An error with a code and a message and nothing else; the `with_` methods add the rest.
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        ErrorBody {
            code,
            message: message.into(),
            hint: None,
            machine: None,
            details: None,
        }
    }

    /// Adds what the reader can do about it.
    #[must_use]
    pub fn with_hint(mut self, hint: impl Into<String>) -> Self {
        self.hint = Some(hint.into());
        self
    }

    /// Names the machine the error happened on.
    #[must_use]
    pub fn with_machine(mut self, machine: impl Into<String>) -> Self {
        self.machine = Some(machine.into());
        self
    }

    /// Attaches structured detail.
    #[must_use]
    pub fn with_details(mut self, details: Value) -> Self {
        self.details = Some(details);
        self
    }
}

impl fmt::Display for ErrorBody {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.code, self.message)?;
        if let Some(hint) = &self.hint {
            write!(f, " ({hint})")?;
        }
        Ok(())
    }
}

impl std::error::Error for ErrorBody {}
