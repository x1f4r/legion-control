//! The internal seam between Tauri commands and the Legion client implementation.
//!
//! Stage one provides [`Unpaired`], which reports honest unpaired responses. Stage two supplies a
//! real implementation backed by `legion-link` without changing the command surface.

use std::fmt;
use std::future::Future;
use std::pin::Pin;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tauri::ipc::Channel;

/// Boxed future returned by [`Backend`] methods so the trait stays object-safe without macro crates.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Structured error codes returned across the Tauri IPC boundary.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    /// The app is not paired with any node yet.
    NotPaired,
    /// One or more arguments were invalid.
    InvalidArgs,
    /// Permission or policy denied the request.
    Denied,
    /// The target node cannot be reached.
    Unreachable,
    /// The operation was cancelled by the caller or user.
    Cancelled,
    /// This platform or build does not support the operation.
    Unsupported,
    /// The operation failed after being attempted.
    Failed,
    /// An internal error inside the native shell.
    Internal,
}

/// Structured error object returned by every Tauri command.
///
/// Errors always serialize as `{ code, message, hint }`, never as bare strings.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppError {
    /// Machine-readable error code.
    pub code: ErrorCode,
    /// Human-readable one-sentence summary of what happened.
    pub message: String,
    /// Optional actionable next step, always present in JSON as a string or `null`.
    pub hint: Option<String>,
}

impl AppError {
    /// Creates an error with no hint.
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            hint: None,
        }
    }

    /// Attaches an actionable hint for the caller.
    #[must_use]
    pub fn with_hint(mut self, hint: impl Into<String>) -> Self {
        self.hint = Some(hint.into());
        self
    }

    /// Standard error returned when an operation requires a paired node.
    pub fn not_paired() -> Self {
        Self::new(
            ErrorCode::NotPaired,
            "This device is not paired with any Legion node yet.",
        )
        .with_hint(
            "Run `legion pair --as phone` on one of your machines and scan or paste the invite.",
        )
    }
}

impl fmt::Display for AppError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}: {}", self.code, self.message)?;
        if let Some(hint) = &self.hint {
            write!(f, " ({hint})")?;
        }
        Ok(())
    }
}

impl std::error::Error for AppError {}

/// What the app still needs before it can run tools.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Needs {
    /// At least one node must be paired before commands can run.
    Pairing,
}

/// Summary of one node link known to the app.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeLink {
    /// Name of the paired or local node.
    pub node: String,
    /// Principal this controller acts as on that node.
    pub principal: String,
    /// Role granted to that principal.
    pub role: String,
    /// Transport used to reach the node (for example `ssh` or `local`).
    pub via: String,
}

/// Snapshot returned by the `state` command.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppState {
    /// True once at least one node link is ready to accept calls.
    pub ready: bool,
    /// Known node links.
    pub nodes: Vec<NodeLink>,
    /// What setup step is still required, or `null` when ready.
    pub needs: Option<Needs>,
}

/// Identifier of an in-flight call for cancellation and streaming events.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum CallId {
    /// String call identifier.
    Text(String),
    /// Numeric call identifier.
    Number(i64),
}

/// Streamed event delivered through a [`tauri::ipc::Channel`] during `call`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CallEvent {
    /// Identifier of the call producing the event.
    pub id: CallId,
    /// Event kind (`stdout`, `stderr`, `progress`, or `log`).
    pub kind: String,
    /// Event payload.
    pub data: Value,
    /// Machine that produced the event, when forwarded or fanned out.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub machine: Option<String>,
}

/// Parameters for `call`.
#[derive(Clone, Debug)]
pub struct CallRequest {
    /// Tool name.
    pub tool: String,
    /// Tool arguments object.
    pub args: Value,
    /// Optional target machine, list of machines, or `"all"`.
    pub on: Option<Value>,
}

/// Result of `pair`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PairResult {
    /// The newly paired node link.
    pub node: NodeLink,
}

/// Result of `unpair`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnpairResult {
    /// Name of the node that was unpaired.
    pub node: String,
}

/// Result of `cancel`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CancelResult {}

/// Backend interface behind the Tauri command surface.
pub trait Backend: Send + Sync {
    /// Returns whether the controller is ready, its known nodes, and whether pairing is needed.
    fn state(&self) -> BoxFuture<'_, Result<AppState, AppError>>;

    /// Lists tool specifications offered on `on` (or the default node when `None`).
    fn tools(&self, on: Option<String>) -> BoxFuture<'_, Result<Vec<Value>, AppError>>;

    /// Invokes a tool, streaming incremental events to `channel`.
    fn call(
        &self,
        request: CallRequest,
        channel: Channel<CallEvent>,
    ) -> BoxFuture<'_, Result<Value, AppError>>;

    /// Requests cancellation of an in-flight call.
    fn cancel(&self, id: CallId) -> BoxFuture<'_, Result<CancelResult, AppError>>;

    /// Enrols this device with a node using a pairing invite.
    fn pair(&self, invite: String, name: String) -> BoxFuture<'_, Result<PairResult, AppError>>;

    /// Removes a paired node from this device.
    fn unpair(&self, node: String) -> BoxFuture<'_, Result<UnpairResult, AppError>>;
}

/// Stage-one backend implementation used before `legion-link` is plugged in.
#[derive(Clone, Copy, Debug, Default)]
pub struct Unpaired;

impl Backend for Unpaired {
    fn state(&self) -> BoxFuture<'_, Result<AppState, AppError>> {
        Box::pin(async {
            Ok(AppState {
                ready: false,
                nodes: Vec::new(),
                needs: Some(Needs::Pairing),
            })
        })
    }

    fn tools(&self, _on: Option<String>) -> BoxFuture<'_, Result<Vec<Value>, AppError>> {
        Box::pin(async { Err(AppError::not_paired()) })
    }

    fn call(
        &self,
        _request: CallRequest,
        _channel: Channel<CallEvent>,
    ) -> BoxFuture<'_, Result<Value, AppError>> {
        Box::pin(async { Err(AppError::not_paired()) })
    }

    fn cancel(&self, _id: CallId) -> BoxFuture<'_, Result<CancelResult, AppError>> {
        Box::pin(async { Err(AppError::not_paired()) })
    }

    fn pair(&self, invite: String, _name: String) -> BoxFuture<'_, Result<PairResult, AppError>> {
        Box::pin(async move {
            if invite.trim().is_empty() {
                return Err(
                    AppError::new(ErrorCode::InvalidArgs, "Invite text cannot be empty.")
                        .with_hint(
                            "Scan the QR code or paste the invite line printed by `legion pair`.",
                        ),
                );
            }
            Err(AppError::new(
                ErrorCode::Unsupported,
                "Pairing is not connected yet in this shell build.",
            )
            .with_hint("The Legion client link is not wired into this preview build yet."))
        })
    }

    fn unpair(&self, _node: String) -> BoxFuture<'_, Result<UnpairResult, AppError>> {
        Box::pin(async { Err(AppError::not_paired()) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_serializes_as_object_with_code_message_hint() -> Result<(), Box<dyn std::error::Error>>
    {
        let err = AppError::not_paired();
        let json = serde_json::to_value(&err)?;
        assert_eq!(json["code"], "not_paired");
        assert!(json["message"].is_string());
        assert!(json["hint"].is_string());

        let bare = AppError::new(ErrorCode::Failed, "Something failed.");
        let bare_json = serde_json::to_value(&bare)?;
        assert_eq!(bare_json["code"], "failed");
        assert_eq!(bare_json["message"], "Something failed.");
        assert!(bare_json["hint"].is_null());
        Ok(())
    }

    #[test]
    fn state_serializes_needs_pairing_and_null() -> Result<(), Box<dyn std::error::Error>> {
        let unpaired = AppState {
            ready: false,
            nodes: Vec::new(),
            needs: Some(Needs::Pairing),
        };
        let json = serde_json::to_value(&unpaired)?;
        assert_eq!(json["ready"], false);
        assert_eq!(json["nodes"], serde_json::json!([]));
        assert_eq!(json["needs"], "pairing");

        let ready = AppState {
            ready: true,
            nodes: vec![NodeLink {
                node: "tower".to_owned(),
                principal: "phone".to_owned(),
                role: "operator".to_owned(),
                via: "ssh".to_owned(),
            }],
            needs: None,
        };
        let ready_json = serde_json::to_value(&ready)?;
        assert_eq!(ready_json["ready"], true);
        assert!(ready_json["needs"].is_null());
        Ok(())
    }
}
