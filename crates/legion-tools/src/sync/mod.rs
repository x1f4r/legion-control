//! The `sync` tool family: managing hubless peer-to-peer sync folders, inspecting status,
//! resolving two-version conflicts, browsing and restoring `.legion-sync/bin`, and controlling
//! the local `legion sync run` process.

pub mod config;
pub mod control;
pub mod service;

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io;
use std::marker::PhantomData;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use jiff::Timestamp;
use legion_core::config::NodeName;
use legion_core::policy::Policy;
use legion_core::proto::{Effect, ErrorBody, ErrorCode, ToolSpec};
use legion_core::{
    Access, BoxFuture, CreateKind, Ctx, NoArgs, Paths, Registry, Tool, ToolError, input_schema,
    parse_args,
};
use legion_sync::{
    BinEntry, ConflictChoice, ConflictInfo, DeletionBrakeDecision, FolderConfig, FolderMarker,
    FolderStatus, IgnoreMatcher, InboundDeletionBrakeDecision, MemberState, MemberStatus,
    ProblemKind, RealFs, RedbIndexStore, RestoreStrategy, SYNC_MARKER_FILE, SYNC_META_DIR,
    SyncEngine, SyncError, SyncFs, SyncProblem, check_folders_not_nested, validate_folder_id,
};
use legion_xfer::Timestamp as XferTimestamp;
use schemars::JsonSchema;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use self::config::{
    DEFAULT_BIN_DAYS, FolderTombstone, MAX_BIN_DAYS, MIN_BIN_DAYS, MemberTombstone, SyncFile,
    SyncFolderEntry, SyncMemberEntry, derive_folder_id, format_stamp_time, load_sync_file,
    load_sync_file_raw, lock_sync_config, next_stamp, save_sync_file_locked,
    validate_folder_name,
};
use self::control::{
    CONTROL_IPC_TIMEOUT, CONTROL_SYNC_NOW_TIMEOUT, ControlRequest, ControlResponse,
    HeldBackSummary, PersistedFolderStatus, RestoreStrategyWire, folder_index_path,
    is_runner_running, read_folder_status, remove_folder_status, send_control_request,
    write_folder_status,
};
use self::service::{ServiceAction, reconcile_host_service};

/// Maximum number of bin entries returned by `sync.bin` in one call.
const MAX_BIN_LIST_LIMIT: usize = 2_000;
const DEFAULT_BIN_LIST_LIMIT: usize = 200;

/// Maximum directory depth scanned while probing for nested `.legion-sync/marker.json` markers
/// inside a newly added directory so huge trees stay fast.
const MAX_NESTED_MARKER_SCAN_ENTRIES: usize = 50_000;

/// Fleet bridge implemented by `legion-net` so `sync.create`, `sync.add`, `sync.remove`, and
/// `sync.delete` can validate paths on remote machines and propagate `sync.toml` across the fleet
/// without introducing a cyclic crate dependency on `legion-net`.
pub trait SyncRemote: Send + Sync + 'static {
    /// Calls `tool` on `machine` as the caller principal from `ctx`.
    fn call_remote<'a>(
        &'a self,
        ctx: &'a Ctx,
        machine: &'a str,
        tool: &'a str,
        args: Value,
    ) -> BoxFuture<'a, Result<Value, ErrorBody>>;

    /// Pushes this node's merged `sync.toml` to `machines` (or every peer in `fleet.toml` when
    /// `machines` is empty), returning per-machine success or failure reason.
    fn push_sync_toml<'a>(
        &'a self,
        ctx: &'a Ctx,
        machines: Vec<String>,
    ) -> BoxFuture<'a, BTreeMap<String, Result<(), String>>>;

    /// Returns `Ok(true)` if `machine` is listed in this node's `fleet.toml`.
    fn fleet_has_machine(&self, ctx: &Ctx, machine: &str) -> Result<bool, ErrorBody>;
}

/// Registers `sync.toml` and the local `sync.*` tools (`sync.list`, `sync.status`, `sync.now`,
/// `sync.pause`, `sync.resume`, `sync.rebuild`, `sync.dismiss`, `sync.conflicts`, `sync.resolve`,
/// `sync.bin`, `sync.restore`).
pub fn register(registry: &mut Registry) {
    registry.add_document::<SyncFile>();

    registry.add_fn(
        "sync.list",
        Effect::Read,
        "List every sync folder defined in sync.toml along with its members, paths, ignore globs, pause state, bin retention days, and last known local status summary.",
        sync_list,
    );
    registry.add_fn(
        "sync.status",
        Effect::Read,
        "Report the current status of one or all sync folders from the runner's atomic status files in plain sentences and structured fields: per-member state (up_to_date, syncing with files_left and bytes_left, waiting_for_member, paused, or problem), outbound and per-peer inbound mass-deletion brakes, damaged sync records, unapplied and held-back items with reasons (including held-back symbolic links), unresolved two-version conflicts, and bin counts.",
        sync_status,
    );
    registry.add_fn(
        "sync.now",
        Effect::Write,
        "Ask the local sync runner to reload sync.toml and run an immediate scan and pull pass across one or all local sync folders.",
        sync_now,
    );
    registry.add_fn(
        "sync.pause",
        Effect::Write,
        "Pause syncing for a sync folder in sync.toml and stop active pulls on this machine until resumed with sync.resume.",
        sync_pause,
    );
    registry.add_fn(
        "sync.resume",
        Effect::Write,
        "Resume a paused sync folder or answer a tripped deletion brake. For an outbound deletion brake pass `brake_decision=\"confirm_deleted\"` or `\"rescan\"`. For a per-peer inbound deletion brake pass `peer=\"<machine>\"` and `inbound_decision=\"apply_deletions\"` or `\"keep_local_files\"`.",
        sync_resume,
    );
    registry.add_fn(
        "sync.rebuild",
        Effect::Write,
        "Explicitly rebuild a damaged sync folder index from live files on disk, `.legion-sync/bin` metadata, and `.legion-sync/deletions.log`. Never runs automatically; refuses on a healthy index unless `force=true`.",
        sync_rebuild,
    );
    registry.add_fn(
        "sync.dismiss",
        Effect::Write,
        "Dismiss informational sync notices (such as `peer_forced_rotation`, `peer_rollback_rate_limited`, `directory_kept_local_files`, or `invalid_peer_record`) without discarding held-back records, brakes, or index damage.",
        sync_dismiss,
    );
    registry.add_fn(
        "sync.conflicts",
        Effect::Read,
        "List files with two versions (unresolved conflicts) in a sync folder on this machine.",
        sync_conflicts,
    );
    registry.add_fn(
        "sync.resolve",
        Effect::Write,
        "Resolve a two-version conflict copy in a sync folder by keeping the original (`keep_original`), promoting the conflict copy (`keep_conflict`), or keeping both as independent files (`keep_both`). Displaced files are moved to .legion-sync/bin.",
        sync_resolve,
    );
    registry.add_fn(
        "sync.bin",
        Effect::Read,
        "List deleted and replaced file versions stored in a sync folder's .legion-sync/bin directory on this machine.",
        sync_bin,
    );
    registry.add_fn(
        "sync.restore",
        Effect::Write,
        "Restore an entry from a sync folder's .legion-sync/bin back into the live folder so it syncs out to every member.",
        sync_restore,
    );
}

/// Registers the fleet-coordinating `sync.*` tools (`sync.create`, `sync.add`, `sync.remove`,
/// `sync.delete`) backed by `remote`. Called from `legion_net::register`.
pub fn register_fleet(registry: &mut Registry, remote: Arc<dyn SyncRemote>) {
    registry.add(SyncFleetTool::<CreateArgs, CreateResult, _>::new(
        "sync.create",
        Effect::Write,
        "Create a new sync folder in sync.toml starting on this machine (or `machine`), verifying that the caller may write the path on that machine, that the path is on a local filesystem able to hold `.legion-sync/marker.json`, and that it does not overlap another sync folder.",
        Arc::clone(&remote),
        |remote, ctx, args| Box::pin(sync_create(remote, ctx, args)),
    ));
    registry.add(SyncFleetTool::<AddArgs, AddResult, _>::new(
        "sync.add",
        Effect::Write,
        "Add a machine and directory path as a new member of an existing sync folder. Checks on that machine from its own policy that the caller may write the path, refuses network filesystems, unwritable markers, duplicate paths, overlapping folders, and peers too old to support sync, and propagates sync.toml.",
        Arc::clone(&remote),
        |remote, ctx, args| Box::pin(sync_add(remote, ctx, args)),
    ));
    registry.add(SyncFleetTool::<RemoveArgs, RemoveResult, _>::new(
        "sync.remove",
        Effect::Write,
        "Remove a machine from a sync folder's member list. Stops syncing on that machine and leaves all files on disk untouched.",
        Arc::clone(&remote),
        |remote, ctx, args| Box::pin(sync_remove(remote, ctx, args)),
    ));
    registry.add(SyncFleetTool::<DeleteArgs, DeleteResult, _>::new(
        "sync.delete",
        Effect::Write,
        "Delete a sync folder definition across the fleet (tombstoning its id in sync.toml) so all members stop syncing it while leaving every file on disk untouched.",
        remote,
        |remote, ctx, args| Box::pin(sync_delete(remote, ctx, args)),
    ));
}

struct SyncFleetTool<A, O, F> {
    spec: ToolSpec,
    remote: Arc<dyn SyncRemote>,
    handler: F,
    _types: PhantomData<fn(A) -> O>,
}

impl<A, O, F> SyncFleetTool<A, O, F>
where
    A: DeserializeOwned + JsonSchema + Send + Sync + 'static,
    O: Serialize + Send + Sync + 'static,
    F: for<'a> Fn(Arc<dyn SyncRemote>, &'a Ctx, A) -> BoxFuture<'a, Result<O, ToolError>>
        + Send
        + Sync
        + 'static,
{
    fn new(
        name: &str,
        effect: Effect,
        description: &str,
        remote: Arc<dyn SyncRemote>,
        handler: F,
    ) -> Self {
        Self {
            spec: ToolSpec {
                name: name.to_owned(),
                description: description.to_owned(),
                effect,
                input_schema: input_schema::<A>(),
            },
            remote,
            handler,
            _types: PhantomData,
        }
    }
}

impl<A, O, F> Tool for SyncFleetTool<A, O, F>
where
    A: DeserializeOwned + JsonSchema + Send + Sync + 'static,
    O: Serialize + Send + Sync + 'static,
    F: for<'a> Fn(Arc<dyn SyncRemote>, &'a Ctx, A) -> BoxFuture<'a, Result<O, ToolError>>
        + Send
        + Sync
        + 'static,
{
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn call<'a>(&'a self, ctx: &'a Ctx, args: Value) -> BoxFuture<'a, Result<Value, ToolError>> {
        Box::pin(async move {
            let parsed = parse_args::<A>(&self.spec.name, args)?;
            let out = (self.handler)(Arc::clone(&self.remote), ctx, parsed).await?;
            serde_json::to_value(out).map_err(|error| {
                ErrorBody::new(
                    ErrorCode::Internal,
                    format!(
                        "{} returned a result that cannot be written as JSON: {error}.",
                        self.spec.name
                    ),
                )
            })
        })
    }
}

// ============================================================================
// Tool argument and result schemas
// ============================================================================

#[derive(Debug, Serialize)]
struct SyncListResult {
    node: String,
    runner_running: bool,
    folders: Vec<SyncFolderSummary>,
}

#[derive(Debug, Serialize)]
struct SyncFolderSummary {
    id: String,
    name: String,
    paused: bool,
    bin_days: u32,
    ignore: Vec<String>,
    members: Vec<SyncMemberSummary>,
    is_local_member: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    local_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    state: Option<MemberState>,
    index_damaged: bool,
    conflicts_count: usize,
    bin_count: usize,
    inbound_brakes_count: usize,
    unapplied_count: usize,
}

#[derive(Debug, Serialize, Deserialize)]
struct SyncMemberSummary {
    machine: String,
    path: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct StatusArgs {
    /// Optional sync folder id; omit to report status for every folder on this node.
    #[serde(default)]
    id: Option<String>,
}

#[derive(Debug, Serialize)]
struct SyncStatusResult {
    node: String,
    runner_running: bool,
    folders: Vec<SyncFolderStatusView>,
}

#[derive(Debug, Serialize)]
struct OutboundBrakeView {
    deleted_count: usize,
    total_live_files: usize,
    sample_paths: Vec<String>,
    sentence: String,
}

#[derive(Debug, Serialize)]
struct InboundBrakeView {
    peer: String,
    deleted_count: usize,
    total_live_files: usize,
    sample_paths: Vec<String>,
    sentence: String,
}

#[derive(Debug, Serialize)]
struct DeferredItemView {
    peer: String,
    path: String,
    reason: String,
    attempts: u32,
    sentence: String,
}

#[derive(Debug, Serialize)]
struct SyncFolderStatusView {
    id: String,
    name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    local_path: Option<String>,
    paused: bool,
    index_damaged: bool,
    state: MemberState,
    summary: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    notices: Vec<String>,
    members: Vec<SyncMemberStatusView>,
    conflicts_count: usize,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    conflicts: Vec<ConflictInfo>,
    bin_count: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pending_brake: Option<OutboundBrakeView>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    inbound_brakes: Vec<InboundBrakeView>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    held_back: Vec<HeldBackSummary>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    deferred: Vec<DeferredItemView>,
    unapplied_count: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    updated_at: Option<String>,
}

#[derive(Debug, Serialize)]
struct SyncMemberStatusView {
    member: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    path: Option<String>,
    state: MemberState,
    sentence: String,
    cursor_seq: u64,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct CreateArgs {
    /// Human-readable display name for the sync folder (e.g. "Documents").
    name: String,
    /// Directory path on the starting machine (absolute or starting with `~`).
    path: String,
    /// Optional stable folder identifier; derived from `name` when omitted.
    #[serde(default)]
    id: Option<String>,
    /// Machine where the folder starts (defaults to this node).
    #[serde(default)]
    machine: Option<String>,
    /// Optional per-folder ignore globs (trailing `/` matches directories only).
    #[serde(default)]
    ignore: Vec<String>,
    /// Retention period in days for `.legion-sync/bin` (1 to 3650, default 30).
    #[serde(default)]
    bin_days: Option<u32>,
    /// Validate permissions, filesystem, marker writability, and nesting without modifying anything.
    #[serde(default)]
    dry_run: bool,
}

#[derive(Debug, Serialize, Deserialize)]
struct CreateResult {
    id: String,
    name: String,
    machine: String,
    path: String,
    bin_days: u32,
    ignore: Vec<String>,
    dry_run: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    folder: Option<SyncFolderEntry>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pushed: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    warnings: Vec<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct AddArgs {
    /// Identifier of the existing sync folder.
    id: String,
    /// Directory path on the member machine (absolute or starting with `~`).
    path: String,
    /// Machine to add as a member (defaults to this node).
    #[serde(default)]
    machine: Option<String>,
    /// Validate permissions, filesystem, marker writability, and nesting without modifying anything.
    #[serde(default)]
    dry_run: bool,
}

#[derive(Debug, Serialize, Deserialize)]
struct AddResult {
    id: String,
    machine: String,
    path: String,
    dry_run: bool,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pushed: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    warnings: Vec<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct RemoveArgs {
    /// Identifier of the sync folder.
    id: String,
    /// Machine to remove from the folder (defaults to this node). Files on disk are left untouched.
    #[serde(default)]
    machine: Option<String>,
}

#[derive(Debug, Serialize)]
struct RemoveResult {
    id: String,
    machine: String,
    remaining_members: Vec<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pushed: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    warnings: Vec<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct DeleteArgs {
    /// Identifier of the sync folder to stop syncing everywhere. Files on disk are left untouched.
    id: String,
}

#[derive(Debug, Serialize)]
struct DeleteResult {
    id: String,
    deleted: bool,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pushed: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    warnings: Vec<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct NowArgs {
    /// Optional sync folder id; omit to reconcile all local sync folders.
    #[serde(default)]
    id: Option<String>,
    /// Wait for the triggered scan and pull pass to finish before returning (default true).
    #[serde(default = "default_true")]
    wait: bool,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Serialize)]
struct NowResult {
    node: String,
    waited: bool,
    folders: Vec<SyncFolderStatusView>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct PauseArgs {
    /// Identifier of the sync folder to pause.
    id: String,
}

#[derive(Debug, Serialize)]
struct PauseResult {
    id: String,
    paused: bool,
    runner_notified: bool,
}

/// Answer to a tripped mass-deletion brake on `sync.resume`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum BrakeDecisionArg {
    /// Confirm the disappeared files really were deleted and propagate the deletions.
    ConfirmDeleted,
    /// Leave the brake armed and rescan the folder (e.g. after reconnecting a drive).
    Rescan,
}

impl From<BrakeDecisionArg> for DeletionBrakeDecision {
    fn from(value: BrakeDecisionArg) -> Self {
        match value {
            BrakeDecisionArg::ConfirmDeleted => DeletionBrakeDecision::ConfirmDeleted,
            BrakeDecisionArg::Rescan => DeletionBrakeDecision::Rescan,
        }
    }
}

/// Answer to a tripped inbound mass-deletion brake for one peer on `sync.resume`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum InboundBrakeDecisionArg {
    /// Allow and apply the peer's blocked deletions (moving displaced local files to the bin).
    ApplyDeletions,
    /// Keep this machine's local files and re-assert them as newer than the peer's deletions.
    KeepLocalFiles,
}

impl From<InboundBrakeDecisionArg> for InboundDeletionBrakeDecision {
    fn from(value: InboundBrakeDecisionArg) -> Self {
        match value {
            InboundBrakeDecisionArg::ApplyDeletions => InboundDeletionBrakeDecision::ApplyDeletions,
            InboundBrakeDecisionArg::KeepLocalFiles => InboundDeletionBrakeDecision::KeepLocalFiles,
        }
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct ResumeArgs {
    /// Identifier of the sync folder to resume.
    id: String,
    /// Decision required when resuming a folder paused by the outbound mass-deletion brake:
    /// `confirm_deleted` or `rescan`.
    #[serde(default)]
    brake_decision: Option<BrakeDecisionArg>,
    /// Peer machine name when resolving a per-peer inbound mass-deletion brake.
    #[serde(default)]
    peer: Option<String>,
    /// Decision required when resolving an inbound mass-deletion brake for `peer`:
    /// `apply_deletions` or `keep_local_files`.
    #[serde(default)]
    inbound_decision: Option<InboundBrakeDecisionArg>,
}

#[derive(Debug, Serialize)]
struct ResumeResult {
    id: String,
    paused: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    brake_decision: Option<BrakeDecisionArg>,
    #[serde(skip_serializing_if = "Option::is_none")]
    peer: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    inbound_decision: Option<InboundBrakeDecisionArg>,
    runner_notified: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    state: Option<MemberState>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct RebuildArgs {
    /// Identifier of the sync folder whose damaged index should be rebuilt from disk.
    id: String,
    /// Rebuild even when the folder's index is currently healthy (default false).
    #[serde(default)]
    force: bool,
}

#[derive(Debug, Serialize)]
struct RebuildResult {
    id: String,
    rebuilt: bool,
    files_indexed: usize,
    bin_entries: usize,
    runner_notified: bool,
    state: MemberState,
}

/// Informational durable notice kind that a person or agent may explicitly dismiss.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum DismissibleProblemArg {
    /// Dismiss a notice that this machine started a fresh history after a peer reported rollback.
    PeerForcedRotation,
    /// Dismiss a notice that a peer's repeated rollback claims were rate-limited.
    PeerRollbackRateLimited,
    /// Dismiss a notice that a deleted directory stayed on disk because it held local files.
    DirectoryKeptLocalFiles,
    /// Dismiss a notice that a peer sent a malformed or unauthorized record.
    InvalidPeerRecord,
}

impl From<DismissibleProblemArg> for ProblemKind {
    fn from(value: DismissibleProblemArg) -> Self {
        match value {
            DismissibleProblemArg::PeerForcedRotation => ProblemKind::PeerForcedRotation,
            DismissibleProblemArg::PeerRollbackRateLimited => ProblemKind::PeerRollbackRateLimited,
            DismissibleProblemArg::DirectoryKeptLocalFiles => ProblemKind::DirectoryKeptLocalFiles,
            DismissibleProblemArg::InvalidPeerRecord => ProblemKind::InvalidPeerRecord,
        }
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct DismissArgs {
    /// Identifier of the sync folder.
    id: String,
    /// Informational notice kind to dismiss.
    kind: DismissibleProblemArg,
    /// Optional peer machine filter.
    #[serde(default)]
    peer: Option<String>,
}

#[derive(Debug, Serialize)]
struct DismissResult {
    id: String,
    kind: DismissibleProblemArg,
    #[serde(skip_serializing_if = "Option::is_none")]
    peer: Option<String>,
    dismissed: usize,
    runner_notified: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    state: Option<MemberState>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct FolderIdArgs {
    /// Identifier of the sync folder.
    id: String,
}

#[derive(Debug, Serialize)]
struct ConflictsResult {
    id: String,
    conflicts: Vec<ConflictInfo>,
}

/// How to settle a two-version conflict copy.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ConflictChoiceArg {
    /// Keep the original path and move the conflict copy to `.legion-sync/bin`.
    KeepOriginal,
    /// Move the original path to `.legion-sync/bin` and promote the conflict copy to the original path.
    KeepConflict,
    /// Keep both files as independent ordinary files and clear the conflict flag.
    KeepBoth,
}

impl From<ConflictChoiceArg> for ConflictChoice {
    fn from(value: ConflictChoiceArg) -> Self {
        match value {
            ConflictChoiceArg::KeepOriginal => ConflictChoice::KeepOriginal,
            ConflictChoiceArg::KeepConflict => ConflictChoice::KeepConflict,
            ConflictChoiceArg::KeepBoth => ConflictChoice::KeepBoth,
        }
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct ResolveArgs {
    /// Identifier of the sync folder.
    id: String,
    /// Relative path of the conflict copy (as reported by `sync.conflicts`).
    conflict_path: String,
    /// Resolution choice: `keep_original`, `keep_conflict`, or `keep_both`.
    choice: ConflictChoiceArg,
}

#[derive(Debug, Serialize)]
struct ResolveResult {
    id: String,
    conflict_path: String,
    choice: ConflictChoiceArg,
    resolved: bool,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct BinArgs {
    /// Identifier of the sync folder.
    id: String,
    /// Maximum number of bin entries to return (1 to 2000, default 200).
    #[serde(default)]
    limit: Option<usize>,
}

#[derive(Debug, Serialize)]
struct BinResult {
    id: String,
    total: usize,
    truncated: bool,
    entries: Vec<BinEntry>,
}

/// Strategy when restoring a bin entry onto an existing live path.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RestoreStrategyArg {
    /// Refuse with `exists` if a live file already sits at the destination (default).
    #[default]
    Fail,
    /// Move the existing indexed live file into `.legion-sync/bin`, then restore.
    Replace,
    /// Restore alongside the existing file under a renamed path.
    Rename,
}

impl From<RestoreStrategyArg> for RestoreStrategyWire {
    fn from(value: RestoreStrategyArg) -> Self {
        match value {
            RestoreStrategyArg::Fail => RestoreStrategyWire::FailIfExists,
            RestoreStrategyArg::Replace => RestoreStrategyWire::Replace,
            RestoreStrategyArg::Rename => RestoreStrategyWire::Rename,
        }
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct RestoreArgs {
    /// Identifier of the sync folder.
    id: String,
    /// Bin entry identifier (as listed by `sync.bin`).
    entry_id: String,
    /// Optional relative path inside the sync folder (defaults to the entry's original path).
    #[serde(default)]
    dest_path: Option<String>,
    /// What to do if a file already exists at the destination: `fail` (default), `replace`, or `rename`.
    #[serde(default)]
    if_exists: RestoreStrategyArg,
}

#[derive(Debug, Serialize)]
struct RestoreResult {
    id: String,
    entry_id: String,
    restored_path: String,
}

// ============================================================================
// Tool implementations
// ============================================================================

async fn sync_list(ctx: &Ctx, _args: NoArgs) -> Result<SyncListResult, ToolError> {
    let paths = ctx.paths().clone();
    let here = ctx.node_name().to_owned();
    tokio::task::spawn_blocking(move || {
        let file = load_sync_file(&paths)?;
        let runner_running = is_runner_running(&paths);
        let mut folders = Vec::with_capacity(file.folders.len());
        for (id, entry) in &file.folders {
            let persisted = read_folder_status(&paths, id)?;
            let local_path = entry.members.get(&here).map(|m| m.path.clone());
            let is_local_member = local_path.is_some();
            let (state, index_damaged, conflicts_count, bin_count, inbound_brakes_count, unapplied_count) =
                match persisted {
                    Some(p) => {
                        let unapplied = p.held_back.len() + p.status.deferred.len();
                        let damaged = p.index_damaged || status_has_index_damaged(&p.status);
                        (
                            Some(p.status.state),
                            damaged,
                            p.status.conflicts.len(),
                            p.status.bin.len(),
                            p.status.inbound_brakes.len(),
                            unapplied,
                        )
                    }
                    None => (None, false, 0, 0, 0, 0),
                };
            folders.push(SyncFolderSummary {
                id: id.clone(),
                name: entry.name.clone(),
                paused: entry.paused,
                bin_days: entry.bin_days,
                ignore: entry.ignore.clone(),
                members: entry
                    .members
                    .iter()
                    .map(|(m, info)| SyncMemberSummary {
                        machine: m.clone(),
                        path: info.path.clone(),
                    })
                    .collect(),
                is_local_member,
                local_path,
                state,
                index_damaged,
                conflicts_count,
                bin_count,
                inbound_brakes_count,
                unapplied_count,
            });
        }
        Ok(SyncListResult {
            node: here,
            runner_running,
            folders,
        })
    })
    .await
    .map_err(join_error)?
}

async fn sync_status(ctx: &Ctx, args: StatusArgs) -> Result<SyncStatusResult, ToolError> {
    let paths = ctx.paths().clone();
    let here = ctx.node_name().to_owned();
    tokio::task::spawn_blocking(move || {
        let file = load_sync_file(&paths)?;
        let runner_running = is_runner_running(&paths);
        let target_ids: Vec<String> = match args.id {
            Some(ref id) => {
                if !file.folders.contains_key(id) {
                    return Err(folder_not_found(id));
                }
                vec![id.clone()]
            }
            None => file.folders.keys().cloned().collect(),
        };

        let mut folders = Vec::with_capacity(target_ids.len());
        for id in target_ids {
            let entry = &file.folders[&id];
            let persisted = read_folder_status(&paths, &id)?;
            folders.push(build_status_view(&id, entry, &here, persisted, runner_running));
        }
        Ok(SyncStatusResult {
            node: here,
            runner_running,
            folders,
        })
    })
    .await
    .map_err(join_error)?
}

const MAX_STATUS_CONFLICTS_INLINE: usize = 50;

fn status_has_index_damaged(status: &FolderStatus) -> bool {
    matches!(
        &status.state,
        MemberState::Problem { problems, .. }
            if problems.iter().any(|p| p.kind == ProblemKind::IndexDamaged)
    )
}

/// Formats a [`MemberState`] as a single plain-language sentence.
pub fn member_state_sentence(state: &MemberState) -> String {
    match state {
        MemberState::UpToDate => "Up to date.".to_owned(),
        MemberState::Syncing {
            files_left,
            bytes_left,
        } => {
            let noun = if *files_left == 1 { "file" } else { "files" };
            if *bytes_left > 0 {
                format!("Syncing ({files_left} {noun}, {} left).", format_bytes(*bytes_left))
            } else if *files_left > 0 {
                format!("Syncing ({files_left} {noun} left).")
            } else {
                "Syncing.".to_owned()
            }
        }
        MemberState::WaitingForMember { members } => {
            if members.is_empty() {
                "Waiting for a machine that is off.".to_owned()
            } else if members.len() == 1 {
                format!("Waiting for {} (currently off or unreachable).", members[0])
            } else {
                format!(
                    "Waiting for {} (currently off or unreachable).",
                    members.join(", ")
                )
            }
        }
        MemberState::Paused => "Paused.".to_owned(),
        MemberState::Problem { sentence, .. } => sentence.clone(),
    }
}

fn format_bytes(bytes: u64) -> String {
    const KIB: f64 = 1024.0;
    const MIB: f64 = 1024.0 * 1024.0;
    const GIB: f64 = 1024.0 * 1024.0 * 1024.0;
    let b = bytes as f64;
    if b >= GIB {
        format!("{:.1} GiB", b / GIB)
    } else if b >= MIB {
        format!("{:.1} MiB", b / MIB)
    } else if b >= KIB {
        format!("{:.1} KiB", b / KIB)
    } else {
        format!("{bytes} B")
    }
}

/// Ensures held-back symbolic link messages clearly state that only relative links staying inside
/// the folder sync.
pub fn enrich_problem_sentence(prob: &SyncProblem) -> String {
    if prob.kind == ProblemKind::UnsupportedSymlink
        && !prob.sentence.to_lowercase().contains("windows")
        && !prob
            .sentence
            .contains("Only relative symbolic links that stay inside the sync folder are synced.")
    {
        format!(
            "{} Only relative symbolic links that stay inside the sync folder are synced.",
            prob.sentence
        )
    } else {
        prob.sentence.clone()
    }
}

/// Collects all unapplied remote held-back records AND local scan-held items (such as escaping
/// symbolic links, Windows-incompatible names, or unreadable paths) with plain-language reasons.
pub fn collect_held_back_summaries(
    engine: &SyncEngine<RealFs, RedbIndexStore>,
    status: &FolderStatus,
) -> Vec<HeldBackSummary> {
    let mut items: Vec<HeldBackSummary> = Vec::new();
    let mut seen: BTreeSet<(Option<String>, String, ProblemKind)> = BTreeSet::new();

    if let Ok(held) = engine.list_held_back() {
        for h in held {
            let key = (
                Some(h.from_peer.clone()),
                h.record.path.clone(),
                h.problem.kind,
            );
            if seen.insert(key) {
                items.push(HeldBackSummary {
                    path: h.record.path,
                    peer: Some(h.from_peer),
                    kind: h.problem.kind,
                    reason: enrich_problem_sentence(&h.problem),
                });
            }
        }
    }

    if let MemberState::Problem { ref problems, .. } = status.state {
        for p in problems {
            if p.path.is_empty() {
                continue;
            }
            let is_item_hold = matches!(
                p.kind,
                ProblemKind::UnsupportedSymlink
                    | ProblemKind::CaseClash
                    | ProblemKind::ForbiddenWindowsChar
                    | ProblemKind::TrailingDotOrSpace
                    | ProblemKind::ReservedWindowsName
                    | ProblemKind::SegmentTooLong
                    | ProblemKind::PathTooLong
                    | ProblemKind::FilesystemError
                    | ProblemKind::UnknownMember
                    | ProblemKind::DeferredLimitExceeded
            );
            if !is_item_hold {
                continue;
            }
            let key = (p.peer.clone(), p.path.clone(), p.kind);
            if seen.insert(key) {
                items.push(HeldBackSummary {
                    path: p.path.clone(),
                    peer: p.peer.clone(),
                    kind: p.kind,
                    reason: enrich_problem_sentence(p),
                });
            }
        }
    }

    items.sort_by(|a, b| {
        a.path
            .cmp(&b.path)
            .then_with(|| a.peer.cmp(&b.peer))
            .then_with(|| a.kind.cmp(&b.kind))
    });
    items
}

fn build_status_view(
    id: &str,
    entry: &SyncFolderEntry,
    here: &str,
    persisted: Option<PersistedFolderStatus>,
    runner_running: bool,
) -> SyncFolderStatusView {
    let local_path = entry.members.get(here).map(|m| m.path.clone());
    match persisted {
        Some(p) => {
            let index_damaged = p.index_damaged || status_has_index_damaged(&p.status);
            let mut notices: Vec<String> = Vec::new();
            if index_damaged {
                notices.push(format!(
                    "{} Run `sync.rebuild id={id}` to rebuild the index from disk.",
                    legion_sync::INDEX_DAMAGED_PROBLEM_SENTENCE
                ));
            }
            let pending_brake = p.status.pending_brake.map(|b| {
                let sentence = format!(
                    "Sync paused because {} of {} files disappeared at once. Confirm the deletions (`sync.resume id={id} brake_decision=confirm_deleted`) or rescan (`sync.resume id={id} brake_decision=rescan`).",
                    b.deleted_count, b.total_live_files
                );
                notices.push(sentence.clone());
                OutboundBrakeView {
                    deleted_count: b.deleted_count,
                    total_live_files: b.total_live_files,
                    sample_paths: b.sample_paths,
                    sentence,
                }
            });
            let inbound_brakes: Vec<InboundBrakeView> = p
                .status
                .inbound_brakes
                .into_iter()
                .map(|ib| {
                    let sentence = format!(
                        "Paused syncing from '{}' because its changes would delete {} of {} files. Apply the deletions (`sync.resume id={id} peer={} inbound_decision=apply_deletions`) or keep this machine's files (`sync.resume id={id} peer={} inbound_decision=keep_local_files`).",
                        ib.peer, ib.deleted_count, ib.total_live_files, ib.peer, ib.peer
                    );
                    notices.push(sentence.clone());
                    InboundBrakeView {
                        peer: ib.peer,
                        deleted_count: ib.deleted_count,
                        total_live_files: ib.total_live_files,
                        sample_paths: ib.sample_paths,
                        sentence,
                    }
                })
                .collect();

            // Collect held-back items (prefer persisted explicit list, fall back to problems).
            let mut held_back = p.held_back;
            if held_back.is_empty()
                && let MemberState::Problem { ref problems, .. } = p.status.state
            {
                for prob in problems {
                    if !prob.path.is_empty()
                        && matches!(
                            prob.kind,
                            ProblemKind::UnsupportedSymlink
                                | ProblemKind::CaseClash
                                | ProblemKind::ForbiddenWindowsChar
                                | ProblemKind::TrailingDotOrSpace
                                | ProblemKind::ReservedWindowsName
                                | ProblemKind::SegmentTooLong
                                | ProblemKind::PathTooLong
                                | ProblemKind::FilesystemError
                                | ProblemKind::UnknownMember
                                | ProblemKind::DeferredLimitExceeded
                        )
                    {
                        held_back.push(HeldBackSummary {
                            path: prob.path.clone(),
                            peer: prob.peer.clone(),
                            kind: prob.kind,
                            reason: enrich_problem_sentence(prob),
                        });
                    }
                }
            }
            for item in &held_back {
                notices.push(item.reason.clone());
            }

            let deferred: Vec<DeferredItemView> = p
                .status
                .deferred
                .into_iter()
                .map(|d| {
                    let tries = if d.attempts == 1 { "attempt" } else { "attempts" };
                    let sentence = format!(
                        "'{}' from '{}' is deferred after {} {tries}: {}.",
                        d.path, d.peer, d.attempts, d.reason
                    );
                    notices.push(sentence.clone());
                    DeferredItemView {
                        peer: d.peer,
                        path: d.path,
                        reason: d.reason,
                        attempts: d.attempts,
                        sentence,
                    }
                })
                .collect();

            let conflicts_count = p.status.conflicts.len();
            if conflicts_count == 1 {
                notices.push(format!(
                    "1 file has two versions ('{}'); inspect with `sync.conflicts id={id}` and settle with `sync.resolve`.",
                    p.status.conflicts[0].conflict_path
                ));
            } else if conflicts_count > 1 {
                notices.push(format!(
                    "{conflicts_count} files have two versions; inspect with `sync.conflicts id={id}` and settle with `sync.resolve`."
                ));
            }
            let mut conflicts = p.status.conflicts;
            if conflicts.len() > MAX_STATUS_CONFLICTS_INLINE {
                conflicts.truncate(MAX_STATUS_CONFLICTS_INLINE);
            }

            let bin_count = p.status.bin.len();
            if bin_count == 1 {
                notices.push(format!(
                    "1 deleted or replaced item is in the folder's bin (inspect with `sync.bin id={id}` or restore with `sync.restore`)."
                ));
            } else if bin_count > 1 {
                notices.push(format!(
                    "{bin_count} deleted or replaced items are in the folder's bin (inspect with `sync.bin id={id}` or restore with `sync.restore`)."
                ));
            }

            let members = p
                .status
                .members
                .into_iter()
                .map(|ms: MemberStatus| {
                    let sentence = member_state_sentence(&ms.state);
                    SyncMemberStatusView {
                        path: entry.members.get(&ms.member).map(|m| m.path.clone()),
                        member: ms.member,
                        state: ms.state,
                        sentence,
                        cursor_seq: ms.cursor_seq,
                    }
                })
                .collect();
            let summary = member_state_sentence(&p.status.state);
            let unapplied_count = held_back.len() + deferred.len();
            SyncFolderStatusView {
                id: id.to_owned(),
                name: entry.name.clone(),
                local_path: Some(p.local_path),
                paused: entry.paused || matches!(p.status.state, MemberState::Paused),
                index_damaged,
                state: p.status.state,
                summary,
                notices,
                members,
                conflicts_count,
                conflicts,
                bin_count,
                pending_brake,
                inbound_brakes,
                held_back,
                deferred,
                unapplied_count,
                updated_at: Some(p.updated_at),
            }
        }
        None => {
            let state = if entry.paused {
                MemberState::Paused
            } else if local_path.is_none() {
                MemberState::UpToDate
            } else if !runner_running {
                MemberState::Problem {
                    sentence: "The sync runner is not running on this machine.".to_owned(),
                    problems: vec![SyncProblem {
                        path: String::new(),
                        kind: ProblemKind::FilesystemError,
                        peer: None,
                        sentence: "The sync runner is not running on this machine.".to_owned(),
                    }],
                }
            } else {
                MemberState::Syncing {
                    files_left: 0,
                    bytes_left: 0,
                }
            };
            let summary = member_state_sentence(&state);
            let notices = if matches!(state, MemberState::Problem { .. }) {
                vec![summary.clone()]
            } else {
                Vec::new()
            };
            let members = entry
                .members
                .iter()
                .map(|(m, info)| SyncMemberStatusView {
                    member: m.clone(),
                    path: Some(info.path.clone()),
                    state: state.clone(),
                    sentence: summary.clone(),
                    cursor_seq: 0,
                })
                .collect();
            SyncFolderStatusView {
                id: id.to_owned(),
                name: entry.name.clone(),
                local_path,
                paused: entry.paused,
                index_damaged: false,
                state,
                summary,
                notices,
                members,
                conflicts_count: 0,
                conflicts: Vec::new(),
                bin_count: 0,
                pending_brake: None,
                inbound_brakes: Vec::new(),
                held_back: Vec::new(),
                deferred: Vec::new(),
                unapplied_count: 0,
                updated_at: None,
            }
        }
    }
}

async fn sync_create(
    remote: Arc<dyn SyncRemote>,
    ctx: &Ctx,
    args: CreateArgs,
) -> Result<CreateResult, ToolError> {
    validate_folder_name(&args.name).map_err(|reason| {
        ErrorBody::new(ErrorCode::InvalidArgs, format!("Invalid folder name: {reason}."))
            .with_hint("Give a short non-empty folder name without control characters.")
    })?;
    if let Some(ref id) = args.id {
        validate_folder_id(id).map_err(sync_error_to_body)?;
    }
    let bin_days = args.bin_days.unwrap_or(DEFAULT_BIN_DAYS);
    if !(MIN_BIN_DAYS..=MAX_BIN_DAYS).contains(&bin_days) {
        return Err(ErrorBody::new(
            ErrorCode::InvalidArgs,
            format!("bin_days must be between {MIN_BIN_DAYS} and {MAX_BIN_DAYS} (got {bin_days})."),
        )
        .with_hint("Choose a positive retention period in days, such as 30."));
    }
    IgnoreMatcher::compile(&args.ignore).map_err(sync_error_to_body)?;

    let here = ctx.node_name().to_owned();
    let target_machine = args.machine.clone().unwrap_or_else(|| here.clone());
    validate_machine_arg(&target_machine)?;

    // Remote target: run sync.create on that machine as the SAME caller principal so that machine
    // checks its own policy, filesystem, marker writability, and nesting — and if that machine's
    // legion is too old to know sync, fail clearly with nothing created anywhere.
    if target_machine != here {
        if !remote.fleet_has_machine(ctx, &target_machine)? {
            return Err(ErrorBody::new(
                ErrorCode::NotFound,
                format!("Machine {target_machine:?} is not listed in fleet.toml."),
            )
            .with_hint("Add the machine with `legion fleet.add` before creating a sync folder on it."));
        }
        let remote_args = json!({
            "name": args.name,
            "path": args.path,
            "id": args.id,
            "machine": target_machine,
            "ignore": args.ignore,
            "bin_days": bin_days,
            "dry_run": args.dry_run,
        });
        let value = match remote
            .call_remote(ctx, &target_machine, "sync.create", remote_args)
            .await
        {
            Ok(v) => v,
            Err(err) if err.code == ErrorCode::UnknownTool => {
                return Err(peer_too_old_error(&target_machine));
            }
            Err(err) => return Err(err),
        };
        let created: CreateResult = serde_json::from_value(value).map_err(|err| {
            ErrorBody::new(
                ErrorCode::Failed,
                format!("Machine {target_machine} returned an unexpected sync.create reply: {err}."),
            )
        })?;
        if !args.dry_run
            && let Some(ref folder_entry) = created.folder
        {
            let mut incoming = SyncFile::default();
            incoming
                .folders
                .insert(created.id.clone(), folder_entry.clone());
            let paths = ctx.paths().clone();
            tokio::task::spawn_blocking(move || {
                config::apply_sync_merge_blocking(&paths, &incoming, |_| Ok(()))
            })
            .await
            .map_err(join_error)??;
            let _ = send_control_request(ctx.paths(), ControlRequest::Reload, CONTROL_IPC_TIMEOUT).await;
        }
        return Ok(created);
    }

    // Local creation on this node.
    let paths = ctx.paths().clone();
    let policy = ctx.config().get::<Policy>()?;
    let principal = ctx.principal().to_owned();
    let grant = ctx.grant().clone();
    let folder_name = args.name.clone();
    let folder_ignore = args.ignore.clone();

    let (folder_id, resolved_str, folder_entry, warnings) = tokio::task::spawn_blocking(move || {
        let _lock = lock_sync_config(&paths)?;
        let (mut sync_file, bytes_before) = load_sync_file_raw(&paths)?;
        let folder_id = match args.id {
            Some(explicit) => {
                if sync_file.folders.contains_key(&explicit) {
                    return Err(ErrorBody::new(
                        ErrorCode::Exists,
                        format!("A sync folder with id {explicit:?} already exists."),
                    )
                    .with_hint("Choose another id, or use `sync.add` to add a member to the existing folder."));
                }
                if sync_file.removed.contains_key(&explicit) {
                    return Err(ErrorBody::new(
                        ErrorCode::Exists,
                        format!("Sync folder id {explicit:?} was previously deleted and cannot be reused."),
                    )
                    .with_hint("Choose a new id or omit `id` to derive one automatically."));
                }
                explicit
            }
            None => derive_folder_id(&folder_name, &sync_file),
        };

        let resolved = validate_local_member_path(
            &paths,
            &policy,
            &principal,
            &grant,
            &sync_file,
            &here,
            &folder_id,
            &args.path,
            args.dry_run,
        )?;
        let resolved_str = resolved.display().to_string();

        if args.dry_run {
            return Ok((folder_id, resolved_str, None, Vec::new()));
        }

        let stamp = next_stamp(&here, None);
        let mut members = BTreeMap::new();
        members.insert(
            here.clone(),
            SyncMemberEntry {
                path: resolved_str.clone(),
                added_at: stamp.at.clone(),
                added_on: stamp.on.clone(),
                updated_at: stamp.at.clone(),
                updated_on: stamp.on.clone(),
            },
        );
        let entry = SyncFolderEntry {
            name: folder_name,
            ignore: folder_ignore,
            paused: false,
            bin_days,
            created_at: stamp.at.clone(),
            created_on: stamp.on.clone(),
            updated_at: stamp.at,
            updated_on: stamp.on,
            name_updated_at: None,
            name_updated_on: None,
            ignore_updated_at: None,
            ignore_updated_on: None,
            paused_updated_at: None,
            paused_updated_on: None,
            bin_days_updated_at: None,
            bin_days_updated_on: None,
            members,
            removed_members: BTreeMap::new(),
        };
        let had_local_before = !sync_file.folders_for_machine(&here).is_empty();
        sync_file.folders.insert(folder_id.clone(), entry.clone());
        save_sync_file_locked(&paths, &sync_file, bytes_before)?;

        let mut warnings = Vec::new();
        if !had_local_before
            && let ServiceAction::Warning(w) = reconcile_host_service(&paths, true)
        {
            warnings.push(w);
        }

        Ok((folder_id, resolved_str, Some(entry), warnings))
    })
    .await
    .map_err(join_error)??;

    let mut pushed = BTreeMap::new();
    if !args.dry_run {
        let _ = send_control_request(ctx.paths(), ControlRequest::Reload, CONTROL_IPC_TIMEOUT).await;
        for (peer, outcome) in remote.push_sync_toml(ctx, Vec::new()).await {
            pushed.insert(
                peer,
                match outcome {
                    Ok(()) => "synced".to_owned(),
                    Err(reason) => format!("pending: {reason}"),
                },
            );
        }
    }

    Ok(CreateResult {
        id: folder_id,
        name: args.name,
        machine: ctx.node_name().to_owned(),
        path: resolved_str,
        bin_days,
        ignore: args.ignore,
        dry_run: args.dry_run,
        folder: folder_entry,
        pushed,
        warnings,
    })
}

async fn sync_add(
    remote: Arc<dyn SyncRemote>,
    ctx: &Ctx,
    args: AddArgs,
) -> Result<AddResult, ToolError> {
    validate_folder_id(&args.id).map_err(sync_error_to_body)?;
    let here = ctx.node_name().to_owned();
    let target_machine = args.machine.clone().unwrap_or_else(|| here.clone());
    validate_machine_arg(&target_machine)?;

    if target_machine != here {
        if !remote.fleet_has_machine(ctx, &target_machine)? {
            return Err(ErrorBody::new(
                ErrorCode::NotFound,
                format!("Machine {target_machine:?} is not listed in fleet.toml."),
            )
            .with_hint("Add the machine with `legion fleet.add` before adding it to a sync folder."));
        }
        // Check local sync.toml has this folder and does not already list this member/path.
        let paths = ctx.paths().clone();
        let id_check = args.id.clone();
        let machine_check = target_machine.clone();
        let raw_path_check = args.path.clone();
        let existing_folder = tokio::task::spawn_blocking(move || -> Result<SyncFolderEntry, ErrorBody> {
            let sync_file = load_sync_file(&paths)?;
            let Some(folder) = sync_file.folders.get(&id_check) else {
                return Err(folder_not_found(&id_check));
            };
            if let Some(existing_member) = folder.members.get(&machine_check) {
                return Err(ErrorBody::new(
                    ErrorCode::Exists,
                    format!(
                        "Machine {machine_check:?} is already a member of sync folder {id_check:?} at {}.",
                        existing_member.path
                    ),
                )
                .with_hint("Remove the member first with `sync.remove` if you want to change its path."));
            }
            for (other_id, other_folder) in &sync_file.folders {
                if let Some(other_member) = other_folder.members.get(&machine_check)
                    && other_member.path == raw_path_check
                {
                    return Err(ErrorBody::new(
                        ErrorCode::Exists,
                        format!(
                            "Path {raw_path_check:?} on {machine_check:?} is already synced by folder {other_id:?}."
                        ),
                    )
                    .with_hint("Choose a separate directory on that machine."));
                }
            }
            Ok(folder.clone())
        })
        .await
        .map_err(join_error)??;

        // Preflight on target_machine AS THE CALLER PRINCIPAL: verifies legion version knows sync,
        // caller holds fs.write on path from target's policy, path is local & marker-writable &
        // non-nested & non-duplicate. Nothing is written anywhere if this fails!
        let preflight_args = json!({
            "id": args.id,
            "machine": target_machine,
            "path": args.path,
            "dry_run": true,
        });
        let preflight_val = match remote
            .call_remote(ctx, &target_machine, "sync.add", preflight_args)
            .await
        {
            Ok(v) => v,
            Err(err) if err.code == ErrorCode::UnknownTool => {
                return Err(peer_too_old_error(&target_machine));
            }
            Err(err) => return Err(err),
        };
        let preflight: AddResult = serde_json::from_value(preflight_val).map_err(|err| {
            ErrorBody::new(
                ErrorCode::Failed,
                format!("Machine {target_machine} returned an unexpected sync.add reply: {err}."),
            )
        })?;
        let resolved_remote_path = preflight.path;

        if args.dry_run {
            return Ok(AddResult {
                id: args.id,
                machine: target_machine,
                path: resolved_remote_path,
                dry_run: true,
                pushed: BTreeMap::new(),
                warnings: Vec::new(),
            });
        }

        // Update local sync.toml under lock, adding `target_machine -> resolved_remote_path`.
        let paths = ctx.paths().clone();
        let id_mut = args.id.clone();
        let machine_mut = target_machine.clone();
        let path_mut = resolved_remote_path.clone();
        let here_mut = here.clone();
        let peer_list: Vec<String> = tokio::task::spawn_blocking(move || -> Result<Vec<String>, ErrorBody> {
            let _lock = lock_sync_config(&paths)?;
            let (mut sync_file, bytes_before) = load_sync_file_raw(&paths)?;
            let Some(folder) = sync_file.folders.get_mut(&id_mut) else {
                return Err(folder_not_found(&id_mut));
            };
            if folder.members.contains_key(&machine_mut) {
                return Err(ErrorBody::new(
                    ErrorCode::Exists,
                    format!("Machine {machine_mut:?} is already a member of sync folder {id_mut:?}."),
                ));
            }
            let min_after = folder.max_stamp();
            let stamp = next_stamp(&here_mut, Some(&min_after));
            folder.removed_members.remove(&machine_mut);
            folder.members.insert(
                machine_mut.clone(),
                SyncMemberEntry {
                    path: path_mut,
                    added_at: stamp.at.clone(),
                    added_on: stamp.on.clone(),
                    updated_at: stamp.at,
                    updated_on: stamp.on,
                },
            );
            let peers: Vec<String> = folder
                .members
                .keys()
                .filter(|m| *m != &here_mut)
                .cloned()
                .collect();
            save_sync_file_locked(&paths, &sync_file, bytes_before)?;
            Ok(peers)
        })
        .await
        .map_err(join_error)??;

        let _ = existing_folder;
        let _ = send_control_request(ctx.paths(), ControlRequest::Reload, CONTROL_IPC_TIMEOUT).await;

        let mut pushed = BTreeMap::new();
        let mut warnings = Vec::new();
        for (peer, outcome) in remote.push_sync_toml(ctx, peer_list).await {
            match outcome {
                Ok(()) => {
                    pushed.insert(peer, "synced".to_owned());
                }
                Err(reason) => {
                    if peer == target_machine {
                        warnings.push(format!(
                            "Could not push sync.toml to {target_machine} immediately: {reason}"
                        ));
                    }
                    pushed.insert(peer, format!("pending: {reason}"));
                }
            }
        }
        // Best-effort wake target_machine's runner if it is running so it joins immediately.
        let _ = remote
            .call_remote(
                ctx,
                &target_machine,
                "sync.now",
                json!({ "id": args.id, "wait": false }),
            )
            .await;

        return Ok(AddResult {
            id: args.id,
            machine: target_machine,
            path: resolved_remote_path,
            dry_run: false,
            pushed,
            warnings,
        });
    }

    // Local add on THIS node (either directly or invoked as remote preflight with dry_run=true).
    let paths = ctx.paths().clone();
    let policy = ctx.config().get::<Policy>()?;
    let principal = ctx.principal().to_owned();
    let grant = ctx.grant().clone();
    let folder_id = args.id.clone();

    let (resolved_str, peers_to_push, warnings) = tokio::task::spawn_blocking(move || {
        let _lock = lock_sync_config(&paths)?;
        let (mut sync_file, bytes_before) = load_sync_file_raw(&paths)?;
        if sync_file.removed.contains_key(&folder_id) {
            return Err(ErrorBody::new(
                ErrorCode::NotFound,
                format!("Sync folder {folder_id:?} was deleted and cannot accept new members."),
            )
            .with_hint("Create a new sync folder with `sync.create`."));
        }
        if let Some(folder) = sync_file.folders.get(&folder_id) {
            if let Some(existing) = folder.members.get(&here) {
                return Err(ErrorBody::new(
                    ErrorCode::Exists,
                    format!(
                        "Machine {here:?} is already a member of sync folder {folder_id:?} at {}.",
                        existing.path
                    ),
                )
                .with_hint("Remove the member first with `sync.remove` if you want to move its path."));
            }
        } else if !args.dry_run {
            return Err(folder_not_found(&folder_id));
        }

        let resolved = validate_local_member_path(
            &paths,
            &policy,
            &principal,
            &grant,
            &sync_file,
            &here,
            &folder_id,
            &args.path,
            args.dry_run,
        )?;
        let resolved_str = resolved.display().to_string();

        if args.dry_run {
            return Ok((resolved_str, Vec::new(), Vec::new()));
        }

        let had_local_before = !sync_file.folders_for_machine(&here).is_empty();
        let Some(folder) = sync_file.folders.get_mut(&folder_id) else {
            return Err(folder_not_found(&folder_id));
        };
        let min_after = folder.max_stamp();
        let stamp = next_stamp(&here, Some(&min_after));
        folder.removed_members.remove(&here);
        folder.members.insert(
            here.clone(),
            SyncMemberEntry {
                path: resolved_str.clone(),
                added_at: stamp.at.clone(),
                added_on: stamp.on.clone(),
                updated_at: stamp.at,
                updated_on: stamp.on,
            },
        );
        let peers: Vec<String> = folder
            .members
            .keys()
            .filter(|m| *m != &here)
            .cloned()
            .collect();
        save_sync_file_locked(&paths, &sync_file, bytes_before)?;

        let mut warnings = Vec::new();
        if !had_local_before
            && let ServiceAction::Warning(w) = reconcile_host_service(&paths, true)
        {
            warnings.push(w);
        }

        Ok((resolved_str, peers, warnings))
    })
    .await
    .map_err(join_error)??;

    let mut pushed = BTreeMap::new();
    if !args.dry_run {
        let _ = send_control_request(ctx.paths(), ControlRequest::Reload, CONTROL_IPC_TIMEOUT).await;
        for (peer, outcome) in remote.push_sync_toml(ctx, peers_to_push).await {
            pushed.insert(
                peer,
                match outcome {
                    Ok(()) => "synced".to_owned(),
                    Err(reason) => format!("pending: {reason}"),
                },
            );
        }
    }

    Ok(AddResult {
        id: args.id,
        machine: ctx.node_name().to_owned(),
        path: resolved_str,
        dry_run: args.dry_run,
        pushed,
        warnings,
    })
}

async fn sync_remove(
    remote: Arc<dyn SyncRemote>,
    ctx: &Ctx,
    args: RemoveArgs,
) -> Result<RemoveResult, ToolError> {
    validate_folder_id(&args.id).map_err(sync_error_to_body)?;
    let here = ctx.node_name().to_owned();
    let target_machine = args.machine.clone().unwrap_or_else(|| here.clone());
    validate_machine_arg(&target_machine)?;

    let paths = ctx.paths().clone();
    let folder_id = args.id.clone();
    let target_clone = target_machine.clone();
    let here_clone = here.clone();

    let (remaining_members, push_targets, warnings) = tokio::task::spawn_blocking(move || {
        let _lock = lock_sync_config(&paths)?;
        let (mut sync_file, bytes_before) = load_sync_file_raw(&paths)?;
        let Some(folder) = sync_file.folders.get_mut(&folder_id) else {
            return Err(folder_not_found(&folder_id));
        };
        if !folder.members.contains_key(&target_clone) {
            return Err(ErrorBody::new(
                ErrorCode::NotFound,
                format!(
                    "Machine {target_clone:?} is not a member of sync folder {folder_id:?}."
                ),
            )
            .with_hint("Run `legion sync.list` to see current members."));
        }

        let had_local_before = !sync_file.folders_for_machine(&here_clone).is_empty();
        let Some(folder) = sync_file.folders.get_mut(&folder_id) else {
            return Err(folder_not_found(&folder_id));
        };
        let min_after = folder.max_stamp();
        let stamp = next_stamp(&here_clone, Some(&min_after));
        folder.members.remove(&target_clone);
        folder.removed_members.insert(
            target_clone.clone(),
            MemberTombstone {
                removed_at: stamp.at,
                removed_on: stamp.on,
            },
        );
        let remaining: Vec<String> = folder.members.keys().cloned().collect();
        let mut push_set: BTreeSet<String> = remaining.iter().cloned().collect();
        push_set.insert(target_clone.clone());
        push_set.remove(&here_clone);

        save_sync_file_locked(&paths, &sync_file, bytes_before)?;

        let mut warnings = Vec::new();
        if target_clone == here_clone {
            remove_folder_status(&paths, &folder_id);
        }
        let has_local_after = !sync_file.folders_for_machine(&here_clone).is_empty();
        if had_local_before
            && !has_local_after
            && let ServiceAction::Warning(w) = reconcile_host_service(&paths, false)
        {
            warnings.push(w);
        }

        Ok((remaining, push_set.into_iter().collect::<Vec<_>>(), warnings))
    })
    .await
    .map_err(join_error)??;

    let _ = send_control_request(ctx.paths(), ControlRequest::Reload, CONTROL_IPC_TIMEOUT).await;
    let mut pushed = BTreeMap::new();
    for (peer, outcome) in remote.push_sync_toml(ctx, push_targets).await {
        pushed.insert(
            peer,
            match outcome {
                Ok(()) => "synced".to_owned(),
                Err(reason) => format!("pending: {reason}"),
            },
        );
    }

    Ok(RemoveResult {
        id: args.id,
        machine: target_machine,
        remaining_members,
        pushed,
        warnings,
    })
}

async fn sync_delete(
    remote: Arc<dyn SyncRemote>,
    ctx: &Ctx,
    args: DeleteArgs,
) -> Result<DeleteResult, ToolError> {
    validate_folder_id(&args.id).map_err(sync_error_to_body)?;
    let here = ctx.node_name().to_owned();
    let paths = ctx.paths().clone();
    let folder_id = args.id.clone();

    let (push_targets, warnings) = tokio::task::spawn_blocking(move || {
        let _lock = lock_sync_config(&paths)?;
        let (mut sync_file, bytes_before) = load_sync_file_raw(&paths)?;
        let Some(existing) = sync_file.folders.get(&folder_id) else {
            return Err(folder_not_found(&folder_id));
        };
        let min_after = existing.max_stamp();
        let mut push_targets: Vec<String> = existing
            .members
            .keys()
            .filter(|m| *m != &here)
            .cloned()
            .collect();
        let had_local_before = !sync_file.folders_for_machine(&here).is_empty();

        sync_file.folders.remove(&folder_id);
        let stamp = next_stamp(&here, Some(&min_after));
        sync_file.removed.insert(
            folder_id.clone(),
            FolderTombstone {
                removed_at: stamp.at,
                removed_on: stamp.on,
            },
        );
        save_sync_file_locked(&paths, &sync_file, bytes_before)?;
        remove_folder_status(&paths, &folder_id);

        let mut warnings = Vec::new();
        let has_local_after = !sync_file.folders_for_machine(&here).is_empty();
        if had_local_before
            && !has_local_after
            && let ServiceAction::Warning(w) = reconcile_host_service(&paths, false)
        {
            warnings.push(w);
        }

        push_targets.sort();
        push_targets.dedup();
        Ok((push_targets, warnings))
    })
    .await
    .map_err(join_error)??;

    let _ = send_control_request(ctx.paths(), ControlRequest::Reload, CONTROL_IPC_TIMEOUT).await;
    let mut pushed = BTreeMap::new();
    for (peer, outcome) in remote.push_sync_toml(ctx, push_targets).await {
        pushed.insert(
            peer,
            match outcome {
                Ok(()) => "synced".to_owned(),
                Err(reason) => format!("pending: {reason}"),
            },
        );
    }

    Ok(DeleteResult {
        id: args.id,
        deleted: true,
        pushed,
        warnings,
    })
}

async fn sync_now(ctx: &Ctx, args: NowArgs) -> Result<NowResult, ToolError> {
    if let Some(ref id) = args.id {
        validate_folder_id(id).map_err(sync_error_to_body)?;
    }
    let timeout = if args.wait {
        CONTROL_SYNC_NOW_TIMEOUT
    } else {
        CONTROL_IPC_TIMEOUT
    };
    let resp = send_control_request(
        ctx.paths(),
        ControlRequest::SyncNow {
            folder_id: args.id.clone(),
            wait: args.wait,
        },
        timeout,
    )
    .await?;

    let Some(resp) = resp else {
        return Err(ErrorBody::new(
            ErrorCode::Failed,
            "No sync runner is running on this machine.",
        )
        .with_hint("Start the sync runner with `legion sync run` (or enable the legion-sync service)."));
    };

    let persisted_list = match resp {
        ControlResponse::Statuses { folders } => folders,
        ControlResponse::Ok => Vec::new(),
        ControlResponse::Error { error } => return Err(error),
        other => {
            return Err(ErrorBody::new(
                ErrorCode::Internal,
                format!("Unexpected runner reply to sync.now: {other:?}."),
            ));
        }
    };

    let paths = ctx.paths().clone();
    let here = ctx.node_name().to_owned();
    let sync_file = tokio::task::spawn_blocking(move || load_sync_file(&paths))
        .await
        .map_err(join_error)??;
    let views = persisted_list
        .into_iter()
        .filter_map(|p| {
            let folder_id = p.folder_id.clone();
            let entry = sync_file.folders.get(&folder_id)?;
            Some(build_status_view(
                &folder_id,
                entry,
                &here,
                Some(p),
                true,
            ))
        })
        .collect();

    Ok(NowResult {
        node: ctx.node_name().to_owned(),
        waited: args.wait,
        folders: views,
    })
}

async fn sync_pause(ctx: &Ctx, args: PauseArgs) -> Result<PauseResult, ToolError> {
    validate_folder_id(&args.id).map_err(sync_error_to_body)?;
    let paths = ctx.paths().clone();
    let here = ctx.node_name().to_owned();
    let folder_id = args.id.clone();

    tokio::task::spawn_blocking(move || -> Result<(), ErrorBody> {
        let _lock = lock_sync_config(&paths)?;
        let (mut sync_file, bytes_before) = load_sync_file_raw(&paths)?;
        let Some(folder) = sync_file.folders.get_mut(&folder_id) else {
            return Err(folder_not_found(&folder_id));
        };
        folder.set_paused(true, &here);
        save_sync_file_locked(&paths, &sync_file, bytes_before)?;
        Ok(())
    })
    .await
    .map_err(join_error)??;

    let runner_notified = match send_control_request(
        ctx.paths(),
        ControlRequest::Pause {
            folder_id: args.id.clone(),
        },
        CONTROL_IPC_TIMEOUT,
    )
    .await?
    {
        Some(ControlResponse::Error { error }) => return Err(error),
        Some(_) => true,
        None => {
            // Update local engine & status file offline if this node is a member.
            let paths = ctx.paths().clone();
            let here = ctx.node_name().to_owned();
            let id = args.id.clone();
            let _ = tokio::task::spawn_blocking(move || {
                with_offline_engine(&paths, &here, &id, |engine, _entry, _local_path| {
                    engine.set_paused(true)?;
                    Ok(())
                })
            })
            .await;
            false
        }
    };

    Ok(PauseResult {
        id: args.id,
        paused: true,
        runner_notified,
    })
}

async fn sync_resume(ctx: &Ctx, args: ResumeArgs) -> Result<ResumeResult, ToolError> {
    validate_folder_id(&args.id).map_err(sync_error_to_body)?;
    if args.brake_decision.is_some() && (args.inbound_decision.is_some() || args.peer.is_some()) {
        return Err(ErrorBody::new(
            ErrorCode::InvalidArgs,
            "Pass either `brake_decision` (for an outbound brake) or `peer` + `inbound_decision` (for a per-peer inbound brake), not both.",
        )
        .with_hint("Resolve one brake at a time with `legion sync.resume`."));
    }
    if args.peer.is_some() && args.inbound_decision.is_none() {
        return Err(ErrorBody::new(
            ErrorCode::InvalidArgs,
            "`peer` requires `inbound_decision=\"apply_deletions\"` or `inbound_decision=\"keep_local_files\"`.",
        )
        .with_hint("Specify how to resolve that peer's inbound mass-deletion brake."));
    }
    if let Some(ref peer) = args.peer {
        validate_machine_arg(peer)?;
    }

    // Check current persisted folder state (damaged index, outbound brake, inbound brakes).
    let paths_check = ctx.paths().clone();
    let id_check = args.id.clone();
    let persisted = tokio::task::spawn_blocking(move || read_folder_status(&paths_check, &id_check))
        .await
        .map_err(join_error)??;
    let index_damaged = persisted
        .as_ref()
        .is_some_and(|p| p.index_damaged || status_has_index_damaged(&p.status));
    let pending_brake = persisted.as_ref().and_then(|p| p.status.pending_brake.clone());
    let inbound_brakes = persisted
        .as_ref()
        .map(|p| p.status.inbound_brakes.clone())
        .unwrap_or_default();

    if index_damaged {
        return Err(ErrorBody::new(
            ErrorCode::Failed,
            format!(
                "Sync folder {:?}: {}",
                args.id,
                legion_sync::INDEX_DAMAGED_PROBLEM_SENTENCE
            ),
        )
        .with_hint(format!(
            "Rebuild this folder's sync record explicitly with `legion sync.rebuild id={}`.",
            args.id
        )));
    }

    if args.inbound_decision.is_some() && args.peer.is_none() {
        let peers: Vec<&str> = inbound_brakes.iter().map(|b| b.peer.as_str()).collect();
        let hint = if peers.is_empty() {
            "Pass `peer=\"<machine>\"` alongside `inbound_decision`.".to_owned()
        } else {
            format!(
                "Pass `peer=\"<machine>\"` alongside `inbound_decision` (paused peer(s): {}).",
                peers.join(", ")
            )
        };
        return Err(ErrorBody::new(
            ErrorCode::InvalidArgs,
            "`inbound_decision` requires `peer` naming which peer's inbound brake to resolve.",
        )
        .with_hint(hint));
    }

    // Resolving a per-peer inbound deletion brake.
    if let (Some(peer), Some(inbound_decision)) = (args.peer.clone(), args.inbound_decision) {
        check_caller_may_write_folder(ctx, &args.id).await?;
        let mut state_after = None;
        let runner_notified = match send_control_request(
            ctx.paths(),
            ControlRequest::ResolveInboundBrake {
                folder_id: args.id.clone(),
                peer: peer.clone(),
                decision: inbound_decision.into(),
            },
            CONTROL_SYNC_NOW_TIMEOUT,
        )
        .await?
        {
            Some(ControlResponse::Error { error }) => return Err(error),
            Some(ControlResponse::Statuses { folders }) => {
                state_after = folders.into_iter().next().map(|p| p.status.state);
                true
            }
            Some(_) => true,
            None => {
                let paths = ctx.paths().clone();
                let here = ctx.node_name().to_owned();
                let id = args.id.clone();
                let peer_clone = peer.clone();
                let status = tokio::task::spawn_blocking(move || {
                    with_offline_engine(&paths, &here, &id, |engine, _entry, _local_path| {
                        engine.resolve_inbound_deletion_brake(
                            &peer_clone,
                            inbound_decision.into(),
                            now_xfer_timestamp(),
                        )?;
                        engine.status()
                    })
                })
                .await
                .map_err(join_error)??;
                state_after = Some(status.state);
                false
            }
        };
        return Ok(ResumeResult {
            id: args.id,
            paused: false,
            brake_decision: None,
            peer: Some(peer),
            inbound_decision: Some(inbound_decision),
            runner_notified,
            state: state_after,
        });
    }

    if let Some(ref brake) = pending_brake
        && args.brake_decision.is_none()
    {
        return Err(ErrorBody::new(
            ErrorCode::Conflict,
            format!(
                "Sync folder {:?} is paused by the mass-deletion brake ({} of {} files disappeared).",
                args.id, brake.deleted_count, brake.total_live_files
            ),
        )
        .with_hint(
            "Pass `brake_decision=\"confirm_deleted\"` if those files really were deleted, or `brake_decision=\"rescan\"` after restoring them or reconnecting the drive.",
        )
        .with_details(json!({
            "folder_id": args.id,
            "deleted_count": brake.deleted_count,
            "total_live_files": brake.total_live_files,
            "sample_paths": brake.sample_paths,
        })));
    }

    if args.brake_decision.is_some() {
        check_caller_may_write_folder(ctx, &args.id).await?;
    }

    // Ensure folder exists and check whether it was manually paused vs blocked only by an inbound brake.
    let paths = ctx.paths().clone();
    let here = ctx.node_name().to_owned();
    let folder_id = args.id.clone();
    let has_inbound = !inbound_brakes.is_empty();
    let no_outbound_decision = args.brake_decision.is_none();
    let inbound_peers: Vec<String> = inbound_brakes.iter().map(|b| b.peer.clone()).collect();
    tokio::task::spawn_blocking(move || -> Result<(), ErrorBody> {
        let _lock = lock_sync_config(&paths)?;
        let (mut sync_file, bytes_before) = load_sync_file_raw(&paths)?;
        let Some(folder) = sync_file.folders.get_mut(&folder_id) else {
            return Err(folder_not_found(&folder_id));
        };
        if !folder.paused && no_outbound_decision && has_inbound {
            return Err(ErrorBody::new(
                ErrorCode::Conflict,
                format!(
                    "Sync folder {folder_id:?} has an inbound mass-deletion brake awaiting confirmation for peer(s): {}.",
                    inbound_peers.join(", ")
                ),
            )
            .with_hint(format!(
                "Pass `peer=\"{}\"` with `inbound_decision=\"apply_deletions\"` or `inbound_decision=\"keep_local_files\"`.",
                inbound_peers[0]
            )));
        }
        if folder.paused {
            folder.set_paused(false, &here);
            save_sync_file_locked(&paths, &sync_file, bytes_before)?;
        }
        Ok(())
    })
    .await
    .map_err(join_error)??;

    let request = match args.brake_decision {
        Some(decision) => ControlRequest::ResolveBrake {
            folder_id: args.id.clone(),
            decision: decision.into(),
        },
        None => ControlRequest::Resume {
            folder_id: args.id.clone(),
        },
    };

    let mut state_after = None;
    let runner_notified =
        match send_control_request(ctx.paths(), request, CONTROL_SYNC_NOW_TIMEOUT).await? {
            Some(ControlResponse::Error { error }) => return Err(error),
            Some(ControlResponse::Statuses { folders }) => {
                state_after = folders.into_iter().next().map(|p| p.status.state);
                true
            }
            Some(_) => true,
            None => {
                let paths = ctx.paths().clone();
                let here = ctx.node_name().to_owned();
                let id = args.id.clone();
                let brake_decision = args.brake_decision;
                let status = tokio::task::spawn_blocking(move || {
                    with_offline_engine(&paths, &here, &id, |engine, _entry, _local_path| {
                        if let Some(decision) = brake_decision {
                            engine.resolve_deletion_brake(decision.into(), now_xfer_timestamp())?;
                        } else {
                            engine.set_paused(false)?;
                        }
                        engine.status()
                    })
                })
                .await
                .map_err(join_error)??;
                state_after = Some(status.state);
                false
            }
        };

    Ok(ResumeResult {
        id: args.id,
        paused: false,
        brake_decision: args.brake_decision,
        peer: None,
        inbound_decision: None,
        runner_notified,
        state: state_after,
    })
}

async fn sync_rebuild(ctx: &Ctx, args: RebuildArgs) -> Result<RebuildResult, ToolError> {
    validate_folder_id(&args.id).map_err(sync_error_to_body)?;
    check_caller_may_write_folder(ctx, &args.id).await?;

    if let Some(resp) = send_control_request(
        ctx.paths(),
        ControlRequest::RebuildIndex {
            folder_id: args.id.clone(),
            force: args.force,
        },
        CONTROL_SYNC_NOW_TIMEOUT,
    )
    .await?
    {
        return match resp {
            ControlResponse::Rebuilt {
                files_indexed,
                bin_entries,
                status,
            } => Ok(RebuildResult {
                id: args.id,
                rebuilt: true,
                files_indexed,
                bin_entries,
                runner_notified: true,
                state: status.status.state,
            }),
            ControlResponse::Error { error } => Err(error),
            other => Err(ErrorBody::new(
                ErrorCode::Internal,
                format!("Unexpected runner reply to sync.rebuild: {other:?}."),
            )),
        };
    }

    // Offline rebuild when no runner is active.
    let paths = ctx.paths().clone();
    let here = ctx.node_name().to_owned();
    let folder_id = args.id.clone();
    let force = args.force;
    let (files_indexed, bin_entries, state) = tokio::task::spawn_blocking(move || {
        let sync_file = load_sync_file(&paths)?;
        let Some(entry) = sync_file.folders.get(&folder_id) else {
            return Err(folder_not_found(&folder_id));
        };
        let Some(member) = entry.members.get(&here) else {
            return Err(ErrorBody::new(
                ErrorCode::NotFound,
                format!("This machine ({here}) is not a member of sync folder {folder_id:?}."),
            ));
        };
        let root = expand_member_path(&paths, &member.path);
        let index_path = folder_index_path(&paths, &folder_id);
        let mut cfg = FolderConfig::new(&folder_id, &here, entry.members.keys().cloned());
        cfg.former_members = entry.removed_members.keys().cloned().collect();
        cfg.ignore = entry.ignore.clone();
        cfg.bin_retention_secs = u64::from(entry.bin_days).saturating_mul(86_400);

        if !force {
            let probe_damaged = match RedbIndexStore::open(&index_path) {
                Err(SyncError::IndexDamaged { .. } | SyncError::CorruptData { .. }) => true,
                Err(err) => return Err(sync_error_to_body(err)),
                Ok(store) => {
                    let probe_fs = RealFs::open(&root).map_err(sync_error_to_body)?;
                    match SyncEngine::open(cfg.clone(), probe_fs, store) {
                        Ok(eng) => eng.is_index_damaged(),
                        Err(SyncError::IndexDamaged { .. } | SyncError::CorruptData { .. }) => true,
                        Err(err) => return Err(sync_error_to_body(err)),
                    }
                }
            };
            if !probe_damaged {
                return Err(ErrorBody::new(
                    ErrorCode::Conflict,
                    format!("Sync folder {folder_id:?} has a healthy sync record; refusing to rebuild without `force=true`."),
                )
                .with_hint("Pass `force=true` only if you intentionally want to rebuild the index from disk."));
            }
        }

        let real_fs = RealFs::open(&root).map_err(sync_error_to_body)?;
        let (engine, report) = SyncEngine::rebuild_redb_from_disk(
            cfg,
            real_fs,
            &index_path,
            now_xfer_timestamp(),
        )
        .map_err(sync_error_to_body)?;
        let files_indexed = report.created + report.modified + report.unchanged;
        let status = engine.status().map_err(sync_error_to_body)?;
        let held_back = collect_held_back_summaries(&engine, &status);
        let bin_entries = status.bin.len();
        let state = status.state.clone();
        let persisted = PersistedFolderStatus {
            folder_id: folder_id.clone(),
            folder_name: entry.name.clone(),
            local_path: root.display().to_string(),
            paused_by_config: entry.paused,
            index_damaged: engine.is_index_damaged(),
            held_back,
            updated_at: format_stamp_time(Timestamp::now()),
            status,
        };
        write_folder_status(&paths, &persisted)?;
        Ok::<_, ErrorBody>((files_indexed, bin_entries, state))
    })
    .await
    .map_err(join_error)??;

    Ok(RebuildResult {
        id: args.id,
        rebuilt: true,
        files_indexed,
        bin_entries,
        runner_notified: false,
        state,
    })
}

async fn sync_dismiss(ctx: &Ctx, args: DismissArgs) -> Result<DismissResult, ToolError> {
    validate_folder_id(&args.id).map_err(sync_error_to_body)?;
    if let Some(ref peer) = args.peer {
        validate_machine_arg(peer)?;
    }
    check_caller_may_write_folder(ctx, &args.id).await?;
    let problem_kind: ProblemKind = args.kind.into();

    if let Some(resp) = send_control_request(
        ctx.paths(),
        ControlRequest::DismissProblem {
            folder_id: args.id.clone(),
            kind: problem_kind,
            peer: args.peer.clone(),
        },
        CONTROL_IPC_TIMEOUT,
    )
    .await?
    {
        return match resp {
            ControlResponse::Dismissed { count, status } => Ok(DismissResult {
                id: args.id,
                kind: args.kind,
                peer: args.peer,
                dismissed: count,
                runner_notified: true,
                state: Some(status.status.state),
            }),
            ControlResponse::Error { error } => Err(error),
            other => Err(ErrorBody::new(
                ErrorCode::Internal,
                format!("Unexpected runner reply to sync.dismiss: {other:?}."),
            )),
        };
    }

    let paths = ctx.paths().clone();
    let here = ctx.node_name().to_owned();
    let folder_id = args.id.clone();
    let peer = args.peer.clone();
    let (count, state) = tokio::task::spawn_blocking(move || {
        with_offline_engine(&paths, &here, &folder_id, |engine, _entry, _root| {
            let count = engine.dismiss_problem(problem_kind, peer.as_deref())?;
            let status = engine.status()?;
            Ok((count, status.state))
        })
    })
    .await
    .map_err(join_error)??;

    Ok(DismissResult {
        id: args.id,
        kind: args.kind,
        peer: args.peer,
        dismissed: count,
        runner_notified: false,
        state: Some(state),
    })
}

async fn sync_conflicts(ctx: &Ctx, args: FolderIdArgs) -> Result<ConflictsResult, ToolError> {
    validate_folder_id(&args.id).map_err(sync_error_to_body)?;

    if let Some(resp) = send_control_request(
        ctx.paths(),
        ControlRequest::ListConflicts {
            folder_id: args.id.clone(),
        },
        CONTROL_IPC_TIMEOUT,
    )
    .await?
    {
        return match resp {
            ControlResponse::Conflicts { conflicts } => Ok(ConflictsResult {
                id: args.id,
                conflicts,
            }),
            ControlResponse::Error { error } => Err(error),
            other => Err(ErrorBody::new(
                ErrorCode::Internal,
                format!("Unexpected runner reply to sync.conflicts: {other:?}."),
            )),
        };
    }

    let paths = ctx.paths().clone();
    let here = ctx.node_name().to_owned();
    let id = args.id.clone();
    let conflicts = tokio::task::spawn_blocking(move || {
        with_offline_engine(&paths, &here, &id, |engine, _entry, _path| {
            engine.list_conflicts()
        })
    })
    .await
    .map_err(join_error)??;

    Ok(ConflictsResult {
        id: args.id,
        conflicts,
    })
}

async fn sync_resolve(ctx: &Ctx, args: ResolveArgs) -> Result<ResolveResult, ToolError> {
    validate_folder_id(&args.id).map_err(sync_error_to_body)?;
    check_caller_may_write_folder(ctx, &args.id).await?;

    if let Some(resp) = send_control_request(
        ctx.paths(),
        ControlRequest::ResolveConflict {
            folder_id: args.id.clone(),
            conflict_path: args.conflict_path.clone(),
            choice: args.choice.into(),
        },
        CONTROL_IPC_TIMEOUT,
    )
    .await?
    {
        return match resp {
            ControlResponse::Ok | ControlResponse::Statuses { .. } => Ok(ResolveResult {
                id: args.id,
                conflict_path: args.conflict_path,
                choice: args.choice,
                resolved: true,
            }),
            ControlResponse::Error { error } => Err(error),
            other => Err(ErrorBody::new(
                ErrorCode::Internal,
                format!("Unexpected runner reply to sync.resolve: {other:?}."),
            )),
        };
    }

    let paths = ctx.paths().clone();
    let here = ctx.node_name().to_owned();
    let id = args.id.clone();
    let conflict_path = args.conflict_path.clone();
    let choice: ConflictChoice = args.choice.into();
    tokio::task::spawn_blocking(move || {
        with_offline_engine(&paths, &here, &id, |engine, _entry, _path| {
            engine.resolve_conflict(&conflict_path, choice, now_xfer_timestamp())
        })
    })
    .await
    .map_err(join_error)??;

    Ok(ResolveResult {
        id: args.id,
        conflict_path: args.conflict_path,
        choice: args.choice,
        resolved: true,
    })
}

async fn sync_bin(ctx: &Ctx, args: BinArgs) -> Result<BinResult, ToolError> {
    validate_folder_id(&args.id).map_err(sync_error_to_body)?;
    let limit = args.limit.unwrap_or(DEFAULT_BIN_LIST_LIMIT);
    if limit == 0 || limit > MAX_BIN_LIST_LIMIT {
        return Err(ErrorBody::new(
            ErrorCode::InvalidArgs,
            format!("limit must be between 1 and {MAX_BIN_LIST_LIMIT} (got {limit})."),
        ));
    }

    let mut entries = if let Some(resp) = send_control_request(
        ctx.paths(),
        ControlRequest::ListBin {
            folder_id: args.id.clone(),
        },
        CONTROL_IPC_TIMEOUT,
    )
    .await?
    {
        match resp {
            ControlResponse::Bin { entries } => entries,
            ControlResponse::Error { error } => return Err(error),
            other => {
                return Err(ErrorBody::new(
                    ErrorCode::Internal,
                    format!("Unexpected runner reply to sync.bin: {other:?}."),
                ));
            }
        }
    } else {
        let paths = ctx.paths().clone();
        let here = ctx.node_name().to_owned();
        let id = args.id.clone();
        tokio::task::spawn_blocking(move || {
            with_offline_engine(&paths, &here, &id, |engine, _entry, _path| {
                engine.list_bin()
            })
        })
        .await
        .map_err(join_error)??
    };

    let total = entries.len();
    let truncated = total > limit;
    if truncated {
        entries.truncate(limit);
    }

    Ok(BinResult {
        id: args.id,
        total,
        truncated,
        entries,
    })
}

async fn sync_restore(ctx: &Ctx, args: RestoreArgs) -> Result<RestoreResult, ToolError> {
    validate_folder_id(&args.id).map_err(sync_error_to_body)?;
    check_caller_may_write_folder(ctx, &args.id).await?;

    let strategy_wire: RestoreStrategyWire = args.if_exists.into();
    if let Some(resp) = send_control_request(
        ctx.paths(),
        ControlRequest::RestoreBin {
            folder_id: args.id.clone(),
            entry_id: args.entry_id.clone(),
            dest_path: args.dest_path.clone(),
            strategy: strategy_wire,
        },
        CONTROL_IPC_TIMEOUT,
    )
    .await?
    {
        return match resp {
            ControlResponse::Restored { path } => Ok(RestoreResult {
                id: args.id,
                entry_id: args.entry_id,
                restored_path: path,
            }),
            ControlResponse::Error { error } => Err(error),
            other => Err(ErrorBody::new(
                ErrorCode::Internal,
                format!("Unexpected runner reply to sync.restore: {other:?}."),
            )),
        };
    }

    let paths = ctx.paths().clone();
    let here = ctx.node_name().to_owned();
    let id = args.id.clone();
    let entry_id = args.entry_id.clone();
    let dest_path = args.dest_path.clone();
    let strategy: RestoreStrategy = strategy_wire.into();
    let restored_path = tokio::task::spawn_blocking(move || {
        with_offline_engine(&paths, &here, &id, |engine, _entry, _path| {
            engine.restore_from_bin(&entry_id, dest_path.as_deref(), strategy, now_xfer_timestamp())
        })
    })
    .await
    .map_err(join_error)??;

    Ok(RestoreResult {
        id: args.id,
        entry_id: args.entry_id,
        restored_path,
    })
}

// ============================================================================
// Validation, policy checks, marker probes, and offline engine helpers
// ============================================================================

async fn check_caller_may_write_folder(ctx: &Ctx, folder_id: &str) -> Result<PathBuf, ErrorBody> {
    let paths = ctx.paths().clone();
    let here = ctx.node_name().to_owned();
    let id = folder_id.to_owned();
    let member_path = tokio::task::spawn_blocking(move || -> Result<String, ErrorBody> {
        let sync_file = load_sync_file(&paths)?;
        let Some(folder) = sync_file.folders.get(&id) else {
            return Err(folder_not_found(&id));
        };
        let Some(member) = folder.members.get(&here) else {
            return Err(ErrorBody::new(
                ErrorCode::NotFound,
                format!("This machine ({here}) is not a member of sync folder {id:?}."),
            )
            .with_hint("Run this command on a member machine with `--on <machine>`."));
        };
        Ok(member.path.clone())
    })
    .await
    .map_err(join_error)??;

    let policy = ctx.config().get::<Policy>()?;
    let fs_write_grant = authorize_fs_write(&policy, ctx.principal(), ctx.paths())?;
    fs_write_grant.check_path(&member_path, Access::Write)
}

fn authorize_fs_write(
    policy: &Policy,
    principal: &str,
    paths: &Paths,
) -> Result<legion_core::Grant, ErrorBody> {
    let spec = ToolSpec {
        name: "fs.write".to_owned(),
        description: String::new(),
        effect: Effect::Write,
        input_schema: json!({"type": "object"}),
    };
    policy.authorize(principal, &spec, paths)
}

#[allow(clippy::too_many_arguments)]
fn validate_local_member_path(
    paths: &Paths,
    policy: &Policy,
    principal: &str,
    _tool_grant: &legion_core::Grant,
    sync_file: &SyncFile,
    here: &str,
    folder_id: &str,
    raw_path: &str,
    dry_run: bool,
) -> Result<PathBuf, ErrorBody> {
    // 1. Check caller holds `fs.write` and may write this path under THIS node's policy.
    let write_grant = authorize_fs_write(policy, principal, paths)?;
    let resolved = write_grant.check_path(raw_path, Access::Write)?;
    write_grant.check_create(&resolved, CreateKind::Dir, true)?;

    // 2. Refuse existing non-directory targets.
    match fs::symlink_metadata(&resolved) {
        Ok(meta) if meta.file_type().is_symlink() => {
            return Err(ErrorBody::new(
                ErrorCode::InvalidArgs,
                format!(
                    "{} is a symbolic link, and a sync folder root must be a real directory.",
                    resolved.display()
                ),
            )
            .with_hint("Pass the real target directory path instead of a symlink."));
        }
        Ok(meta) if !meta.is_dir() => {
            return Err(ErrorBody::new(
                ErrorCode::InvalidArgs,
                format!(
                    "{} is not a directory, so it cannot be a sync folder.",
                    resolved.display()
                ),
            )
            .with_hint("Choose a directory path."));
        }
        Ok(_) => {}
        Err(err) if err.kind() == io::ErrorKind::NotFound => {}
        Err(err) => {
            return Err(ErrorBody::new(
                ErrorCode::Failed,
                format!("{} cannot be inspected: {err}.", resolved.display()),
            ));
        }
    }

    // 3. Refuse network filesystems (NFS, SMB/CIFS, FUSE remote, UNC, mapped network drives).
    check_not_network_filesystem(&resolved)?;

    // 4. Refuse duplicate paths and nested/containing sync folders (both in sync.toml and on disk).
    let mut existing_roots: Vec<PathBuf> = Vec::new();
    for (other_id, other_folder) in &sync_file.folders {
        let Some(other_member) = other_folder.members.get(here) else {
            continue;
        };
        let other_resolved = write_grant
            .check_path(&other_member.path, Access::Read)
            .unwrap_or_else(|_| PathBuf::from(&other_member.path));
        if same_canonical_or_lexical(&resolved, &other_resolved) {
            return Err(ErrorBody::new(
                ErrorCode::Exists,
                format!(
                    "{} is already used by sync folder {other_id:?} on {here}.",
                    resolved.display()
                ),
            )
            .with_hint("Each directory can belong to at most one sync folder."));
        }
        existing_roots.push(other_resolved);
    }
    let existing_refs: Vec<&Path> = existing_roots.iter().map(PathBuf::as_path).collect();
    check_folders_not_nested(&resolved, &existing_refs).map_err(|err| {
        ErrorBody::new(ErrorCode::Conflict, format!("{err}."))
            .with_hint("A sync folder cannot be created inside another sync folder or contain one.")
    })?;
    check_no_disk_marker_overlap(&resolved, folder_id, here)?;

    // 5. Verify the directory can hold `.legion-sync/marker.json` (and write initial marker when
    //    not dry_run).
    verify_or_init_marker(&resolved, folder_id, here, dry_run)?;

    Ok(resolved)
}

fn check_not_network_filesystem(path: &Path) -> Result<(), ErrorBody> {
    #[cfg(any(test, feature = "testing"))]
    if let Ok(prefix) = std::env::var("LEGION_TEST_NETWORK_FS_PREFIX")
        && !prefix.is_empty()
        && path.starts_with(Path::new(&prefix))
    {
        return Err(ErrorBody::new(
            ErrorCode::Unsupported,
            format!(
                "{} is on a network filesystem (simulated network share), which sync folders do not support.",
                path.display()
            ),
        )
        .with_hint("Choose a directory on a local disk."));
    }

    match legion_os::fs_kind::network_filesystem_kind(path) {
        Ok(Some(kind)) => Err(ErrorBody::new(
            ErrorCode::Unsupported,
            format!(
                "{} is on a network filesystem ({kind}), which sync folders do not support.",
                path.display()
            ),
        )
        .with_hint("Choose a directory on a local disk.")),
        Ok(None) => Ok(()),
        Err(err) => Err(ErrorBody::new(
            ErrorCode::Failed,
            format!("Filesystem for {} could not be inspected: {err}.", path.display()),
        )
        .with_hint("Ensure the directory or its parent exists and is accessible.")),
    }
}

fn same_canonical_or_lexical(a: &Path, b: &Path) -> bool {
    let ca = fs::canonicalize(a).unwrap_or_else(|_| a.to_path_buf());
    let cb = fs::canonicalize(b).unwrap_or_else(|_| b.to_path_buf());
    ca == cb
}

fn check_no_disk_marker_overlap(
    root: &Path,
    expected_folder_id: &str,
    expected_member: &str,
) -> Result<(), ErrorBody> {
    // Check strict ancestors for `.legion-sync/marker.json`.
    let canonical_probe = if root.exists() {
        fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf())
    } else {
        root.to_path_buf()
    };
    for ancestor in canonical_probe.ancestors().skip(1) {
        if ancestor.join(SYNC_MARKER_FILE).is_file() {
            return Err(ErrorBody::new(
                ErrorCode::Conflict,
                format!(
                    "{} is inside an existing sync folder rooted at {}.",
                    root.display(),
                    ancestor.display()
                ),
            )
            .with_hint("A sync folder cannot be created inside another sync folder."));
        }
    }

    // Check root's own `.legion-sync/marker.json` if present.
    let own_marker = root.join(SYNC_MARKER_FILE);
    if own_marker.is_file() {
        let bytes = fs::read(&own_marker).map_err(|err| {
            ErrorBody::new(
                ErrorCode::Failed,
                format!(
                    "Existing folder marker {} cannot be read: {err}.",
                    own_marker.display()
                ),
            )
        })?;
        let marker: FolderMarker = serde_json::from_slice(&bytes).map_err(|err| {
            ErrorBody::new(
                ErrorCode::Conflict,
                format!(
                    "{} already contains a corrupt sync marker ({err}).",
                    root.display()
                ),
            )
            .with_hint("Inspect or remove the stale .legion-sync directory before adding this path.")
        })?;
        if marker.folder_id != expected_folder_id || marker.member_id != expected_member {
            return Err(ErrorBody::new(
                ErrorCode::Conflict,
                format!(
                    "{} already holds a sync marker for folder {:?} (member {:?}).",
                    root.display(),
                    marker.folder_id,
                    marker.member_id
                ),
            )
            .with_hint("Choose a different directory, or remove the old sync folder first."));
        }
    }

    // Check strict subdirectories for any nested `.legion-sync/marker.json`.
    if root.is_dir() {
        let mut stack = vec![root.to_path_buf()];
        let mut visited = 0usize;
        while let Some(dir) = stack.pop() {
            let Ok(entries) = fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.flatten() {
                visited += 1;
                if visited > MAX_NESTED_MARKER_SCAN_ENTRIES {
                    return Ok(());
                }
                let Ok(ft) = entry.file_type() else {
                    continue;
                };
                if !ft.is_dir() || ft.is_symlink() {
                    continue;
                }
                let name = entry.file_name();
                if name.to_string_lossy().eq_ignore_ascii_case(SYNC_META_DIR) {
                    if dir != root && entry.path().join("marker.json").is_file() {
                        return Err(ErrorBody::new(
                            ErrorCode::Conflict,
                            format!(
                                "{} contains an existing sync folder at {}.",
                                root.display(),
                                dir.display()
                            ),
                        )
                        .with_hint("A sync folder cannot contain another sync folder."));
                    }
                    continue;
                }
                stack.push(entry.path());
            }
        }
    }

    Ok(())
}

fn verify_or_init_marker(
    root: &Path,
    folder_id: &str,
    member_id: &str,
    dry_run: bool,
) -> Result<(), ErrorBody> {
    let probe_name = format!(".legion-sync-probe-{}", legion_sync::model::random_hex_token());
    if dry_run {
        let probe_dir = if root.exists() {
            let meta_dir = root.join(SYNC_META_DIR);
            if meta_dir.exists() {
                meta_dir
            } else {
                root.to_path_buf()
            }
        } else {
            let mut cur = root;
            loop {
                if cur.exists() {
                    break cur.to_path_buf();
                }
                let Some(parent) = cur.parent() else {
                    return Err(ErrorBody::new(
                        ErrorCode::Failed,
                        format!("{} has no existing parent directory.", root.display()),
                    ));
                };
                cur = parent;
            }
        };
        let probe_file = probe_dir.join(&probe_name);
        legion_core::config::write_atomically(&probe_file, b"ok").map_err(|err| {
            ErrorBody::new(
                ErrorCode::Denied,
                format!(
                    "{} cannot hold the folder identity marker (.legion-sync/marker.json): {err}.",
                    root.display()
                ),
            )
            .with_hint("Check directory permissions and mount options, then try again.")
        })?;
        let _ = fs::remove_file(&probe_file);
        return Ok(());
    }

    if !root.exists() {
        fs::create_dir_all(root).map_err(|err| {
            ErrorBody::new(
                ErrorCode::Failed,
                format!("{} could not be created: {err}.", root.display()),
            )
            .with_hint("Check parent directory permissions and try again.")
        })?;
    }

    let real_fs = RealFs::open(root).map_err(sync_error_to_body)?;
    real_fs.ensure_internal_dirs().map_err(|err| {
        ErrorBody::new(
            ErrorCode::Denied,
            format!(
                "{} cannot hold the folder identity marker (.legion-sync/marker.json): {err}.",
                root.display()
            ),
        )
        .with_hint("Check directory permissions and mount options, then try again.")
    })?;
    match real_fs.read_marker().map_err(sync_error_to_body)? {
        Some(existing) if existing.folder_id == folder_id && existing.member_id == member_id => {
            // Re-verify marker directory is writable.
            let probe = root.join(SYNC_META_DIR).join(&probe_name);
            legion_core::config::write_atomically(&probe, b"ok").map_err(|err| {
                ErrorBody::new(
                    ErrorCode::Denied,
                    format!(
                        "{} cannot update its folder identity marker (.legion-sync/marker.json): {err}.",
                        root.display()
                    ),
                )
            })?;
            let _ = fs::remove_file(&probe);
        }
        Some(other) => {
            return Err(ErrorBody::new(
                ErrorCode::Conflict,
                format!(
                    "{} already holds a marker for folder {:?} (member {:?}).",
                    root.display(),
                    other.folder_id,
                    other.member_id
                ),
            ));
        }
        None => {
            let inc = legion_sync::model::random_incarnation_id().map_err(sync_error_to_body)?;
            let marker = FolderMarker::new(folder_id.to_owned(), member_id.to_owned(), inc);
            real_fs.write_marker(&marker).map_err(|err| {
                ErrorBody::new(
                    ErrorCode::Denied,
                    format!(
                        "{} cannot hold the folder identity marker (.legion-sync/marker.json): {err}.",
                        root.display()
                    ),
                )
                .with_hint("Check directory permissions and mount options, then try again.")
            })?;
        }
    }

    Ok(())
}

/// Opens a local folder's `SyncEngine` when no runner is active, runs `op`, persists updated status,
/// and drops the store cleanly.
fn with_offline_engine<T, F>(
    paths: &Paths,
    here: &str,
    folder_id: &str,
    op: F,
) -> Result<T, ErrorBody>
where
    F: FnOnce(&mut SyncEngine<RealFs, RedbIndexStore>, &SyncFolderEntry, &Path) -> legion_sync::Result<T>,
{
    let sync_file = load_sync_file(paths)?;
    let Some(entry) = sync_file.folders.get(folder_id) else {
        return Err(folder_not_found(folder_id));
    };
    let Some(member) = entry.members.get(here) else {
        return Err(ErrorBody::new(
            ErrorCode::NotFound,
            format!("This machine ({here}) is not a member of sync folder {folder_id:?}."),
        )
        .with_hint("Run this command on a member machine using `--on <machine>`."));
    };

    let root = expand_member_path(paths, &member.path);
    let mut cfg = FolderConfig::new(folder_id, here, entry.members.keys().cloned());
    cfg.former_members = entry.removed_members.keys().cloned().collect();
    cfg.ignore = entry.ignore.clone();
    cfg.bin_retention_secs = u64::from(entry.bin_days).saturating_mul(86_400);

    let real_fs = RealFs::open(&root).map_err(sync_error_to_body)?;
    let store = RedbIndexStore::open(folder_index_path(paths, folder_id)).map_err(sync_error_to_body)?;
    let mut engine = SyncEngine::open(cfg, real_fs, store).map_err(sync_error_to_body)?;
    let out = op(&mut engine, entry, &root).map_err(sync_error_to_body)?;

    if let Ok(status) = engine.status() {
        let held_back = collect_held_back_summaries(&engine, &status);
        let persisted = PersistedFolderStatus {
            folder_id: folder_id.to_owned(),
            folder_name: entry.name.clone(),
            local_path: root.display().to_string(),
            paused_by_config: entry.paused,
            index_damaged: engine.is_index_damaged(),
            held_back,
            updated_at: format_stamp_time(Timestamp::now()),
            status,
        };
        let _ = write_folder_status(paths, &persisted);
    }
    Ok(out)
}

/// Expands a leading `~` in a configured member path using `paths.home()`.
pub fn expand_member_path(paths: &Paths, raw: &str) -> PathBuf {
    if let Some(home) = paths.home() {
        if raw == "~" {
            return home.to_path_buf();
        }
        if let Some(rest) = raw.strip_prefix("~/").or_else(|| raw.strip_prefix("~\\")) {
            return home.join(rest);
        }
    }
    PathBuf::from(raw)
}

/// Converts the current system wall clock to a [`legion_xfer::Timestamp`].
pub fn now_xfer_timestamp() -> XferTimestamp {
    let dur = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO);
    XferTimestamp {
        secs: dur.as_secs().min(i64::MAX as u64) as i64,
        nanos: dur.subsec_nanos(),
    }
}

/// Converts a [`legion_sync::SyncError`] into a structured [`ErrorBody`] with stable code and hint.
pub fn sync_error_to_body(err: SyncError) -> ErrorBody {
    match err {
        SyncError::InvalidConfig { message, hint } => {
            let mut body = ErrorBody::new(ErrorCode::InvalidArgs, format!("{message}."));
            if let Some(h) = hint {
                body = body.with_hint(h);
            }
            body
        }
        SyncError::InvalidPath { path, reason } => ErrorBody::new(
            ErrorCode::InvalidArgs,
            format!("Invalid sync path {path:?}: {reason}."),
        ),
        SyncError::NotFound { what } => {
            ErrorBody::new(ErrorCode::NotFound, format!("{what} was not found."))
        }
        SyncError::Exists { path } => ErrorBody::new(
            ErrorCode::Exists,
            format!("{path:?} already exists."),
        )
        .with_hint("Pass `if_exists=\"replace\"` or `if_exists=\"rename\"`, or choose another destination."),
        SyncError::Denied { member, reason } => ErrorBody::new(
            ErrorCode::Denied,
            format!("Sync access denied for {member:?}: {reason}."),
        ),
        SyncError::Paused { folder_id } => ErrorBody::new(
            ErrorCode::Conflict,
            format!("Sync folder {folder_id:?} is paused."),
        )
        .with_hint("Resume it with `legion sync.resume`."),
        SyncError::FolderUnavailable {
            folder_id,
            sentence,
        } => ErrorBody::new(
            ErrorCode::Failed,
            format!("Sync folder {folder_id:?} is unavailable: {sentence}"),
        )
        .with_hint("Check that the folder or drive is mounted and `.legion-sync/marker.json` is intact."),
        SyncError::IndexDamaged {
            folder_id,
            sentence,
            ..
        } => ErrorBody::new(
            ErrorCode::Failed,
            format!("Sync folder {folder_id:?}: {sentence}"),
        )
        .with_hint("Run `legion sync.rebuild` to rebuild this folder's sync record from disk."),
        SyncError::Conflict { path, reason } => ErrorBody::new(
            ErrorCode::Conflict,
            format!("Sync conflict on {path:?}: {reason}."),
        ),
        SyncError::Unsupported { reason, hint } => {
            let mut body = ErrorBody::new(ErrorCode::Unsupported, format!("{reason}."));
            if let Some(h) = hint {
                body = body.with_hint(h);
            }
            body
        }
        SyncError::SchemaTooNew { found, supported } => ErrorBody::new(
            ErrorCode::Unsupported,
            format!(
                "Sync index schema version {found} is newer than this binary supports ({supported})."
            ),
        )
        .with_hint("Update legion on this machine."),
        SyncError::Cancelled { operation } => ErrorBody::new(
            ErrorCode::Cancelled,
            format!("Sync operation was cancelled ({operation})."),
        ),
        SyncError::Timeout { operation, .. } => ErrorBody::new(
            ErrorCode::Timeout,
            format!("Sync operation timed out ({operation})."),
        ),
        SyncError::PeerUnavailable { member, detail } => ErrorBody::new(
            ErrorCode::Unreachable,
            format!("Peer {member:?} is currently unreachable: {detail}."),
        ),
        other => ErrorBody::new(ErrorCode::Failed, format!("{other}.")),
    }
}

fn validate_machine_arg(machine: &str) -> Result<(), ErrorBody> {
    NodeName::try_from(machine.to_owned()).map(|_| ()).map_err(|reason| {
        ErrorBody::new(
            ErrorCode::InvalidArgs,
            format!("Invalid machine name {machine:?}: {reason}."),
        )
        .with_hint("Use a machine name made of lowercase letters, digits, '-' or '_'.")
    })
}

fn folder_not_found(id: &str) -> ErrorBody {
    ErrorBody::new(
        ErrorCode::NotFound,
        format!("There is no sync folder called {id:?} in sync.toml."),
    )
    .with_hint("Run `legion sync.list` to see configured sync folders.")
}

fn peer_too_old_error(machine: &str) -> ErrorBody {
    ErrorBody::new(
        ErrorCode::Unsupported,
        format!("Machine {machine:?} runs an older version of legion that does not support sync folders."),
    )
    .with_hint(format!(
        "Update legion on {machine} first, then try again. Nothing was created."
    ))
    .with_machine(machine)
}

fn join_error(error: tokio::task::JoinError) -> ErrorBody {
    ErrorBody::new(
        ErrorCode::Internal,
        format!("Sync operation stopped unexpectedly: {error}."),
    )
}
