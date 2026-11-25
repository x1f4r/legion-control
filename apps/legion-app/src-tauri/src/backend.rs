//! The internal seam between Tauri commands and the Legion client implementation.
//!
//! [`Backend`] is implemented by [`crate::link_backend::LinkBackend`] over `legion-link`, and by
//! [`Unpaired`] for lightweight tests.

use std::collections::BTreeMap;
use std::fmt;
use std::future::Future;
use std::pin::Pin;

use legion_link::LearnReport;
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

    /// Attaches structured diagnostic details for the caller.
    #[must_use]
    pub fn with_details(mut self, details: Value) -> Self {
        self.details = Some(details);
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

fn default_origin() -> String {
    "paired".to_owned()
}

fn default_route() -> String {
    "direct".to_owned()
}

/// One conflicting endpoint report received while another pending change was already recorded.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConflictingIdentityInfo {
    /// Directly paired node that reported this conflicting endpoint.
    pub reported_by: String,
    /// Candidate host names / addresses in the conflicting report.
    pub hosts: Vec<String>,
    /// SSH port in the conflicting report.
    pub port: u16,
    /// SSH user in the conflicting report.
    pub user: String,
    /// SHA-256 fingerprints of the conflicting report's host keys.
    #[serde(default)]
    pub fingerprints: Vec<String>,
}

/// Details of a pending host-key or endpoint change reported for an already known machine.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct IdentityChangedInfo {
    /// Directly paired node that reported the pending change.
    pub reported_by: String,
    /// Candidate host names / addresses in the newly reported target.
    pub hosts: Vec<String>,
    /// SSH port in the newly reported target.
    pub port: u16,
    /// SSH user in the newly reported target.
    #[serde(default)]
    pub user: String,
    /// Currently pinned host names / addresses before this change.
    #[serde(default)]
    pub old_hosts: Vec<String>,
    /// Currently pinned SSH port before this change.
    #[serde(default)]
    pub old_port: u16,
    /// Currently pinned SSH user before this change.
    #[serde(default)]
    pub old_user: String,
    /// SHA-256 fingerprints of the currently pinned host keys.
    #[serde(default)]
    pub old_fingerprints: Vec<String>,
    /// SHA-256 fingerprints of the newly reported host keys.
    #[serde(default)]
    pub new_fingerprints: Vec<String>,
    /// Conflicting reports received while this change was pending; when non-empty, acceptance is
    /// blocked until dismissed or paired directly.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub conflicts: Vec<ConflictingIdentityInfo>,
    /// True when the entry is directly paired (informational warning only; cannot be accepted).
    #[serde(default)]
    pub is_paired: bool,
}

/// A fleet machine that did not receive this device fully during pairing.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PairPendingPeer {
    /// Machine name.
    pub node: String,
    /// Delivery status reported by the node (`"partial"` or `"not_reached"`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    /// Optional reason the peer was not reached or refused live changes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// Summary of which fleet machines received this device during the most recent pairing.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PairSummary {
    /// Node paired directly with.
    pub node: String,
    /// Device name enrolled on the node.
    pub device: String,
    /// Role assigned to this device.
    pub role: String,
    /// Other fleet machines that received this device immediately.
    #[serde(default)]
    pub synced: Vec<String>,
    /// Other fleet machines that did not answer and will get this device at their next sync.
    #[serde(default)]
    pub not_synced: Vec<PairPendingPeer>,
    /// Exact sync command reported by the node when `not_synced` is non-empty.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sync_command: Option<String>,
}

/// Observed reachability status for any machine (including relay-only fleet peers).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MachineRouteView {
    /// `"direct"`, `"relay"`, `"not_synced"`, or `"identity_changed"`.
    pub kind: String,
    /// Relay node name (for `"relay"`) or introducer/relay (for `"not_synced"` / `"identity_changed"`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub via: Option<String>,
    /// Introducer node name for `"not_synced"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub introducer: Option<String>,
}

/// Result returned by `learn`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LearnResult {
    /// Summary of what `legion-link` learned from the answering introducer.
    pub report: LearnReport,
    /// Updated application state including any newly learned nodes or identity-change warnings.
    pub state: AppState,
}

/// Summary of one node link known to the app.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeLink {
    /// Name of the paired, learned, or local node.
    pub node: String,
    /// Principal this controller acts as on that node.
    pub principal: String,
    /// Role granted to that principal.
    pub role: String,
    /// RFC 3339 UTC timestamp when the stored role was last confirmed on this node.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role_checked_at: Option<String>,
    /// Transport used to reach the node (`ssh` or `local`).
    pub via: String,
    /// Canonical path to the local `legion` binary when `via == "local"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// `"paired"` (paired directly or local) or `"learned"` (learned via `device.endpoint`).
    #[serde(default = "default_origin")]
    pub origin: String,
    /// Trusted node that introduced this learned entry, when `origin == "learned"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub learned_from: Option<String>,
    /// Current route kind: `"direct"`, `"relay"`, `"not_synced"`, or `"identity_changed"`.
    #[serde(default = "default_route")]
    pub route: String,
    /// Relay node (or introducer for `"not_synced"`) when not direct.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub route_via: Option<String>,
    /// Pending host-key change warning requiring explicit user acceptance.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity_changed: Option<IdentityChangedInfo>,
}

/// One paired introducer's description inside a [`DisagreementView`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DisagreementEntryView {
    /// Directly paired node that reported this endpoint.
    pub reported_by: String,
    /// Candidate host names / addresses reported.
    pub hosts: Vec<String>,
    /// SSH port reported.
    pub port: u16,
    /// SSH user reported.
    pub user: String,
    /// SHA-256 fingerprints of the reported host keys.
    #[serde(default)]
    pub fingerprints: Vec<String>,
}

/// Two or more directly paired introducers described a new machine differently; it was not pinned.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DisagreementView {
    /// Machine name that was not pinned.
    pub machine: String,
    /// Differing reports sorted by `reported_by`.
    pub reports: Vec<DisagreementEntryView>,
}

/// Platform background transfer capabilities reported in [`AppState`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TransferCapabilities {
    /// True when transfers continue running after leaving the app or closing the window.
    pub background: bool,
    /// True when the OS pauses transfers while the app is in the background (iOS and iPadOS).
    pub pauses_in_background: bool,
    /// True when native device-to-machine file picking and upload is implemented on this platform.
    #[serde(default = "default_true")]
    pub can_upload: bool,
    /// True when native "Open in another app" is implemented on this platform.
    #[serde(default = "default_true")]
    pub can_open_externally: bool,
    /// True when tapping a finished download can open it directly in a viewer/chooser.
    #[serde(default = "default_true")]
    pub can_open_downloads: bool,
    /// Plain-language note explaining background behaviour on this platform, when relevant.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub background_note: Option<String>,
}

fn default_true() -> bool {
    true
}

impl TransferCapabilities {
    /// Returns the native transfer capability description for the current compile target.
    pub fn current_platform() -> Self {
        #[cfg(target_os = "ios")]
        {
            Self {
                background: false,
                pauses_in_background: true,
                can_upload: false,
                can_open_externally: false,
                can_open_downloads: false,
                background_note: Some(
                    "Transfers pause when you leave Legion on iPhone and iPad, and resume when you open it again."
                        .to_owned(),
                ),
            }
        }
        #[cfg(not(target_os = "ios"))]
        {
            Self {
                background: true,
                pauses_in_background: false,
                can_upload: true,
                can_open_externally: true,
                can_open_downloads: true,
                background_note: None,
            }
        }
    }
}

/// Resolved link handle and pairing holder identity for content serving and device transfers.
#[derive(Clone)]
pub struct ContentTarget {
    /// Live `legion-link` handle holding the machine's session.
    pub link: legion_link::Link,
    /// Node key configured on `link` (`"local"` for the desktop local pipe, or the SSH node name).
    pub link_node: String,
    /// Machine name displayed to the user.
    pub machine: String,
    /// Principal name associated with this target.
    pub principal: String,
    /// Opaque holder string bound to this specific pairing epoch (`pairing:<link_node>:<epoch>`).
    pub holder: String,
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
    /// True when this platform has a built-in camera QR scanner (`android` and `ios`).
    #[serde(default)]
    pub can_scan: bool,
    /// Observed route for every known or relayed machine.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub routes: BTreeMap<String, MachineRouteView>,
    /// New machines not pinned because paired introducers disagreed on their endpoints or keys.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub disagreements: Vec<DisagreementView>,
    /// Fleet delivery summary from the most recent pairing in this session, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_paired: Option<PairSummary>,
    /// Background transfer behaviour supported by this platform.
    #[serde(default = "TransferCapabilities::current_platform")]
    pub transfer_capabilities: TransferCapabilities,
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

    /// Closes cached transport sessions so the next call opens a fresh connection.
    fn invalidate_sessions(&self) -> BoxFuture<'_, Result<(), AppError>>;

    /// Learns direct SSH endpoints for every answering machine in the fleet and persists them.
    fn learn(&self) -> BoxFuture<'_, Result<LearnResult, AppError>>;

    /// Explicitly accepts a pending host-key or endpoint change for learned node `node`, verifying
    /// `fingerprints` match the reviewed pending target and persisting before updating live state.
    fn accept_identity_change(
        &self,
        node: String,
        fingerprints: Vec<String>,
    ) -> BoxFuture<'_, Result<AppState, AppError>>;

    /// Dismisses a pending identity-change warning for `node` without changing its pinned target.
    fn dismiss_identity_change(&self, node: String) -> BoxFuture<'_, Result<AppState, AppError>>;

    /// Returns the live [`legion_link::Link`] and every directly paired node eligible to serve
    /// update sets (`update.list` / `update.read`), or `None` when not backed by a real link.
    fn update_sources(&self) -> Result<Option<(legion_link::Link, Vec<String>)>, AppError> {
        Ok(None)
    }

    /// Resolves `machine` to its live [`ContentTarget`] for content serving and bulk transfers.
    ///
    /// Never falls back to another machine. Returns `NotFound` when `machine` is not configured on
    /// this device or `NotPaired` when no nodes exist.
    fn content_target(&self, _machine: &str) -> Result<ContentTarget, AppError> {
        Err(AppError::not_paired())
    }

    /// Fast synchronous check that a content ticket's pairing holder is still active.
    fn is_pairing_holder_live(&self, _holder: &str) -> bool {
        false
    }
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
                can_scan: cfg!(any(target_os = "android", target_os = "ios")),
                routes: BTreeMap::new(),
                disagreements: Vec::new(),
                last_paired: None,
                transfer_capabilities: TransferCapabilities::current_platform(),
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

    fn invalidate_sessions(&self) -> BoxFuture<'_, Result<(), AppError>> {
        Box::pin(async { Ok(()) })
    }

    fn learn(&self) -> BoxFuture<'_, Result<LearnResult, AppError>> {
        Box::pin(async { Err(AppError::not_paired()) })
    }

    fn accept_identity_change(
        &self,
        _node: String,
        _fingerprints: Vec<String>,
    ) -> BoxFuture<'_, Result<AppState, AppError>> {
        Box::pin(async { Err(AppError::not_paired()) })
    }

    fn dismiss_identity_change(&self, _node: String) -> BoxFuture<'_, Result<AppState, AppError>> {
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
            can_scan: true,
            routes: BTreeMap::new(),
            disagreements: Vec::new(),
            last_paired: None,
            transfer_capabilities: TransferCapabilities::current_platform(),
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
                role_checked_at: None,
                via: "ssh".to_owned(),
                path: None,
                origin: "paired".to_owned(),
                learned_from: None,
                route: "direct".to_owned(),
                route_via: None,
                identity_changed: None,
            }],
            needs: None,
            repair: None,
            can_scan: false,
            routes: BTreeMap::new(),
            disagreements: Vec::new(),
            last_paired: None,
            transfer_capabilities: TransferCapabilities::current_platform(),
        };
        let ready_json = serde_json::to_value(&ready)?;
        assert_eq!(ready_json["ready"], true);
        assert!(ready_json["needs"].is_null());
        Ok(())
    }
}
