//! The internal seam between Tauri commands and the Legion client implementation.
//!
//! [`Backend`] is implemented by [`crate::link_backend::LinkBackend`] over `legion-link`, and by
//! [`Unpaired`] for lightweight tests.

use std::fmt;
use std::future::Future;
use std::pin::Pin;

use legion_link::proto::{ErrorBody as ProtoErrorBody, ErrorCode as ProtoErrorCode};
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
    /// No tool has that name on the node that was asked.
    UnknownTool,
    /// The named resource does not exist.
    NotFound,
    /// The resource already exists.
    Exists,
    /// Concurrent modification or duplicate call identifier.
    Conflict,
    /// Permission, authentication, or policy denied the request.
    Denied,
    /// The target node cannot be reached.
    Unreachable,
    /// The operation timed out.
    Timeout,
    /// The operation was cancelled by the caller or user.
    Cancelled,
    /// The target machine is busy.
    Busy,
    /// This platform or build does not support the operation.
    Unsupported,
    /// The operation failed after being attempted.
    Failed,
    /// An internal error inside the native shell.
    Internal,
}

impl From<ProtoErrorCode> for ErrorCode {
    fn from(code: ProtoErrorCode) -> Self {
        match code {
            ProtoErrorCode::InvalidArgs => Self::InvalidArgs,
            ProtoErrorCode::UnknownTool => Self::UnknownTool,
            ProtoErrorCode::NotFound => Self::NotFound,
            ProtoErrorCode::Exists => Self::Exists,
            ProtoErrorCode::Conflict => Self::Conflict,
            ProtoErrorCode::Denied => Self::Denied,
            ProtoErrorCode::Unreachable => Self::Unreachable,
            ProtoErrorCode::Timeout => Self::Timeout,
            ProtoErrorCode::Cancelled => Self::Cancelled,
            ProtoErrorCode::Busy => Self::Busy,
            ProtoErrorCode::Unsupported => Self::Unsupported,
            ProtoErrorCode::Failed => Self::Failed,
            ProtoErrorCode::Internal => Self::Internal,
        }
    }
}

/// Structured error object returned by every Tauri command.
///
/// Errors always serialize as `{ code, message, hint, ... }`, never as bare strings.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AppError {
    /// Machine-readable error code.
    pub code: ErrorCode,
    /// Human-readable one-sentence summary of what happened.
    pub message: String,
    /// Optional actionable next step, always present in JSON as a string or `null`.
    pub hint: Option<String>,
    /// Machine the error came from, when applicable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub machine: Option<String>,
    /// Structured error details from the node or SSH transport (e.g. `reason: "auth_failed"`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<Value>,
}

impl AppError {
    /// Creates an error with no hint.
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            hint: None,
            machine: None,
            details: None,
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

impl From<ProtoErrorBody> for AppError {
    fn from(body: ProtoErrorBody) -> Self {
        Self {
            code: body.code.into(),
            message: body.message,
            hint: body.hint,
            machine: body.machine,
            details: body.details,
        }
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
    /// The persisted state file could not be loaded and must be moved aside explicitly.
    Repair,
}

/// Plain-language details about a damaged or unsupported persisted state file.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepairInfo {
    /// Plain explanation of why the state file could not be loaded.
    pub message: String,
    /// Full path of the unreadable or unsupported state file.
    pub path: String,
}

/// Result of moving a damaged state file aside and starting fresh.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepairResult {
    /// Full path where the damaged state file was moved (`legion-app.json.damaged-<timestamp>`).
    pub archived_to: String,
    /// Plain reminder naming where the file went and that paired machines still list this device.
    pub message: String,
    /// Fresh application state after starting clean.
    pub state: AppState,
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
    /// Transport used to reach the node (`ssh` or `local`).
    pub via: String,
}

/// Snapshot returned by the `state` command.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppState {
    /// True once at least one node link is ready to accept calls.
    pub ready: bool,
    /// Known node links, with the active node first.
    pub nodes: Vec<NodeLink>,
    /// What setup step is still required, or `null` when ready.
    pub needs: Option<Needs>,
    /// Populated when `needs == Some(Needs::Repair)`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repair: Option<RepairInfo>,
}

/// Identifier of an in-flight call for cancellation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum CallId {
    /// String call identifier.
    Text(String),
    /// Numeric call identifier.
    Number(i64),
}

impl CallId {
    /// Converts the call identifier to its canonical string key.
    pub fn as_key(&self) -> String {
        match self {
            Self::Text(s) => s.clone(),
            Self::Number(n) => n.to_string(),
        }
    }
}

/// Parameters for `call`.
#[derive(Clone, Debug)]
pub struct CallRequest {
    /// Optional caller-chosen call identifier used by `cancel`.
    pub id: Option<CallId>,
    /// Tool name.
    pub tool: String,
    /// Tool arguments object.
    pub args: Value,
    /// Optional target machine, list of machines, or `"all"`.
    pub on: Option<Value>,
}

/// Result of `unpair`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnpairResult {
    /// Name of the node that was unpaired locally.
    pub node: String,
    /// Device name recorded when this device was paired with the node.
    pub device: String,
    /// Plain reminder that the node keeps the device until revoked there.
    pub message: String,
}

/// Result of `cancel`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CancelResult {}

/// Backend interface behind the Tauri command surface.
pub trait Backend: Send + Sync {
    /// Returns whether the controller is ready, its known nodes, and whether pairing is needed.
    fn state(&self) -> BoxFuture<'_, Result<AppState, AppError>>;

    /// Lists tool specifications offered on `on` (or the default node when `None`).
    fn tools(&self, on: Option<Value>) -> BoxFuture<'_, Result<Vec<Value>, AppError>>;

    /// Invokes a tool, streaming live events to `on_event` when supplied.
    fn call(
        &self,
        request: CallRequest,
        on_event: Option<Channel<Value>>,
    ) -> BoxFuture<'_, Result<Value, AppError>>;

    /// Requests cancellation of an in-flight call.
    fn cancel(&self, id: CallId) -> BoxFuture<'_, Result<CancelResult, AppError>>;

    /// Enrols this device with a node using a pairing invite and returns the updated state.
    fn pair(&self, invite: String, name: String) -> BoxFuture<'_, Result<AppState, AppError>>;

    /// Removes a paired node from this device's local state.
    fn unpair(&self, node: String) -> BoxFuture<'_, Result<UnpairResult, AppError>>;

    /// Selects which configured node calls go through by default and returns the updated state.
    fn select_node(&self, node: String) -> BoxFuture<'_, Result<AppState, AppError>>;

    /// Moves a damaged or unsupported state file aside and starts fresh.
    fn repair_store(&self) -> BoxFuture<'_, Result<RepairResult, AppError>>;
}

/// Fallback backend returning honest unpaired responses.
#[derive(Clone, Copy, Debug, Default)]
pub struct Unpaired;

impl Backend for Unpaired {
    fn state(&self) -> BoxFuture<'_, Result<AppState, AppError>> {
        Box::pin(async {
            Ok(AppState {
                ready: false,
                nodes: Vec::new(),
                needs: Some(Needs::Pairing),
                repair: None,
            })
        })
    }

    fn tools(&self, _on: Option<Value>) -> BoxFuture<'_, Result<Vec<Value>, AppError>> {
        Box::pin(async { Err(AppError::not_paired()) })
    }

    fn call(
        &self,
        _request: CallRequest,
        _on_event: Option<Channel<Value>>,
    ) -> BoxFuture<'_, Result<Value, AppError>> {
        Box::pin(async { Err(AppError::not_paired()) })
    }

    fn cancel(&self, _id: CallId) -> BoxFuture<'_, Result<CancelResult, AppError>> {
        Box::pin(async { Err(AppError::not_paired()) })
    }

    fn pair(&self, invite: String, _name: String) -> BoxFuture<'_, Result<AppState, AppError>> {
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
                "Pairing is not connected in the unpaired stub backend.",
            ))
        })
    }

    fn unpair(&self, _node: String) -> BoxFuture<'_, Result<UnpairResult, AppError>> {
        Box::pin(async { Err(AppError::not_paired()) })
    }

    fn select_node(&self, _node: String) -> BoxFuture<'_, Result<AppState, AppError>> {
        Box::pin(async { Err(AppError::not_paired()) })
    }

    fn repair_store(&self) -> BoxFuture<'_, Result<RepairResult, AppError>> {
        Box::pin(async {
            Err(AppError::new(
                ErrorCode::InvalidArgs,
                "App state does not need repair.",
            ))
        })
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
    fn proto_error_preserves_details_and_machine() -> Result<(), Box<dyn std::error::Error>> {
        let proto = ProtoErrorBody::new(ProtoErrorCode::Denied, "Key rejected.")
            .with_hint("Pair again.")
            .with_machine("tower")
            .with_details(serde_json::json!({ "reason": "auth_failed" }));
        let err = AppError::from(proto);
        let json = serde_json::to_value(&err)?;
        assert_eq!(json["code"], "denied");
        assert_eq!(json["machine"], "tower");
        assert_eq!(json["details"]["reason"], "auth_failed");
        Ok(())
    }

    #[test]
    fn state_serializes_needs_pairing_and_null() -> Result<(), Box<dyn std::error::Error>> {
        let unpaired = AppState {
            ready: false,
            nodes: Vec::new(),
            needs: Some(Needs::Pairing),
            repair: None,
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
            repair: None,
        };
        let ready_json = serde_json::to_value(&ready)?;
        assert_eq!(ready_json["ready"], true);
        assert!(ready_json["needs"].is_null());
        Ok(())
    }
}
