//! Native background transfer queue shared by all platforms (Android, iOS/iPadOS, macOS,
//! Windows, and Linux).
//!
//! Downloads stream through [`legion_link::Link::open_content_with_cancel`] into a private `.part`
//! file with bounded 64 KiB memory, resume from the partial file's byte length across network
//! drops, pauses, process death, or restarts, verify the completed file against `fs.hash`
//! (BLAKE3), and finalize into the platform's Downloads destination (or open-externally cache).
//!
//! Uploads compute the source's BLAKE3 digest in bounded 64 KiB chunks, stream through
//! [`legion_link::Link::open_put_with_cancel`] resuming from the remote `.legion-part` offset, and
//! finish once the node verifies the hash and renames the file into place.

#[cfg(target_os = "android")]
pub mod android;
#[cfg(not(any(target_os = "android", target_os = "ios")))]
pub mod desktop;
pub mod file_policy;
#[cfg(target_os = "ios")]
pub mod ios;

use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io::{SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use legion_content::extract_file_name;
use legion_link::{CAP_IF_EXISTS, ContentTime, PutIfExists, PutRequest};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tauri::ipc::Channel as IpcChannel;
use tempfile::NamedTempFile;
use tokio::fs::File as AsyncFile;
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
use tokio::sync::{Mutex as AsyncMutex, Notify, watch};
use tokio_util::sync::CancellationToken;

use crate::backend::{AppError, Backend, BoxFuture, ContentTarget, ErrorCode};
pub use crate::transfers::file_policy::{
    REFUSED_OPEN_HINT, REFUSED_OPEN_MESSAGE, can_open_file_name, is_refused_open_file_name,
    mime_for_file_name, place_file_without_overwrite, sanitize_file_name, validate_child_in_dir,
};

/// Default maximum concurrent transfers running at once.
pub const DEFAULT_MAX_CONCURRENT: usize = 2;

/// Default size threshold above which a transfer counts as "large" for the Wi-Fi-only rule
/// (50 MB, matching the interface's SI byte units).
pub const DEFAULT_LARGE_THRESHOLD_BYTES: u64 = 50 * 1000 * 1000;

/// Default progress stall timeout when a connected machine stops sending or accepting bytes.
pub const DEFAULT_STALL_TIMEOUT: Duration = Duration::from_secs(45);

/// Maximum session-scoped upload handles retained in memory at once.
const MAX_PICKED_UPLOAD_HANDLES: usize = 128;

/// Maximum live UI progress watchers retained at once.
const MAX_PROGRESS_WATCHERS: usize = 64;

/// Bounded streaming buffer size used for downloads, uploads, and BLAKE3 hashing (64 KiB).
const COPY_BUF_BYTES: usize = 64 * 1024;

/// Minimum interval between progress broadcasts sent to the UI over IPC channels.
const UI_PROGRESS_INTERVAL: Duration = Duration::from_millis(200);

/// Minimum interval between progress updates pushed to the platform notification layer (1 Hz
/// matches Android `NotificationManager` rate limits while keeping progress bars smooth).
const NOTIFY_PROGRESS_INTERVAL: Duration = Duration::from_millis(1000);

/// How long a single transfer keeps retrying transient network drops without making progress
/// before surfacing `failed` (enough to ride through a 20+ second `sshd` restart or brief
/// network switch).
const DEFAULT_RETRY_WINDOW: Duration = Duration::from_secs(90);

/// Maximum terminal (`done`, `failed`, `cancelled`) entries retained in `queue.json`.
const MAX_RETAINED_TERMINAL: usize = 200;

/// Schema version for `transfers/queue.json`.
const QUEUE_FILE_VERSION: u32 = 1;

/// Default retry interval when removing a cancelled transfer's leftover files fails (for example,
/// when another process briefly holds a file lock on Windows).
const DEFAULT_CLEANUP_RETRY_INTERVAL: Duration = Duration::from_millis(500);

/// Quick immediate retry backoffs inside [`TransferManager::control`] before surfacing a
/// `Cancelling` removal failure to the UI and handing off to the background cleanup retry loop.
const CANCEL_IMMEDIATE_RETRY_DELAYS: [Duration; 3] = [
    Duration::from_millis(20),
    Duration::from_millis(50),
    Duration::from_millis(80),
];

/// Direction bytes move relative to this device.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TransferDirection {
    /// Machine -> device.
    Download,
    /// Device -> machine.
    Upload,
}

/// Queue lifecycle state of one device transfer (matches `files-contract.js`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TransferState {
    /// Waiting for an execution slot.
    Queued,
    /// Actively transferring or verifying bytes.
    Running,
    /// Paused by the user, Wi-Fi-only policy, iOS backgrounding, or Android FGS timeout.
    Paused,
    /// Cancel requested; waiting for worker exit and removal of created files.
    Cancelling,
    /// Verified and placed in its destination.
    Done,
    /// Stopped after an unrecoverable error or exhausted retries.
    Failed,
    /// Cancelled by the user; every file created by this transfer has been removed.
    Cancelled,
}

impl TransferState {
    /// True while the transfer is actively queued or running.
    pub const fn is_active(self) -> bool {
        matches!(self, Self::Queued | Self::Running)
    }

    /// True when the transfer has reached a terminal state.
    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::Done | Self::Failed | Self::Cancelled)
    }
}

/// Why a transfer entered [`TransferState::Paused`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PauseReason {
    /// Explicitly paused by the user.
    User,
    /// Waiting for an unmetered Wi-Fi connection because `wifi_only_large` is enabled.
    WifiOnly,
    /// Automatically paused when iOS/iPadOS moved the app to the background.
    Background,
    /// Automatically paused when Android's `dataSync` foreground-service time limit fired.
    FgsTimeout,
    /// Automatically paused after the peer stopped making transfer progress within the stall timeout.
    Stalled,
}

/// Action accepted by `transfer_control`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TransferControlAction {
    /// Pause a queued or running transfer while keeping its partial file.
    Pause,
    /// Resume a paused or failed transfer from its partial file.
    Resume,
    /// Cancel a transfer and discard its partial file.
    Cancel,
}

/// Structured error attached to a failed transfer or external open attempt.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TransferError {
    /// Machine-readable error code (`snake_case`).
    pub code: String,
    /// One-sentence human-readable summary.
    pub message: String,
    /// Optional actionable hint.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
    /// Optional structured sub-reason (for example `"no_handler"`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

impl From<&AppError> for TransferError {
    fn from(err: &AppError) -> Self {
        let code = serde_json::to_value(err.code)
            .ok()
            .and_then(|v| v.as_str().map(str::to_owned))
            .unwrap_or_else(|| "failed".to_owned());
        let reason = err
            .details
            .as_ref()
            .and_then(|d| d.get("reason"))
            .and_then(Value::as_str)
            .map(str::to_owned);
        Self {
            code,
            message: err.message.clone(),
            hint: err.hint.clone(),
            reason,
        }
    }
}

fn is_false(v: &bool) -> bool {
    !*v
}

/// Public snapshot of one device transfer matching `apps/ui/link/files-contract.js`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceTransfer {
    /// Stable transfer identifier.
    pub id: String,
    /// Whether bytes move to or from this device.
    pub direction: TransferDirection,
    /// Peer machine name.
    pub machine: String,
    /// Remote file path on `machine`.
    pub path: String,
    /// Display file name.
    pub name: String,
    /// Device destination or source path/URI (shown in technical mode).
    #[serde(rename = "localPath", default, skip_serializing_if = "Option::is_none")]
    pub local_path: Option<String>,
    /// MIME type when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mime: Option<String>,
    /// Current queue state.
    pub state: TransferState,
    /// Bytes transferred so far.
    pub bytes_done: u64,
    /// Total file size in bytes (0 until discovered).
    pub bytes_total: u64,
    /// Current smoothed throughput in bytes/second when running.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate: Option<u64>,
    /// Failure details when `state == Failed`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<TransferError>,
    /// ISO-8601 UTC creation timestamp.
    pub created: String,
    /// ISO-8601 UTC completion timestamp when terminal.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished: Option<String>,
    /// Verified BLAKE3 digest (`blake3:<64 hex>`) once computed/verified.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hash: Option<String>,
    /// True when tapping the finished row opens the local file directly.
    #[serde(rename = "canOpen")]
    pub can_open: bool,
    /// True when the finished download can be revealed in its folder (offered for saved files
    /// including refused executable/installer types).
    #[serde(rename = "canShowInFolder", default)]
    pub can_show_in_folder: bool,
    /// Plain explanation when opening the saved file directly is refused by policy.
    #[serde(
        rename = "openBlockedReason",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub open_blocked_reason: Option<String>,
    /// Reason this transfer is paused, when `state == Paused`.
    #[serde(
        rename = "pausedReason",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub paused_reason: Option<PauseReason>,
    /// Error surfaced when launching the external viewer/chooser failed after download finished.
    #[serde(rename = "openError", default, skip_serializing_if = "Option::is_none")]
    pub open_error: Option<TransferError>,
    /// True when this download was initiated by "Open in another app" into the app cache.
    #[serde(rename = "openExternally", default, skip_serializing_if = "is_false")]
    pub open_externally: bool,
}

/// User-configurable queue settings persisted alongside the queue.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TransferSettings {
    /// When true, transfers at or above [`Self::large_threshold_bytes`] pause on metered networks
    /// and resume automatically on unmetered Wi-Fi. Defaults to `false`.
    #[serde(default)]
    pub wifi_only_large: bool,
    /// Byte threshold defining a "large" transfer (default 50 MB).
    #[serde(default = "default_large_threshold")]
    pub large_threshold_bytes: u64,
}

fn default_large_threshold() -> u64 {
    DEFAULT_LARGE_THRESHOLD_BYTES
}

impl Default for TransferSettings {
    fn default() -> Self {
        Self {
            wifi_only_large: false,
            large_threshold_bytes: DEFAULT_LARGE_THRESHOLD_BYTES,
        }
    }
}

/// Result returned immediately by `transfer_download`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TransferIdResult {
    /// Identifier of the newly queued transfer.
    pub id: String,
}

/// Result returned immediately by `transfer_open_externally`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpenExternallyResult {
    /// Identifier of the queued or reused transfer.
    pub id: String,
    /// Always true when queued or opened cleanly.
    pub ok: bool,
    /// True when an unchanged cached copy was already on this device and opened immediately
    /// without downloading again.
    #[serde(rename = "openedImmediately", default)]
    pub opened_immediately: bool,
}

/// Pre-selected upload file accepted ONLY in debug builds compiled with the `test-upload-paths`
/// feature (and Rust unit tests).
#[cfg(any(test, all(debug_assertions, feature = "test-upload-paths")))]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct UploadFileInput {
    /// File display name.
    pub name: String,
    /// Local filesystem path or Android `content://` URI.
    #[serde(default)]
    pub path: Option<String>,
    /// Optional Android `content://` URI alias for `path`.
    #[serde(default)]
    pub uri: Option<String>,
    /// Optional known file size in bytes.
    #[serde(default)]
    pub size: Option<u64>,
    /// Optional MIME type.
    #[serde(default)]
    pub mime: Option<String>,
}

/// Opaque session-only handle returned to the page after the user selects a file via the native
/// picker (`transfer_pick_uploads`). Never exposes a local path or Android `content://` URI.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PickedUploadHandle {
    /// Opaque random token valid only for this app session and consumed when enqueued.
    pub handle: String,
    /// Sanitized file display name with its real extension.
    pub name: String,
    /// File size in bytes when known (0 if unknown until read).
    pub size: u64,
    /// Device-derived MIME type from the file's real extension.
    pub mime: String,
}

/// File returned by the platform's native upload picker.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PickedUploadFile {
    /// Local filesystem path or Android `content://` URI.
    pub local_path: String,
    /// Display file name.
    pub name: String,
    /// File size in bytes when known (0 if unknown until read).
    #[serde(default)]
    pub size: u64,
    /// Optional MIME type.
    #[serde(default)]
    pub mime: Option<String>,
}

/// Result returned by `transfer_upload` (or `None` when the user cancelled the picker).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct UploadEnqueueResult {
    /// First queued transfer id.
    pub id: String,
    /// All queued transfer ids (when multiple files were picked).
    pub ids: Vec<String>,
    /// Number of queued uploads.
    pub count: usize,
}

/// Result returned by `open_transfer`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpenTransferResult {
    /// True once the OS opener/chooser was launched.
    pub ok: bool,
}

/// Summary pushed to platform services (Android foreground notification, desktop hidden-window
/// auto-exit).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QueueNotificationSummary {
    /// Count of queued + running transfers.
    pub active_count: usize,
    /// Count of currently running transfers.
    pub running_count: usize,
    /// Count of paused transfers (kept visible on Android so Resume/Cancel buttons stay usable).
    pub paused_count: usize,
    /// Count of transfers in `cancelling` state still waiting for worker exit or file removal.
    #[serde(default)]
    pub cancelling_count: usize,
    /// Notification title line.
    pub title: String,
    /// Notification detail line.
    pub detail: String,
    /// Sum of completed bytes across active (or paused) transfers.
    pub bytes_done: u64,
    /// Sum of total bytes across active (or paused) transfers.
    pub bytes_total: u64,
    /// True when total size is not yet known.
    pub indeterminate: bool,
    /// True when a Pause action is meaningful.
    pub can_pause: bool,
    /// True when a Resume action is meaningful.
    pub can_resume: bool,
    /// Primary transfer id when a single transfer is active/paused (empty when multiple).
    pub primary_transfer_id: Option<String>,
}

/// Streaming byte source for uploads (regular file or Android `content://` URI).
pub trait UploadByteSource: Send {
    /// Total byte length when known (0 if unknown until EOF).
    fn known_size(&self) -> Option<u64>;

    /// Reads the next chunk into `buf`, returning 0 at EOF.
    fn read_chunk<'a>(&'a mut self, buf: &'a mut [u8]) -> BoxFuture<'a, Result<usize, AppError>>;

    /// Positions the reader at `offset` bytes from the start (used when resuming uploads).
    fn seek_to<'a>(&'a mut self, offset: u64) -> BoxFuture<'a, Result<(), AppError>>;
}

/// Standard filesystem implementation of [`UploadByteSource`] (used on desktop, iOS, and tests).
pub struct FileUploadSource {
    file: AsyncFile,
    size: u64,
}

impl FileUploadSource {
    /// Opens `path` as a streaming read-only upload source.
    pub async fn open(path: &Path) -> Result<Self, AppError> {
        let meta = tokio::fs::metadata(path).await.map_err(|err| {
            AppError::new(
                ErrorCode::NotFound,
                format!("Could not read local file {}: {err}.", path.display()),
            )
            .with_hint("Check that the selected file still exists and is readable.")
        })?;
        if !meta.is_file() {
            return Err(AppError::new(
                ErrorCode::InvalidArgs,
                format!("{} is not a regular file.", path.display()),
            )
            .with_hint("Pick a regular file to upload."));
        }
        let file = AsyncFile::open(path).await.map_err(|err| {
            AppError::new(
                ErrorCode::Failed,
                format!("Could not open local file {}: {err}.", path.display()),
            )
        })?;
        Ok(Self {
            file,
            size: meta.len(),
        })
    }
}

impl UploadByteSource for FileUploadSource {
    fn known_size(&self) -> Option<u64> {
        Some(self.size)
    }

    fn read_chunk<'a>(&'a mut self, buf: &'a mut [u8]) -> BoxFuture<'a, Result<usize, AppError>> {
        Box::pin(async move {
            self.file.read(buf).await.map_err(|err| {
                AppError::new(
                    ErrorCode::Failed,
                    format!("Could not read bytes from local upload file: {err}."),
                )
            })
        })
    }

    fn seek_to<'a>(&'a mut self, offset: u64) -> BoxFuture<'a, Result<(), AppError>> {
        Box::pin(async move {
            self.file
                .seek(SeekFrom::Start(offset))
                .await
                .map(|_| ())
                .map_err(|err| {
                    AppError::new(
                        ErrorCode::Failed,
                        format!("Could not seek local upload file to offset {offset}: {err}."),
                    )
                })
        })
    }
}

/// Platform hook implemented for Android, Desktop, iOS, and unit tests.
pub trait TransferPlatform: Send + Sync + 'static {
    /// Runs before the first user-initiated transfer (on Android, requests `POST_NOTIFICATIONS`
    /// with a one-sentence rationale).
    fn before_user_transfer(&self) -> BoxFuture<'_, Result<(), AppError>> {
        Box::pin(async { Ok(()) })
    }

    /// Returns true when the device is currently on an unmetered Wi-Fi (or wired) connection.
    fn is_unmetered_wifi(&self) -> bool {
        true
    }

    /// True when this platform implements "Open in another app".
    fn supports_open_externally(&self) -> bool {
        true
    }

    /// True when this platform can open finished downloads directly in a viewer/chooser.
    fn supports_open_downloads(&self) -> bool {
        true
    }

    /// True when this platform can reveal a finished download in its folder.
    fn supports_show_in_folder(&self) -> bool {
        true
    }

    /// Opens the platform's native file picker and returns chosen upload files (empty if cancelled).
    fn pick_upload_files(&self) -> BoxFuture<'_, Result<Vec<PickedUploadFile>, AppError>>;

    /// Opens an upload source (`local_path` may be a filesystem path or Android `content://` URI).
    fn open_upload_source<'a>(
        &'a self,
        local_path: &'a str,
    ) -> BoxFuture<'a, Result<Box<dyn UploadByteSource>, AppError>> {
        Box::pin(async move {
            let src = FileUploadSource::open(Path::new(local_path)).await?;
            Ok(Box::new(src) as Box<dyn UploadByteSource>)
        })
    }

    /// Releases any platform-held permission on an upload source once terminal (Android URIs).
    fn release_upload_source<'a>(&'a self, _local_path: &'a str) -> BoxFuture<'a, ()> {
        Box::pin(async {})
    }

    /// Moves or copies a verified downloaded `.part` file into its final device location and
    /// returns the final local path or URI.
    fn finalize_download<'a>(
        &'a self,
        id: &'a str,
        part_path: &'a Path,
        file_name: &'a str,
        mime: Option<&'a str>,
        open_after_done: bool,
    ) -> BoxFuture<'a, Result<String, AppError>>;

    /// Removes a download `.part` file once no worker holds it open. Returns `Ok(())` when removed
    /// or already absent; returns `Err` when the OS refuses deletion (e.g. file locked on Windows)
    /// so cancellation stays truthful and retries.
    fn delete_part_file<'a>(&'a self, part_path: &'a Path) -> BoxFuture<'a, Result<(), AppError>> {
        Box::pin(async move { remove_file_if_present(part_path).await })
    }

    /// Deletes a finalized download destination (local file, open-externally cache copy, or
    /// Android MediaStore / FileProvider `content://` entry) when a transfer is cancelled or a
    /// cached copy is replaced by a permanent Downloads save. Returns `Ok(())` when removed or
    /// already absent, or `Err` when removal fails so cancellation stays truthful and retries.
    fn delete_saved_download<'a>(
        &'a self,
        local_path: &'a str,
    ) -> BoxFuture<'a, Result<(), AppError>> {
        Box::pin(async move {
            if local_path.starts_with("content://") {
                return Ok(());
            }
            remove_file_if_present(Path::new(local_path)).await
        })
    }

    /// Returns true when a cached "open in another app" copy at `local_path` still exists and
    /// matches `expected_size` bytes.
    fn cached_file_exists<'a>(
        &'a self,
        local_path: &'a str,
        expected_size: u64,
    ) -> BoxFuture<'a, bool> {
        Box::pin(async move {
            if local_path.starts_with("content://") {
                return false;
            }
            tokio::fs::metadata(local_path)
                .await
                .map(|m| m.is_file() && m.len() == expected_size)
                .unwrap_or(false)
        })
    }

    /// Opens a finished local file or URI in the system viewer or chooser.
    fn open_local_file<'a>(
        &'a self,
        local_path: &'a str,
        mime: Option<&'a str>,
    ) -> BoxFuture<'a, Result<(), AppError>>;

    /// Reveals a finished download in its folder (never executes the file).
    fn show_in_folder<'a>(&'a self, _local_path: &'a str) -> BoxFuture<'a, Result<(), AppError>> {
        Box::pin(async {
            Err(AppError::new(
                ErrorCode::Unsupported,
                "Showing a file in its folder is not supported on this platform.",
            ))
        })
    }

    /// Called whenever queue summary changes (drives Android foreground notification and desktop
    /// hidden-window completion exit).
    fn on_queue_updated(&self, _summary: QueueNotificationSummary) {}

    /// Called when a single transfer completes (drives Android completion notification).
    fn on_transfer_completed(&self, _transfer: &DeviceTransfer) {}
}

/// Internal persisted record for one transfer.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct TransferRecord {
    pub transfer: DeviceTransfer,
    /// Session-only intent to launch the external chooser once this download finishes. Never
    /// persisted across app restarts so restarted queues never launch apps unexpectedly.
    #[serde(skip)]
    pub open_after_done: bool,
    /// Set once `finalize_download` has already saved the verified bytes so resuming after a
    /// pause-during-save race is idempotent and never runs `finalize_download` a second time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finalized_local_path: Option<String>,
    /// Verified BLAKE3 hash recorded alongside `finalized_local_path`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finalized_hash: Option<String>,
    /// True when `finalized_local_path` points at the ephemeral `open-externally` cache rather
    /// than Downloads. If the app restarts and `open_after_done` no longer applies, resuming
    /// promotes or re-downloads the file into Downloads so a `Done` download never lives only in
    /// a 24 h pruned cache.
    #[serde(default)]
    pub finalized_in_cache: bool,
    /// True once an upload has chosen its collision-free remote path so retries resume it.
    #[serde(default)]
    pub remote_path_resolved: bool,
    /// True once an upload has opened a `put` stream on its locked remote path so a restart after
    /// an updater close or crash verifies whether the target already committed the file before
    /// sending again or stepping to a numbered sibling.
    #[serde(default)]
    pub upload_started: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote_mtime_secs: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote_mtime_nanos: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote_file_id: Option<u64>,
    /// Saved files, cache copies, or MediaStore URIs created by this transfer that must be removed
    /// before a `Cancelling` transfer transitions to `Cancelled`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pending_cleanup_paths: Vec<String>,
    /// True while a `Cancelling` download still needs its `.part` file removed (or verified gone).
    #[serde(default)]
    pub cleanup_part: bool,
    /// True once a completed download was staged in the ephemeral `open-externally` cache (kept
    /// across `Done` so repeat "Open in another app" taps can reuse the unchanged cached file).
    #[serde(default)]
    pub cached_open: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct PersistedQueue {
    #[serde(default = "default_queue_version")]
    version: u32,
    #[serde(default)]
    settings: TransferSettings,
    #[serde(default)]
    next_seq: u64,
    #[serde(default)]
    items: Vec<TransferRecord>,
}

fn default_queue_version() -> u32 {
    QUEUE_FILE_VERSION
}

impl Default for PersistedQueue {
    fn default() -> Self {
        Self {
            version: QUEUE_FILE_VERSION,
            settings: TransferSettings::default(),
            next_seq: 1,
            items: Vec::new(),
        }
    }
}

#[derive(Clone)]
struct RunningWorker {
    worker_id: u64,
    cancel: CancellationToken,
    finished: watch::Receiver<bool>,
}

struct WorkerFinishedGuard {
    tx: Option<watch::Sender<bool>>,
}

impl WorkerFinishedGuard {
    fn new(tx: watch::Sender<bool>) -> Self {
        Self { tx: Some(tx) }
    }

    fn signal(&mut self) {
        if let Some(tx) = self.tx.take() {
            let _ = tx.send(true);
        }
    }
}

impl Drop for WorkerFinishedGuard {
    fn drop(&mut self) {
        self.signal();
    }
}

struct QueueState {
    settings: TransferSettings,
    next_seq: u64,
    next_worker_id: u64,
    next_watcher_id: u64,
    items: Vec<TransferRecord>,
    running: HashMap<String, RunningWorker>,
    watchers: HashMap<u64, IpcChannel<Vec<DeviceTransfer>>>,
    picked_uploads: HashMap<String, PickedUploadFile>,
    last_ui_broadcast: Instant,
    last_notify_broadcast: Instant,
}

/// Spawns an async task on the current Tokio runtime if entered (e.g. unit tests), or on Tauri's
/// shared async runtime when called from a platform UI thread (e.g. `setup` or window events).
pub(crate) fn spawn_bg<F>(fut: F)
where
    F: std::future::Future<Output = ()> + Send + 'static,
{
    if let Ok(handle) = tokio::runtime::Handle::try_current() {
        handle.spawn(fut);
    } else {
        tauri::async_runtime::spawn(fut);
    }
}

/// Managed Tauri state wrapper for [`TransferManager`].
pub struct SharedTransfers(pub Arc<TransferManager>);

/// Shared native transfer queue with disk persistence, concurrency limiting, BLAKE3 verification,
/// resumable downloads/uploads, and Wi-Fi-only filtering for large transfers.
pub struct TransferManager {
    data_dir: PathBuf,
    backend: Arc<dyn Backend>,
    platform: Arc<dyn TransferPlatform>,
    max_concurrent: usize,
    retry_window: Duration,
    stall_timeout: Duration,
    cleanup_retry_interval: Duration,
    state: Mutex<QueueState>,
    persist_seq: AtomicU64,
    persist_lock: Mutex<u64>,
    cleanup_lock: AsyncMutex<()>,
    schedule_notify: Notify,
    cleanup_notify: Notify,
    scheduler_started: AtomicBool,
    shutdown_token: CancellationToken,
    #[allow(dead_code)]
    id_salt: AtomicU64,
    #[cfg(test)]
    download_pause_once: Mutex<Option<Arc<DownloadPauseGate>>>,
}

#[cfg(test)]
struct DownloadPauseGate {
    entered: Notify,
    release: Notify,
}

impl TransferManager {
    /// Loads persisted queue state from `<data_dir>/transfers/queue.json` (creating directories as
    /// needed) and constructs the manager. Call [`Self::start`] once a Tokio runtime is active to
    /// resume any unfinished transfers.
    pub fn load_or_new(
        data_dir: impl AsRef<Path>,
        backend: Arc<dyn Backend>,
        platform: Arc<dyn TransferPlatform>,
    ) -> Arc<Self> {
        Self::with_timeouts(
            data_dir,
            backend,
            platform,
            DEFAULT_MAX_CONCURRENT,
            DEFAULT_RETRY_WINDOW,
            DEFAULT_STALL_TIMEOUT,
        )
    }

    /// Constructs a manager with explicit concurrency and retry window (for deterministic tests).
    pub fn with_options(
        data_dir: impl AsRef<Path>,
        backend: Arc<dyn Backend>,
        platform: Arc<dyn TransferPlatform>,
        max_concurrent: usize,
        retry_window: Duration,
    ) -> Arc<Self> {
        Self::with_timeouts(
            data_dir,
            backend,
            platform,
            max_concurrent,
            retry_window,
            DEFAULT_STALL_TIMEOUT,
        )
    }

    /// Constructs a manager with explicit concurrency, retry window, and progress stall timeout.
    pub fn with_timeouts(
        data_dir: impl AsRef<Path>,
        backend: Arc<dyn Backend>,
        platform: Arc<dyn TransferPlatform>,
        max_concurrent: usize,
        retry_window: Duration,
        stall_timeout: Duration,
    ) -> Arc<Self> {
        Self::with_all_options(
            data_dir,
            backend,
            platform,
            max_concurrent,
            retry_window,
            stall_timeout,
            DEFAULT_CLEANUP_RETRY_INTERVAL,
        )
    }

    #[cfg(test)]
    pub(crate) fn with_cleanup_retry_interval(
        data_dir: impl AsRef<Path>,
        backend: Arc<dyn Backend>,
        platform: Arc<dyn TransferPlatform>,
        max_concurrent: usize,
        retry_window: Duration,
        cleanup_retry_interval: Duration,
    ) -> Arc<Self> {
        Self::with_all_options(
            data_dir,
            backend,
            platform,
            max_concurrent,
            retry_window,
            DEFAULT_STALL_TIMEOUT,
            cleanup_retry_interval,
        )
    }

    fn with_all_options(
        data_dir: impl AsRef<Path>,
        backend: Arc<dyn Backend>,
        platform: Arc<dyn TransferPlatform>,
        max_concurrent: usize,
        retry_window: Duration,
        stall_timeout: Duration,
        cleanup_retry_interval: Duration,
    ) -> Arc<Self> {
        let transfers_dir = data_dir.as_ref().join("transfers");
        let parts_dir = transfers_dir.join("parts");
        let _ = fs::create_dir_all(&parts_dir);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(&transfers_dir, fs::Permissions::from_mode(0o700));
            let _ = fs::set_permissions(&parts_dir, fs::Permissions::from_mode(0o700));
        }

        let mut loaded = load_queue_file(&transfers_dir.join("queue.json"));
        let unmetered = platform.is_unmetered_wifi();

        // Normalize persisted transfers after restart / process death / reboot:
        // - Anything that was `running` becomes `queued` so it resumes immediately from `.part`.
        // - Background-paused (iOS) or FGS-timeout-paused transfers become `queued` on next start.
        // - WifiOnly-paused transfers resume if we are now on unmetered Wi-Fi.
        // - Sync `bytes_done` for downloads with the actual `.part` size on disk.
        // - Enforce sanitized names and device-derived MIME / refused-open flags on persisted rows.
        let mut changed = false;
        let mut live_download_ids = std::collections::HashSet::new();
        for rec in &mut loaded.items {
            rec.open_after_done = false;
            let safe_name = sanitize_file_name(&rec.transfer.name);
            if rec.transfer.name != safe_name {
                rec.transfer.name = safe_name;
                changed = true;
            }
            if rec.transfer.direction == TransferDirection::Download {
                let device_mime = mime_for_file_name(&rec.transfer.name).to_owned();
                if rec.transfer.mime.as_deref() != Some(device_mime.as_str()) {
                    rec.transfer.mime = Some(device_mime);
                    changed = true;
                }
                if rec.transfer.state == TransferState::Done {
                    let allowed_open = can_open_file_name(&rec.transfer.name);
                    let want_open = platform.supports_open_downloads() && allowed_open;
                    let want_show = platform.supports_show_in_folder();
                    let want_reason = (!allowed_open).then(|| REFUSED_OPEN_MESSAGE.to_owned());
                    if rec.transfer.can_open != want_open
                        || rec.transfer.can_show_in_folder != want_show
                        || rec.transfer.open_blocked_reason != want_reason
                    {
                        rec.transfer.can_open = want_open;
                        rec.transfer.can_show_in_folder = want_show;
                        rec.transfer.open_blocked_reason = want_reason;
                        changed = true;
                    }
                }
            }
            if rec.transfer.direction == TransferDirection::Download
                && !rec.transfer.state.is_terminal()
                && rec.transfer.state != TransferState::Cancelling
            {
                live_download_ids.insert(rec.transfer.id.clone());
                if rec.finalized_local_path.is_none() {
                    let part = parts_dir.join(format!("{}.part", rec.transfer.id));
                    let on_disk = fs::metadata(&part).map(|m| m.len()).unwrap_or(0);
                    if rec.transfer.bytes_done != on_disk {
                        rec.transfer.bytes_done = on_disk;
                        changed = true;
                    }
                }
            }
            if rec.transfer.state == TransferState::Cancelling {
                if rec.transfer.direction == TransferDirection::Download {
                    live_download_ids.insert(rec.transfer.id.clone());
                    rec.cleanup_part = true;
                    if let Some(saved) = rec.finalized_local_path.take() {
                        push_unique_cleanup_path(&mut rec.pending_cleanup_paths, saved);
                        changed = true;
                    }
                    if let Some(lp) = rec.transfer.local_path.take() {
                        push_unique_cleanup_path(&mut rec.pending_cleanup_paths, lp);
                        changed = true;
                    }
                }
                rec.finalized_hash = None;
                rec.finalized_in_cache = false;
            }
            rec.transfer.rate = None;
            match rec.transfer.state {
                TransferState::Running => {
                    rec.transfer.state = TransferState::Queued;
                    rec.transfer.paused_reason = None;
                    changed = true;
                }
                TransferState::Paused => match rec.transfer.paused_reason {
                    Some(PauseReason::Background | PauseReason::FgsTimeout) => {
                        rec.transfer.state = TransferState::Queued;
                        rec.transfer.paused_reason = None;
                        changed = true;
                    }
                    Some(PauseReason::WifiOnly)
                        if !loaded.settings.wifi_only_large || unmetered =>
                    {
                        rec.transfer.state = TransferState::Queued;
                        rec.transfer.paused_reason = None;
                        changed = true;
                    }
                    _ => {}
                },
                _ => {}
            }
        }

        // Sweep any orphaned `.part` files whose transfer id is no longer active or paused.
        sweep_orphaned_parts(&parts_dir, &live_download_ids);

        let salt = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(1);

        let manager = Arc::new(Self {
            data_dir: transfers_dir,
            backend,
            platform,
            max_concurrent: max_concurrent.max(1),
            retry_window,
            stall_timeout,
            cleanup_retry_interval,
            state: Mutex::new(QueueState {
                settings: loaded.settings,
                next_seq: loaded.next_seq.max(1),
                next_worker_id: 1,
                next_watcher_id: 1,
                items: loaded.items,
                running: HashMap::new(),
                watchers: HashMap::new(),
                picked_uploads: HashMap::new(),
                last_ui_broadcast: Instant::now() - UI_PROGRESS_INTERVAL,
                last_notify_broadcast: Instant::now() - NOTIFY_PROGRESS_INTERVAL,
            }),
            persist_seq: AtomicU64::new(0),
            persist_lock: Mutex::new(0),
            cleanup_lock: AsyncMutex::new(()),
            schedule_notify: Notify::new(),
            cleanup_notify: Notify::new(),
            scheduler_started: AtomicBool::new(false),
            shutdown_token: CancellationToken::new(),
            id_salt: AtomicU64::new(salt),
            #[cfg(test)]
            download_pause_once: Mutex::new(None),
        });

        if changed {
            manager.persist_locked_snapshot();
        }
        manager
    }

    /// Starts the background scheduler and cancellation-cleanup loops on the active Tokio runtime
    /// and resumes any queued or cancelling transfers loaded from disk.
    pub fn start(self: &Arc<Self>) {
        if self.scheduler_started.swap(true, Ordering::SeqCst) {
            self.schedule_notify.notify_one();
            self.cleanup_notify.notify_one();
            return;
        }
        let mgr = Arc::clone(self);
        spawn_bg(async move {
            mgr.run_scheduler().await;
        });
        let cleanup_mgr = Arc::clone(self);
        spawn_bg(async move {
            cleanup_mgr.run_cleanup_loop().await;
        });
        self.broadcast_state_change();
        self.schedule_notify.notify_one();
        self.cleanup_notify.notify_one();
    }

    /// Stops all running workers cleanly without cancelling persisted transfers (so unfinished work
    /// resumes on next start).
    pub fn shutdown(&self) {
        self.shutdown_token.cancel();
        let tokens: Vec<CancellationToken> = {
            let guard = self
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            guard.running.values().map(|w| w.cancel.clone()).collect()
        };
        for tok in tokens {
            tok.cancel();
        }
        self.schedule_notify.notify_one();
        self.cleanup_notify.notify_one();
    }

    /// Waits up to `timeout` for every running transfer worker to observe cancellation, close its
    /// underlying stream, and persist the queue snapshot cleanly. Returns `true` when zero workers
    /// remain running.
    pub async fn wait_for_workers_stopped(&self, timeout: Duration) -> bool {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let watchers: Vec<watch::Receiver<bool>> = {
                let guard = self
                    .state
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                guard.running.values().map(|w| w.finished.clone()).collect()
            };
            if watchers.is_empty() {
                return true;
            }
            for mut rx in watchers {
                if *rx.borrow() {
                    continue;
                }
                let Some(remaining) = deadline.checked_duration_since(tokio::time::Instant::now())
                else {
                    return false;
                };
                match tokio::time::timeout(remaining, rx.wait_for(|done| *done)).await {
                    Ok(_) => {}
                    Err(_) => return false,
                }
            }
        }
    }

    /// Returns true if any transfer is currently `queued`, `running`, or `cancelling` (still
    /// stopping its worker or removing partial/saved files).
    pub fn has_active_work(&self) -> bool {
        let guard = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        guard
            .items
            .iter()
            .any(|r| r.transfer.state.is_active() || r.transfer.state == TransferState::Cancelling)
    }

    /// Returns true if any transfer is `queued`, `running`, `paused`, or `cancelling` (used on
    /// Android so the foreground service stays alive while transfers or cancel cleanup finish).
    pub fn has_unfinished_or_paused(&self) -> bool {
        let guard = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        guard.items.iter().any(|r| {
            r.transfer.state.is_active()
                || matches!(
                    r.transfer.state,
                    TransferState::Paused | TransferState::Cancelling
                )
        })
    }

    /// Returns the current list of transfers (newest first).
    pub fn list(&self) -> Vec<DeviceTransfer> {
        let guard = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        snapshot_transfers(&guard.items)
    }

    /// Returns current queue settings (`wifi_only_large`, `large_threshold_bytes`).
    pub fn settings(&self) -> TransferSettings {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .settings
    }

    /// Updates `wifi_only_large`, pausing large active transfers on metered networks or resuming
    /// `WifiOnly`-paused transfers when disabled or on unmetered Wi-Fi.
    pub fn set_wifi_only_large(self: &Arc<Self>, enabled: bool) -> TransferSettings {
        let unmetered = self.platform.is_unmetered_wifi();
        let mut to_cancel = Vec::new();
        let updated = {
            let mut guard = self
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            guard.settings.wifi_only_large = enabled;
            let threshold = guard.settings.large_threshold_bytes;
            let state = &mut *guard;
            if enabled && !unmetered {
                for rec in &mut state.items {
                    if rec.transfer.state.is_active() && rec.transfer.bytes_total >= threshold {
                        rec.transfer.state = TransferState::Paused;
                        rec.transfer.paused_reason = Some(PauseReason::WifiOnly);
                        rec.transfer.rate = None;
                        if let Some(worker) = state.running.get(&rec.transfer.id) {
                            to_cancel.push(worker.cancel.clone());
                        }
                    }
                }
            } else if !enabled || unmetered {
                for rec in &mut state.items {
                    if rec.transfer.state == TransferState::Paused
                        && rec.transfer.paused_reason == Some(PauseReason::WifiOnly)
                    {
                        rec.transfer.state = TransferState::Queued;
                        rec.transfer.paused_reason = None;
                    }
                }
            }
            state.settings
        };
        for tok in to_cancel {
            tok.cancel();
        }
        self.persist_locked_snapshot();
        self.broadcast_state_change();
        self.schedule_notify.notify_one();
        updated
    }

    /// Notifies the queue that network meter status changed (e.g. from Android
    /// `ConnectivityManager`).
    pub fn on_network_changed(self: &Arc<Self>, unmetered_wifi: bool) {
        let mut to_cancel = Vec::new();
        let mut changed = false;
        {
            let mut guard = self
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if !guard.settings.wifi_only_large {
                return;
            }
            let threshold = guard.settings.large_threshold_bytes;
            let state = &mut *guard;
            if unmetered_wifi {
                for rec in &mut state.items {
                    if rec.transfer.state == TransferState::Paused
                        && rec.transfer.paused_reason == Some(PauseReason::WifiOnly)
                    {
                        rec.transfer.state = TransferState::Queued;
                        rec.transfer.paused_reason = None;
                        changed = true;
                    }
                }
            } else {
                for rec in &mut state.items {
                    if rec.transfer.state.is_active() && rec.transfer.bytes_total >= threshold {
                        rec.transfer.state = TransferState::Paused;
                        rec.transfer.paused_reason = Some(PauseReason::WifiOnly);
                        rec.transfer.rate = None;
                        changed = true;
                        if let Some(worker) = state.running.get(&rec.transfer.id) {
                            to_cancel.push(worker.cancel.clone());
                        }
                    }
                }
            }
        }
        for tok in to_cancel {
            tok.cancel();
        }
        if changed {
            self.persist_locked_snapshot();
            self.broadcast_state_change();
            self.schedule_notify.notify_one();
        }
    }

    /// Pauses all active (`queued` / `running`) transfers with `reason` (used by iOS background
    /// suspension and Android `dataSync` foreground service `onTimeout`), leaving user-paused
    /// transfers untouched.
    pub fn pause_all_active(&self, reason: PauseReason) {
        let mut to_cancel = Vec::new();
        let mut changed = false;
        {
            let mut guard = self
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let state = &mut *guard;
            for rec in &mut state.items {
                if rec.transfer.state.is_active() {
                    rec.transfer.state = TransferState::Paused;
                    rec.transfer.paused_reason = Some(reason);
                    rec.transfer.rate = None;
                    changed = true;
                    if let Some(worker) = state.running.get(&rec.transfer.id) {
                        to_cancel.push(worker.cancel.clone());
                    }
                }
            }
        }
        for tok in to_cancel {
            tok.cancel();
        }
        if changed {
            self.persist_locked_snapshot();
            self.broadcast_state_change();
        }
    }

    /// Resumes transfers previously paused for `reason` (used when iOS returns to foreground).
    pub fn resume_paused_for(&self, reason: PauseReason) {
        let mut changed = false;
        {
            let mut guard = self
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            for rec in &mut guard.items {
                if rec.transfer.state == TransferState::Paused
                    && rec.transfer.paused_reason == Some(reason)
                {
                    rec.transfer.state = TransferState::Queued;
                    rec.transfer.paused_reason = None;
                    changed = true;
                }
            }
        }
        if changed {
            self.persist_locked_snapshot();
            self.broadcast_state_change();
            self.schedule_notify.notify_one();
        }
    }

    /// Applies a notification action (`pause`, `resume`, `cancel`) either to one transfer id or to
    /// all currently active/paused transfers when `transfer_id` is empty.
    pub async fn handle_notification_action(
        self: &Arc<Self>,
        action: TransferControlAction,
        transfer_id: Option<&str>,
    ) {
        if let Some(id) = transfer_id.map(str::trim).filter(|s| !s.is_empty()) {
            let _ = self.control(id, action).await;
            return;
        }
        let target_ids: Vec<String> = {
            let guard = self
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            guard
                .items
                .iter()
                .filter(|r| match action {
                    TransferControlAction::Pause => r.transfer.state.is_active(),
                    TransferControlAction::Resume => r.transfer.state == TransferState::Paused,
                    TransferControlAction::Cancel => {
                        r.transfer.state.is_active()
                            || matches!(
                                r.transfer.state,
                                TransferState::Paused | TransferState::Cancelling
                            )
                    }
                })
                .map(|r| r.transfer.id.clone())
                .collect()
        };
        for id in target_ids {
            let _ = self.control(&id, action).await;
        }
    }

    /// Subscribes an IPC channel to live transfer queue snapshots, immediately sending the current
    /// snapshot, and returns a stable watcher id for explicit removal via [`Self::unwatch`].
    pub fn watch(&self, channel: IpcChannel<Vec<DeviceTransfer>>) -> u64 {
        let (watcher_id, snapshot) = {
            let mut guard = self
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let id = guard.next_watcher_id;
            guard.next_watcher_id = guard.next_watcher_id.saturating_add(1);
            if guard.watchers.len() >= MAX_PROGRESS_WATCHERS
                && let Some(&oldest_id) = guard.watchers.keys().min()
            {
                guard.watchers.remove(&oldest_id);
            }
            let snap = snapshot_transfers(&guard.items);
            guard.watchers.insert(id, channel.clone());
            (id, snap)
        };
        if channel.send(snapshot).is_err() {
            self.unwatch(watcher_id);
        }
        watcher_id
    }

    /// Removes a previously registered progress watcher by its identity.
    pub fn unwatch(&self, watcher_id: u64) {
        let mut guard = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        guard.watchers.remove(&watcher_id);
    }

    /// Enqueues a non-blocking download (either to Downloads or to open-externally cache) and
    /// returns its id immediately.
    pub async fn enqueue_download(
        self: &Arc<Self>,
        machine: String,
        path: String,
        name: Option<String>,
        size: Option<u64>,
        _untrusted_mime: Option<String>,
        open_after_done: bool,
    ) -> Result<String, AppError> {
        let machine_trimmed = machine.trim().to_owned();
        if machine_trimmed.is_empty() {
            return Err(
                AppError::new(ErrorCode::InvalidArgs, "Machine name cannot be empty.")
                    .with_hint("Pass the name of a paired machine."),
            );
        }
        if path.is_empty() {
            return Err(
                AppError::new(ErrorCode::InvalidArgs, "File path cannot be empty.")
                    .with_hint("Pass an absolute or `~`-prefixed file path."),
            );
        }
        let inferred_name = extract_file_name(&path).map_err(AppError::from)?;
        let display_name = name
            .as_deref()
            .filter(|s| !s.trim().is_empty())
            .map(sanitize_file_name)
            .unwrap_or_else(|| sanitize_file_name(&inferred_name));

        if open_after_done {
            if !self.platform.supports_open_externally() {
                return Err(AppError::new(
                    ErrorCode::Unsupported,
                    "Opening files directly in another app is not yet supported on this platform.",
                )
                .with_hint("Download the file to save it instead."));
            }
            if !can_open_file_name(&display_name) {
                return Err(AppError::new(ErrorCode::Denied, REFUSED_OPEN_MESSAGE)
                    .with_hint(REFUSED_OPEN_HINT));
            }
        }

        // Always derive MIME on the device from the sanitized file's real extension, never from the
        // remote machine's listing.
        let resolved_mime = Some(mime_for_file_name(&display_name).to_owned());

        // Verify the machine is paired before enqueueing.
        let _ = self.backend.content_target(&machine_trimmed)?;

        // Ask Android notification permission lazily on first background download/upload, but
        // never block foreground "Open in another app" staging on a modal permission dialog.
        if !open_after_done {
            let _ = self.platform.before_user_transfer().await;
        }

        let id = self.allocate_id();
        let bytes_total = size.unwrap_or(0);
        let initial_state = self.initial_state_for_size(bytes_total);
        let paused_reason =
            (initial_state == TransferState::Paused).then_some(PauseReason::WifiOnly);

        let record = TransferRecord {
            transfer: DeviceTransfer {
                id: id.clone(),
                direction: TransferDirection::Download,
                machine: machine_trimmed,
                path,
                name: display_name,
                local_path: None,
                mime: resolved_mime,
                state: initial_state,
                bytes_done: 0,
                bytes_total,
                rate: None,
                error: None,
                created: now_rfc3339_utc(),
                finished: None,
                hash: None,
                can_open: false,
                can_show_in_folder: false,
                open_blocked_reason: None,
                paused_reason,
                open_error: None,
                open_externally: open_after_done,
            },
            open_after_done,
            finalized_local_path: None,
            finalized_hash: None,
            finalized_in_cache: false,
            remote_path_resolved: false,
            upload_started: false,
            remote_mtime_secs: None,
            remote_mtime_nanos: None,
            remote_file_id: None,
            pending_cleanup_paths: Vec::new(),
            cleanup_part: false,
            cached_open: false,
        };

        let pruned = {
            let mut guard = self
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            guard.items.push(record);
            prune_terminal_locked(&mut guard.items)
        };
        self.cleanup_pruned_records(pruned).await;

        self.start();
        self.persist_locked_snapshot();
        self.broadcast_state_change();
        self.schedule_notify.notify_one();
        Ok(id)
    }

    /// Opens `machine:path` in another app on this device.
    ///
    /// Reuses an already-active external-open download for the same file, opens an unchanged file
    /// directly from the app cache when still present (after verifying remote size/mtime/file_id),
    /// or otherwise enqueues a resumable cache download that opens the system chooser once
    /// verified.
    pub async fn open_externally(
        self: &Arc<Self>,
        machine: String,
        path: String,
        name: Option<String>,
        size: Option<u64>,
        untrusted_mime: Option<String>,
    ) -> Result<OpenExternallyResult, AppError> {
        let machine_trimmed = machine.trim().to_owned();
        if machine_trimmed.is_empty() {
            return Err(
                AppError::new(ErrorCode::InvalidArgs, "Machine name cannot be empty.")
                    .with_hint("Pass the name of a paired machine."),
            );
        }
        if path.is_empty() {
            return Err(
                AppError::new(ErrorCode::InvalidArgs, "File path cannot be empty.")
                    .with_hint("Pass an absolute or `~`-prefixed file path."),
            );
        }
        if !self.platform.supports_open_externally() {
            return Err(AppError::new(
                ErrorCode::Unsupported,
                "Opening files directly in another app is not yet supported on this platform.",
            )
            .with_hint("Download the file to save it instead."));
        }
        let inferred_name = extract_file_name(&path).map_err(AppError::from)?;
        let display_name = name
            .as_deref()
            .filter(|s| !s.trim().is_empty())
            .map(sanitize_file_name)
            .unwrap_or_else(|| sanitize_file_name(&inferred_name));
        if !can_open_file_name(&display_name) {
            return Err(
                AppError::new(ErrorCode::Denied, REFUSED_OPEN_MESSAGE).with_hint(REFUSED_OPEN_HINT)
            );
        }

        let target = self.backend.content_target(&machine_trimmed)?;

        // 1) Deduplicate against an already-active external-open download for the exact same file.
        // 2) Otherwise look for the newest completed cache copy we can reuse if unchanged.
        let cached_candidate = {
            let guard = self
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if let Some(active) = guard.items.iter().rev().find(|r| {
                r.open_after_done
                    && r.transfer.direction == TransferDirection::Download
                    && r.transfer.state.is_active()
                    && r.transfer.machine == machine_trimmed
                    && r.transfer.path == path
            }) {
                return Ok(OpenExternallyResult {
                    id: active.transfer.id.clone(),
                    ok: true,
                    opened_immediately: false,
                });
            }
            guard
                .items
                .iter()
                .rev()
                .find(|r| {
                    r.cached_open
                        && r.transfer.direction == TransferDirection::Download
                        && r.transfer.state == TransferState::Done
                        && r.transfer.machine == machine_trimmed
                        && r.transfer.path == path
                        && r.transfer.local_path.is_some()
                        && r.remote_mtime_secs.is_some()
                })
                .and_then(|r| {
                    Some((
                        r.transfer.id.clone(),
                        r.transfer.local_path.clone()?,
                        r.transfer.name.clone(),
                        r.transfer.bytes_total,
                        r.remote_mtime_secs?,
                        r.remote_mtime_nanos,
                        r.remote_file_id,
                    ))
                })
        };

        if let Some((
            cached_id,
            cached_path,
            cached_name,
            cached_size,
            cached_mtime_secs,
            cached_mtime_nanos,
            cached_file_id,
        )) = cached_candidate
        {
            let probe = target
                .link
                .open_content(&target.link_node, &path, 0, Some(0))
                .await
                .map_err(AppError::from)?;
            let remote_size = probe.size();
            let remote_mtime = probe.mtime();
            let remote_file_id = probe.file_id();
            drop(probe);

            let unchanged = remote_size == cached_size
                && remote_mtime.secs == cached_mtime_secs
                && cached_mtime_nanos == Some(remote_mtime.nanos)
                && cached_file_id == remote_file_id;
            if unchanged
                && self
                    .platform
                    .cached_file_exists(&cached_path, remote_size)
                    .await
            {
                let device_mime = mime_for_file_name(&cached_name);
                self.platform
                    .open_local_file(&cached_path, Some(device_mime))
                    .await?;
                let cleared = {
                    let mut guard = self
                        .state
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                    if let Some(rec) = guard.items.iter_mut().find(|r| r.transfer.id == cached_id)
                        && rec.transfer.open_error.is_some()
                    {
                        rec.transfer.open_error = None;
                        true
                    } else {
                        false
                    }
                };
                if cleared {
                    self.persist_locked_snapshot();
                    self.broadcast_state_change();
                }
                return Ok(OpenExternallyResult {
                    id: cached_id,
                    ok: true,
                    opened_immediately: true,
                });
            }
        }

        let id = self
            .enqueue_download(
                machine_trimmed,
                path,
                Some(display_name),
                size,
                untrusted_mime,
                true,
            )
            .await?;
        Ok(OpenExternallyResult {
            id,
            ok: true,
            opened_immediately: false,
        })
    }

    /// Opens the platform's native file picker and registers each selected file under a fresh
    /// session-only opaque handle so the web view never receives or supplies device paths/URIs.
    pub async fn pick_uploads(&self) -> Result<Vec<PickedUploadHandle>, AppError> {
        let chosen = self.platform.pick_upload_files().await?;
        if chosen.is_empty() {
            return Ok(Vec::new());
        }
        let mut handles = Vec::with_capacity(chosen.len());
        let mut to_release = Vec::new();
        {
            let mut guard = self
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            for mut file in chosen {
                file.name = sanitize_file_name(&file.name);
                let mime = mime_for_file_name(&file.name).to_owned();
                file.mime = Some(mime.clone());
                let token = random_opaque_handle()?;
                if guard.picked_uploads.len() >= MAX_PICKED_UPLOAD_HANDLES
                    && let Some(evict_key) = guard.picked_uploads.keys().next().cloned()
                    && let Some(evicted) = guard.picked_uploads.remove(&evict_key)
                {
                    to_release.push(evicted.local_path);
                }
                handles.push(PickedUploadHandle {
                    handle: token.clone(),
                    name: file.name.clone(),
                    size: file.size,
                    mime,
                });
                guard.picked_uploads.insert(token, file);
            }
        }
        for old_path in to_release {
            self.platform.release_upload_source(&old_path).await;
        }
        Ok(handles)
    }

    /// Enqueues non-blocking bulk-pipe `put` uploads into `machine:dir`.
    ///
    /// When `handles` is `None` or empty, opens the native system file picker directly inside Rust
    /// (returning `Ok(None)` if cancelled). When `handles` is supplied, accepts ONLY opaque
    /// session handles previously returned by [`Self::pick_uploads`]; arbitrary device paths or
    /// URIs are always refused.
    pub async fn enqueue_upload(
        self: &Arc<Self>,
        machine: String,
        dir: String,
        handles: Option<Vec<String>>,
    ) -> Result<Option<UploadEnqueueResult>, AppError> {
        let (machine_trimmed, dir_trimmed) = self.validate_upload_target(&machine, &dir)?;

        let picked: Vec<PickedUploadFile> = match handles {
            Some(supplied) if !supplied.is_empty() => {
                let mut guard = self
                    .state
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                // Verify every handle exists BEFORE consuming any so a partial/forged list never
                // leaks or mutates session state.
                for raw in &supplied {
                    if !guard.picked_uploads.contains_key(raw.trim()) {
                        return Err(AppError::new(
                            ErrorCode::Denied,
                            "Upload handle is invalid or expired.",
                        )
                        .with_hint("Select files again using the device's system file picker."));
                    }
                }
                let mut out = Vec::with_capacity(supplied.len());
                for raw in supplied {
                    if let Some(file) = guard.picked_uploads.remove(raw.trim()) {
                        out.push(file);
                    }
                }
                out
            }
            _ => {
                let chosen = self.platform.pick_upload_files().await?;
                if chosen.is_empty() {
                    return Ok(None);
                }
                chosen
            }
        };

        self.enqueue_picked_uploads(machine_trimmed, dir_trimmed, picked)
            .await
    }

    /// Debug/test-only upload entrypoint accepting pre-selected local file paths. Compiled ONLY
    /// in unit tests or debug builds explicitly enabling the `test-upload-paths` feature.
    #[cfg(any(test, all(debug_assertions, feature = "test-upload-paths")))]
    pub async fn enqueue_upload_paths_for_test(
        self: &Arc<Self>,
        machine: String,
        dir: String,
        files: Vec<UploadFileInput>,
    ) -> Result<Option<UploadEnqueueResult>, AppError> {
        let (machine_trimmed, dir_trimmed) = self.validate_upload_target(&machine, &dir)?;
        if files.is_empty() {
            return Ok(None);
        }
        let mut picked = Vec::with_capacity(files.len());
        for item in files {
            let local_path = item
                .path
                .or(item.uri)
                .map(|s| s.trim().to_owned())
                .filter(|s| !s.is_empty())
                .ok_or_else(|| {
                    AppError::new(
                        ErrorCode::InvalidArgs,
                        "Supplied upload file is missing a local path or URI.",
                    )
                })?;
            let safe_name = sanitize_file_name(&item.name);
            let size = item
                .size
                .unwrap_or_else(|| fs::metadata(&local_path).map(|m| m.len()).unwrap_or(0));
            let mime = Some(mime_for_file_name(&safe_name).to_owned());
            picked.push(PickedUploadFile {
                local_path,
                name: safe_name,
                size,
                mime,
            });
        }
        self.enqueue_picked_uploads(machine_trimmed, dir_trimmed, picked)
            .await
    }

    fn validate_upload_target(
        &self,
        machine: &str,
        dir: &str,
    ) -> Result<(String, String), AppError> {
        let machine_trimmed = machine.trim().to_owned();
        if machine_trimmed.is_empty() {
            return Err(
                AppError::new(ErrorCode::InvalidArgs, "Machine name cannot be empty.")
                    .with_hint("Pass the name of a paired machine."),
            );
        }
        let dir_trimmed = dir.trim().to_owned();
        if dir_trimmed.is_empty() {
            return Err(AppError::new(
                ErrorCode::InvalidArgs,
                "Destination folder path cannot be empty.",
            )
            .with_hint("Pass the target directory path on the machine."));
        }
        // Verify machine is paired before opening the file picker.
        let _ = self.backend.content_target(&machine_trimmed)?;
        Ok((machine_trimmed, dir_trimmed))
    }

    async fn enqueue_picked_uploads(
        self: &Arc<Self>,
        machine_trimmed: String,
        dir_trimmed: String,
        picked: Vec<PickedUploadFile>,
    ) -> Result<Option<UploadEnqueueResult>, AppError> {
        // Ask Android notification permission lazily on first user-initiated transfer.
        let _ = self.platform.before_user_transfer().await;

        let mut ids = Vec::with_capacity(picked.len());
        let created = now_rfc3339_utc();
        let pruned = {
            let mut guard = self
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let wifi_only = guard.settings.wifi_only_large;
            let threshold = guard.settings.large_threshold_bytes;
            let unmetered = self.platform.is_unmetered_wifi();

            for file in picked {
                let safe_name = sanitize_file_name(&file.name);
                let seq = guard.next_seq;
                guard.next_seq = guard.next_seq.saturating_add(1);
                let salt = self.id_salt.load(Ordering::Relaxed) & 0xFFFF;
                let id = format!("tr-{salt:04x}-{seq}");
                let remote_path = join_remote_path(&dir_trimmed, &safe_name);
                let initial_state = if wifi_only && !unmetered && file.size >= threshold {
                    TransferState::Paused
                } else {
                    TransferState::Queued
                };
                let paused_reason =
                    (initial_state == TransferState::Paused).then_some(PauseReason::WifiOnly);
                let mime = Some(mime_for_file_name(&safe_name).to_owned());
                guard.items.push(TransferRecord {
                    transfer: DeviceTransfer {
                        id: id.clone(),
                        direction: TransferDirection::Upload,
                        machine: machine_trimmed.clone(),
                        path: remote_path,
                        name: safe_name,
                        local_path: Some(file.local_path),
                        mime,
                        state: initial_state,
                        bytes_done: 0,
                        bytes_total: file.size,
                        rate: None,
                        error: None,
                        created: created.clone(),
                        finished: None,
                        hash: None,
                        can_open: false,
                        can_show_in_folder: false,
                        open_blocked_reason: None,
                        paused_reason,
                        open_error: None,
                        open_externally: false,
                    },
                    open_after_done: false,
                    finalized_local_path: None,
                    finalized_hash: None,
                    finalized_in_cache: false,
                    remote_path_resolved: false,
                    upload_started: false,
                    remote_mtime_secs: None,
                    remote_mtime_nanos: None,
                    remote_file_id: None,
                    pending_cleanup_paths: Vec::new(),
                    cleanup_part: false,
                    cached_open: false,
                });
                ids.push(id);
            }
            prune_terminal_locked(&mut guard.items)
        };
        self.cleanup_pruned_records(pruned).await;

        let Some(first_id) = ids.first().cloned() else {
            return Ok(None);
        };

        self.start();
        self.persist_locked_snapshot();
        self.broadcast_state_change();
        self.schedule_notify.notify_one();

        Ok(Some(UploadEnqueueResult {
            id: first_id,
            count: ids.len(),
            ids,
        }))
    }

    /// Pauses, resumes, or cancels transfer `id`, returning its updated [`DeviceTransfer`] snapshot.
    ///
    /// For [`TransferControlAction::Cancel`], this waits for any active worker to stop and close
    /// its file handles, then removes every file created by the transfer (`.part`, saved download /
    /// MediaStore entry, or cache copy) before transitioning to [`TransferState::Cancelled`],
    /// broadcasting the state change, and returning. If the OS refuses to remove a file (for
    /// example while locked on Windows), the state remains [`TransferState::Cancelling`] with the
    /// removal error recorded, and the background cleanup loop keeps retrying until removal
    /// succeeds.
    pub async fn control(
        self: &Arc<Self>,
        id: &str,
        action: TransferControlAction,
    ) -> Result<DeviceTransfer, AppError> {
        if action == TransferControlAction::Cancel {
            return self.cancel_transfer(id).await;
        }

        let mut cancel_token: Option<CancellationToken> = None;

        let snapshot = {
            let mut guard = self
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let Some(pos) = guard.items.iter().position(|r| r.transfer.id == id) else {
                return Err(AppError::new(
                    ErrorCode::NotFound,
                    format!("No transfer found with id `{id}`."),
                ));
            };
            let current = guard.items[pos].transfer.state;
            match action {
                TransferControlAction::Pause => {
                    if current.is_active() {
                        guard.items[pos].transfer.state = TransferState::Paused;
                        guard.items[pos].transfer.paused_reason = Some(PauseReason::User);
                        guard.items[pos].transfer.rate = None;
                        cancel_token = guard.running.get(id).map(|w| w.cancel.clone());
                    }
                }
                TransferControlAction::Resume => {
                    if matches!(current, TransferState::Paused | TransferState::Failed) {
                        guard.items[pos].transfer.state = TransferState::Queued;
                        guard.items[pos].transfer.paused_reason = None;
                        guard.items[pos].transfer.error = None;
                        guard.items[pos].transfer.finished = None;
                    }
                }
                TransferControlAction::Cancel => unreachable!(),
            }
            guard.items[pos].transfer.clone()
        };

        if let Some(tok) = cancel_token {
            tok.cancel();
        }
        if action == TransferControlAction::Resume {
            self.start();
        }
        self.persist_locked_snapshot();
        self.broadcast_state_change();
        self.schedule_notify.notify_one();
        Ok(snapshot)
    }

    async fn cancel_transfer(self: &Arc<Self>, id: &str) -> Result<DeviceTransfer, AppError> {
        let (early_return, worker_running, cancel_token) = {
            let mut guard = self
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let Some(pos) = guard.items.iter().position(|r| r.transfer.id == id) else {
                return Err(AppError::new(
                    ErrorCode::NotFound,
                    format!("No transfer found with id `{id}`."),
                ));
            };
            let current = guard.items[pos].transfer.state;
            if matches!(current, TransferState::Done | TransferState::Cancelled) {
                (Some(guard.items[pos].transfer.clone()), false, None)
            } else {
                let was_cancelling = current == TransferState::Cancelling;
                let is_download =
                    guard.items[pos].transfer.direction == TransferDirection::Download;
                guard.items[pos].transfer.state = TransferState::Cancelling;
                guard.items[pos].transfer.paused_reason = None;
                guard.items[pos].transfer.rate = None;
                if !was_cancelling {
                    guard.items[pos].transfer.error = None;
                }
                guard.items[pos].open_after_done = false;
                if is_download {
                    guard.items[pos].cleanup_part = true;
                    if let Some(saved) = guard.items[pos].finalized_local_path.take() {
                        push_unique_cleanup_path(
                            &mut guard.items[pos].pending_cleanup_paths,
                            saved,
                        );
                    }
                    if let Some(lp) = guard.items[pos].transfer.local_path.take() {
                        push_unique_cleanup_path(&mut guard.items[pos].pending_cleanup_paths, lp);
                    }
                } else {
                    guard.items[pos].finalized_local_path = None;
                }
                guard.items[pos].finalized_hash = None;
                guard.items[pos].finalized_in_cache = false;
                let tok = guard.running.get(id).map(|w| w.cancel.clone());
                let running = tok.is_some();
                (None, running, tok)
            }
        };

        if let Some(snapshot) = early_return {
            return Ok(snapshot);
        }

        if let Some(tok) = cancel_token {
            tok.cancel();
        }
        if worker_running {
            self.persist_locked_snapshot();
            self.broadcast_state_change();
        }

        // Wait until any worker for `id` has exited, dropped all file handles, recorded any
        // finalized output into `pending_cleanup_paths`, and released its worker slot.
        loop {
            let next_worker = {
                let guard = self
                    .state
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                guard
                    .running
                    .get(id)
                    .map(|w| (w.cancel.clone(), w.finished.clone()))
            };
            let Some((tok, mut finished_rx)) = next_worker else {
                break;
            };
            tok.cancel();
            if *finished_rx.borrow() {
                tokio::task::yield_now().await;
                continue;
            }
            let _ = finished_rx.changed().await;
        }

        self.attempt_cancel_cleanup(id, true).await
    }

    async fn attempt_cancel_cleanup(
        self: &Arc<Self>,
        id: &str,
        quick_retries: bool,
    ) -> Result<DeviceTransfer, AppError> {
        let _cleanup_guard = self.cleanup_lock.lock().await;

        let (is_download, mut need_part, mut pending_paths, release_upload_uri) = {
            let guard = self
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let Some(rec) = guard.items.iter().find(|r| r.transfer.id == id) else {
                return Err(AppError::new(
                    ErrorCode::NotFound,
                    format!("No transfer found with id `{id}`."),
                ));
            };
            if rec.transfer.state != TransferState::Cancelling {
                return Ok(rec.transfer.clone());
            }
            if guard.running.contains_key(id) {
                return Ok(rec.transfer.clone());
            }
            let is_download = rec.transfer.direction == TransferDirection::Download;
            let release_uri = (!is_download)
                .then(|| rec.transfer.local_path.clone())
                .flatten();
            (
                is_download,
                is_download && rec.cleanup_part,
                rec.pending_cleanup_paths.clone(),
                release_uri,
            )
        };

        let part_path = self.part_path(id);
        let max_attempts = if quick_retries {
            1 + CANCEL_IMMEDIATE_RETRY_DELAYS.len()
        } else {
            1
        };
        let mut last_err: Option<AppError> = None;

        for attempt in 0..max_attempts {
            if attempt > 0 {
                let delay = CANCEL_IMMEDIATE_RETRY_DELAYS[attempt - 1];
                tokio::select! {
                    biased;
                    () = self.shutdown_token.cancelled() => break,
                    () = tokio::time::sleep(delay) => {}
                }
            }
            last_err = None;

            if need_part {
                match self.platform.delete_part_file(&part_path).await {
                    Ok(()) => need_part = false,
                    Err(err) => {
                        if last_err.is_none() {
                            last_err = Some(err);
                        }
                    }
                }
            }

            if !pending_paths.is_empty() {
                let mut still_remaining = Vec::new();
                for path in std::mem::take(&mut pending_paths) {
                    if let Err(err) = self.platform.delete_saved_download(&path).await {
                        if last_err.is_none() {
                            last_err = Some(err);
                        }
                        still_remaining.push(path);
                    }
                }
                pending_paths = still_remaining;
            }

            if !need_part && pending_paths.is_empty() {
                break;
            }
        }

        if !need_part && pending_paths.is_empty() {
            if let Some(uri) = release_upload_uri {
                self.platform.release_upload_source(&uri).await;
            }
            let (snapshot, pruned) = {
                let mut guard = self
                    .state
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                let Some(pos) = guard.items.iter().position(|r| r.transfer.id == id) else {
                    return Err(AppError::new(
                        ErrorCode::NotFound,
                        format!("No transfer found with id `{id}`."),
                    ));
                };
                if guard.items[pos].transfer.state != TransferState::Cancelling {
                    return Ok(guard.items[pos].transfer.clone());
                }
                guard.items[pos].cleanup_part = false;
                guard.items[pos].pending_cleanup_paths.clear();
                guard.items[pos].finalized_local_path = None;
                guard.items[pos].finalized_hash = None;
                guard.items[pos].finalized_in_cache = false;
                guard.items[pos].open_after_done = false;
                guard.items[pos].transfer.state = TransferState::Cancelled;
                guard.items[pos].transfer.paused_reason = None;
                guard.items[pos].transfer.rate = None;
                guard.items[pos].transfer.error = None;
                guard.items[pos].transfer.finished = Some(now_rfc3339_utc());
                let snap = guard.items[pos].transfer.clone();
                let pruned = prune_terminal_locked(&mut guard.items);
                (snap, pruned)
            };
            self.cleanup_pruned_records(pruned).await;
            self.persist_locked_snapshot();
            self.broadcast_state_change();
            self.schedule_notify.notify_one();
            return Ok(snapshot);
        }

        let err = last_err.unwrap_or_else(|| {
            AppError::new(
                ErrorCode::Failed,
                "Could not remove cancelled transfer files yet; retrying automatically.",
            )
            .with_hint(
                "Close any app using the file; Legion will finish cancelling once it is free.",
            )
        });
        let (snapshot, changed) = {
            let mut guard = self
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let Some(pos) = guard.items.iter().position(|r| r.transfer.id == id) else {
                return Err(AppError::new(
                    ErrorCode::NotFound,
                    format!("No transfer found with id `{id}`."),
                ));
            };
            if guard.items[pos].transfer.state != TransferState::Cancelling {
                return Ok(guard.items[pos].transfer.clone());
            }
            let next_cleanup_part = is_download && need_part;
            let next_err = Some(TransferError::from(&err));
            let changed = guard.items[pos].cleanup_part != next_cleanup_part
                || guard.items[pos].pending_cleanup_paths != pending_paths
                || guard.items[pos].transfer.error != next_err;
            guard.items[pos].cleanup_part = next_cleanup_part;
            guard.items[pos].pending_cleanup_paths = pending_paths;
            guard.items[pos].transfer.error = next_err;
            (guard.items[pos].transfer.clone(), changed)
        };
        if changed {
            self.persist_locked_snapshot();
            self.broadcast_state_change();
        }
        if quick_retries {
            self.start();
            self.cleanup_notify.notify_one();
        }
        Ok(snapshot)
    }

    async fn run_cleanup_loop(self: Arc<Self>) {
        loop {
            if self.shutdown_token.is_cancelled() {
                return;
            }

            let cancelling_ids: Vec<String> = {
                let guard = self
                    .state
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                guard
                    .items
                    .iter()
                    .filter(|r| {
                        r.transfer.state == TransferState::Cancelling
                            && !guard.running.contains_key(&r.transfer.id)
                    })
                    .map(|r| r.transfer.id.clone())
                    .collect()
            };

            let mut any_still_pending = false;
            for id in cancelling_ids {
                if self.shutdown_token.is_cancelled() {
                    return;
                }
                if let Ok(snap) = self.attempt_cancel_cleanup(&id, false).await
                    && snap.state == TransferState::Cancelling
                {
                    any_still_pending = true;
                }
            }

            if any_still_pending {
                tokio::select! {
                    biased;
                    () = self.shutdown_token.cancelled() => return,
                    () = self.cleanup_notify.notified() => {}
                    () = tokio::time::sleep(self.cleanup_retry_interval) => {}
                }
            } else {
                tokio::select! {
                    biased;
                    () = self.shutdown_token.cancelled() => return,
                    () = self.cleanup_notify.notified() => {}
                }
            }
        }
    }

    /// Opens a finished downloaded file on this device using the platform viewer / chooser.
    ///
    /// Refuses executable, installer, script, shortcut, and disk-image extensions regardless of
    /// what MIME type the remote machine advertised, and derives the viewer MIME type strictly on
    /// this device from the saved filename's real extension.
    pub async fn open_transfer(&self, id: &str) -> Result<OpenTransferResult, AppError> {
        if !self.platform.supports_open_downloads() {
            return Err(AppError::new(
                ErrorCode::Unsupported,
                "Opening downloaded files directly is not yet supported on this platform.",
            )
            .with_hint("Find the saved file in the Files app under Legion."));
        }
        let (local_path, file_name) = {
            let guard = self
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let Some(rec) = guard.items.iter().find(|r| r.transfer.id == id) else {
                return Err(AppError::new(
                    ErrorCode::NotFound,
                    format!("No transfer found with id `{id}`."),
                ));
            };
            if rec.transfer.state != TransferState::Done {
                return Err(AppError::new(
                    ErrorCode::Conflict,
                    "This transfer is not finished yet.",
                )
                .with_hint("Wait until the download completes before opening it."));
            }
            if rec.transfer.direction != TransferDirection::Download {
                return Err(AppError::new(
                    ErrorCode::Unsupported,
                    "Only downloaded files can be opened from device transfers.",
                ));
            }
            if !can_open_file_name(&rec.transfer.name) {
                return Err(AppError::new(ErrorCode::Denied, REFUSED_OPEN_MESSAGE)
                    .with_hint(REFUSED_OPEN_HINT));
            }
            let Some(path) = rec.transfer.local_path.clone() else {
                return Err(AppError::new(
                    ErrorCode::NotFound,
                    "The downloaded file path is no longer recorded.",
                ));
            };
            (path, rec.transfer.name.clone())
        };
        let device_mime = mime_for_file_name(&file_name);
        self.platform
            .open_local_file(&local_path, Some(device_mime))
            .await?;
        Ok(OpenTransferResult { ok: true })
    }

    /// Reveals a finished downloaded file in the device's file manager / Downloads folder.
    ///
    /// Looks up the path by transfer `id`; the page never supplies a device filesystem path.
    pub async fn show_transfer_in_folder(&self, id: &str) -> Result<OpenTransferResult, AppError> {
        if !self.platform.supports_show_in_folder() {
            return Err(AppError::new(
                ErrorCode::Unsupported,
                "Showing files in a folder is not supported on this platform.",
            ));
        }
        let local_path = {
            let guard = self
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let Some(rec) = guard.items.iter().find(|r| r.transfer.id == id) else {
                return Err(AppError::new(
                    ErrorCode::NotFound,
                    format!("No transfer found with id `{id}`."),
                ));
            };
            if rec.transfer.state != TransferState::Done {
                return Err(AppError::new(
                    ErrorCode::Conflict,
                    "This transfer is not finished yet.",
                ));
            }
            if rec.transfer.direction != TransferDirection::Download {
                return Err(AppError::new(
                    ErrorCode::Unsupported,
                    "Only downloaded files can be revealed in a folder.",
                ));
            }
            rec.transfer.local_path.clone().ok_or_else(|| {
                AppError::new(
                    ErrorCode::NotFound,
                    "The downloaded file path is no longer recorded.",
                )
            })?
        };
        self.platform.show_in_folder(&local_path).await?;
        Ok(OpenTransferResult { ok: true })
    }

    async fn cleanup_pruned_records(&self, pruned: Vec<TransferRecord>) {
        for rec in pruned {
            if rec.transfer.direction == TransferDirection::Download {
                let _ = self
                    .platform
                    .delete_part_file(&self.part_path(&rec.transfer.id))
                    .await;
            } else if let Some(uri) = rec.transfer.local_path.as_deref() {
                self.platform.release_upload_source(uri).await;
            }
        }
    }

    fn allocate_id(&self) -> String {
        let mut guard = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let seq = guard.next_seq;
        guard.next_seq = guard.next_seq.saturating_add(1);
        let salt = self.id_salt.load(Ordering::Relaxed) & 0xFFFF;
        format!("tr-{salt:04x}-{seq}")
    }

    fn initial_state_for_size(&self, size: u64) -> TransferState {
        let settings = self.settings();
        if settings.wifi_only_large
            && size >= settings.large_threshold_bytes
            && !self.platform.is_unmetered_wifi()
        {
            TransferState::Paused
        } else {
            TransferState::Queued
        }
    }

    fn part_path(&self, id: &str) -> PathBuf {
        self.data_dir.join("parts").join(format!("{id}.part"))
    }

    fn queue_file_path(&self) -> PathBuf {
        self.data_dir.join("queue.json")
    }

    fn persist_locked_snapshot(&self) {
        let (seq, payload) = {
            let guard = self
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let seq = self.persist_seq.fetch_add(1, Ordering::AcqRel) + 1;
            let payload = PersistedQueue {
                version: QUEUE_FILE_VERSION,
                settings: guard.settings,
                next_seq: guard.next_seq,
                items: guard.items.clone(),
            };
            (seq, payload)
        };
        let mut last_written = self
            .persist_lock
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if seq <= *last_written {
            return;
        }
        if save_queue_file(&self.queue_file_path(), &payload).is_ok() {
            *last_written = seq;
        }
    }

    fn broadcast_state_change(&self) {
        let (snapshot, summary, watchers) = {
            let mut guard = self
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let now = Instant::now();
            guard.last_ui_broadcast = now;
            guard.last_notify_broadcast = now;
            let snap = snapshot_transfers(&guard.items);
            let sum = build_notification_summary(&guard.items);
            let watchers: Vec<(u64, IpcChannel<Vec<DeviceTransfer>>)> = guard
                .watchers
                .iter()
                .map(|(&id, ch)| (id, ch.clone()))
                .collect();
            (snap, sum, watchers)
        };
        self.platform.on_queue_updated(summary);
        self.send_to_watchers(&watchers, &snapshot);
    }

    fn maybe_broadcast_progress(&self) {
        let now = Instant::now();
        let (ui_payload, notify_summary) = {
            let mut guard = self
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let ui =
                if now.saturating_duration_since(guard.last_ui_broadcast) >= UI_PROGRESS_INTERVAL {
                    guard.last_ui_broadcast = now;
                    let watchers: Vec<(u64, IpcChannel<Vec<DeviceTransfer>>)> = guard
                        .watchers
                        .iter()
                        .map(|(&id, ch)| (id, ch.clone()))
                        .collect();
                    Some((snapshot_transfers(&guard.items), watchers))
                } else {
                    None
                };
            let notify = if now.saturating_duration_since(guard.last_notify_broadcast)
                >= NOTIFY_PROGRESS_INTERVAL
            {
                guard.last_notify_broadcast = now;
                Some(build_notification_summary(&guard.items))
            } else {
                None
            };
            (ui, notify)
        };
        if let Some(summary) = notify_summary {
            self.platform.on_queue_updated(summary);
        }
        if let Some((snapshot, watchers)) = ui_payload {
            self.send_to_watchers(&watchers, &snapshot);
        }
    }

    fn send_to_watchers(
        &self,
        watchers: &[(u64, IpcChannel<Vec<DeviceTransfer>>)],
        snapshot: &[DeviceTransfer],
    ) {
        if watchers.is_empty() {
            return;
        }
        let mut dead_ids = Vec::new();
        for (watcher_id, ch) in watchers {
            if ch.send(snapshot.to_vec()).is_err() {
                dead_ids.push(*watcher_id);
            }
        }
        if !dead_ids.is_empty() {
            let mut guard = self
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            for id in dead_ids {
                guard.watchers.remove(&id);
            }
        }
    }

    async fn run_scheduler(self: Arc<Self>) {
        loop {
            if self.shutdown_token.is_cancelled() {
                return;
            }

            let to_spawn: Vec<(String, u64, CancellationToken, watch::Sender<bool>)> = {
                let mut guard = self
                    .state
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                let wifi_only = guard.settings.wifi_only_large;
                let threshold = guard.settings.large_threshold_bytes;
                let unmetered = self.platform.is_unmetered_wifi();
                let mut spawned = Vec::new();
                let mut wifi_paused_any = false;

                while guard.running.len() < self.max_concurrent {
                    // Never spawn a second worker while an earlier worker for the same transfer id
                    // still holds its slot (e.g. pause -> resume during final save).
                    let next_pos = guard.items.iter().position(|r| {
                        r.transfer.state == TransferState::Queued
                            && !guard.running.contains_key(&r.transfer.id)
                    });
                    let Some(pos) = next_pos else {
                        break;
                    };
                    if wifi_only && !unmetered && guard.items[pos].transfer.bytes_total >= threshold
                    {
                        guard.items[pos].transfer.state = TransferState::Paused;
                        guard.items[pos].transfer.paused_reason = Some(PauseReason::WifiOnly);
                        wifi_paused_any = true;
                        continue;
                    }
                    let id = guard.items[pos].transfer.id.clone();
                    guard.items[pos].transfer.state = TransferState::Running;
                    guard.items[pos].transfer.paused_reason = None;
                    guard.items[pos].transfer.error = None;
                    guard.items[pos].transfer.open_error = None;
                    let worker_id = guard.next_worker_id;
                    guard.next_worker_id = guard.next_worker_id.saturating_add(1);
                    let tok = self.shutdown_token.child_token();
                    let (finished_tx, finished_rx) = watch::channel(false);
                    guard.running.insert(
                        id.clone(),
                        RunningWorker {
                            worker_id,
                            cancel: tok.clone(),
                            finished: finished_rx,
                        },
                    );
                    spawned.push((id, worker_id, tok, finished_tx));
                }

                if wifi_paused_any || !spawned.is_empty() {
                    drop(guard);
                    self.persist_locked_snapshot();
                    self.broadcast_state_change();
                }
                spawned
            };

            for (id, worker_id, cancel, finished_tx) in to_spawn {
                let mgr = Arc::clone(&self);
                tokio::spawn(async move {
                    mgr.run_worker(&id, worker_id, cancel, finished_tx).await;
                });
            }

            tokio::select! {
                biased;
                () = self.shutdown_token.cancelled() => return,
                () = self.schedule_notify.notified() => {}
            }
        }
    }

    async fn run_worker(
        self: Arc<Self>,
        id: &str,
        worker_id: u64,
        cancel: CancellationToken,
        finished_tx: watch::Sender<bool>,
    ) {
        let mut finished_guard = WorkerFinishedGuard::new(finished_tx);

        let record = {
            let guard = self
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            guard.items.iter().find(|r| r.transfer.id == id).cloned()
        };

        let Some(record) = record else {
            self.finish_worker_slot(id, worker_id);
            finished_guard.signal();
            return;
        };

        let result = match record.transfer.direction {
            TransferDirection::Download => self.execute_download(&record, &cancel).await,
            TransferDirection::Upload => self.execute_upload(&record, &cancel).await,
        };

        match result {
            Ok(outcome) => {
                enum PostFinalize {
                    CancellingTracked,
                    RemovedDuringRun {
                        saved_local_path: Option<String>,
                        release_upload_uri: Option<String>,
                    },
                    DeferredUntilResume,
                    Completed {
                        done: Box<DeviceTransfer>,
                        open_after_done: bool,
                        pruned: Vec<TransferRecord>,
                    },
                }

                let action = {
                    let mut guard = self
                        .state
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                    clear_own_worker_slot(&mut guard, id, worker_id);

                    match guard.items.iter_mut().find(|r| r.transfer.id == id) {
                        None => PostFinalize::RemovedDuringRun {
                            saved_local_path: outcome.local_path,
                            release_upload_uri: (record.transfer.direction
                                == TransferDirection::Upload)
                                .then(|| record.transfer.local_path.clone())
                                .flatten(),
                        },
                        Some(rec)
                            if matches!(
                                rec.transfer.state,
                                TransferState::Cancelling | TransferState::Cancelled
                            ) =>
                        {
                            rec.open_after_done = false;
                            rec.finalized_local_path = None;
                            rec.finalized_hash = None;
                            rec.finalized_in_cache = false;
                            if rec.transfer.direction == TransferDirection::Download {
                                rec.cleanup_part = true;
                                if let Some(saved) = outcome.local_path {
                                    push_unique_cleanup_path(&mut rec.pending_cleanup_paths, saved);
                                }
                            }
                            rec.transfer.state = TransferState::Cancelling;
                            PostFinalize::CancellingTracked
                        }
                        Some(rec)
                            if matches!(
                                rec.transfer.state,
                                TransferState::Paused | TransferState::Queued
                            ) =>
                        {
                            // A pause (or pause -> resume) arrived while the final save / put.finish
                            // was running. Record the already-finalized output (and whether it was
                            // staged in cache) so resume finishes cleanly without duplicating files
                            // and a subsequent cancel deletes the saved output too.
                            rec.finalized_local_path = outcome.local_path;
                            rec.finalized_hash = Some(outcome.hash);
                            rec.finalized_in_cache = outcome.finalized_in_cache;
                            rec.transfer.bytes_done = outcome.bytes_total;
                            rec.transfer.bytes_total = outcome.bytes_total;
                            rec.transfer.rate = None;
                            PostFinalize::DeferredUntilResume
                        }
                        Some(rec) => {
                            let open_after_done = std::mem::take(&mut rec.open_after_done);
                            rec.transfer.state = TransferState::Done;
                            rec.transfer.bytes_done = outcome.bytes_total;
                            rec.transfer.bytes_total = outcome.bytes_total;
                            rec.transfer.rate = None;
                            rec.transfer.error = None;
                            rec.transfer.open_error = None;
                            rec.transfer.paused_reason = None;
                            rec.transfer.finished = Some(now_rfc3339_utc());
                            rec.transfer.hash = Some(outcome.hash);
                            if let Some(local_path) = outcome.local_path {
                                rec.transfer.local_path = Some(local_path);
                            }
                            if rec.transfer.direction == TransferDirection::Download {
                                let device_mime = mime_for_file_name(&rec.transfer.name);
                                let allowed_open = can_open_file_name(&rec.transfer.name);
                                rec.transfer.mime = Some(device_mime.to_owned());
                                rec.transfer.can_open =
                                    self.platform.supports_open_downloads() && allowed_open;
                                rec.transfer.can_show_in_folder =
                                    self.platform.supports_show_in_folder()
                                        && !outcome.finalized_in_cache;
                                rec.transfer.open_blocked_reason =
                                    (!allowed_open).then(|| REFUSED_OPEN_MESSAGE.to_owned());
                                rec.cached_open = outcome.finalized_in_cache;
                            } else {
                                rec.transfer.can_open = false;
                                rec.transfer.can_show_in_folder = false;
                                rec.transfer.open_blocked_reason = None;
                                rec.cached_open = false;
                            }
                            rec.finalized_local_path = None;
                            rec.finalized_hash = None;
                            rec.finalized_in_cache = false;
                            rec.pending_cleanup_paths.clear();
                            rec.cleanup_part = false;
                            let done = rec.transfer.clone();
                            let pruned = prune_terminal_locked(&mut guard.items);
                            PostFinalize::Completed {
                                done: Box::new(done),
                                open_after_done,
                                pruned,
                            }
                        }
                    }
                };

                match action {
                    PostFinalize::CancellingTracked => {
                        self.persist_locked_snapshot();
                        finished_guard.signal();
                        self.cleanup_notify.notify_one();
                        self.schedule_notify.notify_one();
                    }
                    PostFinalize::RemovedDuringRun {
                        saved_local_path,
                        release_upload_uri,
                    } => {
                        if record.transfer.direction == TransferDirection::Download {
                            let _ = self.platform.delete_part_file(&self.part_path(id)).await;
                            if let Some(saved) = saved_local_path {
                                let _ = self.platform.delete_saved_download(&saved).await;
                            }
                        }
                        if let Some(uri) = release_upload_uri {
                            self.platform.release_upload_source(&uri).await;
                        }
                        finished_guard.signal();
                        self.persist_locked_snapshot();
                        self.broadcast_state_change();
                        self.schedule_notify.notify_one();
                    }
                    PostFinalize::DeferredUntilResume => {
                        finished_guard.signal();
                        self.persist_locked_snapshot();
                        self.broadcast_state_change();
                        self.schedule_notify.notify_one();
                    }
                    PostFinalize::Completed {
                        done,
                        open_after_done,
                        pruned,
                    } => {
                        finished_guard.signal();
                        self.cleanup_pruned_records(pruned).await;
                        if done.direction == TransferDirection::Upload
                            && let Some(uri) = done.local_path.as_deref()
                        {
                            self.platform.release_upload_source(uri).await;
                        }
                        if open_after_done
                            && done.can_open
                            && let Some(path) = done.local_path.as_deref()
                        {
                            let device_mime = mime_for_file_name(&done.name);
                            if let Err(err) =
                                self.platform.open_local_file(path, Some(device_mime)).await
                            {
                                let mut guard = self
                                    .state
                                    .lock()
                                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                                if let Some(rec) =
                                    guard.items.iter_mut().find(|r| r.transfer.id == done.id)
                                {
                                    rec.transfer.open_error = Some(TransferError::from(&err));
                                }
                            }
                            self.persist_locked_snapshot();
                            self.broadcast_state_change();
                        } else {
                            self.persist_locked_snapshot();
                            self.broadcast_state_change();
                            self.platform.on_transfer_completed(&done);
                        }
                        self.schedule_notify.notify_one();
                    }
                }
            }
            Err(WorkerStop::WifiOnlyPause) => {
                let is_cancelling = {
                    let mut guard = self
                        .state
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                    clear_own_worker_slot(&mut guard, id, worker_id);
                    if let Some(rec) = guard.items.iter_mut().find(|r| r.transfer.id == id) {
                        if rec.transfer.state == TransferState::Running {
                            rec.transfer.state = TransferState::Paused;
                            rec.transfer.paused_reason = Some(PauseReason::WifiOnly);
                            rec.transfer.rate = None;
                        }
                        rec.transfer.state == TransferState::Cancelling
                    } else {
                        false
                    }
                };
                finished_guard.signal();
                if is_cancelling {
                    self.cleanup_notify.notify_one();
                } else {
                    self.persist_locked_snapshot();
                    self.broadcast_state_change();
                }
                self.schedule_notify.notify_one();
            }
            Err(WorkerStop::Stalled(err)) => {
                let is_cancelling = {
                    let mut guard = self
                        .state
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                    clear_own_worker_slot(&mut guard, id, worker_id);
                    if let Some(rec) = guard.items.iter_mut().find(|r| r.transfer.id == id) {
                        if rec.transfer.state == TransferState::Running {
                            rec.transfer.state = TransferState::Paused;
                            rec.transfer.paused_reason = Some(PauseReason::Stalled);
                            rec.transfer.rate = None;
                            rec.transfer.error = Some(TransferError::from(&err));
                        }
                        rec.transfer.state == TransferState::Cancelling
                    } else {
                        false
                    }
                };
                finished_guard.signal();
                if is_cancelling {
                    self.cleanup_notify.notify_one();
                } else {
                    self.persist_locked_snapshot();
                    self.broadcast_state_change();
                }
                self.schedule_notify.notify_one();
            }
            Err(WorkerStop::Interrupted) => {
                enum InterruptedState {
                    Cancelling,
                    RemovedDownload,
                    Other,
                }
                let kind = {
                    let mut guard = self
                        .state
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                    clear_own_worker_slot(&mut guard, id, worker_id);
                    match guard.items.iter().find(|r| r.transfer.id == id) {
                        None if record.transfer.direction == TransferDirection::Download => {
                            InterruptedState::RemovedDownload
                        }
                        Some(r) if r.transfer.state == TransferState::Cancelling => {
                            InterruptedState::Cancelling
                        }
                        _ => InterruptedState::Other,
                    }
                };
                match kind {
                    InterruptedState::Cancelling => {
                        finished_guard.signal();
                        self.cleanup_notify.notify_one();
                        self.schedule_notify.notify_one();
                    }
                    InterruptedState::RemovedDownload => {
                        let _ = self.platform.delete_part_file(&self.part_path(id)).await;
                        finished_guard.signal();
                        self.persist_locked_snapshot();
                        self.broadcast_state_change();
                        self.schedule_notify.notify_one();
                    }
                    InterruptedState::Other => {
                        finished_guard.signal();
                        self.persist_locked_snapshot();
                        self.broadcast_state_change();
                        self.schedule_notify.notify_one();
                    }
                }
            }
            Err(WorkerStop::Failed(err)) => {
                let mut delete_part = false;
                let mut delete_saved_path: Option<String> = None;
                let mut release_upload_uri: Option<String> = None;
                let mut pruned = Vec::new();
                let mut should_mark_failed = false;
                let mut is_cancelling = false;

                {
                    let mut guard = self
                        .state
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                    clear_own_worker_slot(&mut guard, id, worker_id);
                    if let Some(rec) = guard.items.iter_mut().find(|r| r.transfer.id == id) {
                        if rec.transfer.state == TransferState::Cancelling {
                            is_cancelling = true;
                        } else if !self.shutdown_token.is_cancelled()
                            && rec.transfer.state == TransferState::Running
                        {
                            should_mark_failed = true;
                            rec.transfer.state = TransferState::Failed;
                            rec.transfer.paused_reason = None;
                            rec.transfer.rate = None;
                            rec.transfer.finished = Some(now_rfc3339_utc());
                            rec.transfer.error = Some(TransferError::from(&err));
                            rec.open_after_done = false;
                            delete_part = rec.transfer.direction == TransferDirection::Download;
                            if delete_part {
                                delete_saved_path = rec.finalized_local_path.take();
                            } else {
                                rec.finalized_local_path = None;
                            }
                            rec.finalized_hash = None;
                            rec.finalized_in_cache = false;
                            if rec.transfer.direction == TransferDirection::Upload {
                                release_upload_uri = rec.transfer.local_path.clone();
                            }
                            pruned = prune_terminal_locked(&mut guard.items);
                        }
                    }
                }

                finished_guard.signal();
                if is_cancelling {
                    self.cleanup_notify.notify_one();
                    self.schedule_notify.notify_one();
                    return;
                }
                if should_mark_failed {
                    if delete_part {
                        let _ = self.platform.delete_part_file(&self.part_path(id)).await;
                    }
                    if let Some(saved) = delete_saved_path {
                        let _ = self.platform.delete_saved_download(&saved).await;
                    }
                    if let Some(uri) = release_upload_uri {
                        self.platform.release_upload_source(&uri).await;
                    }
                    self.cleanup_pruned_records(pruned).await;
                }
                self.persist_locked_snapshot();
                self.broadcast_state_change();
                self.schedule_notify.notify_one();
            }
        }
    }

    fn finish_worker_slot(&self, id: &str, worker_id: u64) {
        {
            let mut guard = self
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            clear_own_worker_slot(&mut guard, id, worker_id);
        }
        self.persist_locked_snapshot();
        self.broadcast_state_change();
        self.schedule_notify.notify_one();
    }

    async fn execute_download(
        &self,
        initial: &TransferRecord,
        cancel: &CancellationToken,
    ) -> Result<WorkerOutcome, WorkerStop> {
        let id = &initial.transfer.id;
        let part_path = self.part_path(id);

        // Idempotent completion: if a previous worker already finalized the verified file while a
        // pause intervened, reuse that saved destination instead of re-downloading or duplicating.
        // However, if the earlier finalization landed in the ephemeral `open-externally` cache and
        // `open_after_done` is no longer active (e.g. after an app restart), NEVER report Done
        // pointing at a 24 h cache: promote the verified file to Downloads or re-download cleanly.
        if let (Some(saved_path), Some(saved_hash)) = (
            initial.finalized_local_path.as_deref(),
            initial.finalized_hash.as_deref(),
        ) {
            if initial.finalized_in_cache && !initial.open_after_done {
                let device_mime = mime_for_file_name(&initial.transfer.name);
                if !saved_path.starts_with("content://")
                    && tokio::fs::try_exists(saved_path).await.unwrap_or(false)
                    && matches!(
                        hash_file_blake3(Path::new(saved_path), cancel).await,
                        Ok(ref actual) if actual == saved_hash
                    )
                {
                    if cancel.is_cancelled() {
                        return Err(WorkerStop::Interrupted);
                    }
                    let downloads_path = self
                        .platform
                        .finalize_download(
                            id,
                            Path::new(saved_path),
                            &initial.transfer.name,
                            Some(device_mime),
                            false,
                        )
                        .await
                        .map_err(WorkerStop::Failed)?;
                    // Ensure the old cache copy is gone even if the platform copied instead of moving.
                    let _ = self.platform.delete_saved_download(saved_path).await;
                    let _ = self.platform.delete_part_file(&part_path).await;
                    return Ok(WorkerOutcome {
                        bytes_total: initial.transfer.bytes_total,
                        hash: saved_hash.to_owned(),
                        local_path: Some(downloads_path),
                        finalized_in_cache: false,
                    });
                }
                // Android FileProvider cache URI or missing/pruned cache file: discard the cache
                // copy and download cleanly into Downloads.
                let _ = self.platform.delete_saved_download(saved_path).await;
                self.clear_finalized_state(id);
            } else {
                let exists = saved_path.starts_with("content://")
                    || tokio::fs::try_exists(saved_path).await.unwrap_or(false);
                if exists {
                    let _ = tokio::fs::remove_file(&part_path).await;
                    return Ok(WorkerOutcome {
                        bytes_total: initial.transfer.bytes_total,
                        hash: saved_hash.to_owned(),
                        local_path: Some(saved_path.to_owned()),
                        finalized_in_cache: initial.finalized_in_cache,
                    });
                }
                self.clear_finalized_state(id);
            }
        }

        let mut expected_meta = (
            initial.transfer.bytes_total,
            initial.remote_mtime_secs,
            initial.remote_mtime_nanos,
            initial.remote_file_id,
        );

        let mut retry_started: Option<Instant> = None;
        let mut attempt: u32 = 0;

        let total_size = loop {
            if cancel.is_cancelled() {
                return Err(WorkerStop::Interrupted);
            }

            let target = self
                .backend
                .content_target(&initial.transfer.machine)
                .map_err(WorkerStop::Failed)?;

            let mut offset = tokio::fs::metadata(&part_path)
                .await
                .map(|m| m.len())
                .unwrap_or(0);

            // Probe metadata first when we already have a partial file so we detect size/mtime
            // changes or an already-complete partial file cleanly without hitting OutOfRange.
            let opened = if offset > 0 {
                let probe = match target
                    .link
                    .open_content_for_transfer_with_cancel(
                        &target.link_node,
                        &initial.transfer.path,
                        0,
                        Some(0),
                        cancel.clone(),
                    )
                    .await
                {
                    Ok(stream) => stream,
                    Err(err) => {
                        let app_err = AppError::from(err);
                        if self
                            .should_retry(&app_err, &mut retry_started, &mut attempt, cancel)
                            .await?
                        {
                            continue;
                        }
                        return Err(WorkerStop::Failed(app_err));
                    }
                };
                let remote_size = probe.size();
                let remote_mtime = probe.mtime();
                let remote_file_id = probe.file_id();
                drop(probe);

                if self.should_wifi_pause(remote_size) {
                    self.update_remote_meta(id, remote_size, remote_mtime, remote_file_id);
                    return Err(WorkerStop::WifiOnlyPause);
                }

                let stale = expected_meta.1.is_some_and(|secs| {
                    expected_meta.0 != remote_size
                        || secs != remote_mtime.secs
                        || expected_meta.2 != Some(remote_mtime.nanos)
                        || expected_meta.3 != remote_file_id
                }) || offset > remote_size;

                if stale {
                    let _ = tokio::fs::remove_file(&part_path).await;
                    offset = 0;
                    self.update_progress(id, 0, remote_size, None);
                }
                expected_meta = (
                    remote_size,
                    Some(remote_mtime.secs),
                    Some(remote_mtime.nanos),
                    remote_file_id,
                );
                self.update_remote_meta(id, remote_size, remote_mtime, remote_file_id);

                if offset == remote_size {
                    break remote_size;
                }

                match target
                    .link
                    .open_content_for_transfer_with_cancel(
                        &target.link_node,
                        &initial.transfer.path,
                        offset,
                        None,
                        cancel.clone(),
                    )
                    .await
                {
                    Ok(stream) => stream,
                    Err(err) => {
                        let app_err = AppError::from(err);
                        if self
                            .should_retry(&app_err, &mut retry_started, &mut attempt, cancel)
                            .await?
                        {
                            continue;
                        }
                        return Err(WorkerStop::Failed(app_err));
                    }
                }
            } else {
                match target
                    .link
                    .open_content_for_transfer_with_cancel(
                        &target.link_node,
                        &initial.transfer.path,
                        0,
                        None,
                        cancel.clone(),
                    )
                    .await
                {
                    Ok(stream) => stream,
                    Err(err) => {
                        let app_err = AppError::from(err);
                        if self
                            .should_retry(&app_err, &mut retry_started, &mut attempt, cancel)
                            .await?
                        {
                            continue;
                        }
                        return Err(WorkerStop::Failed(app_err));
                    }
                }
            };

            let remote_size = opened.size();
            let remote_mtime = opened.mtime();
            let remote_file_id = opened.file_id();
            expected_meta = (
                remote_size,
                Some(remote_mtime.secs),
                Some(remote_mtime.nanos),
                remote_file_id,
            );
            self.update_remote_meta(id, remote_size, remote_mtime, remote_file_id);

            if self.should_wifi_pause(remote_size) {
                drop(opened);
                return Err(WorkerStop::WifiOnlyPause);
            }
            if cancel.is_cancelled() {
                drop(opened);
                return Err(WorkerStop::Interrupted);
            }

            if remote_size == 0 {
                // Ensure an empty .part exists for 0-byte files.
                let _ = AsyncFile::create(&part_path).await;
                break 0;
            }

            let mut file = open_part_file_for_append(&part_path, offset)
                .await
                .map_err(WorkerStop::Failed)?;
            let mut stream = opened;
            let mut buf = vec![0u8; COPY_BUF_BYTES];
            let mut done = offset;
            let mut rate_window_start = Instant::now();
            let mut rate_window_bytes: u64 = 0;
            let mut current_rate: Option<u64> = None;

            let stream_err: Option<AppError> = loop {
                if cancel.is_cancelled() {
                    let _ = file.flush().await;
                    return Err(WorkerStop::Interrupted);
                }
                if self.should_wifi_pause(remote_size) {
                    let _ = file.flush().await;
                    return Err(WorkerStop::WifiOnlyPause);
                }
                let read_res = tokio::select! {
                    biased;
                    () = cancel.cancelled() => {
                        let _ = file.flush().await;
                        return Err(WorkerStop::Interrupted);
                    }
                    timed = tokio::time::timeout(self.stall_timeout, stream.read(&mut buf)) => {
                        match timed {
                            Ok(res) => res,
                            Err(_) => {
                                let _ = file.flush().await;
                                return Err(WorkerStop::Stalled(
                                    AppError::new(
                                        ErrorCode::Timeout,
                                        "Download paused: the machine stopped sending data.",
                                    )
                                    .with_hint("Resume the download once the connection recovers."),
                                ));
                            }
                        }
                    }
                };
                match read_res {
                    Ok(0) => break None,
                    Ok(n) => {
                        if let Err(err) = file.write_all(&buf[..n]).await {
                            return Err(WorkerStop::Failed(AppError::new(
                                ErrorCode::Failed,
                                format!("Could not write downloaded bytes to disk: {err}."),
                            )));
                        }
                        done = done.saturating_add(n as u64);
                        // Any forward progress resets transient retry timer.
                        retry_started = None;
                        attempt = 0;

                        rate_window_bytes = rate_window_bytes.saturating_add(n as u64);
                        let elapsed = rate_window_start.elapsed();
                        if elapsed >= Duration::from_millis(500) {
                            current_rate =
                                Some(((rate_window_bytes as f64) / elapsed.as_secs_f64()) as u64);
                            rate_window_start = Instant::now();
                            rate_window_bytes = 0;
                        }
                        self.update_progress(id, done, remote_size, current_rate);
                        self.maybe_broadcast_progress();
                        #[cfg(test)]
                        {
                            let pause_gate = self
                                .download_pause_once
                                .lock()
                                .unwrap_or_else(|p| p.into_inner())
                                .take();
                            if let Some(gate) = pause_gate {
                                gate.entered.notify_one();
                                gate.release.notified().await;
                            }
                        }
                    }
                    Err(err) => {
                        if cancel.is_cancelled() {
                            let _ = file.flush().await;
                            return Err(WorkerStop::Interrupted);
                        }
                        break Some(AppError::new(
                            ErrorCode::Unreachable,
                            format!("Download stream interrupted: {err}."),
                        ));
                    }
                }
            };

            let _ = file.flush().await;
            drop(file);

            if let Some(err) = stream_err {
                if self
                    .should_retry(&err, &mut retry_started, &mut attempt, cancel)
                    .await?
                {
                    continue;
                }
                return Err(WorkerStop::Failed(err));
            }
            if done == remote_size {
                break remote_size;
            }
            let short_err = AppError::new(
                ErrorCode::Unreachable,
                format!("Download ended early at {done} of {remote_size} bytes."),
            );
            if self
                .should_retry(&short_err, &mut retry_started, &mut attempt, cancel)
                .await?
            {
                continue;
            }
            return Err(WorkerStop::Failed(short_err));
        };

        if cancel.is_cancelled() {
            return Err(WorkerStop::Interrupted);
        }

        // Verify completed partial file against remote `fs.hash` (BLAKE3) using bounded memory.
        let local_hash = hash_file_blake3(&part_path, cancel).await?;
        let remote_hash = self
            .fetch_remote_hash(&initial.transfer.machine, &initial.transfer.path, cancel)
            .await?;

        if local_hash != remote_hash {
            let _ = self.platform.delete_part_file(&part_path).await;
            return Err(WorkerStop::Failed(
                AppError::new(
                    ErrorCode::Conflict,
                    "Downloaded file verification failed: BLAKE3 hash did not match the machine's file.",
                )
                .with_hint("Retry the download in case the file changed while transferring."),
            ));
        }

        if cancel.is_cancelled() {
            return Err(WorkerStop::Interrupted);
        }

        let device_mime = mime_for_file_name(&initial.transfer.name);
        let final_path = self
            .platform
            .finalize_download(
                id,
                &part_path,
                &initial.transfer.name,
                Some(device_mime),
                initial.open_after_done,
            )
            .await
            .map_err(WorkerStop::Failed)?;
        let _ = self.platform.delete_part_file(&part_path).await;

        Ok(WorkerOutcome {
            bytes_total: total_size,
            hash: local_hash,
            local_path: Some(final_path),
            finalized_in_cache: initial.open_after_done,
        })
    }

    async fn fetch_remote_hash(
        &self,
        machine: &str,
        remote_path: &str,
        cancel: &CancellationToken,
    ) -> Result<String, WorkerStop> {
        let mut retry_started: Option<Instant> = None;
        let mut attempt: u32 = 0;

        loop {
            if cancel.is_cancelled() {
                return Err(WorkerStop::Interrupted);
            }
            let target = self
                .backend
                .content_target(machine)
                .map_err(WorkerStop::Failed)?;
            let call_fut = target.link.call_machine(
                &target.link_node,
                "fs.hash",
                json!({ "path": remote_path }),
                |_| {},
            );
            let res = tokio::select! {
                biased;
                () = cancel.cancelled() => return Err(WorkerStop::Interrupted),
                res = call_fut => res,
            };
            match res {
                Ok(routed) => {
                    let Some(hash) = routed
                        .value
                        .get("hash")
                        .and_then(Value::as_str)
                        .map(str::trim)
                        .filter(|s| s.starts_with("blake3:"))
                    else {
                        return Err(WorkerStop::Failed(AppError::new(
                            ErrorCode::Failed,
                            "The remote machine returned an unexpected `fs.hash` response.",
                        )));
                    };
                    return Ok(hash.to_owned());
                }
                Err(err) => {
                    let app_err = AppError::from(err);
                    if self
                        .should_retry(&app_err, &mut retry_started, &mut attempt, cancel)
                        .await?
                    {
                        continue;
                    }
                    return Err(WorkerStop::Failed(app_err));
                }
            }
        }
    }

    async fn resolve_unique_remote_path(
        &self,
        machine: &str,
        requested_path: &str,
        requested_name: &str,
        cancel: &CancellationToken,
    ) -> Result<(String, String, u32), WorkerStop> {
        let (dir_prefix, sep) = split_remote_dir(requested_path);
        let safe_base = sanitize_file_name(requested_name);
        let path_ref = Path::new(&safe_base);
        let stem = path_ref
            .file_stem()
            .and_then(|s| s.to_str())
            .filter(|s| !s.is_empty())
            .unwrap_or(&safe_base)
            .to_owned();
        let ext = path_ref
            .extension()
            .and_then(|e| e.to_str())
            .filter(|e| !e.is_empty())
            .map(str::to_owned);

        let mut retry_started: Option<Instant> = None;
        let mut attempt: u32 = 0;

        for n in 1..=9999u32 {
            let candidate_name = file_policy::numbered_candidate_name(&safe_base, n);
            let candidate_path = join_remote_with_sep(&dir_prefix, sep, &candidate_name);

            loop {
                if cancel.is_cancelled() {
                    return Err(WorkerStop::Interrupted);
                }
                let target = self
                    .backend
                    .content_target(machine)
                    .map_err(WorkerStop::Failed)?;
                let call_fut = target.link.call_machine(
                    &target.link_node,
                    "fs.stat",
                    json!({ "path": candidate_path }),
                    |_| {},
                );
                let res = tokio::select! {
                    biased;
                    () = cancel.cancelled() => return Err(WorkerStop::Interrupted),
                    timed = tokio::time::timeout(self.stall_timeout, call_fut) => {
                        match timed {
                            Ok(res) => res,
                            Err(_) => {
                                return Err(WorkerStop::Stalled(
                                    AppError::new(
                                        ErrorCode::Timeout,
                                        "Upload paused: the machine stopped responding.",
                                    )
                                    .with_hint("Resume the upload once the connection recovers."),
                                ));
                            }
                        }
                    }
                };
                match res {
                    Ok(_) => {
                        // A file or directory already occupies this remote path; try the next
                        // numbered sibling (`name (2).ext`) rather than overwriting it.
                        retry_started = None;
                        attempt = 0;
                        break;
                    }
                    Err(err) => {
                        let app_err = AppError::from(err);
                        let is_unknown_machine = app_err
                            .details
                            .as_ref()
                            .and_then(|d| d.get("reason"))
                            .and_then(Value::as_str)
                            == Some(legion_link::REASON_UNKNOWN_MACHINE);
                        if app_err.code == ErrorCode::NotFound && !is_unknown_machine {
                            return Ok((candidate_path, candidate_name, n));
                        }
                        if self
                            .should_retry(&app_err, &mut retry_started, &mut attempt, cancel)
                            .await?
                        {
                            continue;
                        }
                        return Err(WorkerStop::Failed(app_err));
                    }
                }
            }
        }

        let fallback_name = match ext.as_deref() {
            Some(e) => format!("{stem}-{}.{e}", now_unix_secs()),
            None => format!("{stem}-{}", now_unix_secs()),
        };
        let fallback_path = join_remote_with_sep(&dir_prefix, sep, &fallback_name);
        Ok((fallback_path, fallback_name, 9999))
    }

    fn lock_resolved_upload_path(&self, id: &str, remote_path: &str, name: &str) {
        {
            let mut guard = self
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if let Some(rec) = guard.items.iter_mut().find(|r| r.transfer.id == id) {
                rec.transfer.path = remote_path.to_owned();
                rec.transfer.name = name.to_owned();
                rec.remote_path_resolved = true;
                rec.upload_started = false;
            }
        }
        self.persist_locked_snapshot();
        self.broadcast_state_change();
    }

    fn mark_upload_started(&self, id: &str) {
        let mut changed = false;
        {
            let mut guard = self
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if let Some(rec) = guard.items.iter_mut().find(|r| r.transfer.id == id)
                && !rec.upload_started
            {
                rec.upload_started = true;
                changed = true;
            }
        }
        if changed {
            self.persist_locked_snapshot();
        }
    }

    async fn execute_upload(
        &self,
        initial: &TransferRecord,
        cancel: &CancellationToken,
    ) -> Result<WorkerOutcome, WorkerStop> {
        let id = &initial.transfer.id;
        if let Some(saved_hash) = initial.finalized_hash.as_deref() {
            return Ok(WorkerOutcome {
                bytes_total: initial.transfer.bytes_total,
                hash: saved_hash.to_owned(),
                local_path: None,
                finalized_in_cache: false,
            });
        }

        let Some(local_path) = initial.transfer.local_path.as_deref() else {
            return Err(WorkerStop::Failed(AppError::new(
                ErrorCode::InvalidArgs,
                "Upload entry has no local source file.",
            )));
        };

        // Compute exact source size and BLAKE3 digest in bounded 64 KiB chunks.
        let (total_size, local_hash) = self.hash_upload_source(local_path, cancel).await?;
        self.update_upload_meta(id, total_size, &local_hash);

        if self.should_wifi_pause(total_size) {
            return Err(WorkerStop::WifiOnlyPause);
        }

        let (dir_prefix, sep) = split_remote_dir(&initial.transfer.path);
        let base_name = sanitize_file_name(&initial.transfer.name);

        // Choose the remote destination path without overwriting existing files:
        // - When the machine is already known to support `if_exists`, skip `fs.stat` and rely on
        //   `put` with `"if_exists": "fail"`, stepping to the next numbered sibling on `Exists`.
        // - When the machine is older (`Some(false)`), keep `fs.stat`-then-`put` without sending
        //   `if_exists`.
        // - Before the first direct stream greeting (`None`, including relayed targets where the
        //   target has no direct session), probe with `fs.stat` (so older machines never overwrite
        //   even on 0-byte uploads) and still send `"if_exists": "fail"` so current machines
        //   enforce atomic refusal on the very first upload (catching races between `fs.stat` and
        //   `put`, or before final commit) and cache their capability on direct channels.
        let initial_target: ContentTarget = self
            .backend
            .content_target(&initial.transfer.machine)
            .map_err(WorkerStop::Failed)?;
        let initial_supports_if_exists = initial_target
            .link
            .stream_supports(&initial_target.link_node, CAP_IF_EXISTS);

        let (mut remote_path, mut candidate_n) = if initial.remote_path_resolved {
            (initial.transfer.path.clone(), 1u32)
        } else if initial_supports_if_exists == Some(true) {
            let candidate_path = join_remote_with_sep(&dir_prefix, sep, &base_name);
            self.lock_resolved_upload_path(id, &candidate_path, &base_name);
            (candidate_path, 1u32)
        } else {
            let (resolved_path, resolved_name, resolved_n) = self
                .resolve_unique_remote_path(
                    &initial.transfer.machine,
                    &initial.transfer.path,
                    &initial.transfer.name,
                    cancel,
                )
                .await?;
            self.lock_resolved_upload_path(id, &resolved_path, &resolved_name);
            (resolved_path, resolved_n)
        };
        let mut fallback_exists_attempts: u32 = 0;

        let mut retry_started: Option<Instant> = None;
        let mut attempt: u32 = 0;
        let mut highest_ack_offset: u64 = 0;
        let mut consecutive_cancelled_no_progress: u32 = 0;
        let mut sent_any_bytes = initial.upload_started;

        loop {
            if cancel.is_cancelled() {
                return Err(WorkerStop::Interrupted);
            }
            if self.should_wifi_pause(total_size) {
                return Err(WorkerStop::WifiOnlyPause);
            }

            if sent_any_bytes
                && self
                    .verify_uploaded_destination(
                        &initial.transfer.machine,
                        &remote_path,
                        total_size,
                        &local_hash,
                        cancel,
                    )
                    .await?
            {
                self.update_progress(id, total_size, total_size, None);
                self.maybe_broadcast_progress();
                return Ok(WorkerOutcome {
                    bytes_total: total_size,
                    hash: local_hash,
                    local_path: None,
                    finalized_in_cache: false,
                });
            }

            let target: ContentTarget = self
                .backend
                .content_target(&initial.transfer.machine)
                .map_err(WorkerStop::Failed)?;

            let use_if_exists_fail = target
                .link
                .stream_supports(&target.link_node, CAP_IF_EXISTS)
                != Some(false);

            let req = PutRequest {
                path: remote_path.clone(),
                size: total_size,
                hash: local_hash.clone(),
                mode: Some(0o644),
                mtime: local_file_mtime(local_path),
                if_exists: if use_if_exists_fail {
                    PutIfExists::Fail
                } else {
                    PutIfExists::Replace
                },
            };

            let mut put = match target
                .link
                .open_put_with_cancel(&target.link_node, req, cancel.clone())
                .await
            {
                Ok(put) => put,
                Err(err) => {
                    if use_if_exists_fail
                        && err.code == legion_link::proto::ErrorCode::Exists
                        && let Some((next_path, next_name)) = advance_upload_candidate(
                            &dir_prefix,
                            sep,
                            &base_name,
                            &mut candidate_n,
                            &mut fallback_exists_attempts,
                        )
                    {
                        remote_path = next_path;
                        sent_any_bytes = false;
                        self.lock_resolved_upload_path(id, &remote_path, &next_name);
                        self.update_progress(id, 0, total_size, None);
                        retry_started = None;
                        attempt = 0;
                        highest_ack_offset = 0;
                        consecutive_cancelled_no_progress = 0;
                        continue;
                    }
                    let app_err = AppError::from(err);
                    if self
                        .should_retry_upload(
                            &app_err,
                            &mut retry_started,
                            &mut attempt,
                            &mut consecutive_cancelled_no_progress,
                            cancel,
                        )
                        .await?
                    {
                        continue;
                    }
                    return Err(WorkerStop::Failed(app_err));
                }
            };

            let resume_offset = put.offset();
            if resume_offset > highest_ack_offset {
                // Only acknowledged remote progress (`put.offset()` advancing) resets the
                // retry window and unsolicited-cancellation counter — local `write_all` calls
                // succeed into the local socket/channel buffer before the target saves anything.
                highest_ack_offset = resume_offset;
                retry_started = None;
                attempt = 0;
                consecutive_cancelled_no_progress = 0;
            }
            // The target accepted our `put` for this locked path; persist `upload_started` before
            // sending bytes so an updater close or crash that loses the final `ok` reply checks
            // `verify_uploaded_destination` on restart instead of re-uploading or duplicating.
            self.mark_upload_started(id);
            sent_any_bytes = true;
            self.update_progress(id, resume_offset, total_size, None);
            self.maybe_broadcast_progress();

            let mut source = self
                .platform
                .open_upload_source(local_path)
                .await
                .map_err(WorkerStop::Failed)?;
            if resume_offset > 0 {
                source
                    .seek_to(resume_offset)
                    .await
                    .map_err(WorkerStop::Failed)?;
            }

            let mut buf = vec![0u8; COPY_BUF_BYTES];
            let mut sent = resume_offset;
            let mut rate_window_start = Instant::now();
            let mut rate_window_bytes: u64 = 0;
            let mut current_rate: Option<u64> = None;

            let write_err: Option<AppError> = loop {
                if sent >= total_size {
                    break None;
                }
                if cancel.is_cancelled() {
                    put.cancel();
                    return Err(WorkerStop::Interrupted);
                }
                if self.should_wifi_pause(total_size) {
                    put.cancel();
                    return Err(WorkerStop::WifiOnlyPause);
                }

                let remaining = (total_size - sent) as usize;
                let max_read = remaining.min(buf.len());
                let read_n = tokio::select! {
                    biased;
                    () = cancel.cancelled() => {
                        put.cancel();
                        return Err(WorkerStop::Interrupted);
                    }
                    timed = tokio::time::timeout(
                        self.stall_timeout,
                        source.read_chunk(&mut buf[..max_read]),
                    ) => {
                        match timed {
                            Ok(res) => res.map_err(WorkerStop::Failed)?,
                            Err(_) => {
                                put.cancel();
                                return Err(WorkerStop::Stalled(
                                    AppError::new(
                                        ErrorCode::Timeout,
                                        "Upload paused: reading the source file stopped responding.",
                                    )
                                    .with_hint("Resume the upload once the file is accessible."),
                                ));
                            }
                        }
                    }
                };
                if read_n == 0 {
                    put.cancel();
                    return Err(WorkerStop::Failed(
                        AppError::new(
                            ErrorCode::Conflict,
                            "Local upload file shrank while uploading.",
                        )
                        .with_hint("Retry the upload once the file stops changing."),
                    ));
                }

                let write_res = tokio::select! {
                    biased;
                    () = cancel.cancelled() => {
                        put.cancel();
                        return Err(WorkerStop::Interrupted);
                    }
                    timed = tokio::time::timeout(
                        self.stall_timeout,
                        put.write_all(&buf[..read_n]),
                    ) => {
                        match timed {
                            Ok(res) => res,
                            Err(_) => {
                                put.cancel();
                                return Err(WorkerStop::Stalled(
                                    AppError::new(
                                        ErrorCode::Timeout,
                                        "Upload paused: the machine stopped accepting data.",
                                    )
                                    .with_hint("Resume the upload once the connection recovers."),
                                ));
                            }
                        }
                    }
                };
                sent_any_bytes = true;
                if let Err(err) = write_res {
                    if cancel.is_cancelled() {
                        return Err(WorkerStop::Interrupted);
                    }
                    break Some(AppError::new(
                        ErrorCode::Unreachable,
                        format!("Upload stream interrupted: {err}."),
                    ));
                }
                sent = sent.saturating_add(read_n as u64);

                rate_window_bytes = rate_window_bytes.saturating_add(read_n as u64);
                let elapsed = rate_window_start.elapsed();
                if elapsed >= Duration::from_millis(500) {
                    current_rate =
                        Some(((rate_window_bytes as f64) / elapsed.as_secs_f64()) as u64);
                    rate_window_start = Instant::now();
                    rate_window_bytes = 0;
                }
                self.update_progress(id, sent, total_size, current_rate);
                self.maybe_broadcast_progress();
            };

            if let Some(err) = write_err {
                let peer_err = put.read_error_after_write_failure().await;
                if self
                    .verify_uploaded_destination(
                        &initial.transfer.machine,
                        &remote_path,
                        total_size,
                        &local_hash,
                        cancel,
                    )
                    .await?
                {
                    self.update_progress(id, total_size, total_size, None);
                    self.maybe_broadcast_progress();
                    return Ok(WorkerOutcome {
                        bytes_total: total_size,
                        hash: local_hash,
                        local_path: None,
                        finalized_in_cache: false,
                    });
                }
                let effective_err = peer_err.map(AppError::from).unwrap_or(err);
                if use_if_exists_fail
                    && effective_err.code == ErrorCode::Exists
                    && let Some((next_path, next_name)) = advance_upload_candidate(
                        &dir_prefix,
                        sep,
                        &base_name,
                        &mut candidate_n,
                        &mut fallback_exists_attempts,
                    )
                {
                    remote_path = next_path;
                    sent_any_bytes = false;
                    self.lock_resolved_upload_path(id, &remote_path, &next_name);
                    self.update_progress(id, 0, total_size, None);
                    retry_started = None;
                    attempt = 0;
                    highest_ack_offset = 0;
                    consecutive_cancelled_no_progress = 0;
                    continue;
                }
                if self
                    .should_retry_upload(
                        &effective_err,
                        &mut retry_started,
                        &mut attempt,
                        &mut consecutive_cancelled_no_progress,
                        cancel,
                    )
                    .await?
                {
                    continue;
                }
                return Err(WorkerStop::Failed(effective_err));
            }

            match put.finish().await {
                Ok(done) => {
                    return Ok(WorkerOutcome {
                        bytes_total: done.size,
                        hash: done.hash,
                        local_path: None,
                        finalized_in_cache: false,
                    });
                }
                Err(err) => {
                    if cancel.is_cancelled() {
                        return Err(WorkerStop::Interrupted);
                    }
                    if use_if_exists_fail
                        && err.code == legion_link::proto::ErrorCode::Exists
                        && let Some((next_path, next_name)) = advance_upload_candidate(
                            &dir_prefix,
                            sep,
                            &base_name,
                            &mut candidate_n,
                            &mut fallback_exists_attempts,
                        )
                    {
                        remote_path = next_path;
                        sent_any_bytes = false;
                        self.lock_resolved_upload_path(id, &remote_path, &next_name);
                        self.update_progress(id, 0, total_size, None);
                        retry_started = None;
                        attempt = 0;
                        highest_ack_offset = 0;
                        consecutive_cancelled_no_progress = 0;
                        continue;
                    }
                    if self
                        .verify_uploaded_destination(
                            &initial.transfer.machine,
                            &remote_path,
                            total_size,
                            &local_hash,
                            cancel,
                        )
                        .await?
                    {
                        self.update_progress(id, total_size, total_size, None);
                        self.maybe_broadcast_progress();
                        return Ok(WorkerOutcome {
                            bytes_total: total_size,
                            hash: local_hash,
                            local_path: None,
                            finalized_in_cache: false,
                        });
                    }
                    let app_err = AppError::from(err);
                    if self
                        .should_retry_upload(
                            &app_err,
                            &mut retry_started,
                            &mut attempt,
                            &mut consecutive_cancelled_no_progress,
                            cancel,
                        )
                        .await?
                    {
                        continue;
                    }
                    return Err(WorkerStop::Failed(app_err));
                }
            }
        }
    }

    /// Checks whether `remote_path` already exists on `machine` with the exact `expected_size`
    /// and BLAKE3 `expected_hash` that were uploaded.
    ///
    /// Called whenever an upload ends without its final `ok` reply (on any route, direct or
    /// relayed) so a transfer whose bytes were already verified and renamed on the target is
    /// reported `Done` instead of `Failed` or duplicated.
    async fn verify_uploaded_destination(
        &self,
        machine: &str,
        remote_path: &str,
        expected_size: u64,
        expected_hash: &str,
        cancel: &CancellationToken,
    ) -> Result<bool, WorkerStop> {
        if cancel.is_cancelled() {
            return Err(WorkerStop::Interrupted);
        }
        let Ok(target) = self.backend.content_target(machine) else {
            return Ok(false);
        };
        let call_fut = target.link.call_machine(
            &target.link_node,
            "fs.hash",
            json!({ "path": remote_path }),
            |_| {},
        );
        let res = tokio::select! {
            biased;
            () = cancel.cancelled() => return Err(WorkerStop::Interrupted),
            timed = tokio::time::timeout(self.stall_timeout, call_fut) => {
                match timed {
                    Ok(res) => res,
                    Err(_) => return Ok(false),
                }
            }
        };
        let Ok(routed) = res else {
            return Ok(false);
        };
        let size_matches = routed.value.get("size").and_then(Value::as_u64) == Some(expected_size);
        let hash_matches = routed
            .value
            .get("hash")
            .and_then(Value::as_str)
            .map(str::trim)
            == Some(expected_hash);
        Ok(size_matches && hash_matches)
    }

    async fn hash_upload_source(
        &self,
        local_path: &str,
        cancel: &CancellationToken,
    ) -> Result<(u64, String), WorkerStop> {
        let mut source = self
            .platform
            .open_upload_source(local_path)
            .await
            .map_err(WorkerStop::Failed)?;
        let mut hasher = blake3::Hasher::new();
        let mut buf = vec![0u8; COPY_BUF_BYTES];
        let mut total: u64 = 0;
        loop {
            if cancel.is_cancelled() {
                return Err(WorkerStop::Interrupted);
            }
            let n = tokio::select! {
                biased;
                () = cancel.cancelled() => return Err(WorkerStop::Interrupted),
                res = source.read_chunk(&mut buf) => res.map_err(WorkerStop::Failed)?,
            };
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
            total = total.saturating_add(n as u64);
        }
        Ok((total, format!("blake3:{}", hasher.finalize().to_hex())))
    }

    fn should_wifi_pause(&self, total_size: u64) -> bool {
        let settings = self.settings();
        settings.wifi_only_large
            && total_size >= settings.large_threshold_bytes
            && !self.platform.is_unmetered_wifi()
    }

    async fn should_retry_upload(
        &self,
        err: &AppError,
        retry_started: &mut Option<Instant>,
        attempt: &mut u32,
        consecutive_cancelled_no_progress: &mut u32,
        cancel: &CancellationToken,
    ) -> Result<bool, WorkerStop> {
        if cancel.is_cancelled() {
            return Err(WorkerStop::Interrupted);
        }
        if err.code == ErrorCode::Cancelled {
            *consecutive_cancelled_no_progress =
                consecutive_cancelled_no_progress.saturating_add(1);
            if *consecutive_cancelled_no_progress >= MAX_CONSECUTIVE_UNSOLICITED_UPLOAD_CANCELS {
                return Err(WorkerStop::Failed(unsolicited_upload_cancel_error()));
            }
        } else {
            *consecutive_cancelled_no_progress = 0;
        }
        self.should_retry(err, retry_started, attempt, cancel).await
    }

    async fn should_retry(
        &self,
        err: &AppError,
        retry_started: &mut Option<Instant>,
        attempt: &mut u32,
        cancel: &CancellationToken,
    ) -> Result<bool, WorkerStop> {
        if cancel.is_cancelled() {
            return Err(WorkerStop::Interrupted);
        }
        if !is_transient_error(err) {
            return Ok(false);
        }
        let started = *retry_started.get_or_insert_with(Instant::now);
        if started.elapsed() >= self.retry_window {
            return Ok(false);
        }
        *attempt = attempt.saturating_add(1);
        let delay = match *attempt {
            1 => Duration::from_millis(750),
            2 => Duration::from_secs(2),
            3 => Duration::from_secs(3),
            _ => Duration::from_secs(5),
        };
        let remaining = self.retry_window.saturating_sub(started.elapsed());
        let wait_for = delay.min(remaining);
        tokio::select! {
            biased;
            () = cancel.cancelled() => Err(WorkerStop::Interrupted),
            () = tokio::time::sleep(wait_for) => Ok(true),
        }
    }

    fn update_remote_meta(&self, id: &str, size: u64, mtime: ContentTime, file_id: Option<u64>) {
        let mut changed = false;
        {
            let mut guard = self
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if let Some(rec) = guard.items.iter_mut().find(|r| r.transfer.id == id)
                && (rec.transfer.bytes_total != size
                    || rec.remote_mtime_secs != Some(mtime.secs)
                    || rec.remote_mtime_nanos != Some(mtime.nanos)
                    || rec.remote_file_id != file_id)
            {
                rec.transfer.bytes_total = size;
                rec.remote_mtime_secs = Some(mtime.secs);
                rec.remote_mtime_nanos = Some(mtime.nanos);
                rec.remote_file_id = file_id;
                changed = true;
            }
        }
        if changed {
            self.persist_locked_snapshot();
            self.maybe_broadcast_progress();
        }
    }

    fn update_upload_meta(&self, id: &str, size: u64, hash: &str) {
        let mut guard = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(rec) = guard.items.iter_mut().find(|r| r.transfer.id == id) {
            rec.transfer.bytes_total = size;
            rec.transfer.hash = Some(hash.to_owned());
        }
        drop(guard);
        self.persist_locked_snapshot();
    }

    fn update_progress(&self, id: &str, bytes_done: u64, bytes_total: u64, rate: Option<u64>) {
        let mut guard = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(rec) = guard.items.iter_mut().find(|r| r.transfer.id == id) {
            rec.transfer.bytes_done = bytes_done;
            rec.transfer.bytes_total = bytes_total;
            rec.transfer.rate = rate;
        }
    }

    fn clear_finalized_state(&self, id: &str) {
        {
            let mut guard = self
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if let Some(rec) = guard.items.iter_mut().find(|r| r.transfer.id == id) {
                rec.finalized_local_path = None;
                rec.finalized_hash = None;
                rec.finalized_in_cache = false;
                rec.transfer.bytes_done = 0;
            }
        }
        self.persist_locked_snapshot();
    }
}

fn advance_upload_candidate(
    dir_prefix: &str,
    sep: char,
    base_name: &str,
    candidate_n: &mut u32,
    fallback_attempts: &mut u32,
) -> Option<(String, String)> {
    if *candidate_n < 9999 {
        *candidate_n += 1;
        let next_name = file_policy::numbered_candidate_name(base_name, *candidate_n);
        let next_path = join_remote_with_sep(dir_prefix, sep, &next_name);
        return Some((next_path, next_name));
    }
    if *fallback_attempts >= 16 {
        return None;
    }
    *fallback_attempts += 1;
    let path_ref = Path::new(base_name);
    let stem = path_ref
        .file_stem()
        .and_then(|s| s.to_str())
        .filter(|s| !s.is_empty())
        .unwrap_or(base_name);
    let ext = path_ref
        .extension()
        .and_then(|e| e.to_str())
        .filter(|e| !e.is_empty());
    let suffix = format!("{}-{}", now_unix_secs(), *fallback_attempts);
    let next_name = match ext {
        Some(e) => format!("{stem}-{suffix}.{e}"),
        None => format!("{stem}-{suffix}"),
    };
    let next_path = join_remote_with_sep(dir_prefix, sep, &next_name);
    Some((next_path, next_name))
}

fn push_unique_cleanup_path(paths: &mut Vec<String>, path: String) {
    if !path.is_empty() && !paths.iter().any(|existing| existing == &path) {
        paths.push(path);
    }
}

pub(crate) async fn remove_file_if_present(path: &Path) -> Result<(), AppError> {
    match tokio::fs::remove_file(path).await {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(AppError::new(
            ErrorCode::Failed,
            format!("Could not remove {}: {err}.", path.display()),
        )
        .with_hint(
            "Close any app using this file; Legion will keep retrying removal automatically.",
        )),
    }
}

fn clear_own_worker_slot(guard: &mut QueueState, id: &str, worker_id: u64) {
    if guard
        .running
        .get(id)
        .is_some_and(|w| w.worker_id == worker_id)
    {
        guard.running.remove(id);
    }
}

struct WorkerOutcome {
    bytes_total: u64,
    hash: String,
    local_path: Option<String>,
    finalized_in_cache: bool,
}

enum WorkerStop {
    Interrupted,
    WifiOnlyPause,
    Stalled(AppError),
    Failed(AppError),
}

const MAX_CONSECUTIVE_UNSOLICITED_UPLOAD_CANCELS: u32 = 3;

fn unsolicited_upload_cancel_error() -> AppError {
    AppError::new(ErrorCode::Cancelled, "The upload kept being interrupted.")
        .with_hint("Resume the upload to try again.")
}

fn is_transient_error(err: &AppError) -> bool {
    matches!(
        err.code,
        ErrorCode::Unreachable | ErrorCode::Timeout | ErrorCode::Busy | ErrorCode::Cancelled
    )
}

fn random_opaque_handle() -> Result<String, AppError> {
    let mut bytes = [0u8; 24];
    getrandom::fill(&mut bytes).map_err(|err| {
        AppError::new(
            ErrorCode::Internal,
            format!("Could not generate upload handle: {err}."),
        )
    })?;
    let mut out = String::with_capacity("up-".len() + bytes.len() * 2);
    out.push_str("up-");
    for b in bytes {
        use std::fmt::Write as _;
        let _ = write!(&mut out, "{b:02x}");
    }
    Ok(out)
}

fn sweep_orphaned_parts(parts_dir: &Path, live_download_ids: &std::collections::HashSet<String>) {
    let Ok(entries) = fs::read_dir(parts_dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if let Some(stem) = name.strip_suffix(".part")
            && !live_download_ids.contains(stem)
        {
            let _ = fs::remove_file(&path);
        }
    }
}

async fn open_part_file_for_append(path: &Path, offset: u64) -> Result<AsyncFile, AppError> {
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await.map_err(|err| {
            AppError::new(
                ErrorCode::Internal,
                format!("Could not create transfer staging directory: {err}."),
            )
        })?;
    }
    let std_file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(offset == 0)
        .open(path)
        .map_err(|err| {
            AppError::new(
                ErrorCode::Failed,
                format!("Could not open partial download file: {err}."),
            )
        })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std_file.set_permissions(fs::Permissions::from_mode(0o600));
    }
    let mut file = AsyncFile::from_std(std_file);
    file.seek(SeekFrom::Start(offset)).await.map_err(|err| {
        AppError::new(
            ErrorCode::Failed,
            format!("Could not seek partial download file to offset {offset}: {err}."),
        )
    })?;
    Ok(file)
}

async fn hash_file_blake3(path: &Path, cancel: &CancellationToken) -> Result<String, WorkerStop> {
    let mut file = AsyncFile::open(path).await.map_err(|err| {
        WorkerStop::Failed(AppError::new(
            ErrorCode::Failed,
            format!("Could not reopen downloaded file for verification: {err}."),
        ))
    })?;
    let mut hasher = blake3::Hasher::new();
    let mut buf = vec![0u8; COPY_BUF_BYTES];
    loop {
        if cancel.is_cancelled() {
            return Err(WorkerStop::Interrupted);
        }
        let n = file.read(&mut buf).await.map_err(|err| {
            WorkerStop::Failed(AppError::new(
                ErrorCode::Failed,
                format!("Could not hash downloaded file: {err}."),
            ))
        })?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(format!("blake3:{}", hasher.finalize().to_hex()))
}

fn local_file_mtime(path: &str) -> Option<ContentTime> {
    if path.starts_with("content://") {
        return None;
    }
    let modified = fs::metadata(path).ok()?.modified().ok()?;
    match modified.duration_since(UNIX_EPOCH) {
        Ok(dur) => Some(ContentTime {
            secs: dur.as_secs() as i64,
            nanos: dur.subsec_nanos(),
        }),
        Err(_) => None,
    }
}

fn snapshot_transfers(items: &[TransferRecord]) -> Vec<DeviceTransfer> {
    items.iter().rev().map(|r| r.transfer.clone()).collect()
}

fn prune_terminal_locked(items: &mut Vec<TransferRecord>) -> Vec<TransferRecord> {
    let terminal_count = items
        .iter()
        .filter(|r| r.transfer.state.is_terminal())
        .count();
    if terminal_count <= MAX_RETAINED_TERMINAL {
        return Vec::new();
    }
    let mut to_drop = terminal_count - MAX_RETAINED_TERMINAL;
    let mut pruned = Vec::with_capacity(to_drop);
    let mut kept = Vec::with_capacity(items.len() - to_drop);
    for rec in items.drain(..) {
        if to_drop > 0 && rec.transfer.state.is_terminal() {
            to_drop -= 1;
            pruned.push(rec);
        } else {
            kept.push(rec);
        }
    }
    *items = kept;
    pruned
}

fn build_notification_summary(items: &[TransferRecord]) -> QueueNotificationSummary {
    let active: Vec<&DeviceTransfer> = items
        .iter()
        .map(|r| &r.transfer)
        .filter(|t| t.state.is_active())
        .collect();
    let running_count = active
        .iter()
        .filter(|t| t.state == TransferState::Running)
        .count();
    let paused: Vec<&DeviceTransfer> = items
        .iter()
        .map(|r| &r.transfer)
        .filter(|t| t.state == TransferState::Paused)
        .collect();
    let cancelling: Vec<&DeviceTransfer> = items
        .iter()
        .map(|r| &r.transfer)
        .filter(|t| t.state == TransferState::Cancelling)
        .collect();

    let focus: &[&DeviceTransfer] = if !active.is_empty() {
        &active
    } else if !paused.is_empty() {
        &paused
    } else {
        &cancelling
    };

    let bytes_done: u64 = focus.iter().map(|t| t.bytes_done).sum();
    let bytes_total: u64 = focus.iter().map(|t| t.bytes_total).sum();
    let indeterminate = bytes_total == 0 || focus.iter().any(|t| t.bytes_total == 0);

    let (title, detail, primary_transfer_id) = if active.len() == 1 {
        let t = active[0];
        let heading = match t.direction {
            TransferDirection::Download => format!("Downloading {}", t.name),
            TransferDirection::Upload => format!("Uploading {} to {}", t.name, t.machine),
        };
        let detail = format_progress_line(t.bytes_done, t.bytes_total);
        (heading, detail, Some(t.id.clone()))
    } else if active.len() > 1 {
        let detail = format_progress_line(bytes_done, bytes_total);
        (
            format!("{} transfers in progress", active.len()),
            detail,
            None,
        )
    } else if paused.len() == 1 {
        let t = paused[0];
        let reason = match t.paused_reason {
            Some(PauseReason::WifiOnly) => "Waiting for Wi-Fi",
            Some(PauseReason::FgsTimeout) => "Paused by Android",
            Some(PauseReason::Stalled) => "Stopped responding",
            _ => "Paused",
        };
        (
            format!("{reason} · {}", t.name),
            format_progress_line(t.bytes_done, t.bytes_total),
            Some(t.id.clone()),
        )
    } else if paused.len() > 1 {
        (
            format!("{} transfers paused", paused.len()),
            format_progress_line(bytes_done, bytes_total),
            None,
        )
    } else if cancelling.len() == 1 {
        let t = cancelling[0];
        let detail = t
            .error
            .as_ref()
            .map(|e| e.message.clone())
            .unwrap_or_else(|| "Removing partial files…".to_owned());
        (format!("Cancelling {}", t.name), detail, Some(t.id.clone()))
    } else if cancelling.len() > 1 {
        (
            format!("Cancelling {} transfers", cancelling.len()),
            "Removing partial files…".to_owned(),
            None,
        )
    } else {
        (String::new(), String::new(), None)
    };

    QueueNotificationSummary {
        active_count: active.len(),
        running_count,
        paused_count: paused.len(),
        cancelling_count: cancelling.len(),
        title,
        detail,
        bytes_done,
        bytes_total,
        indeterminate,
        can_pause: !active.is_empty(),
        can_resume: active.is_empty() && !paused.is_empty(),
        primary_transfer_id,
    }
}

fn format_progress_line(done: u64, total: u64) -> String {
    if total == 0 {
        return format_bytes(done);
    }
    let pct = ((done.min(total) as u128 * 100) / (total as u128)) as u64;
    format!("{} ({pct}%)", format_byte_pair(done, total))
}

fn format_decimal_value(v: f64) -> String {
    if v >= 10.0 {
        format!("{}", v.round() as u64)
    } else {
        let tenths = (v * 10.0).round() as u64;
        if tenths.is_multiple_of(10) {
            format!("{}", tenths / 10)
        } else {
            format!("{}.{}", tenths / 10, tenths % 10)
        }
    }
}

/// Formats a byte count using the same SI decimal units (`B`, `KB`, `MB`, `GB`, `TB`, `PB`) as
/// `apps/ui/lib/format.js`.
pub fn format_bytes(bytes: u64) -> String {
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    const UNITS: [&str; 5] = ["KB", "MB", "GB", "TB", "PB"];
    let mut value = bytes as f64;
    let mut index = 0usize;
    value /= 1000.0;
    while value >= 1000.0 && index < UNITS.len() - 1 {
        value /= 1000.0;
        index += 1;
    }
    format!("{} {}", format_decimal_value(value), UNITS[index])
}

/// Formats a completed/total byte pair using the same SI decimal rules as `formatBytePair` in
/// `apps/ui/lib/format.js`.
pub fn format_byte_pair(used: u64, total: u64) -> String {
    if total == 0 {
        return "0 B".to_owned();
    }
    const UNITS: [&str; 6] = ["B", "KB", "MB", "GB", "TB", "PB"];
    let mut scale = 1.0_f64;
    let mut unit = 0usize;
    while (total as f64) / scale >= 1000.0 && unit < UNITS.len() - 1 {
        scale *= 1000.0;
        unit += 1;
    }
    if (used as f64) / scale < 1.0 && unit > 0 {
        return format!("{} of {}", format_bytes(used), format_bytes(total));
    }
    format!(
        "{} of {} {}",
        format_decimal_value((used as f64) / scale),
        format_decimal_value((total as f64) / scale),
        UNITS[unit]
    )
}

/// Joins a remote destination folder and filename without creating duplicate separators.
fn join_remote_path(dir: &str, name: &str) -> String {
    let sep = if dir.contains('\\') && !dir.contains('/') {
        '\\'
    } else {
        '/'
    };
    join_remote_with_sep(dir, sep, name)
}

fn split_remote_dir(full_path: &str) -> (String, char) {
    let sep = if full_path.contains('\\') && !full_path.contains('/') {
        '\\'
    } else {
        '/'
    };
    match full_path.rfind(['/', '\\']) {
        Some(0) => (sep.to_string(), sep),
        Some(idx) => (full_path[..idx].to_owned(), sep),
        None => (String::new(), sep),
    }
}

fn join_remote_with_sep(dir: &str, sep: char, name: &str) -> String {
    if dir.is_empty() {
        name.to_owned()
    } else if dir.ends_with('/') || dir.ends_with('\\') {
        format!("{dir}{name}")
    } else {
        format!("{dir}{sep}{name}")
    }
}

fn load_queue_file(path: &Path) -> PersistedQueue {
    let Ok(bytes) = fs::read(path) else {
        return PersistedQueue::default();
    };
    serde_json::from_slice(&bytes).unwrap_or_default()
}

fn save_queue_file(path: &Path, data: &PersistedQueue) -> Result<(), AppError> {
    let Some(parent) = path.parent() else {
        return Ok(());
    };
    fs::create_dir_all(parent).map_err(|err| {
        AppError::new(
            ErrorCode::Internal,
            format!("Could not create transfer queue directory: {err}."),
        )
    })?;
    let mut payload = serde_json::to_vec_pretty(data).map_err(|err| {
        AppError::new(
            ErrorCode::Internal,
            format!("Could not serialize transfer queue: {err}."),
        )
    })?;
    payload.push(b'\n');

    let mut tmp = NamedTempFile::new_in(parent).map_err(|err| {
        AppError::new(
            ErrorCode::Internal,
            format!("Could not create temp file for transfer queue: {err}."),
        )
    })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = tmp
            .as_file()
            .set_permissions(fs::Permissions::from_mode(0o600));
    }
    tmp.write_all(&payload).map_err(|err| {
        AppError::new(
            ErrorCode::Internal,
            format!("Could not write transfer queue: {err}."),
        )
    })?;
    let _ = tmp.as_file().sync_all();
    tmp.persist(path).map_err(|err| {
        AppError::new(
            ErrorCode::Internal,
            format!("Could not replace transfer queue file: {}.", err.error),
        )
    })?;
    Ok(())
}

fn now_unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn now_rfc3339_utc() -> String {
    let secs = now_unix_secs();
    let days = (secs / 86_400) as i64;
    let sod = secs % 86_400;
    let hour = sod / 3_600;
    let minute = (sod % 3_600) / 60;
    let second = sod % 60;
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = (yoe as i64) + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = y + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use std::future::Future;
    use std::pin::Pin;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    use legion_link::proto::{
        ErrorBody, ErrorCode as ProtoCode, HelloResult, PROTOCOL, Response as ProtoResp, VERSION,
    };
    use legion_link::{
        Link, LinkConfig, LinkOptions, NodeLink, RawStream, Session, SessionOpener, SshTarget,
        StreamOpenError, StreamOpener, TransportHandle,
    };
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

    use super::*;
    use crate::backend::{
        AppState, CallId, CallRequest, CancelResult, LearnResult, RepairResult, UnpairResult,
    };

    struct FakeTransport {
        opener: Arc<dyn StreamOpener>,
    }

    impl TransportHandle for FakeTransport {
        fn collect_exit_info(
            &mut self,
            _wait_limit: Duration,
        ) -> Pin<Box<dyn Future<Output = (Option<i32>, String)> + Send + '_>> {
            Box::pin(async { (Some(0), String::new()) })
        }

        fn close(self: Box<Self>, _grace: Duration) -> Pin<Box<dyn Future<Output = ()> + Send>> {
            Box::pin(async {})
        }

        fn stream_opener(&self) -> Option<Arc<dyn StreamOpener>> {
            Some(Arc::clone(&self.opener))
        }
    }

    #[derive(Default)]
    struct MockRemoteState {
        files: HashMap<String, Vec<u8>>,
        uploaded: HashMap<String, Vec<u8>>,
        partial_uploads: HashMap<String, Vec<u8>>,
        override_hash: HashMap<String, String>,
        per_chunk_delay_ms: u64,
        fail_first_gets: usize,
        stall_get: bool,
        stall_put: bool,
        advertise_cap_if_exists: bool,
        race_create_after_stat: std::collections::HashSet<String>,
        race_create_on_put_finish: std::collections::HashSet<String>,
        cancel_after_put_commit: bool,
        cancel_mid_put_once: bool,
        cancel_mid_put_no_progress: bool,
        cancel_mid_put_with_progress_remaining: usize,
        drop_mid_put_no_progress: bool,
        stat_calls: AtomicUsize,
        put_requests: Mutex<Vec<Value>>,
        active_streams: AtomicUsize,
        max_concurrent_observed: AtomicUsize,
        opened_get_streams: AtomicUsize,
    }

    struct MockStreamOpener {
        remote: Arc<Mutex<MockRemoteState>>,
        window_policy: Option<legion_link::content::WindowPolicy>,
    }

    impl StreamOpener for MockStreamOpener {
        fn window_policy(&self) -> Option<legion_link::content::WindowPolicy> {
            self.window_policy
        }

        fn open_stream(
            &self,
            _cancel: CancellationToken,
        ) -> Pin<Box<dyn Future<Output = Result<RawStream, StreamOpenError>> + Send + '_>> {
            let remote = Arc::clone(&self.remote);
            let supports_keep_open = self.window_policy.is_some();
            Box::pin(async move {
                let (client_in, mut server_out) = tokio::io::duplex(64 * 1024);
                let (server_in, client_out) = tokio::io::duplex(64 * 1024);
                let stream_cancel = CancellationToken::new();
                let stream_park = CancellationToken::new();
                let watch_cancel = stream_cancel.clone();
                let watch_park = stream_park.clone();

                tokio::spawn(async move {
                    let mut reader = BufReader::new(server_in);
                    let mut line = String::new();
                    if reader.read_line(&mut line).await.unwrap_or(0) == 0 {
                        return;
                    }
                    let mut req: Value = serde_json::from_str(&line).unwrap();
                    let advertise_cap = remote.lock().unwrap().advertise_cap_if_exists;
                    let greeting = if advertise_cap {
                        json!({ "protocol": 1, "capabilities": [CAP_IF_EXISTS] })
                    } else {
                        json!({ "protocol": 1 })
                    };
                    let _ = server_out
                        .write_all(format!("{greeting}\n").as_bytes())
                        .await;
                    let op = req["op"].as_str().unwrap_or("");
                    if op == "get" {
                        remote
                            .lock()
                            .unwrap()
                            .opened_get_streams
                            .fetch_add(1, Ordering::SeqCst);
                        loop {
                            let path = req["path"].as_str().unwrap_or("").to_owned();
                            let offset = req["offset"].as_u64().unwrap_or(0) as usize;
                            let req_len = req
                                .get("length")
                                .and_then(Value::as_u64)
                                .map(|n| n as usize);
                            let keep_open =
                                supports_keep_open && req["keep_open"].as_bool() == Some(true);

                            let (payload, delay_ms, fail_now, stall_get) = {
                                let mut st = remote.lock().unwrap();
                                let fail_now = if st.fail_first_gets > 0 && req_len != Some(0) {
                                    st.fail_first_gets -= 1;
                                    true
                                } else {
                                    false
                                };
                                (
                                    st.files.get(&path).cloned(),
                                    st.per_chunk_delay_ms,
                                    fail_now,
                                    st.stall_get,
                                )
                            };
                            if fail_now {
                                let err = json!({
                                    "error": ErrorBody::new(ProtoCode::Unreachable, "temporary network drop")
                                });
                                let _ = server_out.write_all(format!("{err}\n").as_bytes()).await;
                                return;
                            }
                            let Some(data) = payload else {
                                let err = json!({
                                    "error": ErrorBody::new(ProtoCode::NotFound, "missing file")
                                });
                                let _ = server_out.write_all(format!("{err}\n").as_bytes()).await;
                                return;
                            };
                            let avail = data.len().saturating_sub(offset);
                            let len = req_len.map_or(avail, |n| n.min(avail));
                            let mut ready = json!({
                                "size": data.len(),
                                "offset": offset,
                                "length": len,
                                "mtime": { "secs": 1_700_000_000i64, "nanos": 0u32 },
                                "file_id": 11u64,
                            });
                            if keep_open {
                                ready["keep_open"] = Value::Bool(true);
                            }
                            if server_out
                                .write_all(format!("{ready}\n").as_bytes())
                                .await
                                .is_err()
                            {
                                return;
                            }
                            if len > 0 {
                                let cur = remote
                                    .lock()
                                    .unwrap()
                                    .active_streams
                                    .fetch_add(1, Ordering::SeqCst)
                                    + 1;
                                remote
                                    .lock()
                                    .unwrap()
                                    .max_concurrent_observed
                                    .fetch_max(cur, Ordering::SeqCst);
                            }
                            if stall_get && len > 0 {
                                // Hold the stream open without sending bytes until cancelled or dropped.
                                tokio::select! {
                                    () = watch_cancel.cancelled() => {}
                                    () = watch_park.cancelled() => {}
                                    () = tokio::time::sleep(Duration::from_secs(30)) => {}
                                }
                                remote
                                    .lock()
                                    .unwrap()
                                    .active_streams
                                    .fetch_sub(1, Ordering::SeqCst);
                                return;
                            }
                            let mut pos = offset;
                            let end = offset + len;
                            let mut stopped = false;
                            while pos < end {
                                let next = (pos + 16 * 1024).min(end);
                                if delay_ms > 0 {
                                    tokio::select! {
                                        biased;
                                        () = watch_cancel.cancelled() => { stopped = true; break; }
                                        () = watch_park.cancelled() => { stopped = true; break; }
                                        () = tokio::time::sleep(Duration::from_millis(delay_ms)) => {}
                                    }
                                }
                                if watch_cancel.is_cancelled() || watch_park.is_cancelled() {
                                    stopped = true;
                                    break;
                                }
                                let write_ok = tokio::select! {
                                    biased;
                                    () = watch_cancel.cancelled() => false,
                                    () = watch_park.cancelled() => false,
                                    res = server_out.write_all(&data[pos..next]) => res.is_ok(),
                                };
                                if !write_ok {
                                    stopped = true;
                                    break;
                                }
                                pos = next;
                            }
                            if len > 0 {
                                remote
                                    .lock()
                                    .unwrap()
                                    .active_streams
                                    .fetch_sub(1, Ordering::SeqCst);
                            }
                            if stopped {
                                return;
                            }
                            if !keep_open {
                                break;
                            }
                            if server_out.write_all(b"{\"done\":true}\n").await.is_err() {
                                return;
                            }
                            line.clear();
                            let next_read = tokio::select! {
                                biased;
                                () = watch_cancel.cancelled() => return,
                                () = watch_park.cancelled() => return,
                                res = reader.read_line(&mut line) => res,
                            };
                            if next_read.unwrap_or(0) == 0 {
                                return;
                            }
                            req = serde_json::from_str(&line).unwrap();
                        }
                    } else if op == "put" {
                        let path = req["path"].as_str().unwrap_or("").to_owned();
                        let size = req["size"].as_u64().unwrap() as usize;
                        let expected_hash = req["hash"].as_str().unwrap_or("").to_owned();
                        let fail_if_exists = req.get("if_exists").and_then(Value::as_str)
                            == Some("fail")
                            && advertise_cap;
                        let (mut existing, stall_put, already_exists) = {
                            let st = remote.lock().unwrap();
                            st.put_requests.lock().unwrap().push(req.clone());
                            let exists =
                                st.files.contains_key(&path) || st.uploaded.contains_key(&path);
                            (
                                st.partial_uploads.get(&path).cloned().unwrap_or_default(),
                                st.stall_put,
                                exists,
                            )
                        };
                        if fail_if_exists && already_exists {
                            let err = json!({
                                "error": ErrorBody::new(
                                    ProtoCode::Exists,
                                    format!("{path} already exists."),
                                )
                                .with_hint(
                                    "Pass if_exists=\"replace\", \"skip\", or \"rename\", or choose another path.",
                                )
                            });
                            let _ = server_out.write_all(format!("{err}\n").as_bytes()).await;
                            let _ = server_out.shutdown().await;
                            return;
                        }
                        if existing.len() > size {
                            existing.clear();
                        }
                        let resume_off = existing.len();
                        let ready = json!({ "offset": resume_off });
                        let _ = server_out.write_all(format!("{ready}\n").as_bytes()).await;
                        if stall_put {
                            // Stop reading from the upload duplex so the writer back-pressures
                            // and stalls cleanly.
                            tokio::select! {
                                () = watch_cancel.cancelled() => {}
                                () = tokio::time::sleep(Duration::from_secs(30)) => {}
                            }
                            return;
                        }
                        let mut buf = vec![0u8; 16 * 1024];
                        let mut got = existing;
                        let (
                            cancel_mid_once,
                            cancel_no_progress,
                            cancel_with_progress,
                            drop_no_progress,
                        ) = {
                            let mut st = remote.lock().unwrap();
                            let with_prog = if st.cancel_mid_put_with_progress_remaining > 0 {
                                st.cancel_mid_put_with_progress_remaining -= 1;
                                true
                            } else {
                                false
                            };
                            (
                                std::mem::take(&mut st.cancel_mid_put_once),
                                st.cancel_mid_put_no_progress,
                                with_prog,
                                st.drop_mid_put_no_progress,
                            )
                        };
                        while got.len() < size {
                            tokio::select! {
                                biased;
                                () = watch_cancel.cancelled() => {
                                    remote.lock().unwrap().partial_uploads.insert(path, got);
                                    return;
                                }
                                res = reader.read(&mut buf) => match res {
                                    Ok(0) => break,
                                    Ok(n) => {
                                        got.extend_from_slice(&buf[..n]);
                                        if drop_no_progress {
                                            // Discard bytes without saving partials and close
                                            // without a reply (simulates abrupt transport drop).
                                            let _ = server_out.shutdown().await;
                                            return;
                                        }
                                        if cancel_no_progress {
                                            // Discard bytes without saving partials and send an
                                            // unsolicited `cancelled` reply.
                                            let err = json!({
                                                "error": ErrorBody::new(
                                                    ProtoCode::Cancelled,
                                                    "The stream stopped because the node is shutting down.",
                                                )
                                                .with_machine("tower")
                                            });
                                            let _ = server_out
                                                .write_all(format!("{err}\n").as_bytes())
                                                .await;
                                            let _ = server_out.shutdown().await;
                                            return;
                                        }
                                        if (cancel_mid_once || cancel_with_progress) && got.len() < size {
                                            remote.lock().unwrap().partial_uploads.insert(path, got);
                                            let err = json!({
                                                "error": ErrorBody::new(
                                                    ProtoCode::Cancelled,
                                                    "The stream stopped because the node is shutting down.",
                                                )
                                                .with_machine("tower")
                                            });
                                            let _ = server_out
                                                .write_all(format!("{err}\n").as_bytes())
                                                .await;
                                            let _ = server_out.shutdown().await;
                                            return;
                                        }
                                    }
                                    Err(_) => return,
                                }
                            }
                        }
                        if got.len() != size {
                            remote.lock().unwrap().partial_uploads.insert(path, got);
                            return;
                        }
                        let actual = format!("blake3:{}", blake3::hash(&got).to_hex());
                        if actual != expected_hash {
                            let err = json!({
                                "error": ErrorBody::new(ProtoCode::Conflict, "upload hash mismatch")
                            });
                            let _ = server_out.write_all(format!("{err}\n").as_bytes()).await;
                            return;
                        }
                        let raced_exists = {
                            let mut st = remote.lock().unwrap();
                            if fail_if_exists {
                                if st.race_create_on_put_finish.remove(&path) {
                                    st.files
                                        .insert(path.clone(), b"concurrent-device-file".to_vec());
                                    st.partial_uploads.remove(&path);
                                    true
                                } else if st.files.contains_key(&path)
                                    || st.uploaded.contains_key(&path)
                                {
                                    st.partial_uploads.remove(&path);
                                    true
                                } else {
                                    st.partial_uploads.remove(&path);
                                    st.uploaded.insert(path, got);
                                    false
                                }
                            } else {
                                st.partial_uploads.remove(&path);
                                st.uploaded.insert(path, got);
                                false
                            }
                        };
                        if raced_exists {
                            let err = json!({
                                "error": ErrorBody::new(ProtoCode::Exists, "destination created concurrently")
                            });
                            let _ = server_out.write_all(format!("{err}\n").as_bytes()).await;
                            let _ = server_out.shutdown().await;
                            return;
                        }
                        let cancel_after_commit = remote.lock().unwrap().cancel_after_put_commit;
                        if cancel_after_commit {
                            let err = json!({
                                "error": ErrorBody::new(
                                    ProtoCode::Cancelled,
                                    "The stream stopped because the node is shutting down.",
                                )
                                .with_machine("tower")
                            });
                            let _ = server_out.write_all(format!("{err}\n").as_bytes()).await;
                            let _ = server_out.shutdown().await;
                            return;
                        }
                        let done = json!({ "ok": true, "size": size, "hash": actual });
                        let _ = server_out.write_all(format!("{done}\n").as_bytes()).await;
                    }
                    let _ = server_out.shutdown().await;
                });

                Ok(RawStream {
                    reader: Box::new(client_in),
                    writer: Box::new(client_out),
                    stderr: None,
                    cancel: stream_cancel,
                    park: Some(stream_park),
                    guard: None,
                })
            })
        }
    }

    struct MockBackend {
        link: Link,
    }

    impl MockBackend {
        fn new(remote: Arc<Mutex<MockRemoteState>>) -> Self {
            Self::with_options(remote, LinkOptions::default(), None)
        }

        fn with_options(
            remote: Arc<Mutex<MockRemoteState>>,
            link_options: LinkOptions,
            window_policy: Option<legion_link::content::WindowPolicy>,
        ) -> Self {
            let stream_opener: Arc<dyn StreamOpener> = Arc::new(MockStreamOpener {
                remote: Arc::clone(&remote),
                window_policy,
            });
            let opener: SessionOpener = Arc::new(move |entry, _, cancel| {
                let remote = Arc::clone(&remote);
                let stream_opener = Arc::clone(&stream_opener);
                Box::pin(async move {
                    let (client_in, mut server_out) = tokio::io::duplex(16 * 1024);
                    let (server_in, client_out) = tokio::io::duplex(16 * 1024);
                    tokio::spawn(async move {
                        let mut reader = BufReader::new(server_in);
                        let mut line = String::new();
                        while reader.read_line(&mut line).await.unwrap_or(0) > 0 {
                            let req: legion_link::proto::Request =
                                serde_json::from_str(&line).unwrap();
                            let reply = if req.method == "hello" {
                                ProtoResp::ok(
                                    req.id,
                                    serde_json::to_value(HelloResult {
                                        node: "tower".to_owned(),
                                        node_explicit: None,
                                        node_id: None,
                                        version: VERSION.to_owned(),
                                        build: None,
                                        protocol: PROTOCOL,
                                        os: "linux".to_owned(),
                                        arch: "x86_64".to_owned(),
                                        systems: vec![],
                                        system: None,
                                        boot_id: None,
                                    })
                                    .unwrap(),
                                )
                            } else if req.method == "call" {
                                let tool =
                                    req.params.get("tool").and_then(Value::as_str).unwrap_or("");
                                let path = req
                                    .params
                                    .pointer("/args/path")
                                    .and_then(Value::as_str)
                                    .unwrap_or("");
                                let mut st = remote.lock().unwrap();
                                if tool == "fs.stat" {
                                    st.stat_calls.fetch_add(1, Ordering::SeqCst);
                                    if let Some(data) =
                                        st.files.get(path).or_else(|| st.uploaded.get(path))
                                    {
                                        ProtoResp::ok(
                                            req.id,
                                            json!({ "kind": "file", "size": data.len() }),
                                        )
                                    } else {
                                        if st.race_create_after_stat.remove(path) {
                                            st.files.insert(
                                                path.to_owned(),
                                                b"created-between-stat-and-put".to_vec(),
                                            );
                                        }
                                        ProtoResp::error(
                                            req.id,
                                            legion_link::proto::RpcError::new(
                                                -32000,
                                                ErrorBody::new(ProtoCode::NotFound, "not found"),
                                            ),
                                        )
                                    }
                                } else if let Some(forced) = st.override_hash.get(path) {
                                    ProtoResp::ok(
                                        req.id,
                                        json!({ "path": path, "size": 0, "hash": forced }),
                                    )
                                } else if let Some(data) =
                                    st.files.get(path).or_else(|| st.uploaded.get(path))
                                {
                                    let h = format!("blake3:{}", blake3::hash(data).to_hex());
                                    ProtoResp::ok(
                                        req.id,
                                        json!({ "path": path, "size": data.len(), "hash": h }),
                                    )
                                } else {
                                    ProtoResp::error(
                                        req.id,
                                        legion_link::proto::RpcError::new(
                                            -32000,
                                            ErrorBody::new(ProtoCode::NotFound, "not found"),
                                        ),
                                    )
                                }
                            } else {
                                ProtoResp::ok(req.id, json!({}))
                            };
                            let _ = server_out
                                .write_all(
                                    format!("{}\n", serde_json::to_string(&reply).unwrap())
                                        .as_bytes(),
                                )
                                .await;
                            line.clear();
                        }
                    });
                    Session::open_transport(
                        client_in,
                        client_out,
                        Box::new(FakeTransport {
                            opener: stream_opener,
                        }),
                        &entry.node,
                        None,
                        Duration::from_secs(5),
                        cancel,
                    )
                    .await
                    .map_err(|err| err.into_error())
                })
            });
            let cfg = LinkConfig {
                nodes: vec![NodeLink::ssh(
                    "tower",
                    SshTarget {
                        hosts: vec!["192.0.2.1".to_owned()],
                        port: 22,
                        user: "me".to_owned(),
                        host_keys: vec!["ssh-ed25519 AAAA".to_owned()],
                    },
                    Some("phone".to_owned()),
                )],
                ..LinkConfig::default()
            };
            Self {
                link: Link::with_opener(cfg, link_options, opener),
            }
        }
    }

    impl Backend for MockBackend {
        fn state(&self) -> BoxFuture<'_, Result<AppState, AppError>> {
            Box::pin(async { Err(AppError::not_paired()) })
        }
        fn tools(&self, _on: Option<Value>) -> BoxFuture<'_, Result<Vec<Value>, AppError>> {
            Box::pin(async { Err(AppError::not_paired()) })
        }
        fn call(
            &self,
            _request: CallRequest,
            _on_event: Option<tauri::ipc::Channel<Value>>,
        ) -> BoxFuture<'_, Result<Value, AppError>> {
            Box::pin(async { Err(AppError::not_paired()) })
        }
        fn cancel(&self, _id: CallId) -> BoxFuture<'_, Result<CancelResult, AppError>> {
            Box::pin(async { Err(AppError::not_paired()) })
        }
        fn pair(
            &self,
            _invite: String,
            _name: String,
        ) -> BoxFuture<'_, Result<AppState, AppError>> {
            Box::pin(async { Err(AppError::not_paired()) })
        }
        fn unpair(&self, _node: String) -> BoxFuture<'_, Result<UnpairResult, AppError>> {
            Box::pin(async { Err(AppError::not_paired()) })
        }
        fn select_node(&self, _node: String) -> BoxFuture<'_, Result<AppState, AppError>> {
            Box::pin(async { Err(AppError::not_paired()) })
        }
        fn repair_store(&self) -> BoxFuture<'_, Result<RepairResult, AppError>> {
            Box::pin(async { Err(AppError::not_paired()) })
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
        fn dismiss_identity_change(
            &self,
            _node: String,
        ) -> BoxFuture<'_, Result<AppState, AppError>> {
            Box::pin(async { Err(AppError::not_paired()) })
        }
        fn content_target(&self, machine: &str) -> Result<ContentTarget, AppError> {
            if machine != "tower" {
                return Err(AppError::new(ErrorCode::NotFound, "unknown machine"));
            }
            Ok(ContentTarget {
                link: self.link.clone(),
                link_node: "tower".to_owned(),
                machine: "tower".to_owned(),
                principal: "phone".to_owned(),
                holder: "pairing:tower:1".to_owned(),
                learned_identity: None,
            })
        }
        fn is_pairing_holder_live(&self, holder: &str) -> bool {
            holder == "pairing:tower:1"
        }
    }

    fn consume_injected_failure(counter: &AtomicUsize) -> bool {
        let mut cur = counter.load(Ordering::SeqCst);
        while cur > 0 {
            match counter.compare_exchange_weak(cur, cur - 1, Ordering::SeqCst, Ordering::SeqCst) {
                Ok(_) => return true,
                Err(next) => cur = next,
            }
        }
        false
    }

    type OpenedLog = Arc<Mutex<Vec<(String, Option<String>)>>>;

    struct MockPlatform {
        downloads_dir: PathBuf,
        cache_dir: PathBuf,
        unmetered_wifi: Arc<AtomicBool>,
        opened: OpenedLog,
        shown_in_folder: Arc<Mutex<Vec<String>>>,
        released_uris: Arc<Mutex<Vec<String>>>,
        deleted_saved: Arc<Mutex<Vec<String>>>,
        fail_delete_part: Arc<AtomicUsize>,
        fail_delete_saved: Arc<AtomicUsize>,
        completed_notifications: Arc<Mutex<Vec<String>>>,
        picked_files: Arc<Mutex<Vec<PickedUploadFile>>>,
        finalize_calls: Arc<AtomicUsize>,
        finalize_entered: Arc<Notify>,
        finalize_release: Arc<Notify>,
        gate_finalize: Arc<AtomicBool>,
        return_content_uris: Arc<AtomicBool>,
        next_content_id: Arc<AtomicU64>,
        live_content_uris: Arc<Mutex<HashMap<String, Vec<u8>>>>,
        open_error_once: Arc<Mutex<Option<AppError>>>,
        supports_open_externally: bool,
        supports_open_downloads: bool,
        supports_show_in_folder: bool,
    }

    impl MockPlatform {
        fn new(dir: &Path) -> Self {
            let downloads_dir = dir.join("Downloads");
            let cache_dir = dir.join("cache").join("open-externally");
            fs::create_dir_all(&downloads_dir).unwrap();
            fs::create_dir_all(&cache_dir).unwrap();
            Self {
                downloads_dir,
                cache_dir,
                unmetered_wifi: Arc::new(AtomicBool::new(true)),
                opened: Arc::new(Mutex::new(Vec::new())),
                shown_in_folder: Arc::new(Mutex::new(Vec::new())),
                released_uris: Arc::new(Mutex::new(Vec::new())),
                deleted_saved: Arc::new(Mutex::new(Vec::new())),
                fail_delete_part: Arc::new(AtomicUsize::new(0)),
                fail_delete_saved: Arc::new(AtomicUsize::new(0)),
                completed_notifications: Arc::new(Mutex::new(Vec::new())),
                picked_files: Arc::new(Mutex::new(Vec::new())),
                finalize_calls: Arc::new(AtomicUsize::new(0)),
                finalize_entered: Arc::new(Notify::new()),
                finalize_release: Arc::new(Notify::new()),
                gate_finalize: Arc::new(AtomicBool::new(false)),
                return_content_uris: Arc::new(AtomicBool::new(false)),
                next_content_id: Arc::new(AtomicU64::new(1)),
                live_content_uris: Arc::new(Mutex::new(HashMap::new())),
                open_error_once: Arc::new(Mutex::new(None)),
                supports_open_externally: true,
                supports_open_downloads: true,
                supports_show_in_folder: true,
            }
        }
    }

    impl TransferPlatform for MockPlatform {
        fn supports_open_externally(&self) -> bool {
            self.supports_open_externally
        }

        fn supports_open_downloads(&self) -> bool {
            self.supports_open_downloads
        }

        fn supports_show_in_folder(&self) -> bool {
            self.supports_show_in_folder
        }

        fn is_unmetered_wifi(&self) -> bool {
            self.unmetered_wifi.load(Ordering::Acquire)
        }

        fn pick_upload_files(&self) -> BoxFuture<'_, Result<Vec<PickedUploadFile>, AppError>> {
            let files = self.picked_files.lock().unwrap().clone();
            Box::pin(async move { Ok(files) })
        }

        fn release_upload_source<'a>(&'a self, local_path: &'a str) -> BoxFuture<'a, ()> {
            self.released_uris
                .lock()
                .unwrap()
                .push(local_path.to_owned());
            Box::pin(async {})
        }

        fn finalize_download<'a>(
            &'a self,
            id: &'a str,
            part_path: &'a Path,
            file_name: &'a str,
            _mime: Option<&'a str>,
            open_after_done: bool,
        ) -> BoxFuture<'a, Result<String, AppError>> {
            Box::pin(async move {
                self.finalize_calls.fetch_add(1, Ordering::SeqCst);
                if self.return_content_uris.load(Ordering::SeqCst) {
                    let bytes = tokio::fs::read(part_path).await.map_err(|err| {
                        AppError::new(ErrorCode::Failed, format!("mock read part: {err}"))
                    })?;
                    let safe = sanitize_file_name(file_name);
                    let uri = if open_after_done {
                        format!(
                            "content://com.x1f4r.legion.fileprovider/open_externally/{id}-{safe}"
                        )
                    } else {
                        let seq = self.next_content_id.fetch_add(1, Ordering::SeqCst);
                        format!("content://media/external/downloads/{seq}")
                    };
                    self.live_content_uris
                        .lock()
                        .unwrap()
                        .insert(uri.clone(), bytes);
                    if self.gate_finalize.load(Ordering::SeqCst) {
                        self.finalize_entered.notify_waiters();
                        self.finalize_release.notified().await;
                    }
                    let _ = tokio::fs::remove_file(part_path).await;
                    return Ok(uri);
                }
                if self.gate_finalize.load(Ordering::SeqCst) {
                    self.finalize_entered.notify_waiters();
                    self.finalize_release.notified().await;
                }
                let dir = if open_after_done {
                    &self.cache_dir
                } else {
                    &self.downloads_dir
                };
                let target_name = if open_after_done {
                    format!("{id}-{}", sanitize_file_name(file_name))
                } else {
                    file_name.to_owned()
                };
                let dst =
                    place_file_without_overwrite(part_path, dir, &target_name, open_after_done)
                        .await?;
                Ok(dst.to_string_lossy().into_owned())
            })
        }

        fn delete_part_file<'a>(
            &'a self,
            part_path: &'a Path,
        ) -> BoxFuture<'a, Result<(), AppError>> {
            Box::pin(async move {
                if consume_injected_failure(&self.fail_delete_part) {
                    return Err(AppError::new(
                        ErrorCode::Failed,
                        format!(
                            "Could not remove {}: sharing violation (mock).",
                            part_path.display()
                        ),
                    ));
                }
                remove_file_if_present(part_path).await
            })
        }

        fn delete_saved_download<'a>(
            &'a self,
            local_path: &'a str,
        ) -> BoxFuture<'a, Result<(), AppError>> {
            Box::pin(async move {
                if consume_injected_failure(&self.fail_delete_saved) {
                    return Err(AppError::new(
                        ErrorCode::Failed,
                        format!("Could not remove {local_path}: file in use (mock)."),
                    ));
                }
                self.deleted_saved
                    .lock()
                    .unwrap()
                    .push(local_path.to_owned());
                if local_path.starts_with("content://") {
                    self.live_content_uris.lock().unwrap().remove(local_path);
                    Ok(())
                } else {
                    remove_file_if_present(Path::new(local_path)).await
                }
            })
        }

        fn cached_file_exists<'a>(
            &'a self,
            local_path: &'a str,
            expected_size: u64,
        ) -> BoxFuture<'a, bool> {
            Box::pin(async move {
                if local_path.starts_with("content://") {
                    return self
                        .live_content_uris
                        .lock()
                        .unwrap()
                        .get(local_path)
                        .is_some_and(|bytes| bytes.len() as u64 == expected_size);
                }
                tokio::fs::metadata(local_path)
                    .await
                    .map(|m| m.is_file() && m.len() == expected_size)
                    .unwrap_or(false)
            })
        }

        fn open_local_file<'a>(
            &'a self,
            local_path: &'a str,
            mime: Option<&'a str>,
        ) -> BoxFuture<'a, Result<(), AppError>> {
            let mime = mime.map(str::to_owned);
            Box::pin(async move {
                if let Some(err) = self.open_error_once.lock().unwrap().take() {
                    return Err(err);
                }
                self.opened
                    .lock()
                    .unwrap()
                    .push((local_path.to_owned(), mime));
                Ok(())
            })
        }

        fn show_in_folder<'a>(
            &'a self,
            local_path: &'a str,
        ) -> BoxFuture<'a, Result<(), AppError>> {
            Box::pin(async move {
                self.shown_in_folder
                    .lock()
                    .unwrap()
                    .push(local_path.to_owned());
                Ok(())
            })
        }

        fn on_transfer_completed(&self, transfer: &DeviceTransfer) {
            self.completed_notifications
                .lock()
                .unwrap()
                .push(transfer.id.clone());
        }
    }

    async fn wait_for_entry(
        mgr: &TransferManager,
        id: &str,
        step: &str,
        predicate: impl Fn(&DeviceTransfer) -> bool,
    ) -> DeviceTransfer {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if let Some(t) = mgr.list().into_iter().find(|t| t.id == id)
                && predicate(&t)
            {
                return t;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!(
            "timed out waiting for transfer {id} ({step}); current: {:?}",
            mgr.list()
        );
    }

    async fn wait_for_finalized_path(
        mgr: &TransferManager,
        id: &str,
        step: &str,
    ) -> DeviceTransfer {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            let recorded = {
                let guard = mgr
                    .state
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                !guard.running.contains_key(id)
                    && guard
                        .items
                        .iter()
                        .find(|r| r.transfer.id == id)
                        .is_some_and(|r| r.finalized_local_path.is_some())
            };
            let persisted = recorded
                && fs::read_to_string(mgr.queue_file_path())
                    .ok()
                    .and_then(|raw| serde_json::from_str::<PersistedQueue>(&raw).ok())
                    .and_then(|q| q.items.into_iter().find(|r| r.transfer.id == id))
                    .is_some_and(|r| r.finalized_local_path.is_some());
            if persisted && let Some(t) = mgr.list().into_iter().find(|t| t.id == id) {
                return t;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!(
            "timed out waiting for transfer {id} ({step}); current: {:?}",
            mgr.list()
        );
    }

    async fn wait_for_state(
        mgr: &TransferManager,
        id: &str,
        predicate: impl Fn(TransferState) -> bool,
    ) -> DeviceTransfer {
        wait_for_entry(mgr, id, "target state", |t| predicate(t.state)).await
    }

    #[tokio::test]
    async fn download_resumes_from_partial_retries_transient_error_and_verifies_blake3() {
        let dir = tempfile::tempdir().unwrap();
        let data: Vec<u8> = (0..120_000usize).map(|i| (i * 17 % 251) as u8).collect();
        let expected_hash = format!("blake3:{}", blake3::hash(&data).to_hex());

        let remote = Arc::new(Mutex::new(MockRemoteState::default()));
        {
            let mut st = remote.lock().unwrap();
            st.files
                .insert("/home/me/movie.mp4".to_owned(), data.clone());
            st.files
                .insert("/home/me/tampered.mp4".to_owned(), data.clone());
            st.override_hash.insert(
                "/home/me/tampered.mp4".to_owned(),
                "blake3:0000000000000000000000000000000000000000000000000000000000000000"
                    .to_owned(),
            );
            st.fail_first_gets = 1;
        }

        let backend: Arc<dyn Backend> = Arc::new(MockBackend::new(Arc::clone(&remote)));
        let platform = Arc::new(MockPlatform::new(dir.path()));
        let mgr = TransferManager::with_options(
            dir.path(),
            backend,
            platform.clone(),
            2,
            Duration::from_secs(10),
        );

        let id = mgr
            .enqueue_download(
                "tower".to_owned(),
                "/home/me/movie.mp4".to_owned(),
                None,
                Some(data.len() as u64),
                None,
                false,
            )
            .await
            .unwrap();
        let done = wait_for_state(&mgr, &id, |s| s == TransferState::Done).await;
        assert_eq!(done.bytes_done, data.len() as u64);
        assert_eq!(done.bytes_total, data.len() as u64);
        assert_eq!(done.hash.as_deref(), Some(expected_hash.as_str()));
        assert!(done.can_open);
        assert!(done.can_show_in_folder);
        assert_eq!(done.mime.as_deref(), Some("video/mp4"));
        let saved_bytes = fs::read(done.local_path.as_deref().unwrap()).unwrap();
        assert_eq!(saved_bytes, data);

        // Hash mismatch rejects and deletes partial file.
        let bad_id = mgr
            .enqueue_download(
                "tower".to_owned(),
                "/home/me/tampered.mp4".to_owned(),
                None,
                None,
                None,
                false,
            )
            .await
            .unwrap();
        let failed = wait_for_state(&mgr, &bad_id, |s| s == TransferState::Failed).await;
        assert_eq!(failed.error.as_ref().unwrap().code, "conflict");
        assert!(!mgr.part_path(&bad_id).exists());
        mgr.shutdown();
    }

    #[tokio::test]
    async fn concurrency_caps_at_two_and_persistence_resumes_across_restart() {
        let dir = tempfile::tempdir().unwrap();
        let data: Vec<u8> = (0..128 * 1024usize).map(|i| (i % 251) as u8).collect();
        let remote = Arc::new(Mutex::new(MockRemoteState::default()));
        {
            let mut st = remote.lock().unwrap();
            for name in ["a.dat", "b.dat", "c.dat"] {
                st.files.insert(format!("/home/me/{name}"), data.clone());
            }
            st.per_chunk_delay_ms = 25;
        }

        let backend: Arc<dyn Backend> = Arc::new(MockBackend::new(Arc::clone(&remote)));
        let platform = Arc::new(MockPlatform::new(dir.path()));
        let mgr = TransferManager::load_or_new(dir.path(), Arc::clone(&backend), platform.clone());

        let id1 = mgr
            .enqueue_download(
                "tower".into(),
                "/home/me/a.dat".into(),
                None,
                None,
                None,
                false,
            )
            .await
            .unwrap();
        let id2 = mgr
            .enqueue_download(
                "tower".into(),
                "/home/me/b.dat".into(),
                None,
                None,
                None,
                false,
            )
            .await
            .unwrap();
        let id3 = mgr
            .enqueue_download(
                "tower".into(),
                "/home/me/c.dat".into(),
                None,
                None,
                None,
                false,
            )
            .await
            .unwrap();

        for id in [&id1, &id2, &id3] {
            let _ = wait_for_state(&mgr, id, |s| s == TransferState::Done).await;
        }
        assert!(
            remote
                .lock()
                .unwrap()
                .max_concurrent_observed
                .load(Ordering::SeqCst)
                <= 2,
            "at most 2 transfers must run concurrently by default"
        );
        mgr.shutdown();

        // Persistence across restart: start a slow download, shut down mid-transfer so `.part`
        // stays on disk, construct a brand new TransferManager on the same data_dir, and confirm
        // it resumes from the partial file and finishes cleanly.
        remote
            .lock()
            .unwrap()
            .files
            .insert("/home/me/resume.dat".into(), data.clone());
        remote.lock().unwrap().per_chunk_delay_ms = 40;
        let mgr2 = TransferManager::load_or_new(dir.path(), Arc::clone(&backend), platform.clone());
        let rid = mgr2
            .enqueue_download(
                "tower".into(),
                "/home/me/resume.dat".into(),
                None,
                None,
                None,
                false,
            )
            .await
            .unwrap();
        // Wait until some bytes have landed in `.part`, then simulate process termination.
        let part_deadline = Instant::now() + Duration::from_secs(10);
        while fs::metadata(mgr2.part_path(&rid))
            .map(|m| m.len())
            .unwrap_or(0)
            < 16 * 1024
        {
            assert!(
                Instant::now() < part_deadline,
                "timed out waiting for .part file to accumulate 16 KiB before simulated restart"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        mgr2.shutdown();
        let partial_bytes = fs::metadata(mgr2.part_path(&rid))
            .map(|m| m.len())
            .unwrap_or(0);
        assert!(partial_bytes > 0 && partial_bytes < data.len() as u64);

        // Fresh manager on same directory resumes automatically.
        remote.lock().unwrap().per_chunk_delay_ms = 0;
        let mgr3 = TransferManager::load_or_new(dir.path(), backend, platform);
        let before_start = mgr3.list().into_iter().find(|t| t.id == rid).unwrap();
        assert_eq!(before_start.state, TransferState::Queued);
        assert_eq!(before_start.bytes_done, partial_bytes);
        mgr3.start();
        let done = wait_for_state(&mgr3, &rid, |s| s == TransferState::Done).await;
        assert_eq!(fs::read(done.local_path.as_deref().unwrap()).unwrap(), data);
        mgr3.shutdown();
    }

    #[tokio::test]
    async fn wifi_only_rule_pauses_large_transfers_on_metered_and_resumes_on_wifi() {
        let dir = tempfile::tempdir().unwrap();
        let small = vec![1u8; 4096];
        let remote = Arc::new(Mutex::new(MockRemoteState::default()));
        remote
            .lock()
            .unwrap()
            .files
            .insert("/home/me/small.dat".into(), small.clone());
        remote
            .lock()
            .unwrap()
            .files
            .insert("/home/me/large.dat".into(), vec![2u8; 64 * 1024]);

        let backend: Arc<dyn Backend> = Arc::new(MockBackend::new(Arc::clone(&remote)));
        let platform = Arc::new(MockPlatform::new(dir.path()));
        platform.unmetered_wifi.store(false, Ordering::Release);

        let mgr = TransferManager::load_or_new(dir.path(), backend, platform.clone());
        // Lower threshold to 32 KiB in queue settings to test without allocating 50 MB in RAM.
        {
            let mut guard = mgr.state.lock().unwrap();
            guard.settings.large_threshold_bytes = 32 * 1024;
        }
        assert!(!mgr.settings().wifi_only_large, "default must be off");
        let s = mgr.set_wifi_only_large(true);
        assert!(s.wifi_only_large);

        // Small file runs immediately even on metered network.
        let small_id = mgr
            .enqueue_download(
                "tower".into(),
                "/home/me/small.dat".into(),
                None,
                Some(small.len() as u64),
                None,
                false,
            )
            .await
            .unwrap();
        let _ = wait_for_state(&mgr, &small_id, |s| s == TransferState::Done).await;

        // Large file pauses with WifiOnly on metered network.
        let large_id = mgr
            .enqueue_download(
                "tower".into(),
                "/home/me/large.dat".into(),
                None,
                Some(64 * 1024),
                None,
                false,
            )
            .await
            .unwrap();
        let paused = wait_for_state(&mgr, &large_id, |s| s == TransferState::Paused).await;
        assert_eq!(paused.paused_reason, Some(PauseReason::WifiOnly));

        // Switching to unmetered Wi-Fi automatically resumes and completes the large transfer.
        platform.unmetered_wifi.store(true, Ordering::Release);
        mgr.on_network_changed(true);
        let done = wait_for_state(&mgr, &large_id, |s| s == TransferState::Done).await;
        assert_eq!(done.bytes_done, 64 * 1024);
        mgr.shutdown();
    }

    #[tokio::test]
    async fn upload_resumes_and_cancel_works_across_every_transfer_state() {
        let dir = tempfile::tempdir().unwrap();
        let upload_bytes: Vec<u8> = (0..80_000usize).map(|i| (i * 13 % 251) as u8).collect();
        let local_src = dir.path().join("photo.jpg");
        fs::write(&local_src, &upload_bytes).unwrap();

        let remote = Arc::new(Mutex::new(MockRemoteState::default()));
        {
            let mut st = remote.lock().unwrap();
            // Seed a partial upload on the remote node so `open_put` resumes from 20_000.
            st.partial_uploads.insert(
                "/home/me/uploads/photo.jpg".to_owned(),
                upload_bytes[..20_000].to_vec(),
            );
            st.files
                .insert("/home/me/slow1.dat".into(), vec![7u8; 256 * 1024]);
            st.files
                .insert("/home/me/slow2.dat".into(), vec![8u8; 256 * 1024]);
            st.files
                .insert("/home/me/slow3.dat".into(), vec![9u8; 256 * 1024]);
            st.per_chunk_delay_ms = 30;
        }

        let backend: Arc<dyn Backend> = Arc::new(MockBackend::new(Arc::clone(&remote)));
        let platform = Arc::new(MockPlatform::new(dir.path()));
        platform
            .picked_files
            .lock()
            .unwrap()
            .push(PickedUploadFile {
                local_path: local_src.to_string_lossy().into_owned(),
                name: "photo.jpg".into(),
                size: upload_bytes.len() as u64,
                mime: Some("image/jpeg".into()),
            });
        let mgr =
            TransferManager::with_options(dir.path(), backend, platform, 2, Duration::from_secs(2));

        // 1) Upload via opaque picker handle with pre-seeded remote partial resumes cleanly.
        let picked = mgr.pick_uploads().await.unwrap();
        assert_eq!(picked.len(), 1);
        assert!(picked[0].handle.starts_with("up-"));
        assert!(!picked[0].handle.contains("photo.jpg"));
        let up = mgr
            .enqueue_upload(
                "tower".into(),
                "/home/me/uploads".into(),
                Some(vec![picked[0].handle.clone()]),
            )
            .await
            .unwrap()
            .unwrap();
        let up_done = wait_for_state(&mgr, &up.id, |s| s == TransferState::Done).await;
        assert_eq!(
            remote
                .lock()
                .unwrap()
                .uploaded
                .get("/home/me/uploads/photo.jpg")
                .unwrap(),
            &upload_bytes
        );

        // Cancelling an already-Done transfer leaves it Done.
        let still_done = mgr
            .control(&up_done.id, TransferControlAction::Cancel)
            .await
            .unwrap();
        assert_eq!(still_done.state, TransferState::Done);

        // 2) Cancel during `running`, `queued`, `paused`, and `failed`.
        let r1 = mgr
            .enqueue_download(
                "tower".into(),
                "/home/me/slow1.dat".into(),
                None,
                None,
                None,
                false,
            )
            .await
            .unwrap();
        let r2 = mgr
            .enqueue_download(
                "tower".into(),
                "/home/me/slow2.dat".into(),
                None,
                None,
                None,
                false,
            )
            .await
            .unwrap();
        let q3 = mgr
            .enqueue_download(
                "tower".into(),
                "/home/me/slow3.dat".into(),
                None,
                None,
                None,
                false,
            )
            .await
            .unwrap();
        let _ = wait_for_state(&mgr, &r1, |s| s == TransferState::Running).await;
        let _ = wait_for_state(&mgr, &r2, |s| s == TransferState::Running).await;

        // Cancel while Queued.
        let c_queued = mgr
            .control(&q3, TransferControlAction::Cancel)
            .await
            .unwrap();
        assert_eq!(c_queued.state, TransferState::Cancelled);

        // Cancel while Running.
        let c_running = mgr
            .control(&r1, TransferControlAction::Cancel)
            .await
            .unwrap();
        assert_eq!(c_running.state, TransferState::Cancelled);
        assert!(!mgr.part_path(&r1).exists());

        // Pause r2, then cancel while Paused.
        let paused = mgr
            .control(&r2, TransferControlAction::Pause)
            .await
            .unwrap();
        assert_eq!(paused.state, TransferState::Paused);
        let c_paused = mgr
            .control(&r2, TransferControlAction::Cancel)
            .await
            .unwrap();
        assert_eq!(c_paused.state, TransferState::Cancelled);

        // Cancel while Failed.
        let missing = mgr
            .enqueue_download(
                "tower".into(),
                "/home/me/missing.dat".into(),
                None,
                None,
                None,
                false,
            )
            .await
            .unwrap();
        let _ = wait_for_state(&mgr, &missing, |s| s == TransferState::Failed).await;
        let c_failed = mgr
            .control(&missing, TransferControlAction::Cancel)
            .await
            .unwrap();
        assert_eq!(c_failed.state, TransferState::Cancelled);

        // Background pause/resume preserves User-paused transfers.
        let bg = mgr
            .enqueue_download(
                "tower".into(),
                "/home/me/slow1.dat".into(),
                None,
                None,
                None,
                false,
            )
            .await
            .unwrap();
        let _ = wait_for_state(&mgr, &bg, |s| s == TransferState::Running).await;
        mgr.pause_all_active(PauseReason::Background);
        let bg_paused = mgr.list().into_iter().find(|t| t.id == bg).unwrap();
        assert_eq!(bg_paused.state, TransferState::Paused);
        assert_eq!(bg_paused.paused_reason, Some(PauseReason::Background));
        mgr.resume_paused_for(PauseReason::Background);
        let _ = wait_for_state(&mgr, &bg, |s| s == TransferState::Done).await;

        mgr.shutdown();
    }

    #[tokio::test]
    async fn opaque_upload_handles_reject_raw_paths_forged_handles_and_reuse() {
        let dir = tempfile::tempdir().unwrap();
        let local_src = dir.path().join("secret.txt");
        fs::write(&local_src, b"confidential").unwrap();

        let remote = Arc::new(Mutex::new(MockRemoteState::default()));
        let backend: Arc<dyn Backend> = Arc::new(MockBackend::new(Arc::clone(&remote)));
        let platform = Arc::new(MockPlatform::new(dir.path()));
        platform
            .picked_files
            .lock()
            .unwrap()
            .push(PickedUploadFile {
                local_path: local_src.to_string_lossy().into_owned(),
                name: "secret.txt".into(),
                size: 12,
                mime: Some("text/plain".into()),
            });

        let mgr =
            TransferManager::with_options(dir.path(), backend, platform, 2, Duration::from_secs(2));

        // Raw device paths and content URIs supplied directly by the page must be refused.
        for hostile in [
            local_src.to_string_lossy().into_owned(),
            "/etc/passwd".to_owned(),
            "content://com.android.providers.downloads.documents/document/1".to_owned(),
            "C:\\Users\\me\\.ssh\\id_ed25519".to_owned(),
        ] {
            let err = mgr
                .enqueue_upload(
                    "tower".into(),
                    "/home/me/uploads".into(),
                    Some(vec![hostile]),
                )
                .await
                .unwrap_err();
            assert_eq!(err.code, ErrorCode::Denied);
        }

        // Valid handle + forged handle fails atomically without consuming the valid handle.
        let picked = mgr.pick_uploads().await.unwrap();
        let handle = picked[0].handle.clone();
        let mixed_err = mgr
            .enqueue_upload(
                "tower".into(),
                "/home/me/uploads".into(),
                Some(vec![handle.clone(), "up-forged".into()]),
            )
            .await
            .unwrap_err();
        assert_eq!(mixed_err.code, ErrorCode::Denied);

        // The unconsumed valid handle still works once, and cannot be replayed a second time.
        let enq = mgr
            .enqueue_upload(
                "tower".into(),
                "/home/me/uploads".into(),
                Some(vec![handle.clone()]),
            )
            .await
            .unwrap()
            .unwrap();
        let _ = wait_for_state(&mgr, &enq.id, |s| s == TransferState::Done).await;

        let replay_err = mgr
            .enqueue_upload(
                "tower".into(),
                "/home/me/uploads".into(),
                Some(vec![handle]),
            )
            .await
            .unwrap_err();
        assert_eq!(replay_err.code, ErrorCode::Denied);
        mgr.shutdown();
    }

    #[tokio::test]
    async fn pause_resume_and_cancel_during_finalize_stay_single_worker_and_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let data = b"verified-payload-for-finalize-race".to_vec();
        let remote = Arc::new(Mutex::new(MockRemoteState::default()));
        {
            let mut st = remote.lock().unwrap();
            st.files.insert("/home/me/race1.txt".into(), data.clone());
            st.files.insert("/home/me/race2.txt".into(), data.clone());
            st.files.insert("/home/me/race3.txt".into(), data.clone());
        }

        let backend: Arc<dyn Backend> = Arc::new(MockBackend::new(Arc::clone(&remote)));
        let platform = Arc::new(MockPlatform::new(dir.path()));
        platform.gate_finalize.store(true, Ordering::SeqCst);

        let mgr = TransferManager::with_options(
            dir.path(),
            backend,
            platform.clone(),
            2,
            Duration::from_secs(2),
        );

        // Case 1: Pause -> Resume WHILE finalize_download is in flight.
        let entered = platform.finalize_entered.notified();
        let id1 = mgr
            .enqueue_download(
                "tower".into(),
                "/home/me/race1.txt".into(),
                None,
                None,
                None,
                false,
            )
            .await
            .unwrap();
        entered.await;

        let paused = mgr
            .control(&id1, TransferControlAction::Pause)
            .await
            .unwrap();
        assert_eq!(paused.state, TransferState::Paused);
        let resumed = mgr
            .control(&id1, TransferControlAction::Resume)
            .await
            .unwrap();
        assert_eq!(resumed.state, TransferState::Queued);

        // Give the scheduler a chance to wake — it MUST NOT spawn a second worker while worker 1
        // still owns the slot inside finalize_download.
        tokio::time::sleep(Duration::from_millis(60)).await;
        assert_eq!(platform.finalize_calls.load(Ordering::SeqCst), 1);

        // Release finalize_download -> completes cleanly once without duplicating files.
        platform.finalize_release.notify_waiters();
        let done1 = wait_for_state(&mgr, &id1, |s| s == TransferState::Done).await;
        assert_eq!(platform.finalize_calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            fs::read(done1.local_path.as_deref().unwrap()).unwrap(),
            data
        );
        assert!(!platform.downloads_dir.join("race1 (2).txt").exists());

        // Case 2: Pause WITHOUT resuming while finalize_download is in flight -> MUST remain Paused
        // and not notify until the user explicitly resumes, then finish idempotently.
        platform.finalize_calls.store(0, Ordering::SeqCst);
        let entered2 = platform.finalize_entered.notified();
        let id2 = mgr
            .enqueue_download(
                "tower".into(),
                "/home/me/race2.txt".into(),
                None,
                None,
                None,
                false,
            )
            .await
            .unwrap();
        entered2.await;

        mgr.control(&id2, TransferControlAction::Pause)
            .await
            .unwrap();
        platform.finalize_release.notify_waiters();
        let still_paused = wait_for_finalized_path(
            &mgr,
            &id2,
            "paused finalize_download recording finalized_local_path",
        )
        .await;
        assert_eq!(still_paused.state, TransferState::Paused);
        assert!(
            !platform
                .completed_notifications
                .lock()
                .unwrap()
                .contains(&id2)
        );

        // Now resume -> finishes idempotently with zero second finalize_download call.
        mgr.control(&id2, TransferControlAction::Resume)
            .await
            .unwrap();
        let done2 = wait_for_state(&mgr, &id2, |s| s == TransferState::Done).await;
        assert_eq!(platform.finalize_calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            fs::read(done2.local_path.as_deref().unwrap()).unwrap(),
            data
        );
        assert!(
            platform
                .completed_notifications
                .lock()
                .unwrap()
                .contains(&id2)
        );

        // Case 3: Cancel while finalize_download is in flight -> reports Cancelling until worker
        // exits and deletes output, then returns Cancelled with the output already gone.
        platform.finalize_calls.store(0, Ordering::SeqCst);
        let entered3 = platform.finalize_entered.notified();
        let id3 = mgr
            .enqueue_download(
                "tower".into(),
                "/home/me/race3.txt".into(),
                None,
                None,
                None,
                false,
            )
            .await
            .unwrap();
        entered3.await;

        let cancel_handle = {
            let mgr = Arc::clone(&mgr);
            let id3 = id3.clone();
            tokio::spawn(async move { mgr.control(&id3, TransferControlAction::Cancel).await })
        };
        let mid_cancel = wait_for_state(&mgr, &id3, |s| s == TransferState::Cancelling).await;
        assert_eq!(mid_cancel.state, TransferState::Cancelling);

        platform.finalize_release.notify_waiters();
        let cancelled = cancel_handle.await.unwrap().unwrap();
        assert_eq!(cancelled.state, TransferState::Cancelled);
        assert!(!platform.downloads_dir.join("race3.txt").exists());
        assert!(!mgr.part_path(&id3).exists());
        assert!(
            !platform
                .completed_notifications
                .lock()
                .unwrap()
                .contains(&id3)
        );

        mgr.shutdown();
    }

    #[tokio::test]
    async fn pause_resume_cancel_loop_across_download_stages_keeps_invariants() {
        let dir = tempfile::tempdir().unwrap();
        let data: Vec<u8> = (0..96 * 1024usize).map(|i| (i % 251) as u8).collect();
        let remote = Arc::new(Mutex::new(MockRemoteState::default()));
        {
            let mut st = remote.lock().unwrap();
            st.per_chunk_delay_ms = 5;
            for i in 0..8 {
                st.files
                    .insert(format!("/home/me/loop-{i}.txt"), data.clone());
            }
        }
        let backend: Arc<dyn Backend> = Arc::new(MockBackend::new(Arc::clone(&remote)));
        let platform = Arc::new(MockPlatform::new(dir.path()));
        let mgr = TransferManager::with_options(
            dir.path(),
            backend,
            platform.clone(),
            2,
            Duration::from_secs(2),
        );

        for i in 0..8usize {
            let id = mgr
                .enqueue_download(
                    "tower".into(),
                    format!("/home/me/loop-{i}.txt"),
                    None,
                    None,
                    None,
                    false,
                )
                .await
                .unwrap();
            tokio::time::sleep(Duration::from_millis((i as u64) * 6)).await;
            let _ = mgr.control(&id, TransferControlAction::Pause).await;
            let _ = mgr.control(&id, TransferControlAction::Resume).await;
            if i % 3 == 0 {
                let _ = mgr.control(&id, TransferControlAction::Cancel).await;
                let final_row = wait_for_state(&mgr, &id, |s| {
                    matches!(s, TransferState::Cancelled | TransferState::Done)
                })
                .await;
                if final_row.state == TransferState::Cancelled {
                    assert!(!mgr.part_path(&id).exists());
                }
            } else {
                let done = wait_for_state(&mgr, &id, |s| s == TransferState::Done).await;
                assert_eq!(fs::read(done.local_path.as_deref().unwrap()).unwrap(), data);
            }
        }
        mgr.shutdown();
    }

    #[tokio::test]
    async fn refused_open_extensions_device_mime_and_session_only_open_after_done() {
        let dir = tempfile::tempdir().unwrap();
        let remote = Arc::new(Mutex::new(MockRemoteState::default()));
        {
            let mut st = remote.lock().unwrap();
            st.files
                .insert("/home/me/holiday.jpg".into(), b"jpeg-bytes".to_vec());
            st.files
                .insert("/home/me/evil.apk".into(), b"apk-bytes".to_vec());
            st.files
                .insert("/home/me/setup.exe".into(), b"exe-bytes".to_vec());
            st.files
                .insert("/home/me/page.html".into(), b"<html></html>".to_vec());
            st.files
                .insert("/home/me/vector.svg".into(), b"<svg/>".to_vec());
            st.files
                .insert("/home/me/bundle.zip".into(), b"zip-bytes".to_vec());
            st.files
                .insert("/home/me/remote.rdp".into(), b"rdp-bytes".to_vec());
            st.files
                .insert("/home/me/macro.xlsb".into(), b"xlsb-bytes".to_vec());
            st.files
                .insert("/home/me/blob.unknown".into(), b"blob-bytes".to_vec());
            st.files
                .insert("/home/me/x.ap\u{212A}".into(), b"kelvin-bytes".to_vec());
        }
        let backend: Arc<dyn Backend> = Arc::new(MockBackend::new(Arc::clone(&remote)));
        let platform = Arc::new(MockPlatform::new(dir.path()));
        let mgr = TransferManager::with_options(
            dir.path(),
            Arc::clone(&backend),
            platform.clone(),
            2,
            Duration::from_secs(2),
        );

        // Remote claims holiday.jpg is an APK -> device derives image/jpeg and opens as image/jpeg.
        let img_id = mgr
            .enqueue_download(
                "tower".into(),
                "/home/me/holiday.jpg".into(),
                None,
                None,
                Some("application/vnd.android.package-archive".into()),
                false,
            )
            .await
            .unwrap();
        let img_done = wait_for_state(&mgr, &img_id, |s| s == TransferState::Done).await;
        assert_eq!(img_done.mime.as_deref(), Some("image/jpeg"));
        assert!(img_done.can_open);
        assert!(img_done.can_show_in_folder);
        assert!(img_done.open_blocked_reason.is_none());
        mgr.open_transfer(&img_id).await.unwrap();
        assert_eq!(
            platform.opened.lock().unwrap().last().unwrap().1.as_deref(),
            Some("image/jpeg")
        );

        // Refused open_after_done on non-allow-listed extensions immediately returns Denied.
        for blocked_ext_path in [
            "/home/me/evil.apk",
            "/home/me/page.html",
            "/home/me/vector.svg",
            "/home/me/bundle.zip",
            "/home/me/remote.rdp",
            "/home/me/x.ap\u{212A}",
        ] {
            let open_ext_err = mgr
                .enqueue_download(
                    "tower".into(),
                    blocked_ext_path.into(),
                    None,
                    None,
                    Some("image/jpeg".into()),
                    true,
                )
                .await
                .unwrap_err();
            assert_eq!(
                open_ext_err.code,
                ErrorCode::Denied,
                "expected open_after_done Denied for {blocked_ext_path}"
            );
        }

        // Regular download of non-allow-listed types succeeds, but canOpen=false, canShowInFolder=true,
        // openBlockedReason is set, open_transfer refuses with Denied, and show_transfer_in_folder works.
        for path in [
            "/home/me/evil.apk",
            "/home/me/setup.exe",
            "/home/me/page.html",
            "/home/me/vector.svg",
            "/home/me/bundle.zip",
            "/home/me/remote.rdp",
            "/home/me/macro.xlsb",
            "/home/me/blob.unknown",
            "/home/me/x.ap\u{212A}",
        ] {
            let id = mgr
                .enqueue_download(
                    "tower".into(),
                    path.into(),
                    None,
                    None,
                    Some("image/jpeg".into()),
                    false,
                )
                .await
                .unwrap();
            let done = wait_for_state(&mgr, &id, |s| s == TransferState::Done).await;
            assert!(!done.can_open, "{path} must not be openable directly");
            assert!(done.can_show_in_folder);
            assert_eq!(
                done.open_blocked_reason.as_deref(),
                Some(REFUSED_OPEN_MESSAGE)
            );
            let open_err = mgr.open_transfer(&id).await.unwrap_err();
            assert_eq!(open_err.code, ErrorCode::Denied);
            mgr.show_transfer_in_folder(&id).await.unwrap();
            assert!(
                platform
                    .shown_in_folder
                    .lock()
                    .unwrap()
                    .contains(&done.local_path.unwrap())
            );
        }

        // Verify open_after_done is session-only and never written to queue.json.
        let queue_json = fs::read_to_string(mgr.queue_file_path()).unwrap();
        assert!(!queue_json.contains("open_after_done"));

        // Platform without open support (iOS) returns Unsupported instead of no-op Ok.
        let mut ios_plat = MockPlatform::new(dir.path());
        ios_plat.supports_open_externally = false;
        ios_plat.supports_open_downloads = false;
        ios_plat.supports_show_in_folder = false;
        let ios_dir = tempfile::tempdir().unwrap();
        let ios_mgr = TransferManager::with_options(
            ios_dir.path(),
            backend,
            Arc::new(ios_plat),
            2,
            Duration::from_secs(2),
        );
        let ios_ext_err = ios_mgr
            .enqueue_download(
                "tower".into(),
                "/home/me/holiday.jpg".into(),
                None,
                None,
                None,
                true,
            )
            .await
            .unwrap_err();
        assert_eq!(ios_ext_err.code, ErrorCode::Unsupported);

        let ios_dl = ios_mgr
            .enqueue_download(
                "tower".into(),
                "/home/me/holiday.jpg".into(),
                None,
                None,
                None,
                false,
            )
            .await
            .unwrap();
        let ios_done = wait_for_state(&ios_mgr, &ios_dl, |s| s == TransferState::Done).await;
        assert!(!ios_done.can_open);
        assert!(!ios_done.can_show_in_folder);
        assert_eq!(
            ios_mgr.open_transfer(&ios_dl).await.unwrap_err().code,
            ErrorCode::Unsupported
        );
        assert_eq!(
            ios_mgr
                .show_transfer_in_folder(&ios_dl)
                .await
                .unwrap_err()
                .code,
            ErrorCode::Unsupported
        );
        ios_mgr.shutdown();
        mgr.shutdown();
    }

    #[tokio::test]
    async fn progress_stall_timeout_pauses_download_and_upload_with_stalled_reason() {
        let dir = tempfile::tempdir().unwrap();
        let payload = vec![0x42u8; 256 * 1024];
        let local_src = dir.path().join("big-upload.dat");
        fs::write(&local_src, &payload).unwrap();

        let remote = Arc::new(Mutex::new(MockRemoteState::default()));
        {
            let mut st = remote.lock().unwrap();
            st.files
                .insert("/home/me/stalled.dat".into(), payload.clone());
            st.stall_get = true;
            st.stall_put = true;
        }
        let backend: Arc<dyn Backend> = Arc::new(MockBackend::new(Arc::clone(&remote)));
        let platform = Arc::new(MockPlatform::new(dir.path()));
        platform
            .picked_files
            .lock()
            .unwrap()
            .push(PickedUploadFile {
                local_path: local_src.to_string_lossy().into_owned(),
                name: "big-upload.dat".into(),
                size: payload.len() as u64,
                mime: None,
            });

        let mgr = TransferManager::with_timeouts(
            dir.path(),
            backend,
            platform,
            2,
            Duration::from_secs(2),
            Duration::from_millis(120),
        );

        // Stalled download pauses with PauseReason::Stalled and visible reason, then finishes on resume.
        let dl_id = mgr
            .enqueue_download(
                "tower".into(),
                "/home/me/stalled.dat".into(),
                None,
                None,
                None,
                false,
            )
            .await
            .unwrap();
        let dl_paused = wait_for_state(&mgr, &dl_id, |s| s == TransferState::Paused).await;
        assert_eq!(dl_paused.paused_reason, Some(PauseReason::Stalled));
        assert!(dl_paused.error.is_some());

        remote.lock().unwrap().stall_get = false;
        mgr.control(&dl_id, TransferControlAction::Resume)
            .await
            .unwrap();
        let _ = wait_for_state(&mgr, &dl_id, |s| s == TransferState::Done).await;

        // Stalled upload pauses with PauseReason::Stalled and finishes cleanly after unstall + resume.
        let picked = mgr.pick_uploads().await.unwrap();
        let up = mgr
            .enqueue_upload(
                "tower".into(),
                "/home/me/uploads".into(),
                Some(vec![picked[0].handle.clone()]),
            )
            .await
            .unwrap()
            .unwrap();
        let up_paused = wait_for_state(&mgr, &up.id, |s| s == TransferState::Paused).await;
        assert_eq!(up_paused.paused_reason, Some(PauseReason::Stalled));
        assert!(up_paused.error.is_some());

        remote.lock().unwrap().stall_put = false;
        mgr.control(&up.id, TransferControlAction::Resume)
            .await
            .unwrap();
        let _ = wait_for_state(&mgr, &up.id, |s| s == TransferState::Done).await;
        assert_eq!(
            remote
                .lock()
                .unwrap()
                .uploaded
                .get("/home/me/uploads/big-upload.dat")
                .unwrap(),
            &payload
        );
        mgr.shutdown();
    }

    #[tokio::test]
    async fn upload_collision_renames_remote_path_without_overwriting_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        let existing_bytes = b"do-not-overwrite-original".to_vec();
        let new_bytes = b"fresh-upload-payload".to_vec();
        let local_src = dir.path().join("report.pdf");
        fs::write(&local_src, &new_bytes).unwrap();

        let remote = Arc::new(Mutex::new(MockRemoteState::default()));
        remote
            .lock()
            .unwrap()
            .files
            .insert("/home/me/uploads/report.pdf".into(), existing_bytes.clone());

        let backend: Arc<dyn Backend> = Arc::new(MockBackend::new(Arc::clone(&remote)));
        let platform = Arc::new(MockPlatform::new(dir.path()));
        platform
            .picked_files
            .lock()
            .unwrap()
            .push(PickedUploadFile {
                local_path: local_src.to_string_lossy().into_owned(),
                name: "report.pdf".into(),
                size: new_bytes.len() as u64,
                mime: None,
            });
        let mgr =
            TransferManager::with_options(dir.path(), backend, platform, 2, Duration::from_secs(2));

        let picked = mgr.pick_uploads().await.unwrap();
        let up = mgr
            .enqueue_upload(
                "tower".into(),
                "/home/me/uploads".into(),
                Some(vec![picked[0].handle.clone()]),
            )
            .await
            .unwrap()
            .unwrap();
        let done = wait_for_state(&mgr, &up.id, |s| s == TransferState::Done).await;
        assert_eq!(done.path, "/home/me/uploads/report (2).pdf");
        assert_eq!(done.name, "report (2).pdf");
        let st = remote.lock().unwrap();
        assert_eq!(
            st.files.get("/home/me/uploads/report.pdf").unwrap(),
            &existing_bytes
        );
        assert_eq!(
            st.uploaded.get("/home/me/uploads/report (2).pdf").unwrap(),
            &new_bytes
        );
        mgr.shutdown();
    }

    #[tokio::test]
    async fn startup_sweeps_orphaned_parts_and_pruning_cleans_resources() {
        let dir = tempfile::tempdir().unwrap();
        let parts_dir = dir.path().join("transfers").join("parts");
        fs::create_dir_all(&parts_dir).unwrap();
        let orphan = parts_dir.join("orphan-999.part");
        fs::write(&orphan, b"stale").unwrap();

        let remote = Arc::new(Mutex::new(MockRemoteState::default()));
        let backend: Arc<dyn Backend> = Arc::new(MockBackend::new(Arc::clone(&remote)));
        let platform = Arc::new(MockPlatform::new(dir.path()));
        let mgr = TransferManager::load_or_new(dir.path(), backend, platform.clone());

        assert!(
            !orphan.exists(),
            "startup must sweep orphaned .part files not referenced by live transfers"
        );

        // Verify pruning past MAX_RETAINED_TERMINAL releases pruned upload URIs and deletes .part files.
        let mut pruned_part = parts_dir.join("tr-old-dl.part");
        fs::write(&pruned_part, b"leftover").unwrap();
        let pruned_records = {
            let mut guard = mgr.state.lock().unwrap();
            guard.items.push(TransferRecord {
                transfer: DeviceTransfer {
                    id: "tr-old-dl".into(),
                    direction: TransferDirection::Download,
                    machine: "tower".into(),
                    path: "/home/me/old.txt".into(),
                    name: "old.txt".into(),
                    local_path: None,
                    mime: None,
                    state: TransferState::Failed,
                    bytes_done: 0,
                    bytes_total: 10,
                    rate: None,
                    error: None,
                    created: "2026-01-01T00:00:00Z".into(),
                    finished: Some("2026-01-01T00:00:01Z".into()),
                    hash: None,
                    can_open: false,
                    can_show_in_folder: false,
                    open_blocked_reason: None,
                    paused_reason: None,
                    open_error: None,
                    open_externally: false,
                },
                open_after_done: false,
                finalized_local_path: None,
                finalized_hash: None,
                finalized_in_cache: false,
                remote_path_resolved: false,
                upload_started: false,
                remote_mtime_secs: None,
                remote_mtime_nanos: None,
                remote_file_id: None,
                pending_cleanup_paths: Vec::new(),
                cleanup_part: false,
                cached_open: false,
            });
            guard.items.push(TransferRecord {
                transfer: DeviceTransfer {
                    id: "tr-old-up".into(),
                    direction: TransferDirection::Upload,
                    machine: "tower".into(),
                    path: "/home/me/up.txt".into(),
                    name: "up.txt".into(),
                    local_path: Some("content://example/old-upload".into()),
                    mime: None,
                    state: TransferState::Failed,
                    bytes_done: 0,
                    bytes_total: 10,
                    rate: None,
                    error: None,
                    created: "2026-01-01T00:00:00Z".into(),
                    finished: Some("2026-01-01T00:00:01Z".into()),
                    hash: None,
                    can_open: false,
                    can_show_in_folder: false,
                    open_blocked_reason: None,
                    paused_reason: None,
                    open_error: None,
                    open_externally: false,
                },
                open_after_done: false,
                finalized_local_path: None,
                finalized_hash: None,
                finalized_in_cache: false,
                remote_path_resolved: true,
                upload_started: false,
                remote_mtime_secs: None,
                remote_mtime_nanos: None,
                remote_file_id: None,
                pending_cleanup_paths: Vec::new(),
                cleanup_part: false,
                cached_open: false,
            });
            for i in 0..MAX_RETAINED_TERMINAL {
                guard.items.push(TransferRecord {
                    transfer: DeviceTransfer {
                        id: format!("tr-keep-{i}"),
                        direction: TransferDirection::Download,
                        machine: "tower".into(),
                        path: format!("/home/me/f{i}.txt"),
                        name: format!("f{i}.txt"),
                        local_path: None,
                        mime: None,
                        state: TransferState::Done,
                        bytes_done: 1,
                        bytes_total: 1,
                        rate: None,
                        error: None,
                        created: "2026-01-01T00:00:00Z".into(),
                        finished: Some("2026-01-01T00:00:01Z".into()),
                        hash: None,
                        can_open: false,
                        can_show_in_folder: false,
                        open_blocked_reason: None,
                        paused_reason: None,
                        open_error: None,
                        open_externally: false,
                    },
                    open_after_done: false,
                    finalized_local_path: None,
                    finalized_hash: None,
                    finalized_in_cache: false,
                    remote_path_resolved: false,
                    upload_started: false,
                    remote_mtime_secs: None,
                    remote_mtime_nanos: None,
                    remote_file_id: None,
                    pending_cleanup_paths: Vec::new(),
                    cleanup_part: false,
                    cached_open: false,
                });
            }
            prune_terminal_locked(&mut guard.items)
        };
        assert_eq!(pruned_records.len(), 2);
        mgr.cleanup_pruned_records(pruned_records).await;
        assert!(!pruned_part.exists());
        assert!(
            platform
                .released_uris
                .lock()
                .unwrap()
                .contains(&"content://example/old-upload".to_owned())
        );
        pruned_part.clear();
        mgr.shutdown();
    }

    #[test]
    fn si_byte_formatting_and_notification_wording_match_ui() {
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(1023), "1023 B");
        assert_eq!(format_bytes(1500), "1.5 KB");
        assert_eq!(format_bytes(45_000_000), "45 MB");
        assert_eq!(format_bytes(1_500_000_000), "1.5 GB");
        assert_eq!(format_byte_pair(45_000_000, 120_000_000), "45 of 120 MB");
        assert_eq!(
            format_byte_pair(29_600_000, 1_500_000_000),
            "30 MB of 1.5 GB"
        );
        assert_eq!(
            format_progress_line(45_000_000, 100_000_000),
            "45 of 100 MB (45%)"
        );

        let upload_rec = TransferRecord {
            transfer: DeviceTransfer {
                id: "tr-1".into(),
                direction: TransferDirection::Upload,
                machine: "tower".into(),
                path: "/home/me/clip.mp4".into(),
                name: "clip.mp4".into(),
                local_path: Some("/tmp/clip.mp4".into()),
                mime: Some("video/mp4".into()),
                state: TransferState::Running,
                bytes_done: 50_000_000,
                bytes_total: 100_000_000,
                rate: None,
                error: None,
                created: "2026-01-01T00:00:00Z".into(),
                finished: None,
                hash: None,
                can_open: false,
                can_show_in_folder: false,
                open_blocked_reason: None,
                paused_reason: None,
                open_error: None,
                open_externally: false,
            },
            open_after_done: false,
            finalized_local_path: None,
            finalized_hash: None,
            finalized_in_cache: false,
            remote_path_resolved: true,
            upload_started: false,
            remote_mtime_secs: None,
            remote_mtime_nanos: None,
            remote_file_id: None,
            pending_cleanup_paths: Vec::new(),
            cleanup_part: false,
            cached_open: false,
        };
        let summary = build_notification_summary(&[upload_rec]);
        assert_eq!(summary.title, "Uploading clip.mp4 to tower");
        assert_eq!(summary.detail, "50 of 100 MB (50%)");
        assert!(!summary.detail.contains("MiB"));
    }

    #[test]
    fn monotonic_persistence_discards_older_sequence_writes() {
        let dir = tempfile::tempdir().unwrap();
        let remote = Arc::new(Mutex::new(MockRemoteState::default()));
        let backend: Arc<dyn Backend> = Arc::new(MockBackend::new(remote));
        let platform = Arc::new(MockPlatform::new(dir.path()));
        let mgr = TransferManager::load_or_new(dir.path(), backend, platform);

        // Simulate a newer sequence already written to disk while a stale sequence tries to write.
        {
            let mut guard = mgr.state.lock().unwrap();
            guard.settings.wifi_only_large = true;
        }
        mgr.persist_locked_snapshot();
        assert!(
            load_queue_file(&mgr.queue_file_path())
                .settings
                .wifi_only_large
        );

        // Hold persist_lock at a higher sequence number than the next snapshot's seq to prove
        // out-of-order writes cannot clobber newer disk state.
        *mgr.persist_lock.lock().unwrap() = u64::MAX - 1;
        {
            let mut guard = mgr.state.lock().unwrap();
            guard.settings.wifi_only_large = false;
        }
        mgr.persist_locked_snapshot();
        assert!(
            load_queue_file(&mgr.queue_file_path())
                .settings
                .wifi_only_large,
            "stale sequence write must be discarded instead of overwriting newer queue.json"
        );
        mgr.shutdown();
    }

    #[tokio::test]
    async fn cancel_after_pause_during_finalize_removes_saved_download_file() {
        let dir = tempfile::tempdir().unwrap();
        let data = b"payload-saved-before-cancel".to_vec();
        let remote = Arc::new(Mutex::new(MockRemoteState::default()));
        remote
            .lock()
            .unwrap()
            .files
            .insert("/home/me/paused-cancel.jpg".into(), data);

        let backend: Arc<dyn Backend> = Arc::new(MockBackend::new(Arc::clone(&remote)));
        let platform = Arc::new(MockPlatform::new(dir.path()));
        platform.gate_finalize.store(true, Ordering::SeqCst);

        let mgr = TransferManager::with_options(
            dir.path(),
            backend,
            platform.clone(),
            2,
            Duration::from_secs(2),
        );

        let entered = platform.finalize_entered.notified();
        let id = mgr
            .enqueue_download(
                "tower".into(),
                "/home/me/paused-cancel.jpg".into(),
                None,
                None,
                None,
                false,
            )
            .await
            .unwrap();
        entered.await;

        // Pause while final save is held, then let final save finish -> transfer remains Paused
        // with the finalized file sitting in Downloads.
        let paused = mgr
            .control(&id, TransferControlAction::Pause)
            .await
            .unwrap();
        assert_eq!(paused.state, TransferState::Paused);
        platform.finalize_release.notify_waiters();
        let _ = wait_for_finalized_path(
            &mgr,
            &id,
            "paused finalize_download finishing before Cancel",
        )
        .await;

        let saved_path = platform.downloads_dir.join("paused-cancel.jpg");
        assert!(
            saved_path.exists(),
            "save should have landed before Cancel was requested"
        );

        // Cancel must remove the already-saved Downloads file AND any .part file.
        let cancelled = mgr
            .control(&id, TransferControlAction::Cancel)
            .await
            .unwrap();
        assert_eq!(cancelled.state, TransferState::Cancelled);
        assert!(
            !saved_path.exists(),
            "Cancel must delete saved Downloads file"
        );
        assert!(!mgr.part_path(&id).exists());
        assert!(
            platform
                .deleted_saved
                .lock()
                .unwrap()
                .contains(&saved_path.to_string_lossy().into_owned())
        );
        mgr.shutdown();
    }

    #[tokio::test]
    async fn cancel_during_and_after_finalize_removes_android_content_uri() {
        let dir = tempfile::tempdir().unwrap();
        let data = b"android-mediastore-payload".to_vec();
        let remote = Arc::new(Mutex::new(MockRemoteState::default()));
        {
            let mut st = remote.lock().unwrap();
            st.files
                .insert("/home/me/android-during.jpg".into(), data.clone());
            st.files
                .insert("/home/me/android-after-pause.jpg".into(), data);
        }

        let backend: Arc<dyn Backend> = Arc::new(MockBackend::new(Arc::clone(&remote)));
        let platform = Arc::new(MockPlatform::new(dir.path()));
        platform.gate_finalize.store(true, Ordering::SeqCst);
        platform.return_content_uris.store(true, Ordering::SeqCst);

        let mgr = TransferManager::with_options(
            dir.path(),
            backend,
            platform.clone(),
            2,
            Duration::from_secs(2),
        );

        // Case A: Cancel while Android MediaStore save is in flight -> reports Cancelling until
        // MediaStore save finishes and the content:// URI is deleted, then returns Cancelled.
        let entered1 = platform.finalize_entered.notified();
        let id1 = mgr
            .enqueue_download(
                "tower".into(),
                "/home/me/android-during.jpg".into(),
                None,
                None,
                None,
                false,
            )
            .await
            .unwrap();
        entered1.await;

        let cancel_handle = {
            let mgr = Arc::clone(&mgr);
            let id1 = id1.clone();
            tokio::spawn(async move { mgr.control(&id1, TransferControlAction::Cancel).await })
        };
        let mid_cancel = wait_for_state(&mgr, &id1, |s| s == TransferState::Cancelling).await;
        assert_eq!(mid_cancel.state, TransferState::Cancelling);
        assert_eq!(platform.live_content_uris.lock().unwrap().len(), 1);

        platform.finalize_release.notify_waiters();
        let c1 = cancel_handle.await.unwrap().unwrap();
        assert_eq!(c1.state, TransferState::Cancelled);

        assert!(
            platform.live_content_uris.lock().unwrap().is_empty(),
            "Cancel during Android MediaStore save must delete the content:// entry before returning Cancelled"
        );
        assert!(
            platform
                .deleted_saved
                .lock()
                .unwrap()
                .iter()
                .any(|u| u.starts_with("content://media/external/downloads/"))
        );

        // Case B: Pause during save -> save finishes -> Cancel -> content:// URI deleted.
        let entered2 = platform.finalize_entered.notified();
        let id2 = mgr
            .enqueue_download(
                "tower".into(),
                "/home/me/android-after-pause.jpg".into(),
                None,
                None,
                None,
                false,
            )
            .await
            .unwrap();
        entered2.await;

        mgr.control(&id2, TransferControlAction::Pause)
            .await
            .unwrap();
        platform.finalize_release.notify_waiters();
        let _ = wait_for_finalized_path(
            &mgr,
            &id2,
            "paused Android finalize_download recording content URI",
        )
        .await;
        assert_eq!(platform.live_content_uris.lock().unwrap().len(), 1);

        let c2 = mgr
            .control(&id2, TransferControlAction::Cancel)
            .await
            .unwrap();
        assert_eq!(c2.state, TransferState::Cancelled);
        assert!(
            platform.live_content_uris.lock().unwrap().is_empty(),
            "Cancel after Pause-during-save must delete the Android MediaStore content:// entry"
        );
        assert!(!mgr.part_path(&id1).exists());
        assert!(!mgr.part_path(&id2).exists());
        mgr.shutdown();
    }

    #[tokio::test]
    async fn open_after_done_paused_during_finalize_and_resumed_after_restart_moves_to_downloads_not_cache()
     {
        let payload = b"important-photo-must-not-vanish-after-24h".to_vec();

        // Subcase 1: Desktop / local filesystem cache -> promoted to Downloads and cache copy removed.
        {
            let dir = tempfile::tempdir().unwrap();
            let remote = Arc::new(Mutex::new(MockRemoteState::default()));
            remote
                .lock()
                .unwrap()
                .files
                .insert("/home/me/holiday.jpg".into(), payload.clone());
            let backend: Arc<dyn Backend> = Arc::new(MockBackend::new(Arc::clone(&remote)));
            let platform = Arc::new(MockPlatform::new(dir.path()));
            platform.gate_finalize.store(true, Ordering::SeqCst);

            let mgr = TransferManager::with_options(
                dir.path(),
                Arc::clone(&backend),
                platform.clone(),
                2,
                Duration::from_secs(2),
            );
            let entered = platform.finalize_entered.notified();
            let id = mgr
                .enqueue_download(
                    "tower".into(),
                    "/home/me/holiday.jpg".into(),
                    None,
                    None,
                    None,
                    true,
                )
                .await
                .unwrap();
            entered.await;

            // Pause while staging in cache, then release finalize so cache copy lands on disk.
            mgr.control(&id, TransferControlAction::Pause)
                .await
                .unwrap();
            platform.finalize_release.notify_waiters();
            let _ = wait_for_finalized_path(
                &mgr,
                &id,
                "paused cache finalize_download recording finalized_local_path",
            )
            .await;
            assert_eq!(fs::read_dir(&platform.cache_dir).unwrap().count(), 1);
            assert_eq!(fs::read_dir(&platform.downloads_dir).unwrap().count(), 0);

            // Simulate app restart (open_after_done is session-only and resets to false).
            mgr.shutdown();
            platform.gate_finalize.store(false, Ordering::SeqCst);

            let mgr2 = TransferManager::with_options(
                dir.path(),
                backend,
                platform.clone(),
                2,
                Duration::from_secs(2),
            );
            mgr2.control(&id, TransferControlAction::Resume)
                .await
                .unwrap();
            let done = wait_for_state(&mgr2, &id, |s| s == TransferState::Done).await;
            let final_path = PathBuf::from(done.local_path.as_deref().unwrap());
            assert_eq!(
                final_path.parent(),
                Some(platform.downloads_dir.as_path()),
                "Done transfer after restart must live in Downloads, never in cache: {final_path:?}"
            );
            assert_eq!(fs::read(&final_path).unwrap(), payload);
            assert!(done.can_show_in_folder);
            assert_eq!(
                fs::read_dir(&platform.cache_dir).unwrap().count(),
                0,
                "old open-externally cache copy must be cleaned up"
            );
            mgr2.shutdown();
        }

        // Subcase 2: Android FileProvider content:// cache URI -> cache URI deleted and re-saved to
        // MediaStore Downloads on resume after restart.
        {
            let dir = tempfile::tempdir().unwrap();
            let remote = Arc::new(Mutex::new(MockRemoteState::default()));
            remote
                .lock()
                .unwrap()
                .files
                .insert("/home/me/holiday.jpg".into(), payload.clone());
            let backend: Arc<dyn Backend> = Arc::new(MockBackend::new(Arc::clone(&remote)));
            let platform = Arc::new(MockPlatform::new(dir.path()));
            platform.gate_finalize.store(true, Ordering::SeqCst);
            platform.return_content_uris.store(true, Ordering::SeqCst);

            let mgr = TransferManager::with_options(
                dir.path(),
                Arc::clone(&backend),
                platform.clone(),
                2,
                Duration::from_secs(2),
            );
            let entered = platform.finalize_entered.notified();
            let id = mgr
                .enqueue_download(
                    "tower".into(),
                    "/home/me/holiday.jpg".into(),
                    None,
                    None,
                    None,
                    true,
                )
                .await
                .unwrap();
            entered.await;

            mgr.control(&id, TransferControlAction::Pause)
                .await
                .unwrap();
            platform.finalize_release.notify_waiters();
            let _ = wait_for_finalized_path(
                &mgr,
                &id,
                "paused Android cache finalize_download recording FileProvider URI",
            )
            .await;
            assert!(
                platform
                    .live_content_uris
                    .lock()
                    .unwrap()
                    .keys()
                    .all(|u| u.contains(".fileprovider/open_externally/"))
            );

            // Simulate app restart and resume.
            mgr.shutdown();
            platform.gate_finalize.store(false, Ordering::SeqCst);

            let mgr2 = TransferManager::with_options(
                dir.path(),
                backend,
                platform.clone(),
                2,
                Duration::from_secs(2),
            );
            mgr2.control(&id, TransferControlAction::Resume)
                .await
                .unwrap();
            let done = wait_for_state(&mgr2, &id, |s| s == TransferState::Done).await;
            let final_uri = done.local_path.as_deref().unwrap();
            assert!(
                final_uri.starts_with("content://media/external/downloads/"),
                "Done Android transfer after restart must land in MediaStore Downloads, got {final_uri}"
            );
            assert!(done.can_show_in_folder);
            assert!(
                platform
                    .deleted_saved
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|u| u.contains(".fileprovider/open_externally/")),
                "stale Android FileProvider cache URI must be deleted"
            );
            let live = platform.live_content_uris.lock().unwrap();
            assert_eq!(live.len(), 1);
            assert_eq!(live.get(final_uri).unwrap(), &payload);
            mgr2.shutdown();
        }
    }

    #[tokio::test]
    async fn download_survives_parked_stream_eviction_and_finishes_byte_exact() {
        let dir = tempfile::tempdir().unwrap();
        let payload: Vec<u8> = (0..(512 * 1024))
            .map(|i| ((i * 37 + 11) % 251) as u8)
            .collect();
        let remote = Arc::new(Mutex::new(MockRemoteState::default()));
        remote
            .lock()
            .unwrap()
            .files
            .insert("/home/me/archive.bin".into(), payload.clone());

        let mock_backend = Arc::new(MockBackend::with_options(
            Arc::clone(&remote),
            LinkOptions {
                max_parked_streams: 1,
                ..LinkOptions::default()
            },
            Some(legion_link::content::WindowPolicy {
                initial_bytes: 64 * 1024,
                max_bytes: 64 * 1024,
            }),
        ));
        let backend: Arc<dyn Backend> = mock_backend.clone();
        let platform = Arc::new(MockPlatform::new(dir.path()));
        let mgr = TransferManager::with_options(
            dir.path(),
            backend,
            platform.clone(),
            2,
            Duration::from_secs(5),
        );

        // Pause the download worker right after writing its first chunk so its ContentStream sits
        // idle past the 2-second park timeout and releases its channel.
        let gate = Arc::new(DownloadPauseGate {
            entered: Notify::new(),
            release: Notify::new(),
        });
        *mgr.download_pause_once.lock().unwrap() = Some(Arc::clone(&gate));

        let entered = gate.entered.notified();
        let id = mgr
            .enqueue_download(
                "tower".into(),
                "/home/me/archive.bin".into(),
                None,
                None,
                None,
                false,
            )
            .await
            .unwrap();
        entered.await;

        // Wait past the 2s idle-park timeout so the download's stream parks and releases its channel.
        tokio::time::sleep(Duration::from_millis(2400)).await;
        assert_eq!(
            remote.lock().unwrap().active_streams.load(Ordering::SeqCst),
            0,
            "idle download stream must park and release its active channel"
        );

        // Now open and park two viewer streams on the same cap-1 Link so the second evicts the
        // first from the link's ParkedStreamPool.
        let mut viewer1 = mock_backend
            .link
            .open_content("tower", "/home/me/archive.bin", 0, None)
            .await
            .unwrap();
        let mut one_byte = [0u8; 1];
        viewer1.read_exact(&mut one_byte).await.unwrap();
        tokio::time::sleep(Duration::from_millis(2400)).await;

        let mut viewer2 = mock_backend
            .link
            .open_content("tower", "/home/me/archive.bin", 0, None)
            .await
            .unwrap();
        viewer2.read_exact(&mut one_byte).await.unwrap();
        tokio::time::sleep(Duration::from_millis(2400)).await;

        // Confirm the cap-1 parked pool actually evicted viewer1 with PermissionDenied.
        let mut drain_buf = [0u8; 64];
        let evicted_err = viewer1
            .read(&mut drain_buf)
            .await
            .expect_err("cap-1 parked pool must evict viewer1 when viewer2 parks");
        assert_eq!(evicted_err.kind(), std::io::ErrorKind::PermissionDenied);

        let opens_before_resume = remote
            .lock()
            .unwrap()
            .opened_get_streams
            .load(Ordering::SeqCst);

        // Resume the paused download: because transfer downloads are exempt from the parked-stream
        // eviction cap, the stream transparently reopens at the exact next byte offset and finishes
        // byte-exact without error.
        gate.release.notify_one();
        let done = wait_for_state(&mgr, &id, |s| s == TransferState::Done).await;
        assert!(done.error.is_none(), "unexpected error: {:?}", done.error);
        let final_path = PathBuf::from(done.local_path.as_deref().unwrap());
        assert_eq!(fs::read(&final_path).unwrap(), payload);
        assert!(
            remote
                .lock()
                .unwrap()
                .opened_get_streams
                .load(Ordering::SeqCst)
                > opens_before_resume,
            "parked download stream must have transparently reopened after parking"
        );

        drop(viewer2);
        mgr.shutdown();
    }

    #[tokio::test]
    async fn cancel_then_immediate_redownload_of_same_file_succeeds_without_leftovers() {
        let dir = tempfile::tempdir().unwrap();
        let payload: Vec<u8> = (0..64 * 1024usize).map(|i| (i * 7 % 251) as u8).collect();
        let remote = Arc::new(Mutex::new(MockRemoteState::default()));
        {
            let mut st = remote.lock().unwrap();
            st.per_chunk_delay_ms = 10;
            st.files
                .insert("/home/me/manual.pdf".into(), payload.clone());
        }
        let backend: Arc<dyn Backend> = Arc::new(MockBackend::new(Arc::clone(&remote)));
        let platform = Arc::new(MockPlatform::new(dir.path()));
        let mgr = TransferManager::with_options(
            dir.path(),
            backend,
            platform.clone(),
            2,
            Duration::from_secs(2),
        );

        // 1. Cancel while actively streaming, then immediately re-download the exact same file.
        let id1 = mgr
            .enqueue_download(
                "tower".into(),
                "/home/me/manual.pdf".into(),
                None,
                None,
                None,
                false,
            )
            .await
            .unwrap();
        let _ = wait_for_state(&mgr, &id1, |s| s == TransferState::Running).await;
        tokio::time::sleep(Duration::from_millis(15)).await;
        let c1 = mgr
            .control(&id1, TransferControlAction::Cancel)
            .await
            .unwrap();
        assert_eq!(c1.state, TransferState::Cancelled);
        assert!(!mgr.part_path(&id1).exists());
        assert!(!platform.downloads_dir.join("manual.pdf").exists());

        // 2. Cancel while finalize_download is in flight, then immediately re-download.
        remote.lock().unwrap().per_chunk_delay_ms = 0;
        platform.gate_finalize.store(true, Ordering::SeqCst);
        let entered = platform.finalize_entered.notified();
        let id2 = mgr
            .enqueue_download(
                "tower".into(),
                "/home/me/manual.pdf".into(),
                None,
                None,
                None,
                false,
            )
            .await
            .unwrap();
        entered.await;

        let cancel2 = {
            let mgr = Arc::clone(&mgr);
            let id2 = id2.clone();
            tokio::spawn(async move { mgr.control(&id2, TransferControlAction::Cancel).await })
        };
        let _ = wait_for_state(&mgr, &id2, |s| s == TransferState::Cancelling).await;
        platform.finalize_release.notify_waiters();
        let c2 = cancel2.await.unwrap().unwrap();
        assert_eq!(c2.state, TransferState::Cancelled);
        assert!(!mgr.part_path(&id2).exists());
        assert!(!platform.downloads_dir.join("manual.pdf").exists());

        // 3. Re-downloading right after cancel lands cleanly at Downloads/manual.pdf (no `(2)`).
        platform.gate_finalize.store(false, Ordering::SeqCst);
        let id3 = mgr
            .enqueue_download(
                "tower".into(),
                "/home/me/manual.pdf".into(),
                None,
                None,
                None,
                false,
            )
            .await
            .unwrap();
        let done = wait_for_state(&mgr, &id3, |s| s == TransferState::Done).await;
        let expected_dst = platform.downloads_dir.join("manual.pdf");
        assert_eq!(
            done.local_path.as_deref(),
            Some(expected_dst.to_string_lossy().as_ref())
        );
        assert_eq!(fs::read(&expected_dst).unwrap(), payload);
        assert!(!platform.downloads_dir.join("manual (2).pdf").exists());
        assert!(!mgr.part_path(&id1).exists());
        assert!(!mgr.part_path(&id2).exists());
        assert!(!mgr.part_path(&id3).exists());
        mgr.shutdown();
    }

    #[tokio::test]
    async fn cancel_removal_failure_reports_cancelling_and_retries_until_removed() {
        let dir = tempfile::tempdir().unwrap();
        let payload = b"locked-on-windows-payload".to_vec();
        let remote = Arc::new(Mutex::new(MockRemoteState::default()));
        {
            let mut st = remote.lock().unwrap();
            st.files
                .insert("/home/me/locked-part.dat".into(), vec![7u8; 64 * 1024]);
            st.files
                .insert("/home/me/locked-saved.pdf".into(), payload.clone());
            st.files
                .insert("/home/me/brief-lock.dat".into(), vec![9u8; 64 * 1024]);
            st.per_chunk_delay_ms = 15;
        }
        let backend: Arc<dyn Backend> = Arc::new(MockBackend::new(Arc::clone(&remote)));
        let platform = Arc::new(MockPlatform::new(dir.path()));
        let mgr = TransferManager::with_cleanup_retry_interval(
            dir.path(),
            backend,
            platform.clone(),
            2,
            Duration::from_secs(2),
            Duration::from_millis(20),
        );

        // Case 1: Brief transient lock (2 failures) clears within control(Cancel)'s immediate retries.
        let id_brief = mgr
            .enqueue_download(
                "tower".into(),
                "/home/me/brief-lock.dat".into(),
                None,
                None,
                None,
                false,
            )
            .await
            .unwrap();
        let brief_deadline = Instant::now() + Duration::from_secs(10);
        while !mgr.part_path(&id_brief).exists() {
            assert!(
                Instant::now() < brief_deadline,
                "timed out waiting for brief-lock .part file to appear"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        platform.fail_delete_part.store(2, Ordering::SeqCst);
        let c_brief = mgr
            .control(&id_brief, TransferControlAction::Cancel)
            .await
            .unwrap();
        assert_eq!(c_brief.state, TransferState::Cancelled);
        assert!(c_brief.error.is_none());
        assert!(!mgr.part_path(&id_brief).exists());

        // Case 2: Persistent lock on `.part` (exceeds immediate retries) reports Cancelling + error,
        // keeps retrying automatically, and transitions to Cancelled only after removal succeeds.
        let id_part = mgr
            .enqueue_download(
                "tower".into(),
                "/home/me/locked-part.dat".into(),
                None,
                None,
                None,
                false,
            )
            .await
            .unwrap();
        let part_deadline = Instant::now() + Duration::from_secs(10);
        while !mgr.part_path(&id_part).exists() {
            assert!(
                Instant::now() < part_deadline,
                "timed out waiting for locked-part .part file to appear"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        platform.fail_delete_part.store(100, Ordering::SeqCst);
        let c_part = mgr
            .control(&id_part, TransferControlAction::Cancel)
            .await
            .unwrap();
        assert_eq!(c_part.state, TransferState::Cancelling);
        assert!(
            c_part
                .error
                .as_ref()
                .is_some_and(|e| e.message.contains("sharing violation"))
        );
        assert!(mgr.part_path(&id_part).exists());

        // Release the lock -> background retry loop finishes removal and transitions to Cancelled.
        platform.fail_delete_part.store(0, Ordering::SeqCst);
        let done_part = wait_for_state(&mgr, &id_part, |s| s == TransferState::Cancelled).await;
        assert!(done_part.error.is_none());
        assert!(!mgr.part_path(&id_part).exists());

        // Case 3: Persistent lock on saved Downloads file after pause-during-finalize reports
        // Cancelling + error until unlocked, then deletes the saved file and becomes Cancelled.
        remote.lock().unwrap().per_chunk_delay_ms = 0;
        platform.gate_finalize.store(true, Ordering::SeqCst);
        let entered = platform.finalize_entered.notified();
        let id_saved = mgr
            .enqueue_download(
                "tower".into(),
                "/home/me/locked-saved.pdf".into(),
                None,
                None,
                None,
                false,
            )
            .await
            .unwrap();
        entered.await;
        mgr.control(&id_saved, TransferControlAction::Pause)
            .await
            .unwrap();
        platform.finalize_release.notify_waiters();
        let saved_path = platform.downloads_dir.join("locked-saved.pdf");
        let _ = wait_for_finalized_path(
            &mgr,
            &id_saved,
            "paused finalize_download finishing before Cancel with locked saved file",
        )
        .await;
        assert!(saved_path.exists());

        platform.fail_delete_saved.store(100, Ordering::SeqCst);
        let c_saved = mgr
            .control(&id_saved, TransferControlAction::Cancel)
            .await
            .unwrap();
        assert_eq!(c_saved.state, TransferState::Cancelling);
        assert!(
            c_saved
                .error
                .as_ref()
                .is_some_and(|e| e.message.contains("file in use"))
        );
        assert!(saved_path.exists());

        platform.fail_delete_saved.store(0, Ordering::SeqCst);
        let done_saved = wait_for_state(&mgr, &id_saved, |s| s == TransferState::Cancelled).await;
        assert!(done_saved.error.is_none());
        assert!(!saved_path.exists());
        assert!(!mgr.part_path(&id_saved).exists());
        mgr.shutdown();
    }

    #[tokio::test]
    async fn persisted_cancelling_resumes_cleanup_after_restart() {
        let dir = tempfile::tempdir().unwrap();
        let payload = b"persisted-cancelling-restart-payload".to_vec();
        let remote = Arc::new(Mutex::new(MockRemoteState::default()));
        remote
            .lock()
            .unwrap()
            .files
            .insert("/home/me/restart-cancel.pdf".into(), payload);

        let backend: Arc<dyn Backend> = Arc::new(MockBackend::new(Arc::clone(&remote)));
        let platform = Arc::new(MockPlatform::new(dir.path()));
        platform.gate_finalize.store(true, Ordering::SeqCst);

        let mgr1 = TransferManager::with_cleanup_retry_interval(
            dir.path(),
            Arc::clone(&backend),
            platform.clone(),
            2,
            Duration::from_secs(2),
            Duration::from_secs(60),
        );
        let entered = platform.finalize_entered.notified();
        let id = mgr1
            .enqueue_download(
                "tower".into(),
                "/home/me/restart-cancel.pdf".into(),
                None,
                None,
                None,
                false,
            )
            .await
            .unwrap();
        entered.await;
        mgr1.control(&id, TransferControlAction::Pause)
            .await
            .unwrap();
        platform.finalize_release.notify_waiters();
        let saved_path = platform.downloads_dir.join("restart-cancel.pdf");
        // Wait for the worker to finish post-finalize `.part` cleanup and clear its slot before
        // recreating the stray `.part` file, so the worker cannot race and delete our test file.
        let _ = wait_for_finalized_path(
            &mgr1,
            &id,
            "paused finalize_download finishing before recreating stray .part",
        )
        .await;
        assert!(saved_path.exists());
        // Recreate a stray `.part` alongside the saved file and lock both so cancel persists as
        // `Cancelling` across app exit.
        fs::write(mgr1.part_path(&id), b"partial").unwrap();
        platform.fail_delete_part.store(100, Ordering::SeqCst);
        platform.fail_delete_saved.store(100, Ordering::SeqCst);
        let cancelling = mgr1
            .control(&id, TransferControlAction::Cancel)
            .await
            .unwrap();
        assert_eq!(cancelling.state, TransferState::Cancelling);
        assert!(saved_path.exists());
        assert!(mgr1.part_path(&id).exists());
        mgr1.shutdown();

        // Simulate app restart after the external lock is released.
        platform.fail_delete_part.store(0, Ordering::SeqCst);
        platform.fail_delete_saved.store(0, Ordering::SeqCst);
        let mgr2 = TransferManager::with_cleanup_retry_interval(
            dir.path(),
            backend,
            platform.clone(),
            2,
            Duration::from_secs(2),
            Duration::from_millis(20),
        );
        let loaded = mgr2.list().into_iter().find(|t| t.id == id).unwrap();
        assert_eq!(loaded.state, TransferState::Cancelling);
        assert!(mgr2.has_unfinished_or_paused());

        mgr2.start();
        let cancelled = wait_for_state(&mgr2, &id, |s| s == TransferState::Cancelled).await;
        assert!(cancelled.error.is_none());
        assert!(!saved_path.exists());
        assert!(!mgr2.part_path(&id).exists());
        mgr2.shutdown();
    }

    #[tokio::test]
    async fn upload_uses_atomic_if_exists_fail_when_machine_supports_it_and_skips_stat() {
        let dir = tempfile::tempdir().unwrap();
        let bytes1 = b"first-upload-raced-between-stat-and-put".to_vec();
        let bytes2 = b"second-upload-skips-stat-and-catches-commit-race".to_vec();
        let src1 = dir.path().join("notes.txt");
        let src2 = dir.path().join("report.pdf");
        fs::write(&src1, &bytes1).unwrap();
        fs::write(&src2, &bytes2).unwrap();

        let remote = Arc::new(Mutex::new(MockRemoteState::default()));
        {
            let mut st = remote.lock().unwrap();
            st.advertise_cap_if_exists = true;
            // Upload #1: simulate a file created *between `fs.stat` and `put`*, so the fallback
            // `fs.stat` sees NotFound and only `"if_exists": "fail"` catches it atomically.
            st.race_create_after_stat
                .insert("/home/me/uploads/notes.txt".into());
            // Upload #2: pre-existing file refused on `PutReady`, and numbered sibling #2 raced
            // right before final commit (`PutDone`), landing cleanly at `report (3).pdf`.
            st.files.insert(
                "/home/me/uploads/report.pdf".into(),
                b"original-pdf".to_vec(),
            );
            st.race_create_on_put_finish
                .insert("/home/me/uploads/report (2).pdf".into());
        }

        let backend: Arc<dyn Backend> = Arc::new(MockBackend::new(Arc::clone(&remote)));
        let platform = Arc::new(MockPlatform::new(dir.path()));
        let mgr = TransferManager::with_options(
            dir.path(),
            backend,
            platform.clone(),
            2,
            Duration::from_secs(2),
        );

        // Upload #1 (before any stream greeting): probes `fs.stat`, sees NotFound, another writer
        // creates `notes.txt` before `put`, `"if_exists": "fail"` refuses atomically with
        // `ErrorCode::Exists`, leaves the raced file untouched, and succeeds at `notes (2).txt`.
        platform
            .picked_files
            .lock()
            .unwrap()
            .push(PickedUploadFile {
                local_path: src1.to_string_lossy().into_owned(),
                name: "notes.txt".into(),
                size: bytes1.len() as u64,
                mime: None,
            });
        let picked1 = mgr.pick_uploads().await.unwrap();
        let up1 = mgr
            .enqueue_upload(
                "tower".into(),
                "/home/me/uploads".into(),
                Some(vec![picked1[0].handle.clone()]),
            )
            .await
            .unwrap()
            .unwrap();
        let done1 = wait_for_state(&mgr, &up1.id, |s| s == TransferState::Done).await;
        assert_eq!(done1.path, "/home/me/uploads/notes (2).txt");
        assert_eq!(done1.name, "notes (2).txt");
        {
            let st = remote.lock().unwrap();
            assert_eq!(
                st.files.get("/home/me/uploads/notes.txt").unwrap(),
                b"created-between-stat-and-put"
            );
            assert_eq!(
                st.uploaded.get("/home/me/uploads/notes (2).txt").unwrap(),
                &bytes1
            );
            assert!(
                st.stat_calls.load(Ordering::SeqCst) >= 1,
                "first upload before stream greeting probes fs.stat for older-machine safety"
            );
        }

        // Upload #2: capability `if_exists` is now cached -> makes ZERO `fs.stat` calls, hits
        // `Exists` on `PutReady` for `report.pdf`, hits `Exists` at `PutDone` for `report (2).pdf`,
        // and succeeds at `report (3).pdf`.
        remote.lock().unwrap().stat_calls.store(0, Ordering::SeqCst);
        {
            let mut picked = platform.picked_files.lock().unwrap();
            picked.clear();
            picked.push(PickedUploadFile {
                local_path: src2.to_string_lossy().into_owned(),
                name: "report.pdf".into(),
                size: bytes2.len() as u64,
                mime: None,
            });
        }
        let picked2 = mgr.pick_uploads().await.unwrap();
        let up2 = mgr
            .enqueue_upload(
                "tower".into(),
                "/home/me/uploads".into(),
                Some(vec![picked2[0].handle.clone()]),
            )
            .await
            .unwrap()
            .unwrap();
        let done2 = wait_for_state(&mgr, &up2.id, |s| s == TransferState::Done).await;
        assert_eq!(done2.path, "/home/me/uploads/report (3).pdf");
        assert_eq!(done2.name, "report (3).pdf");
        {
            let st = remote.lock().unwrap();
            assert_eq!(
                st.stat_calls.load(Ordering::SeqCst),
                0,
                "upload must skip fs.stat once machine advertised if_exists"
            );
            assert_eq!(
                st.files.get("/home/me/uploads/report.pdf").unwrap(),
                b"original-pdf"
            );
            assert_eq!(
                st.files.get("/home/me/uploads/report (2).pdf").unwrap(),
                b"concurrent-device-file"
            );
            assert_eq!(
                st.uploaded.get("/home/me/uploads/report (3).pdf").unwrap(),
                &bytes2
            );
            let reqs = st.put_requests.lock().unwrap();
            assert!(reqs.iter().all(|r| r["if_exists"] == json!("fail")));
        }
        mgr.shutdown();
    }

    #[tokio::test]
    async fn upload_falls_back_to_stat_on_older_machine_and_uses_if_exists_fail_over_relay() {
        // Subcase A: Older machine omitting `if_exists` in its stream greeting keeps
        // fs.stat-then-put and stops sending `if_exists` once lack of support is cached.
        {
            let dir = tempfile::tempdir().unwrap();
            let src1 = dir.path().join("a.txt");
            let src2 = dir.path().join("b.txt");
            fs::write(&src1, b"aaa").unwrap();
            fs::write(&src2, b"bbb").unwrap();

            let remote = Arc::new(Mutex::new(MockRemoteState::default()));
            remote
                .lock()
                .unwrap()
                .files
                .insert("/home/me/uploads/b.txt".into(), b"existing-b".to_vec());

            let backend: Arc<dyn Backend> = Arc::new(MockBackend::new(Arc::clone(&remote)));
            let platform = Arc::new(MockPlatform::new(dir.path()));
            let mgr = TransferManager::with_options(
                dir.path(),
                backend,
                platform.clone(),
                2,
                Duration::from_secs(2),
            );

            platform
                .picked_files
                .lock()
                .unwrap()
                .push(PickedUploadFile {
                    local_path: src1.to_string_lossy().into_owned(),
                    name: "a.txt".into(),
                    size: 3,
                    mime: None,
                });
            let p1 = mgr.pick_uploads().await.unwrap();
            let u1 = mgr
                .enqueue_upload(
                    "tower".into(),
                    "/home/me/uploads".into(),
                    Some(vec![p1[0].handle.clone()]),
                )
                .await
                .unwrap()
                .unwrap();
            let _ = wait_for_state(&mgr, &u1.id, |s| s == TransferState::Done).await;

            remote.lock().unwrap().stat_calls.store(0, Ordering::SeqCst);
            {
                let mut picked = platform.picked_files.lock().unwrap();
                picked.clear();
                picked.push(PickedUploadFile {
                    local_path: src2.to_string_lossy().into_owned(),
                    name: "b.txt".into(),
                    size: 3,
                    mime: None,
                });
            }
            let p2 = mgr.pick_uploads().await.unwrap();
            let u2 = mgr
                .enqueue_upload(
                    "tower".into(),
                    "/home/me/uploads".into(),
                    Some(vec![p2[0].handle.clone()]),
                )
                .await
                .unwrap()
                .unwrap();
            let d2 = wait_for_state(&mgr, &u2.id, |s| s == TransferState::Done).await;
            assert_eq!(d2.path, "/home/me/uploads/b (2).txt");
            let st = remote.lock().unwrap();
            assert!(
                st.stat_calls.load(Ordering::SeqCst) >= 2,
                "older machine must keep fs.stat collision probing"
            );
            assert_eq!(
                st.files.get("/home/me/uploads/b.txt").unwrap(),
                b"existing-b"
            );
            let reqs = st.put_requests.lock().unwrap();
            assert!(
                reqs.last().unwrap().get("if_exists").is_none(),
                "older machine must not receive if_exists once lack of support is known"
            );
            mgr.shutdown();
        }

        // Subcase B: Relayed upload onto `studio` via `tower` where a file appears on `studio`
        // *between `fs.stat` and `put`*: relayed `put` carries `"if_exists": "fail"`, `studio`
        // refuses atomically with `Exists`, leaves the raced file untouched, and the app lands
        // the upload at `raced (2).txt`.
        {
            use legion_link::CAP_STREAM_RELAY;
            let dir = tempfile::tempdir().unwrap();
            let src = dir.path().join("raced.txt");
            let payload = b"relayed-upload-after-race".to_vec();
            fs::write(&src, &payload).unwrap();

            let studio_files: Arc<Mutex<HashMap<String, Vec<u8>>>> =
                Arc::new(Mutex::new(HashMap::new()));
            let studio_uploaded: Arc<Mutex<HashMap<String, Vec<u8>>>> =
                Arc::new(Mutex::new(HashMap::new()));
            let race_after_stat = Arc::new(AtomicBool::new(true));
            let seen_inner_puts: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
            let tower_key =
                "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAITowerKeyRelayedIfExists00000000000000000000";

            struct RelayIfExistsOpener {
                files: Arc<Mutex<HashMap<String, Vec<u8>>>>,
                uploaded: Arc<Mutex<HashMap<String, Vec<u8>>>>,
                seen: Arc<Mutex<Vec<Value>>>,
            }

            impl StreamOpener for RelayIfExistsOpener {
                fn open_stream(
                    &self,
                    _cancel: CancellationToken,
                ) -> Pin<Box<dyn Future<Output = Result<RawStream, StreamOpenError>> + Send>>
                {
                    let files = Arc::clone(&self.files);
                    let uploaded = Arc::clone(&self.uploaded);
                    let seen = Arc::clone(&self.seen);
                    Box::pin(async move {
                        let (client_in, mut server_out) = tokio::io::duplex(64 * 1024);
                        let (server_in, client_out) = tokio::io::duplex(64 * 1024);
                        let stream_cancel = CancellationToken::new();
                        tokio::spawn(async move {
                            let greeting = json!({
                                "protocol": 1,
                                "capabilities": [CAP_IF_EXISTS, CAP_STREAM_RELAY],
                            });
                            let _ = server_out
                                .write_all(format!("{greeting}\n").as_bytes())
                                .await;
                            let mut reader = BufReader::new(server_in);
                            let mut line = String::new();
                            if reader.read_line(&mut line).await.unwrap_or(0) == 0 {
                                return;
                            }
                            let outer: Value = serde_json::from_str(&line).unwrap();
                            assert_eq!(outer["op"], json!("relay"));
                            assert_eq!(outer["on"], json!("studio"));
                            let req = outer["request"].clone();
                            assert_eq!(req["op"], json!("put"));
                            seen.lock().unwrap().push(req.clone());

                            let path = req["path"].as_str().unwrap().to_owned();
                            let size = req["size"].as_u64().unwrap() as usize;
                            let fail_if_exists =
                                req.get("if_exists").and_then(Value::as_str) == Some("fail");
                            let exists = files.lock().unwrap().contains_key(&path)
                                || uploaded.lock().unwrap().contains_key(&path);
                            if fail_if_exists && exists {
                                let err = json!({
                                    "error": ErrorBody::new(
                                        ProtoCode::Exists,
                                        format!("{path} already exists."),
                                    )
                                    .with_machine("studio")
                                });
                                let _ = server_out.write_all(format!("{err}\n").as_bytes()).await;
                                let _ = server_out.shutdown().await;
                                return;
                            }
                            let ready = json!({ "offset": 0 });
                            let _ = server_out.write_all(format!("{ready}\n").as_bytes()).await;
                            let mut got = Vec::new();
                            let _ = reader.read_to_end(&mut got).await;
                            assert_eq!(got.len(), size);
                            let hash = format!("blake3:{}", blake3::hash(&got).to_hex());
                            uploaded.lock().unwrap().insert(path, got);
                            let done = json!({ "ok": true, "size": size, "hash": hash });
                            let _ = server_out.write_all(format!("{done}\n").as_bytes()).await;
                            let _ = server_out.shutdown().await;
                        });
                        Ok(RawStream {
                            reader: Box::new(client_in),
                            writer: Box::new(client_out),
                            stderr: None,
                            cancel: stream_cancel,
                            park: None,
                            guard: None,
                        })
                    })
                }
            }

            let opener: SessionOpener = {
                let files = Arc::clone(&studio_files);
                let uploaded = Arc::clone(&studio_uploaded);
                let race_after_stat = Arc::clone(&race_after_stat);
                let seen = Arc::clone(&seen_inner_puts);
                Arc::new(move |entry, _, cancel| {
                    let files = Arc::clone(&files);
                    let uploaded = Arc::clone(&uploaded);
                    let race_after_stat = Arc::clone(&race_after_stat);
                    let seen = Arc::clone(&seen);
                    Box::pin(async move {
                        let (client_in, mut server_out) = tokio::io::duplex(16 * 1024);
                        let (server_in, client_out) = tokio::io::duplex(16 * 1024);
                        let files_rpc = Arc::clone(&files);
                        let uploaded_rpc = Arc::clone(&uploaded);
                        tokio::spawn(async move {
                            let mut reader = BufReader::new(server_in);
                            let mut line = String::new();
                            while reader.read_line(&mut line).await.unwrap_or(0) > 0 {
                                let req: legion_link::proto::Request =
                                    serde_json::from_str(&line).unwrap();
                                let reply = if req.method == "hello" {
                                    ProtoResp::ok(
                                        req.id,
                                        serde_json::to_value(HelloResult {
                                            node: "tower".to_owned(),
                                            node_explicit: None,
                                            node_id: None,
                                            version: VERSION.to_owned(),
                                            build: None,
                                            protocol: PROTOCOL,
                                            os: "linux".to_owned(),
                                            arch: "x86_64".to_owned(),
                                            systems: vec![],
                                            system: None,
                                            boot_id: None,
                                        })
                                        .unwrap(),
                                    )
                                } else if req.method == "call" {
                                    let tool = req
                                        .params
                                        .get("tool")
                                        .and_then(Value::as_str)
                                        .unwrap_or("");
                                    let path = req
                                        .params
                                        .pointer("/args/path")
                                        .and_then(Value::as_str)
                                        .unwrap_or("");
                                    if tool == "fs.stat" {
                                        let found = files_rpc
                                            .lock()
                                            .unwrap()
                                            .get(path)
                                            .map(Vec::len)
                                            .or_else(|| {
                                                uploaded_rpc.lock().unwrap().get(path).map(Vec::len)
                                            });
                                        if let Some(len) = found {
                                            ProtoResp::ok(
                                                req.id,
                                                json!({ "kind": "file", "size": len }),
                                            )
                                        } else {
                                            // Create the colliding file right after fs.stat returns NotFound!
                                            if path == "/home/me/Uploads/raced.txt"
                                                && race_after_stat.swap(false, Ordering::SeqCst)
                                            {
                                                files_rpc.lock().unwrap().insert(
                                                    path.to_owned(),
                                                    b"raced-on-studio-between-stat-and-put"
                                                        .to_vec(),
                                                );
                                            }
                                            ProtoResp::error(
                                                req.id,
                                                legion_link::proto::RpcError::new(
                                                    -32000,
                                                    ErrorBody::new(
                                                        ProtoCode::NotFound,
                                                        "not found",
                                                    )
                                                    .with_machine("studio"),
                                                ),
                                            )
                                        }
                                    } else {
                                        ProtoResp::ok(req.id, json!({}))
                                    }
                                } else {
                                    ProtoResp::ok(req.id, json!({}))
                                };
                                let _ = server_out
                                    .write_all(
                                        format!("{}\n", serde_json::to_string(&reply).unwrap())
                                            .as_bytes(),
                                    )
                                    .await;
                                line.clear();
                            }
                        });
                        Session::open_transport(
                            client_in,
                            client_out,
                            Box::new(FakeTransport {
                                opener: Arc::new(RelayIfExistsOpener {
                                    files,
                                    uploaded,
                                    seen,
                                }),
                            }),
                            &entry.node,
                            None,
                            Duration::from_secs(5),
                            cancel,
                        )
                        .await
                        .map_err(|err| err.into_error())
                    })
                })
            };

            let link = Link::with_opener(
                LinkConfig {
                    nodes: vec![NodeLink::ssh(
                        "tower",
                        SshTarget {
                            hosts: vec!["192.0.2.21".to_owned()],
                            port: 22,
                            user: "me".to_owned(),
                            host_keys: vec![tower_key.to_owned()],
                        },
                        Some("device:pixel".to_owned()),
                    )],
                    ..LinkConfig::default()
                },
                LinkOptions::default(),
                opener,
            );
            struct RelayedTargetBackend {
                link: Link,
            }
            impl Backend for RelayedTargetBackend {
                fn state(&self) -> BoxFuture<'_, Result<AppState, AppError>> {
                    Box::pin(async { Err(AppError::not_paired()) })
                }
                fn tools(&self, _on: Option<Value>) -> BoxFuture<'_, Result<Vec<Value>, AppError>> {
                    Box::pin(async { Err(AppError::not_paired()) })
                }
                fn call(
                    &self,
                    request: CallRequest,
                    _on_event: Option<tauri::ipc::Channel<Value>>,
                ) -> BoxFuture<'_, Result<Value, AppError>> {
                    let link = self.link.clone();
                    Box::pin(async move {
                        let machine = request
                            .on
                            .as_ref()
                            .and_then(|v| v.as_str())
                            .unwrap_or("tower")
                            .to_owned();
                        link.call_machine(&machine, &request.tool, request.args, |_| {})
                            .await
                            .map(|r| r.value)
                            .map_err(AppError::from)
                    })
                }
                fn cancel(&self, _id: CallId) -> BoxFuture<'_, Result<CancelResult, AppError>> {
                    Box::pin(async { Err(AppError::not_paired()) })
                }
                fn pair(
                    &self,
                    _invite: String,
                    _name: String,
                ) -> BoxFuture<'_, Result<AppState, AppError>> {
                    Box::pin(async { Err(AppError::not_paired()) })
                }
                fn unpair(&self, _node: String) -> BoxFuture<'_, Result<UnpairResult, AppError>> {
                    Box::pin(async { Err(AppError::not_paired()) })
                }
                fn select_node(&self, _node: String) -> BoxFuture<'_, Result<AppState, AppError>> {
                    Box::pin(async { Err(AppError::not_paired()) })
                }
                fn repair_store(&self) -> BoxFuture<'_, Result<RepairResult, AppError>> {
                    Box::pin(async { Err(AppError::not_paired()) })
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
                fn dismiss_identity_change(
                    &self,
                    _node: String,
                ) -> BoxFuture<'_, Result<AppState, AppError>> {
                    Box::pin(async { Err(AppError::not_paired()) })
                }
                fn content_target(&self, machine: &str) -> Result<ContentTarget, AppError> {
                    Ok(ContentTarget {
                        link: self.link.clone(),
                        link_node: machine.to_owned(),
                        machine: machine.to_owned(),
                        principal: "device:pixel".to_owned(),
                        holder: "fleet:1".to_owned(),
                        learned_identity: None,
                    })
                }
                fn is_pairing_holder_live(&self, _: &str) -> bool {
                    true
                }
            }

            let backend: Arc<dyn Backend> = Arc::new(RelayedTargetBackend { link });
            let platform = Arc::new(MockPlatform::new(dir.path()));
            let mgr = TransferManager::with_options(
                dir.path(),
                backend,
                platform.clone(),
                2,
                Duration::from_secs(2),
            );
            platform
                .picked_files
                .lock()
                .unwrap()
                .push(PickedUploadFile {
                    local_path: src.to_string_lossy().into_owned(),
                    name: "raced.txt".into(),
                    size: payload.len() as u64,
                    mime: None,
                });
            let picked = mgr.pick_uploads().await.unwrap();
            let up = mgr
                .enqueue_upload(
                    "studio".into(),
                    "/home/me/Uploads".into(),
                    Some(vec![picked[0].handle.clone()]),
                )
                .await
                .unwrap()
                .unwrap();
            let done = wait_for_state(&mgr, &up.id, |s| s == TransferState::Done).await;
            assert_eq!(done.path, "/home/me/Uploads/raced (2).txt");
            assert_eq!(done.name, "raced (2).txt");
            assert_eq!(
                studio_files
                    .lock()
                    .unwrap()
                    .get("/home/me/Uploads/raced.txt")
                    .map(Vec::as_slice),
                Some(b"raced-on-studio-between-stat-and-put".as_slice()),
                "raced file on studio must remain untouched"
            );
            assert_eq!(
                studio_uploaded
                    .lock()
                    .unwrap()
                    .get("/home/me/Uploads/raced (2).txt")
                    .map(Vec::as_slice),
                Some(payload.as_slice())
            );
            let reqs = seen_inner_puts.lock().unwrap();
            assert_eq!(reqs.len(), 2, "expected Exists refusal then numbered retry");
            assert!(reqs.iter().all(|r| r["if_exists"] == json!("fail")));
            mgr.shutdown();
        }
    }

    #[tokio::test]
    async fn relayed_download_upload_and_open_externally_then_learn_switches_to_direct() {
        let dir = tempfile::tempdir().unwrap();
        let sample_download: Vec<u8> = (0..40_000usize)
            .map(|i| ((i * 3 + 7) % 251) as u8)
            .collect();
        let sample_upload: Vec<u8> = (0..32_000usize)
            .map(|i| ((i * 5 + 1) % 251) as u8)
            .collect();
        let upload_src = dir.path().join("notes.bin");
        fs::write(&upload_src, &sample_upload).unwrap();

        let studio_files = Arc::new(Mutex::new(HashMap::from([(
            "/home/me/photo.png".to_owned(),
            sample_download.clone(),
        )])));
        let studio_uploaded = Arc::new(Mutex::new(HashMap::<String, Vec<u8>>::new()));
        let relay_stream_hits = Arc::new(AtomicUsize::new(0));
        let direct_stream_hits = Arc::new(AtomicUsize::new(0));
        let tower_key = legion_link::DeviceKey::from_seed([81u8; 32])
            .public_key_line("tower")
            .unwrap();
        let studio_key = legion_link::DeviceKey::from_seed([82u8; 32])
            .public_key_line("studio")
            .unwrap();

        struct FleetRelayOpener {
            node: String,
            files: Arc<Mutex<HashMap<String, Vec<u8>>>>,
            uploaded: Arc<Mutex<HashMap<String, Vec<u8>>>>,
            relay_hits: Arc<AtomicUsize>,
            direct_hits: Arc<AtomicUsize>,
        }

        impl StreamOpener for FleetRelayOpener {
            fn open_stream(
                &self,
                _cancel: CancellationToken,
            ) -> Pin<Box<dyn Future<Output = Result<RawStream, StreamOpenError>> + Send + '_>>
            {
                let node = self.node.clone();
                let files = Arc::clone(&self.files);
                let uploaded = Arc::clone(&self.uploaded);
                let relay_hits = Arc::clone(&self.relay_hits);
                let direct_hits = Arc::clone(&self.direct_hits);
                Box::pin(async move {
                    let (client_in, mut server_out) = tokio::io::duplex(64 * 1024);
                    let (server_in, client_out) = tokio::io::duplex(64 * 1024);
                    let cancel = CancellationToken::new();
                    tokio::spawn(async move {
                        let _ = server_out
                            .write_all(
                                format!(
                                    "{}\n",
                                    json!({
                                        "protocol": 1,
                                        "capabilities": [legion_link::CAP_STREAM_RELAY]
                                    })
                                )
                                .as_bytes(),
                            )
                            .await;
                        let mut reader = BufReader::new(server_in);
                        let mut line = String::new();
                        if reader.read_line(&mut line).await.unwrap_or(0) == 0 {
                            return;
                        }
                        let req: Value = serde_json::from_str(&line).unwrap();
                        let inner = if node == "tower" && req["op"] == json!("relay") {
                            assert_eq!(req["on"], json!("studio"));
                            relay_hits.fetch_add(1, Ordering::SeqCst);
                            &req["request"]
                        } else {
                            assert_eq!(node, "studio");
                            direct_hits.fetch_add(1, Ordering::SeqCst);
                            &req
                        };
                        match inner["op"].as_str().unwrap_or("") {
                            "get" => {
                                let path = inner["path"].as_str().unwrap_or("");
                                let Some(data) = files.lock().unwrap().get(path).cloned() else {
                                    let err = json!({
                                        "error": ErrorBody::new(ProtoCode::NotFound, "missing")
                                            .with_machine("studio")
                                    });
                                    let _ =
                                        server_out.write_all(format!("{err}\n").as_bytes()).await;
                                    return;
                                };
                                let offset = inner["offset"].as_u64().unwrap_or(0) as usize;
                                let avail = data.len().saturating_sub(offset);
                                let len = inner
                                    .get("length")
                                    .and_then(Value::as_u64)
                                    .map_or(avail, |n| (n as usize).min(avail));
                                let ready = json!({
                                    "size": data.len(),
                                    "offset": offset,
                                    "length": len,
                                    "mtime": { "secs": 1_700_000_000i64, "nanos": 0u32 },
                                    "file_id": 21u64,
                                });
                                let _ = server_out.write_all(format!("{ready}\n").as_bytes()).await;
                                let _ = server_out.write_all(&data[offset..offset + len]).await;
                            }
                            "put" => {
                                let path = inner["path"].as_str().unwrap_or("").to_owned();
                                let size = inner["size"].as_u64().unwrap_or(0) as usize;
                                let hash = inner["hash"].as_str().unwrap_or("").to_owned();
                                let _ = server_out.write_all(b"{\"offset\":0}\n").await;
                                let mut got = vec![0u8; size];
                                reader.read_exact(&mut got).await.unwrap();
                                uploaded.lock().unwrap().insert(path, got);
                                let done = json!({ "ok": true, "size": size, "hash": hash });
                                let _ = server_out.write_all(format!("{done}\n").as_bytes()).await;
                            }
                            _ => {}
                        }
                        let _ = server_out.shutdown().await;
                    });
                    Ok(RawStream {
                        reader: Box::new(client_in),
                        writer: Box::new(client_out),
                        stderr: None,
                        cancel,
                        park: None,
                        guard: None,
                    })
                })
            }
        }

        struct RelayedTransferBackend {
            link: Link,
        }

        impl Backend for RelayedTransferBackend {
            fn state(&self) -> BoxFuture<'_, Result<AppState, AppError>> {
                Box::pin(async { Err(AppError::not_paired()) })
            }
            fn tools(&self, _on: Option<Value>) -> BoxFuture<'_, Result<Vec<Value>, AppError>> {
                Box::pin(async { Err(AppError::not_paired()) })
            }
            fn call(
                &self,
                _request: CallRequest,
                _on_event: Option<tauri::ipc::Channel<Value>>,
            ) -> BoxFuture<'_, Result<Value, AppError>> {
                Box::pin(async { Err(AppError::not_paired()) })
            }
            fn cancel(&self, _id: CallId) -> BoxFuture<'_, Result<CancelResult, AppError>> {
                Box::pin(async { Err(AppError::not_paired()) })
            }
            fn pair(
                &self,
                _invite: String,
                _name: String,
            ) -> BoxFuture<'_, Result<AppState, AppError>> {
                Box::pin(async { Err(AppError::not_paired()) })
            }
            fn unpair(&self, _node: String) -> BoxFuture<'_, Result<UnpairResult, AppError>> {
                Box::pin(async { Err(AppError::not_paired()) })
            }
            fn select_node(&self, _node: String) -> BoxFuture<'_, Result<AppState, AppError>> {
                Box::pin(async { Err(AppError::not_paired()) })
            }
            fn repair_store(&self) -> BoxFuture<'_, Result<RepairResult, AppError>> {
                Box::pin(async { Err(AppError::not_paired()) })
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
            fn dismiss_identity_change(
                &self,
                _node: String,
            ) -> BoxFuture<'_, Result<AppState, AppError>> {
                Box::pin(async { Err(AppError::not_paired()) })
            }
            fn content_target(&self, machine: &str) -> Result<ContentTarget, AppError> {
                let holder = if self.link.config().node(machine).is_some() {
                    format!("pairing:{machine}:1")
                } else {
                    "fleet:1".to_owned()
                };
                Ok(ContentTarget {
                    link: self.link.clone(),
                    link_node: machine.to_owned(),
                    machine: machine.to_owned(),
                    principal: "device:pixel".to_owned(),
                    holder,
                    learned_identity: None,
                })
            }
            fn is_pairing_holder_live(&self, _: &str) -> bool {
                true
            }
        }

        let opener: SessionOpener = {
            let files = Arc::clone(&studio_files);
            let uploaded = Arc::clone(&studio_uploaded);
            let relay_hits = Arc::clone(&relay_stream_hits);
            let direct_hits = Arc::clone(&direct_stream_hits);
            let studio_key = studio_key.clone();
            Arc::new(move |entry, _, cancel| {
                let files = Arc::clone(&files);
                let uploaded = Arc::clone(&uploaded);
                let relay_hits = Arc::clone(&relay_hits);
                let direct_hits = Arc::clone(&direct_hits);
                let studio_key = studio_key.clone();
                Box::pin(async move {
                    let node_name = entry.node.clone();
                    let (client_in, mut server_out) = tokio::io::duplex(16 * 1024);
                    let (server_in, client_out) = tokio::io::duplex(16 * 1024);
                    let node_for_rpc = node_name.clone();
                    let files_rpc = Arc::clone(&files);
                    let uploaded_rpc = Arc::clone(&uploaded);
                    tokio::spawn(async move {
                        let mut reader = BufReader::new(server_in);
                        let mut line = String::new();
                        while reader.read_line(&mut line).await.unwrap_or(0) > 0 {
                            let req: legion_link::proto::Request =
                                serde_json::from_str(&line).unwrap();
                            let reply = if req.method == "hello" {
                                ProtoResp::ok(
                                    req.id,
                                    serde_json::to_value(HelloResult {
                                        node: node_for_rpc.clone(),
                                        node_explicit: None,
                                        node_id: None,
                                        version: VERSION.to_owned(),
                                        build: None,
                                        protocol: PROTOCOL,
                                        os: "linux".to_owned(),
                                        arch: "x86_64".to_owned(),
                                        systems: vec![],
                                        system: None,
                                        boot_id: None,
                                    })
                                    .unwrap(),
                                )
                            } else if req.method == "call" {
                                let tool =
                                    req.params.get("tool").and_then(Value::as_str).unwrap_or("");
                                if tool == "device.endpoint" {
                                    ProtoResp::ok(
                                        req.id,
                                        json!({
                                            "results": {
                                                "studio": {
                                                    "ok": {
                                                        "node": "studio",
                                                        "hosts": ["studio.example.net"],
                                                        "port": 22,
                                                        "user": "me",
                                                        "host_keys": [studio_key],
                                                    }
                                                }
                                            }
                                        }),
                                    )
                                } else {
                                    let path = req
                                        .params
                                        .pointer("/args/path")
                                        .and_then(Value::as_str)
                                        .unwrap_or("");
                                    let data =
                                        files_rpc.lock().unwrap().get(path).cloned().or_else(
                                            || uploaded_rpc.lock().unwrap().get(path).cloned(),
                                        );
                                    if tool == "fs.stat" {
                                        if let Some(d) = data {
                                            ProtoResp::ok(
                                                req.id,
                                                json!({ "kind": "file", "size": d.len() }),
                                            )
                                        } else {
                                            ProtoResp::error(
                                                req.id,
                                                legion_link::proto::RpcError::new(
                                                    -32000,
                                                    ErrorBody::new(
                                                        ProtoCode::NotFound,
                                                        "not found",
                                                    )
                                                    .with_machine("studio"),
                                                ),
                                            )
                                        }
                                    } else if let Some(d) = data {
                                        let h = format!("blake3:{}", blake3::hash(&d).to_hex());
                                        ProtoResp::ok(
                                            req.id,
                                            json!({ "path": path, "size": d.len(), "hash": h }),
                                        )
                                    } else {
                                        ProtoResp::error(
                                            req.id,
                                            legion_link::proto::RpcError::new(
                                                -32000,
                                                ErrorBody::new(ProtoCode::NotFound, "not found")
                                                    .with_machine("studio"),
                                            ),
                                        )
                                    }
                                }
                            } else {
                                ProtoResp::ok(req.id, json!({}))
                            };
                            let _ = server_out
                                .write_all(
                                    format!("{}\n", serde_json::to_string(&reply).unwrap())
                                        .as_bytes(),
                                )
                                .await;
                            line.clear();
                        }
                    });
                    let stream_opener = Arc::new(FleetRelayOpener {
                        node: node_name.clone(),
                        files,
                        uploaded,
                        relay_hits,
                        direct_hits,
                    });
                    Session::open_transport(
                        client_in,
                        client_out,
                        Box::new(FakeTransport {
                            opener: stream_opener,
                        }),
                        &node_name,
                        None,
                        Duration::from_secs(5),
                        cancel,
                    )
                    .await
                    .map_err(|e| e.into_error())
                })
            })
        };

        // Phone is paired ONLY with `tower`; `studio` is reached only through the fleet.
        let link = Link::with_opener(
            LinkConfig {
                device_key: Some("dummy".to_owned()),
                nodes: vec![NodeLink::ssh(
                    "tower",
                    SshTarget {
                        hosts: vec!["tower.example.net".to_owned()],
                        port: 22,
                        user: "me".to_owned(),
                        host_keys: vec![tower_key],
                    },
                    Some("device:pixel".to_owned()),
                )],
                ..LinkConfig::default()
            },
            LinkOptions::default(),
            opener,
        );
        let backend: Arc<dyn Backend> = Arc::new(RelayedTransferBackend { link: link.clone() });
        let platform = Arc::new(MockPlatform::new(dir.path()));
        let mgr = TransferManager::with_options(
            dir.path(),
            backend,
            platform.clone(),
            2,
            Duration::from_secs(2),
        );

        // 1. Download from `studio` via relay `tower`.
        let dl_id = mgr
            .enqueue_download(
                "studio".into(),
                "/home/me/photo.png".into(),
                None,
                Some(sample_download.len() as u64),
                None,
                false,
            )
            .await
            .unwrap();
        let dl_done = wait_for_state(&mgr, &dl_id, |s| s == TransferState::Done).await;
        assert_eq!(
            fs::read(dl_done.local_path.as_deref().unwrap()).unwrap(),
            sample_download
        );

        // 2. Open in another app from `studio` via relay `tower`.
        let open_res = mgr
            .open_externally(
                "studio".into(),
                "/home/me/photo.png".into(),
                Some("photo.png".into()),
                Some(sample_download.len() as u64),
                None,
            )
            .await
            .unwrap();
        let _ = wait_for_state(&mgr, &open_res.id, |s| s == TransferState::Done).await;
        assert_eq!(platform.opened.lock().unwrap().len(), 1);

        // 3. Upload to `studio` via relay `tower`.
        platform
            .picked_files
            .lock()
            .unwrap()
            .push(PickedUploadFile {
                local_path: upload_src.to_string_lossy().into_owned(),
                name: "notes.bin".into(),
                size: sample_upload.len() as u64,
                mime: None,
            });
        let picked = mgr.pick_uploads().await.unwrap();
        let up = mgr
            .enqueue_upload(
                "studio".into(),
                "/home/me/uploads".into(),
                Some(vec![picked[0].handle.clone()]),
            )
            .await
            .unwrap()
            .unwrap();
        let up_done = wait_for_state(&mgr, &up.id, |s| s == TransferState::Done).await;
        assert_eq!(up_done.path, "/home/me/uploads/notes.bin");
        assert_eq!(
            studio_uploaded
                .lock()
                .unwrap()
                .get("/home/me/uploads/notes.bin")
                .unwrap(),
            &sample_upload
        );
        assert!(relay_stream_hits.load(Ordering::SeqCst) >= 3);
        assert_eq!(direct_stream_hits.load(Ordering::SeqCst), 0);

        // 4. Learn `studio` directly -> subsequent download switches to direct!
        let learned = link.learn().await.unwrap();
        assert_eq!(learned.added, vec!["studio".to_owned()]);
        let relay_before = relay_stream_hits.load(Ordering::SeqCst);
        let dl2_id = mgr
            .enqueue_download(
                "studio".into(),
                "/home/me/photo.png".into(),
                None,
                Some(sample_download.len() as u64),
                None,
                false,
            )
            .await
            .unwrap();
        let dl2_done = wait_for_state(&mgr, &dl2_id, |s| s == TransferState::Done).await;
        assert_eq!(
            fs::read(dl2_done.local_path.as_deref().unwrap()).unwrap(),
            sample_download
        );
        assert_eq!(relay_stream_hits.load(Ordering::SeqCst), relay_before);
        assert!(direct_stream_hits.load(Ordering::SeqCst) >= 1);

        mgr.shutdown();
        link.close().await;
    }

    #[tokio::test]
    async fn open_externally_reuses_unchanged_cache_and_redownloads_when_remote_changes() {
        let dir = tempfile::tempdir().unwrap();
        let flac_v1: Vec<u8> = (0..120_000usize).map(|i| (i % 251) as u8).collect();
        let flac_v2: Vec<u8> = (0..180_000usize).map(|i| ((i * 7) % 251) as u8).collect();
        let remote = Arc::new(Mutex::new(MockRemoteState::default()));
        remote
            .lock()
            .unwrap()
            .files
            .insert("/home/me/track.flac".into(), flac_v1.clone());

        let backend: Arc<dyn Backend> = Arc::new(MockBackend::new(Arc::clone(&remote)));
        let platform = Arc::new(MockPlatform::new(dir.path()));
        let mgr = TransferManager::with_options(
            dir.path(),
            backend,
            platform.clone(),
            2,
            Duration::from_secs(2),
        );

        // First open downloads into cache and launches the opener once.
        let first = mgr
            .open_externally(
                "tower".into(),
                "/home/me/track.flac".into(),
                Some("track.flac".into()),
                Some(flac_v1.len() as u64),
                None,
            )
            .await
            .unwrap();
        assert!(first.ok);
        assert!(!first.opened_immediately);
        let done = wait_for_state(&mgr, &first.id, |s| s == TransferState::Done).await;
        assert!(done.open_externally);
        assert!(done.open_error.is_none());
        assert_eq!(platform.finalize_calls.load(Ordering::SeqCst), 1);
        assert_eq!(platform.opened.lock().unwrap().len(), 1);
        assert_eq!(
            platform.opened.lock().unwrap()[0].1.as_deref(),
            Some("audio/flac")
        );

        // Second open of the unchanged file reuses the cache copy immediately without downloading.
        let second = mgr
            .open_externally(
                "tower".into(),
                "/home/me/track.flac".into(),
                Some("track.flac".into()),
                Some(flac_v1.len() as u64),
                None,
            )
            .await
            .unwrap();
        assert!(second.ok);
        assert!(second.opened_immediately);
        assert_eq!(second.id, first.id);
        assert_eq!(platform.finalize_calls.load(Ordering::SeqCst), 1);
        assert_eq!(platform.opened.lock().unwrap().len(), 2);

        // When the remote file changes size, open_externally re-downloads instead of opening stale bytes.
        remote
            .lock()
            .unwrap()
            .files
            .insert("/home/me/track.flac".into(), flac_v2.clone());
        let third = mgr
            .open_externally(
                "tower".into(),
                "/home/me/track.flac".into(),
                Some("track.flac".into()),
                Some(flac_v2.len() as u64),
                None,
            )
            .await
            .unwrap();
        assert!(third.ok);
        assert!(!third.opened_immediately);
        assert_ne!(third.id, first.id);
        let done3 = wait_for_state(&mgr, &third.id, |s| s == TransferState::Done).await;
        assert_eq!(done3.bytes_total, flac_v2.len() as u64);
        assert_eq!(platform.finalize_calls.load(Ordering::SeqCst), 2);
        assert_eq!(platform.opened.lock().unwrap().len(), 3);

        mgr.shutdown();
    }

    #[tokio::test]
    async fn open_externally_surfaces_no_handler_error_and_deduplicates_active_download() {
        let dir = tempfile::tempdir().unwrap();
        let data = vec![0x22u8; 64 * 1024];
        let remote = Arc::new(Mutex::new(MockRemoteState::default()));
        {
            let mut st = remote.lock().unwrap();
            st.files.insert("/home/me/scan.dng".into(), data.clone());
            st.per_chunk_delay_ms = 25;
        }
        let backend: Arc<dyn Backend> = Arc::new(MockBackend::new(Arc::clone(&remote)));
        let platform = Arc::new(MockPlatform::new(dir.path()));
        *platform.open_error_once.lock().unwrap() = Some(
            AppError::new(
                ErrorCode::Unsupported,
                "No app on this phone can open this file type.",
            )
            .with_hint("Download the file to save it in Downloads instead.")
            .with_details(json!({ "reason": "no_handler" })),
        );
        let mgr = TransferManager::with_options(
            dir.path(),
            backend,
            platform.clone(),
            2,
            Duration::from_secs(2),
        );

        // Two rapid taps on the same file share one active download instead of starting two.
        let r1 = mgr
            .open_externally(
                "tower".into(),
                "/home/me/scan.dng".into(),
                Some("scan.dng".into()),
                Some(data.len() as u64),
                None,
            )
            .await
            .unwrap();
        let r2 = mgr
            .open_externally(
                "tower".into(),
                "/home/me/scan.dng".into(),
                Some("scan.dng".into()),
                Some(data.len() as u64),
                None,
            )
            .await
            .unwrap();
        assert_eq!(r1.id, r2.id);

        // When download finishes and opener reports no_handler, open_error is recorded (never swallowed).
        let done = wait_for_state(&mgr, &r1.id, |s| s == TransferState::Done).await;
        let open_err = done
            .open_error
            .expect("open_error must be populated when open_local_file fails");
        assert_eq!(open_err.code, "unsupported");
        assert_eq!(open_err.reason.as_deref(), Some("no_handler"));
        assert!(open_err.message.contains("No app on this phone"));

        // Refused types (e.g. .apk, .svg) are rejected immediately without starting a transfer.
        for refused in ["/home/me/installer.apk", "/home/me/diagram.svg"] {
            let err = mgr
                .open_externally("tower".into(), refused.into(), None, None, None)
                .await
                .unwrap_err();
            assert_eq!(err.code, ErrorCode::Denied, "expected Denied for {refused}");
        }

        mgr.shutdown();
    }

    #[tokio::test]
    async fn direct_upload_checks_destination_when_final_ok_lost_and_retries_unsolicited_cancel() {
        let dir = tempfile::tempdir().unwrap();
        let remote = Arc::new(Mutex::new(MockRemoteState {
            advertise_cap_if_exists: true,
            cancel_after_put_commit: true,
            ..MockRemoteState::default()
        }));
        let backend: Arc<dyn Backend> = Arc::new(MockBackend::new(Arc::clone(&remote)));
        let platform = Arc::new(MockPlatform::new(dir.path()));
        let mgr = TransferManager::with_options(
            dir.path(),
            backend,
            platform.clone(),
            2,
            Duration::from_secs(4),
        );

        // Case A: All bytes are committed and renamed on `tower`, then the stream ends with
        // `cancelled` before the final `ok` reply. Destination verification (`fs.hash` size +
        // hash match) marks the upload `Done`, never `Failed` or duplicated.
        let payload_a: Vec<u8> = (0..180_000).map(|i| (i % 251) as u8).collect();
        let src_a = dir.path().join("committed.bin");
        fs::write(&src_a, &payload_a).unwrap();
        platform
            .picked_files
            .lock()
            .unwrap()
            .push(PickedUploadFile {
                local_path: src_a.to_string_lossy().into_owned(),
                name: "committed.bin".into(),
                size: payload_a.len() as u64,
                mime: None,
            });
        let id_a = mgr
            .enqueue_upload("tower".into(), "/home/me/Uploads".into(), None)
            .await
            .unwrap()
            .unwrap()
            .id;
        let done_a = wait_for_state(&mgr, &id_a, |s| s.is_terminal()).await;
        assert_eq!(
            done_a.state,
            TransferState::Done,
            "committed upload without final ok must verify and finish Done, got: {:?}",
            done_a.error
        );
        assert_eq!(done_a.bytes_done, payload_a.len() as u64);
        assert_eq!(
            remote
                .lock()
                .unwrap()
                .uploaded
                .get("/home/me/Uploads/committed.bin")
                .map(Vec::as_slice),
            Some(payload_a.as_slice())
        );
        assert_eq!(
            remote.lock().unwrap().put_requests.lock().unwrap().len(),
            1,
            "must not re-upload after destination hash matched"
        );

        // Case B: Stream returns an unsolicited `cancelled` mid-upload before commit ->
        // destination check sees missing file, treats unsolicited `cancelled` as retryable,
        // resumes from partial bytes, and finishes `Done`.
        {
            let mut st = remote.lock().unwrap();
            st.cancel_after_put_commit = false;
            st.cancel_mid_put_once = true;
        }
        let payload_b: Vec<u8> = (0..220_000).map(|i| ((i * 7) % 251) as u8).collect();
        let src_b = dir.path().join("resumed.bin");
        fs::write(&src_b, &payload_b).unwrap();
        {
            let mut picked = platform.picked_files.lock().unwrap();
            picked.clear();
            picked.push(PickedUploadFile {
                local_path: src_b.to_string_lossy().into_owned(),
                name: "resumed.bin".into(),
                size: payload_b.len() as u64,
                mime: None,
            });
        }
        let id_b = mgr
            .enqueue_upload("tower".into(), "/home/me/Uploads".into(), None)
            .await
            .unwrap()
            .unwrap()
            .id;
        let done_b = wait_for_state(&mgr, &id_b, |s| s.is_terminal()).await;
        assert_eq!(
            done_b.state,
            TransferState::Done,
            "unsolicited cancelled mid-upload must retry/resume and finish Done, got: {:?}",
            done_b.error
        );
        assert_eq!(
            remote
                .lock()
                .unwrap()
                .uploaded
                .get("/home/me/Uploads/resumed.bin")
                .map(Vec::as_slice),
            Some(payload_b.as_slice())
        );

        mgr.shutdown();
    }

    #[tokio::test]
    async fn relayed_upload_checks_destination_when_relay_dies_before_final_ok_and_retries_unsolicited_cancel()
     {
        use legion_link::proto::{
            ErrorBody, ErrorCode as ProtoCode, HelloResult, PROTOCOL, Response as ProtoResp,
            VERSION,
        };
        use legion_link::{
            CAP_STREAM_RELAY, Link, LinkConfig, LinkOptions, NodeLink, Session, SessionOpener,
            SshTarget,
        };

        let dir = tempfile::tempdir().unwrap();
        let tower_key =
            "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAITowerKeyUploadCheck000000000000000000000000";
        let studio_uploaded: Arc<Mutex<HashMap<String, Vec<u8>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let studio_partials: Arc<Mutex<HashMap<String, Vec<u8>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let cancel_after_commit = Arc::new(AtomicBool::new(true));
        let cancel_mid_once = Arc::new(AtomicBool::new(false));
        let relay_puts = Arc::new(AtomicUsize::new(0));

        struct RelayUploadFaultOpener {
            uploaded: Arc<Mutex<HashMap<String, Vec<u8>>>>,
            partials: Arc<Mutex<HashMap<String, Vec<u8>>>>,
            cancel_after_commit: Arc<AtomicBool>,
            cancel_mid_once: Arc<AtomicBool>,
            relay_puts: Arc<AtomicUsize>,
        }

        impl StreamOpener for RelayUploadFaultOpener {
            fn open_stream(
                &self,
                _cancel: CancellationToken,
            ) -> Pin<Box<dyn Future<Output = Result<RawStream, StreamOpenError>> + Send>>
            {
                let uploaded = Arc::clone(&self.uploaded);
                let partials = Arc::clone(&self.partials);
                let cancel_after_commit = Arc::clone(&self.cancel_after_commit);
                let cancel_mid_once = Arc::clone(&self.cancel_mid_once);
                let relay_puts = Arc::clone(&self.relay_puts);
                Box::pin(async move {
                    let (client_in, mut server_out) = tokio::io::duplex(64 * 1024);
                    let (server_in, client_out) = tokio::io::duplex(64 * 1024);
                    let stream_cancel = CancellationToken::new();
                    tokio::spawn(async move {
                        let greeting = json!({
                            "protocol": 1,
                            "capabilities": [CAP_STREAM_RELAY, CAP_IF_EXISTS],
                        });
                        let _ = server_out
                            .write_all(format!("{greeting}\n").as_bytes())
                            .await;
                        let mut reader = BufReader::new(server_in);
                        let mut line = String::new();
                        if reader.read_line(&mut line).await.unwrap_or(0) == 0 {
                            return;
                        }
                        let outer: Value = serde_json::from_str(&line).unwrap();
                        assert_eq!(outer["op"], json!("relay"));
                        assert_eq!(outer["on"], json!("studio"));
                        let req = outer["request"].clone();
                        assert_eq!(req["op"], json!("put"));
                        relay_puts.fetch_add(1, Ordering::SeqCst);

                        let path = req["path"].as_str().unwrap().to_owned();
                        let size = req["size"].as_u64().unwrap() as usize;
                        let mut got = partials
                            .lock()
                            .unwrap()
                            .get(&path)
                            .cloned()
                            .unwrap_or_default();
                        let offset = got.len();
                        let ready = json!({ "offset": offset });
                        let _ = server_out.write_all(format!("{ready}\n").as_bytes()).await;

                        let abort_mid = cancel_mid_once.swap(false, Ordering::SeqCst);
                        let mut buf = vec![0u8; 16 * 1024];
                        while got.len() < size {
                            match reader.read(&mut buf).await {
                                Ok(0) | Err(_) => break,
                                Ok(n) => {
                                    got.extend_from_slice(&buf[..n]);
                                    if abort_mid && got.len() < size {
                                        partials.lock().unwrap().insert(path, got);
                                        let err = json!({
                                            "error": ErrorBody::new(
                                                ProtoCode::Cancelled,
                                                "The stream stopped because the node is shutting down.",
                                            )
                                            .with_machine("tower")
                                        });
                                        let _ = server_out
                                            .write_all(format!("{err}\n").as_bytes())
                                            .await;
                                        let _ = server_out.shutdown().await;
                                        return;
                                    }
                                }
                            }
                        }
                        if got.len() != size {
                            partials.lock().unwrap().insert(path, got);
                            let _ = server_out.shutdown().await;
                            return;
                        }
                        partials.lock().unwrap().remove(&path);
                        let hash = format!("blake3:{}", blake3::hash(&got).to_hex());
                        uploaded.lock().unwrap().insert(path, got);

                        if cancel_after_commit.load(Ordering::SeqCst) {
                            // Exact reviewer reproduction: all bytes committed and renamed on
                            // `studio`, then relay `tower` terminated before the final `ok`.
                            let err = json!({
                                "error": ErrorBody::new(
                                    ProtoCode::Cancelled,
                                    "The stream stopped because the node is shutting down.",
                                )
                                .with_machine("tower")
                            });
                            let _ = server_out.write_all(format!("{err}\n").as_bytes()).await;
                        } else {
                            let done = json!({ "ok": true, "size": size, "hash": hash });
                            let _ = server_out.write_all(format!("{done}\n").as_bytes()).await;
                        }
                        let _ = server_out.shutdown().await;
                    });
                    Ok(RawStream {
                        reader: Box::new(client_in),
                        writer: Box::new(client_out),
                        stderr: None,
                        cancel: stream_cancel,
                        park: None,
                        guard: None,
                    })
                })
            }
        }

        struct RelayedOnlyBackend {
            link: Link,
        }

        impl Backend for RelayedOnlyBackend {
            fn state(&self) -> BoxFuture<'_, Result<AppState, AppError>> {
                Box::pin(async { Err(AppError::not_paired()) })
            }
            fn tools(&self, _on: Option<Value>) -> BoxFuture<'_, Result<Vec<Value>, AppError>> {
                Box::pin(async { Err(AppError::not_paired()) })
            }
            fn call(
                &self,
                _request: CallRequest,
                _on_event: Option<tauri::ipc::Channel<Value>>,
            ) -> BoxFuture<'_, Result<Value, AppError>> {
                Box::pin(async { Err(AppError::not_paired()) })
            }
            fn cancel(&self, _id: CallId) -> BoxFuture<'_, Result<CancelResult, AppError>> {
                Box::pin(async { Err(AppError::not_paired()) })
            }
            fn pair(
                &self,
                _invite: String,
                _name: String,
            ) -> BoxFuture<'_, Result<AppState, AppError>> {
                Box::pin(async { Err(AppError::not_paired()) })
            }
            fn unpair(&self, _node: String) -> BoxFuture<'_, Result<UnpairResult, AppError>> {
                Box::pin(async { Err(AppError::not_paired()) })
            }
            fn select_node(&self, _node: String) -> BoxFuture<'_, Result<AppState, AppError>> {
                Box::pin(async { Err(AppError::not_paired()) })
            }
            fn repair_store(&self) -> BoxFuture<'_, Result<RepairResult, AppError>> {
                Box::pin(async { Err(AppError::not_paired()) })
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
            fn dismiss_identity_change(
                &self,
                _node: String,
            ) -> BoxFuture<'_, Result<AppState, AppError>> {
                Box::pin(async { Err(AppError::not_paired()) })
            }
            fn content_target(&self, machine: &str) -> Result<ContentTarget, AppError> {
                Ok(ContentTarget {
                    link: self.link.clone(),
                    link_node: machine.to_owned(),
                    machine: machine.to_owned(),
                    principal: "device:pixel".to_owned(),
                    holder: "fleet:1".to_owned(),
                    learned_identity: None,
                })
            }
            fn is_pairing_holder_live(&self, _: &str) -> bool {
                true
            }
        }

        let opener: SessionOpener = {
            let uploaded = Arc::clone(&studio_uploaded);
            let partials = Arc::clone(&studio_partials);
            let cancel_after_commit = Arc::clone(&cancel_after_commit);
            let cancel_mid_once = Arc::clone(&cancel_mid_once);
            let relay_puts = Arc::clone(&relay_puts);
            Arc::new(move |entry, _, cancel| {
                let uploaded = Arc::clone(&uploaded);
                let partials = Arc::clone(&partials);
                let cancel_after_commit = Arc::clone(&cancel_after_commit);
                let cancel_mid_once = Arc::clone(&cancel_mid_once);
                let relay_puts = Arc::clone(&relay_puts);
                Box::pin(async move {
                    let (client_in, mut server_out) = tokio::io::duplex(16 * 1024);
                    let (server_in, client_out) = tokio::io::duplex(16 * 1024);
                    let uploaded_rpc = Arc::clone(&uploaded);
                    tokio::spawn(async move {
                        let mut reader = BufReader::new(server_in);
                        let mut line = String::new();
                        while reader.read_line(&mut line).await.unwrap_or(0) > 0 {
                            let req: legion_link::proto::Request =
                                serde_json::from_str(&line).unwrap();
                            let reply = if req.method == "hello" {
                                ProtoResp::ok(
                                    req.id,
                                    serde_json::to_value(HelloResult {
                                        node: "tower".to_owned(),
                                        node_explicit: None,
                                        node_id: None,
                                        version: VERSION.to_owned(),
                                        build: None,
                                        protocol: PROTOCOL,
                                        os: "linux".to_owned(),
                                        arch: "x86_64".to_owned(),
                                        systems: vec![],
                                        system: None,
                                        boot_id: None,
                                    })
                                    .unwrap(),
                                )
                            } else if req.method == "call" {
                                let tool =
                                    req.params.get("tool").and_then(Value::as_str).unwrap_or("");
                                let path = req
                                    .params
                                    .pointer("/args/path")
                                    .and_then(Value::as_str)
                                    .unwrap_or("");
                                let data = uploaded_rpc.lock().unwrap().get(path).cloned();
                                if tool == "fs.stat" {
                                    if let Some(d) = data {
                                        ProtoResp::ok(
                                            req.id,
                                            json!({ "kind": "file", "size": d.len() }),
                                        )
                                    } else {
                                        ProtoResp::error(
                                            req.id,
                                            legion_link::proto::RpcError::new(
                                                -32000,
                                                ErrorBody::new(ProtoCode::NotFound, "not found")
                                                    .with_machine("studio"),
                                            ),
                                        )
                                    }
                                } else if tool == "fs.hash" {
                                    if let Some(d) = data {
                                        let h = format!("blake3:{}", blake3::hash(&d).to_hex());
                                        ProtoResp::ok(
                                            req.id,
                                            json!({ "path": path, "size": d.len(), "hash": h }),
                                        )
                                    } else {
                                        ProtoResp::error(
                                            req.id,
                                            legion_link::proto::RpcError::new(
                                                -32000,
                                                ErrorBody::new(ProtoCode::NotFound, "not found")
                                                    .with_machine("studio"),
                                            ),
                                        )
                                    }
                                } else {
                                    ProtoResp::ok(req.id, json!({}))
                                }
                            } else {
                                ProtoResp::ok(req.id, json!({}))
                            };
                            let _ = server_out
                                .write_all(
                                    format!("{}\n", serde_json::to_string(&reply).unwrap())
                                        .as_bytes(),
                                )
                                .await;
                            line.clear();
                        }
                    });
                    let stream_opener = Arc::new(RelayUploadFaultOpener {
                        uploaded,
                        partials,
                        cancel_after_commit,
                        cancel_mid_once,
                        relay_puts,
                    });
                    Session::open_transport(
                        client_in,
                        client_out,
                        Box::new(FakeTransport {
                            opener: stream_opener,
                        }),
                        &entry.node,
                        None,
                        Duration::from_secs(5),
                        cancel,
                    )
                    .await
                    .map_err(|e| e.into_error())
                })
            })
        };

        let link = Link::with_opener(
            LinkConfig {
                device_key: Some("dummy".to_owned()),
                nodes: vec![NodeLink::ssh(
                    "tower",
                    SshTarget {
                        hosts: vec!["tower.example.net".to_owned()],
                        port: 22,
                        user: "me".to_owned(),
                        host_keys: vec![tower_key.to_owned()],
                    },
                    Some("device:pixel".to_owned()),
                )],
                ..LinkConfig::default()
            },
            LinkOptions::default(),
            opener,
        );
        let backend: Arc<dyn Backend> = Arc::new(RelayedOnlyBackend { link });
        let platform = Arc::new(MockPlatform::new(dir.path()));
        let mgr = TransferManager::with_options(
            dir.path(),
            backend,
            platform.clone(),
            2,
            Duration::from_secs(4),
        );

        // Case A: Relayed put commits & renames on `studio`, then relay `tower` terminates with
        // `cancelled` before final `ok` -> transfer checks destination on `studio` and reports Done.
        let payload_a: Vec<u8> = (0..180_000).map(|i| ((i * 3) % 251) as u8).collect();
        let src_a = dir.path().join("relayed-committed.bin");
        fs::write(&src_a, &payload_a).unwrap();
        platform
            .picked_files
            .lock()
            .unwrap()
            .push(PickedUploadFile {
                local_path: src_a.to_string_lossy().into_owned(),
                name: "relayed-committed.bin".into(),
                size: payload_a.len() as u64,
                mime: None,
            });
        let id_a = mgr
            .enqueue_upload("studio".into(), "/home/me/Uploads".into(), None)
            .await
            .unwrap()
            .unwrap()
            .id;
        let done_a = wait_for_state(&mgr, &id_a, |s| s.is_terminal()).await;
        assert_eq!(
            done_a.state,
            TransferState::Done,
            "committed relayed upload without final ok must verify and finish Done, got: {:?}",
            done_a.error
        );
        assert_eq!(relay_puts.load(Ordering::SeqCst), 1);
        assert_eq!(
            studio_uploaded
                .lock()
                .unwrap()
                .get("/home/me/Uploads/relayed-committed.bin")
                .map(Vec::as_slice),
            Some(payload_a.as_slice())
        );

        // Case B: Relay shuts down mid-upload before commit (unsolicited `cancelled`) ->
        // destination check sees missing file, worker retries, resumes over relay, and finishes Done.
        cancel_after_commit.store(false, Ordering::SeqCst);
        cancel_mid_once.store(true, Ordering::SeqCst);
        let payload_b: Vec<u8> = (0..220_000).map(|i| ((i * 11) % 251) as u8).collect();
        let src_b = dir.path().join("relayed-resumed.bin");
        fs::write(&src_b, &payload_b).unwrap();
        {
            let mut picked = platform.picked_files.lock().unwrap();
            picked.clear();
            picked.push(PickedUploadFile {
                local_path: src_b.to_string_lossy().into_owned(),
                name: "relayed-resumed.bin".into(),
                size: payload_b.len() as u64,
                mime: None,
            });
        }
        let id_b = mgr
            .enqueue_upload("studio".into(), "/home/me/Uploads".into(), None)
            .await
            .unwrap()
            .unwrap()
            .id;
        let done_b = wait_for_state(&mgr, &id_b, |s| s.is_terminal()).await;
        assert_eq!(
            done_b.state,
            TransferState::Done,
            "relayed upload interrupted by unsolicited relay cancel must resume and finish Done, got: {:?}",
            done_b.error
        );
        assert_eq!(relay_puts.load(Ordering::SeqCst), 3);
        assert_eq!(
            studio_uploaded
                .lock()
                .unwrap()
                .get("/home/me/Uploads/relayed-resumed.bin")
                .map(Vec::as_slice),
            Some(payload_b.as_slice())
        );

        mgr.shutdown();
    }

    #[tokio::test]
    async fn upload_bounds_unsolicited_cancels_without_offset_progress_and_allows_resume() {
        let dir = tempfile::tempdir().unwrap();
        let remote = Arc::new(Mutex::new(MockRemoteState {
            advertise_cap_if_exists: true,
            cancel_mid_put_no_progress: true,
            ..MockRemoteState::default()
        }));
        let backend: Arc<dyn Backend> = Arc::new(MockBackend::new(Arc::clone(&remote)));
        let platform = Arc::new(MockPlatform::new(dir.path()));
        // Long retry window (60s): proves the 3-cancel bound stops the upload promptly rather
        // than relying on the retry window or looping forever.
        let mgr = TransferManager::with_options(
            dir.path(),
            backend,
            platform.clone(),
            2,
            Duration::from_secs(60),
        );

        // Case 1: Large payload (> 64 KiB pipe buffer) where `put.write_all` succeeds locally on
        // the first chunk before hitting BrokenPipe and recovering the peer's `cancelled` reply.
        let payload_large: Vec<u8> = (0..180_000).map(|i| ((i * 5) % 251) as u8).collect();
        let src_large = dir.path().join("bounded-large.bin");
        fs::write(&src_large, &payload_large).unwrap();
        platform
            .picked_files
            .lock()
            .unwrap()
            .push(PickedUploadFile {
                local_path: src_large.to_string_lossy().into_owned(),
                name: "bounded-large.bin".into(),
                size: payload_large.len() as u64,
                mime: None,
            });
        let id_large = mgr
            .enqueue_upload("tower".into(), "/home/me/Uploads".into(), None)
            .await
            .unwrap()
            .unwrap()
            .id;
        let failed_large = wait_for_state(&mgr, &id_large, |s| s.is_terminal()).await;
        assert_eq!(failed_large.state, TransferState::Failed);
        let err_large = failed_large
            .error
            .as_ref()
            .expect("failed upload must carry an error");
        assert_eq!(err_large.code, "cancelled");
        assert!(
            err_large.message.contains("kept being interrupted"),
            "unexpected message: {}",
            err_large.message
        );
        assert_eq!(
            remote.lock().unwrap().put_requests.lock().unwrap().len(),
            MAX_CONSECUTIVE_UNSOLICITED_UPLOAD_CANCELS as usize,
            "must stop after exactly 3 unsolicited cancellations without progress"
        );

        // Case 2: Small payload (< 64 KiB) where `put.write_all` writes the whole file into the
        // local buffer and `put.finish()` receives the peer's `cancelled` reply.
        remote.lock().unwrap().put_requests.lock().unwrap().clear();
        let payload_small: Vec<u8> = (0..8_192).map(|i| ((i * 3) % 251) as u8).collect();
        let src_small = dir.path().join("bounded-small.bin");
        fs::write(&src_small, &payload_small).unwrap();
        {
            let mut picked = platform.picked_files.lock().unwrap();
            picked.clear();
            picked.push(PickedUploadFile {
                local_path: src_small.to_string_lossy().into_owned(),
                name: "bounded-small.bin".into(),
                size: payload_small.len() as u64,
                mime: None,
            });
        }
        let id_small = mgr
            .enqueue_upload("tower".into(), "/home/me/Uploads".into(), None)
            .await
            .unwrap()
            .unwrap()
            .id;
        let failed_small = wait_for_state(&mgr, &id_small, |s| s.is_terminal()).await;
        assert_eq!(failed_small.state, TransferState::Failed);
        let err_small = failed_small
            .error
            .as_ref()
            .expect("failed small upload must carry an error");
        assert_eq!(err_small.code, "cancelled");
        assert!(err_small.message.contains("kept being interrupted"));
        assert_eq!(
            remote.lock().unwrap().put_requests.lock().unwrap().len(),
            MAX_CONSECUTIVE_UNSOLICITED_UPLOAD_CANCELS as usize
        );

        // Once the peer stops cancelling, the user can resume the failed upload cleanly.
        remote.lock().unwrap().cancel_mid_put_no_progress = false;
        mgr.control(&id_large, TransferControlAction::Resume)
            .await
            .unwrap();
        let resumed = wait_for_state(&mgr, &id_large, |s| s.is_terminal()).await;
        assert_eq!(
            resumed.state,
            TransferState::Done,
            "resumed upload must succeed after peer recovers, got: {:?}",
            resumed.error
        );
        assert_eq!(
            remote
                .lock()
                .unwrap()
                .uploaded
                .get("/home/me/Uploads/bounded-large.bin")
                .map(Vec::as_slice),
            Some(payload_large.as_slice())
        );

        mgr.shutdown();
    }

    #[tokio::test]
    async fn upload_resets_cancel_bound_on_offset_advance_and_expires_retry_window_without_progress()
     {
        let dir = tempfile::tempdir().unwrap();
        let remote = Arc::new(Mutex::new(MockRemoteState {
            advertise_cap_if_exists: true,
            // Four consecutive unsolicited cancellations, each saving a partial chunk so
            // `put.offset()` genuinely advances across attempts.
            cancel_mid_put_with_progress_remaining: 4,
            ..MockRemoteState::default()
        }));
        let backend: Arc<dyn Backend> = Arc::new(MockBackend::new(Arc::clone(&remote)));
        let platform = Arc::new(MockPlatform::new(dir.path()));
        // Short retry window (2s) verifies BOTH:
        //  A) advancing `put.offset()` resets both the 3-cancel bound AND the retry window
        //     (4 interruptions take ~3s total yet still succeed), and
        //  B) repeated transport drops with local `write_all` success but NO `put.offset()`
        //     advance do NOT reset the retry window and fail promptly instead of looping forever.
        let mgr = TransferManager::with_options(
            dir.path(),
            backend,
            platform.clone(),
            2,
            Duration::from_secs(2),
        );

        // Part A: 4 unsolicited cancellations WITH remote offset progress -> resets on each
        // acknowledged offset advance and completes `Done`.
        let payload_prog: Vec<u8> = (0..220_000).map(|i| ((i * 13) % 251) as u8).collect();
        let src_prog = dir.path().join("progress-resets.bin");
        fs::write(&src_prog, &payload_prog).unwrap();
        platform
            .picked_files
            .lock()
            .unwrap()
            .push(PickedUploadFile {
                local_path: src_prog.to_string_lossy().into_owned(),
                name: "progress-resets.bin".into(),
                size: payload_prog.len() as u64,
                mime: None,
            });
        let id_prog = mgr
            .enqueue_upload("tower".into(), "/home/me/Uploads".into(), None)
            .await
            .unwrap()
            .unwrap()
            .id;
        let done_prog = wait_for_state(&mgr, &id_prog, |s| s.is_terminal()).await;
        assert_eq!(
            done_prog.state,
            TransferState::Done,
            "4 cancellations with advancing put.offset() must reset counter & window and finish Done, got: {:?}",
            done_prog.error
        );
        assert_eq!(
            remote.lock().unwrap().put_requests.lock().unwrap().len(),
            5,
            "4 interrupted attempts + 1 final attempt"
        );
        assert_eq!(
            remote
                .lock()
                .unwrap()
                .uploaded
                .get("/home/me/Uploads/progress-resets.bin")
                .map(Vec::as_slice),
            Some(payload_prog.as_slice())
        );

        // Part B: Abrupt EOF drops with local `write_all` success and `put.offset() == 0`
        // must NOT reset `retry_started` and must fail once the 2s retry window elapses.
        {
            let mut st = remote.lock().unwrap();
            st.put_requests.lock().unwrap().clear();
            st.drop_mid_put_no_progress = true;
        }
        let payload_drop: Vec<u8> = (0..180_000).map(|i| ((i * 17) % 251) as u8).collect();
        let src_drop = dir.path().join("drop-no-progress.bin");
        fs::write(&src_drop, &payload_drop).unwrap();
        {
            let mut picked = platform.picked_files.lock().unwrap();
            picked.clear();
            picked.push(PickedUploadFile {
                local_path: src_drop.to_string_lossy().into_owned(),
                name: "drop-no-progress.bin".into(),
                size: payload_drop.len() as u64,
                mime: None,
            });
        }
        let id_drop = mgr
            .enqueue_upload("tower".into(), "/home/me/Uploads".into(), None)
            .await
            .unwrap()
            .unwrap()
            .id;
        let failed_drop = wait_for_state(&mgr, &id_drop, |s| s.is_terminal()).await;
        assert_eq!(
            failed_drop.state,
            TransferState::Failed,
            "abrupt drops without put.offset() progress must hit retry window and fail"
        );
        assert_eq!(
            failed_drop.error.as_ref().map(|e| e.code.as_str()),
            Some("unreachable")
        );

        mgr.shutdown();
    }

    #[cfg(all(unix, not(any(target_os = "android", target_os = "ios"))))]
    #[tokio::test]
    async fn real_binary_uploads_atomic_if_exists_direct_relayed_and_older_stat_fallback() {
        use std::os::unix::fs::PermissionsExt;

        const TEST_NAME: &str = "transfers::tests::real_binary_uploads_atomic_if_exists_direct_relayed_and_older_stat_fallback";

        let Some(legion_bin) = crate::test_support::real_legion_bin(TEST_NAME) else {
            return;
        };

        fn write_wrapper(
            path: &Path,
            bin: &Path,
            legion_home: &Path,
            user_home: &Path,
            race_on_stream: Option<(&Path, &Path, &str)>,
        ) {
            let race_block = match race_on_stream {
                Some((trigger, target, content)) => format!(
                    "if [ \"$1\" = \"stream\" ] && [ -f '{}' ]; then\n  rm -f '{}'\n  printf '%s' '{}' > '{}'\nfi\n",
                    trigger.display(),
                    trigger.display(),
                    content,
                    target.display(),
                ),
                None => String::new(),
            };
            let script = format!(
                "#!/bin/sh\nexport LEGION_HOME='{}'\nexport HOME='{}'\nunset XDG_CONFIG_HOME XDG_STATE_HOME\n{}exec '{}' \"$@\"\n",
                legion_home.display(),
                user_home.display(),
                race_block,
                bin.display(),
            );
            fs::write(path, script).unwrap();
            fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
        }

        fn count_audit_fs_stat(legion_home: &Path) -> usize {
            let audit = legion_home.join("state/audit.jsonl");
            let Ok(text) = fs::read_to_string(&audit) else {
                return 0;
            };
            text.lines()
                .filter(|line| line.contains("\"fs.stat\""))
                .count()
        }

        struct LocalBinaryBackend {
            link: Link,
        }

        impl Backend for LocalBinaryBackend {
            fn state(&self) -> BoxFuture<'_, Result<AppState, AppError>> {
                Box::pin(async { Err(AppError::not_paired()) })
            }
            fn tools(&self, _on: Option<Value>) -> BoxFuture<'_, Result<Vec<Value>, AppError>> {
                Box::pin(async { Err(AppError::not_paired()) })
            }
            fn call(
                &self,
                _request: CallRequest,
                _on_event: Option<tauri::ipc::Channel<Value>>,
            ) -> BoxFuture<'_, Result<Value, AppError>> {
                Box::pin(async { Err(AppError::not_paired()) })
            }
            fn cancel(&self, _id: CallId) -> BoxFuture<'_, Result<CancelResult, AppError>> {
                Box::pin(async { Err(AppError::not_paired()) })
            }
            fn pair(
                &self,
                _invite: String,
                _name: String,
            ) -> BoxFuture<'_, Result<AppState, AppError>> {
                Box::pin(async { Err(AppError::not_paired()) })
            }
            fn unpair(&self, _node: String) -> BoxFuture<'_, Result<UnpairResult, AppError>> {
                Box::pin(async { Err(AppError::not_paired()) })
            }
            fn select_node(&self, _node: String) -> BoxFuture<'_, Result<AppState, AppError>> {
                Box::pin(async { Err(AppError::not_paired()) })
            }
            fn repair_store(&self) -> BoxFuture<'_, Result<RepairResult, AppError>> {
                Box::pin(async { Err(AppError::not_paired()) })
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
            fn dismiss_identity_change(
                &self,
                _node: String,
            ) -> BoxFuture<'_, Result<AppState, AppError>> {
                Box::pin(async { Err(AppError::not_paired()) })
            }
            fn content_target(&self, machine: &str) -> Result<ContentTarget, AppError> {
                Ok(ContentTarget {
                    link: self.link.clone(),
                    link_node: machine.to_owned(),
                    machine: machine.to_owned(),
                    principal: "owner".to_owned(),
                    holder: "local:test".to_owned(),
                    learned_identity: None,
                })
            }
            fn is_pairing_holder_live(&self, _: &str) -> bool {
                true
            }
        }

        // Part 1: Current `legion` binary — direct AND relayed uploads raced between `fs.stat`
        // and `put` are refused atomically by `"if_exists": "fail"`.
        {
            let root = tempfile::tempdir().unwrap();
            let home_a = root.path().join("legion-a");
            let home_b = root.path().join("legion-b");
            let user_a = root.path().join("user-a");
            let user_b = root.path().join("user-b");
            let uploads_a = user_a.join("uploads");
            let uploads_b = user_b.join("uploads");
            for d in [
                home_a.join("config"),
                home_b.join("config"),
                uploads_a.clone(),
                uploads_b.clone(),
            ] {
                fs::create_dir_all(&d).unwrap();
            }
            let trigger_a = root.path().join("trigger-a");
            let trigger_b = root.path().join("trigger-b");
            fs::write(&trigger_a, b"arm").unwrap();
            fs::write(&trigger_b, b"arm").unwrap();

            let wrap_a = root.path().join("tower.sh");
            let wrap_b = root.path().join("pi.sh");
            write_wrapper(
                &wrap_a,
                &legion_bin,
                &home_a,
                &user_a,
                Some((
                    &trigger_a,
                    &uploads_a.join("raced.txt"),
                    "created-between-stat-and-put-direct",
                )),
            );
            write_wrapper(
                &wrap_b,
                &legion_bin,
                &home_b,
                &user_b,
                Some((
                    &trigger_b,
                    &uploads_b.join("raced-relayed.txt"),
                    "created-between-stat-and-put-relayed",
                )),
            );

            fs::write(home_a.join("config/node.toml"), "name = \"tower\"\n").unwrap();
            fs::write(home_b.join("config/node.toml"), "name = \"pi\"\n").unwrap();
            fs::write(home_a.join("config/policy.toml"), "default = \"owner\"\n").unwrap();
            fs::write(home_b.join("config/policy.toml"), "default = \"owner\"\n").unwrap();
            fs::write(
                home_a.join("config/fleet.toml"),
                format!(
                    "[machines.tower]\nssh = \"127.0.0.1\"\n\n[machines.pi]\ncommand = ['{}', \"rpc\"]\n",
                    wrap_b.display()
                ),
            )
            .unwrap();
            fs::write(
                home_b.join("config/fleet.toml"),
                format!(
                    "[machines.pi]\nssh = \"127.0.0.1\"\n\n[machines.tower]\ncommand = ['{}', \"rpc\"]\n",
                    wrap_a.display()
                ),
            )
            .unwrap();

            let link = Link::new(LinkConfig {
                nodes: vec![NodeLink::local("tower", wrap_a.to_string_lossy())],
                ..LinkConfig::default()
            });
            let backend: Arc<dyn Backend> = Arc::new(LocalBinaryBackend { link: link.clone() });
            let mgr_dir = root.path().join("mgr-current");
            fs::create_dir_all(&mgr_dir).unwrap();
            let platform = Arc::new(MockPlatform::new(&mgr_dir));
            let mgr = TransferManager::with_options(
                &mgr_dir,
                backend,
                platform.clone(),
                2,
                Duration::from_secs(5),
            );

            // Direct upload #1: `fs.stat` sees NotFound, then `tower.sh` creates `raced.txt` right
            // as `legion stream` opens -> `"if_exists": "fail"` refuses atomically -> `raced (2).txt`.
            let src1 = root.path().join("src1.txt");
            fs::write(&src1, b"new-direct-upload-1").unwrap();
            platform
                .picked_files
                .lock()
                .unwrap()
                .push(PickedUploadFile {
                    local_path: src1.to_string_lossy().into_owned(),
                    name: "raced.txt".into(),
                    size: 19,
                    mime: None,
                });
            let p1 = mgr.pick_uploads().await.unwrap();
            let u1 = mgr
                .enqueue_upload(
                    "tower".into(),
                    uploads_a.to_string_lossy().into_owned(),
                    Some(vec![p1[0].handle.clone()]),
                )
                .await
                .unwrap()
                .unwrap();
            let d1 = wait_for_state(&mgr, &u1.id, |s| s.is_terminal()).await;
            assert_eq!(d1.state, TransferState::Done, "err={:?}", d1.error);
            assert_eq!(d1.name, "raced (2).txt");
            assert!(
                !trigger_a.exists(),
                "race trigger must have fired between fs.stat and stream put"
            );
            assert_eq!(
                fs::read(uploads_a.join("raced.txt")).unwrap(),
                b"created-between-stat-and-put-direct"
            );
            assert_eq!(
                fs::read(uploads_a.join("raced (2).txt")).unwrap(),
                b"new-direct-upload-1"
            );
            assert_eq!(link.stream_supports("tower", CAP_IF_EXISTS), Some(true));
            let stats_after_first = count_audit_fs_stat(&home_a);
            assert!(
                stats_after_first >= 1,
                "first upload before stream greeting probes fs.stat"
            );

            // Direct upload #2: CAP_IF_EXISTS is cached -> zero new fs.stat calls, still refuses
            // pre-existing file atomically via `"if_exists": "fail"`.
            fs::write(uploads_a.join("second.txt"), b"keep-second").unwrap();
            let src2 = root.path().join("src2.txt");
            fs::write(&src2, b"new-direct-upload-2").unwrap();
            {
                let mut picked = platform.picked_files.lock().unwrap();
                picked.clear();
                picked.push(PickedUploadFile {
                    local_path: src2.to_string_lossy().into_owned(),
                    name: "second.txt".into(),
                    size: 19,
                    mime: None,
                });
            }
            let p2 = mgr.pick_uploads().await.unwrap();
            let u2 = mgr
                .enqueue_upload(
                    "tower".into(),
                    uploads_a.to_string_lossy().into_owned(),
                    Some(vec![p2[0].handle.clone()]),
                )
                .await
                .unwrap()
                .unwrap();
            let d2 = wait_for_state(&mgr, &u2.id, |s| s.is_terminal()).await;
            assert_eq!(d2.state, TransferState::Done, "err={:?}", d2.error);
            assert_eq!(d2.name, "second (2).txt");
            assert_eq!(
                count_audit_fs_stat(&home_a),
                stats_after_first,
                "upload #2 must skip fs.stat once CAP_IF_EXISTS is cached"
            );
            assert_eq!(
                fs::read(uploads_a.join("second.txt")).unwrap(),
                b"keep-second"
            );
            assert_eq!(
                fs::read(uploads_a.join("second (2).txt")).unwrap(),
                b"new-direct-upload-2"
            );

            // Relayed upload to `pi` via `tower`: `fs.stat` sees NotFound, then `pi.sh` creates
            // `raced-relayed.txt` right as `legion stream` opens on `pi` -> atomic `if_exists: "fail"`.
            let src3 = root.path().join("src3.txt");
            fs::write(&src3, b"new-relayed-upload").unwrap();
            {
                let mut picked = platform.picked_files.lock().unwrap();
                picked.clear();
                picked.push(PickedUploadFile {
                    local_path: src3.to_string_lossy().into_owned(),
                    name: "raced-relayed.txt".into(),
                    size: 18,
                    mime: None,
                });
            }
            let p3 = mgr.pick_uploads().await.unwrap();
            let u3 = mgr
                .enqueue_upload(
                    "pi".into(),
                    uploads_b.to_string_lossy().into_owned(),
                    Some(vec![p3[0].handle.clone()]),
                )
                .await
                .unwrap()
                .unwrap();
            let d3 = wait_for_state(&mgr, &u3.id, |s| s.is_terminal()).await;
            assert_eq!(d3.state, TransferState::Done, "err={:?}", d3.error);
            assert_eq!(d3.name, "raced-relayed (2).txt");
            assert!(
                !trigger_b.exists(),
                "relayed race trigger must have fired between fs.stat and stream put on pi"
            );
            assert_eq!(
                fs::read(uploads_b.join("raced-relayed.txt")).unwrap(),
                b"created-between-stat-and-put-relayed"
            );
            assert_eq!(
                fs::read(uploads_b.join("raced-relayed (2).txt")).unwrap(),
                b"new-relayed-upload"
            );

            mgr.shutdown();
            link.close().await;
        }

        // Part 2: Optional real legacy node check (e.g. `LEGION_OLD_BIN` built from `d1ce1b49`),
        // proving the `fs.stat` fallback still works cleanly when `if_exists` is absent.
        if let Ok(old_bin_str) = std::env::var("LEGION_OLD_BIN") {
            let old_bin = PathBuf::from(&old_bin_str);
            assert!(
                old_bin.is_file(),
                "LEGION_OLD_BIN does not exist: {}",
                old_bin.display()
            );
            let root = tempfile::tempdir().unwrap();
            let home_old = root.path().join("legion-old");
            let user_old = root.path().join("user-old");
            let uploads_old = user_old.join("uploads");
            fs::create_dir_all(home_old.join("config")).unwrap();
            fs::create_dir_all(&uploads_old).unwrap();
            fs::write(home_old.join("config/node.toml"), "name = \"oldnode\"\n").unwrap();
            fs::write(home_old.join("config/policy.toml"), "default = \"owner\"\n").unwrap();
            let wrap_old = root.path().join("oldnode.sh");
            write_wrapper(&wrap_old, &old_bin, &home_old, &user_old, None);

            fs::write(uploads_old.join("legacy.txt"), b"keep-legacy-1").unwrap();
            fs::write(uploads_old.join("legacy2.txt"), b"keep-legacy-2").unwrap();

            let link = Link::new(LinkConfig {
                nodes: vec![NodeLink::local("oldnode", wrap_old.to_string_lossy())],
                ..LinkConfig::default()
            });
            let backend: Arc<dyn Backend> = Arc::new(LocalBinaryBackend { link: link.clone() });
            let mgr_dir = root.path().join("mgr-old");
            fs::create_dir_all(&mgr_dir).unwrap();
            let platform = Arc::new(MockPlatform::new(&mgr_dir));
            let mgr = TransferManager::with_options(
                &mgr_dir,
                backend,
                platform.clone(),
                2,
                Duration::from_secs(5),
            );

            // Upload #1 onto pre-existing `legacy.txt`: fs.stat picks `legacy (2).txt`, and
            // greeting from `d1ce1b49` caches `CAP_IF_EXISTS == Some(false)`.
            let src1 = root.path().join("legacy-src1.txt");
            fs::write(&src1, b"uploaded-legacy-1").unwrap();
            platform
                .picked_files
                .lock()
                .unwrap()
                .push(PickedUploadFile {
                    local_path: src1.to_string_lossy().into_owned(),
                    name: "legacy.txt".into(),
                    size: 17,
                    mime: None,
                });
            let p1 = mgr.pick_uploads().await.unwrap();
            let u1 = mgr
                .enqueue_upload(
                    "oldnode".into(),
                    uploads_old.to_string_lossy().into_owned(),
                    Some(vec![p1[0].handle.clone()]),
                )
                .await
                .unwrap()
                .unwrap();
            let d1 = wait_for_state(&mgr, &u1.id, |s| s.is_terminal()).await;
            assert_eq!(d1.state, TransferState::Done, "err={:?}", d1.error);
            assert_eq!(d1.name, "legacy (2).txt");
            assert_eq!(
                fs::read(uploads_old.join("legacy.txt")).unwrap(),
                b"keep-legacy-1"
            );
            assert_eq!(
                fs::read(uploads_old.join("legacy (2).txt")).unwrap(),
                b"uploaded-legacy-1"
            );
            assert_eq!(
                link.stream_supports("oldnode", CAP_IF_EXISTS),
                Some(false),
                "d1ce1b49 node must omit if_exists from greeting"
            );
            let old_stats_after_first = count_audit_fs_stat(&home_old);

            // Upload #2 onto pre-existing `legacy2.txt`: continues using fs.stat fallback cleanly.
            let src2 = root.path().join("legacy-src2.txt");
            fs::write(&src2, b"uploaded-legacy-2").unwrap();
            {
                let mut picked = platform.picked_files.lock().unwrap();
                picked.clear();
                picked.push(PickedUploadFile {
                    local_path: src2.to_string_lossy().into_owned(),
                    name: "legacy2.txt".into(),
                    size: 17,
                    mime: None,
                });
            }
            let p2 = mgr.pick_uploads().await.unwrap();
            let u2 = mgr
                .enqueue_upload(
                    "oldnode".into(),
                    uploads_old.to_string_lossy().into_owned(),
                    Some(vec![p2[0].handle.clone()]),
                )
                .await
                .unwrap()
                .unwrap();
            let d2 = wait_for_state(&mgr, &u2.id, |s| s.is_terminal()).await;
            assert_eq!(d2.state, TransferState::Done, "err={:?}", d2.error);
            assert_eq!(d2.name, "legacy2 (2).txt");
            assert!(
                count_audit_fs_stat(&home_old) >= old_stats_after_first + 2,
                "older node must keep fs.stat collision fallback"
            );
            assert_eq!(
                fs::read(uploads_old.join("legacy2.txt")).unwrap(),
                b"keep-legacy-2"
            );
            assert_eq!(
                fs::read(uploads_old.join("legacy2 (2).txt")).unwrap(),
                b"uploaded-legacy-2"
            );

            mgr.shutdown();
            link.close().await;
        }
    }

    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    #[tokio::test]
    async fn updater_close_path_tears_down_relayed_stream_and_resumes_or_verifies_relayed_uploads_on_restart()
     {
        use crate::backend::NodeRouteIdentity;
        use crate::content::{AppContentServer, ContentMountMode};
        use legion_content::TicketConfig;
        use legion_link::proto::{
            ErrorBody, ErrorCode as ProtoCode, HelloResult, PROTOCOL, Response as ProtoResp,
            RpcError, VERSION,
        };
        use legion_link::{
            CAP_STREAM_RELAY, Link, LinkConfig, LinkOptions, NodeLink, NodeOrigin, Session,
            SessionOpener, SshTarget,
        };
        use tokio::net::TcpSocket;

        let dir = tempfile::tempdir().unwrap();
        let tower_key =
            "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAITowerKeyUpdaterClose0000000000000000000000000";
        let studio_media: Arc<Vec<u8>> =
            Arc::new((0..512 * 1024).map(|i| ((i * 29) % 251) as u8).collect());
        let studio_uploaded: Arc<Mutex<HashMap<String, Vec<u8>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let studio_partials: Arc<Mutex<HashMap<String, Vec<u8>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let observed_put_offsets: Arc<Mutex<Vec<usize>>> = Arc::new(Mutex::new(Vec::new()));
        let live_relayed_streams = Arc::new(AtomicUsize::new(0));
        let hold_get_stream = Arc::new(AtomicBool::new(true));
        let hold_put_mid = Arc::new(AtomicBool::new(false));
        let hold_put_after_commit = Arc::new(AtomicBool::new(false));
        let put_mid_entered = Arc::new(tokio::sync::Notify::new());
        let put_committed_entered = Arc::new(tokio::sync::Notify::new());
        let invalidations = Arc::new(AtomicUsize::new(0));

        struct LiveStreamDropGuard(Arc<AtomicUsize>);
        impl Drop for LiveStreamDropGuard {
            fn drop(&mut self) {
                self.0.fetch_sub(1, Ordering::SeqCst);
            }
        }

        struct UpdaterCloseRelayOpener {
            media: Arc<Vec<u8>>,
            uploaded: Arc<Mutex<HashMap<String, Vec<u8>>>>,
            partials: Arc<Mutex<HashMap<String, Vec<u8>>>>,
            put_offsets: Arc<Mutex<Vec<usize>>>,
            live_streams: Arc<AtomicUsize>,
            hold_get: Arc<AtomicBool>,
            hold_put_mid: Arc<AtomicBool>,
            hold_put_after_commit: Arc<AtomicBool>,
            put_mid_entered: Arc<tokio::sync::Notify>,
            put_committed_entered: Arc<tokio::sync::Notify>,
        }

        impl StreamOpener for UpdaterCloseRelayOpener {
            fn open_stream(
                &self,
                _cancel: CancellationToken,
            ) -> Pin<Box<dyn Future<Output = Result<RawStream, StreamOpenError>> + Send>>
            {
                let media = Arc::clone(&self.media);
                let uploaded = Arc::clone(&self.uploaded);
                let partials = Arc::clone(&self.partials);
                let put_offsets = Arc::clone(&self.put_offsets);
                let live_streams = Arc::clone(&self.live_streams);
                let hold_get = Arc::clone(&self.hold_get);
                let hold_put_mid = Arc::clone(&self.hold_put_mid);
                let hold_put_after_commit = Arc::clone(&self.hold_put_after_commit);
                let put_mid_entered = Arc::clone(&self.put_mid_entered);
                let put_committed_entered = Arc::clone(&self.put_committed_entered);
                Box::pin(async move {
                    let (client_in, mut server_out) = tokio::io::duplex(64 * 1024);
                    let (server_in, client_out) = tokio::io::duplex(64 * 1024);
                    let stream_cancel = CancellationToken::new();
                    let watch_cancel = stream_cancel.clone();
                    tokio::spawn(async move {
                        let _guard = LiveStreamDropGuard(Arc::clone(&live_streams));
                        live_streams.fetch_add(1, Ordering::SeqCst);
                        let greeting = json!({
                            "protocol": 1,
                            "capabilities": [CAP_STREAM_RELAY, CAP_IF_EXISTS],
                        });
                        if server_out
                            .write_all(format!("{greeting}\n").as_bytes())
                            .await
                            .is_err()
                        {
                            return;
                        }
                        let mut reader = BufReader::new(server_in);
                        let mut line = String::new();
                        if reader.read_line(&mut line).await.unwrap_or(0) == 0 {
                            return;
                        }
                        let outer: Value = serde_json::from_str(&line).unwrap();
                        assert_eq!(outer["op"], json!("relay"));
                        assert_eq!(outer["on"], json!("studio"));
                        let req = outer["request"].clone();
                        match req["op"].as_str().unwrap_or("") {
                            "get" => {
                                let offset = req["offset"].as_u64().unwrap_or(0) as usize;
                                let req_len = req
                                    .get("length")
                                    .and_then(Value::as_u64)
                                    .map(|n| n as usize);
                                let avail = media.len().saturating_sub(offset);
                                let len = req_len.map_or(avail, |n| n.min(avail));
                                let ready = json!({
                                    "size": media.len(),
                                    "offset": offset,
                                    "length": len,
                                    "mtime": { "secs": 1_700_000_000i64, "nanos": 0u32 },
                                    "file_id": 77u64,
                                });
                                if server_out
                                    .write_all(format!("{ready}\n").as_bytes())
                                    .await
                                    .is_err()
                                {
                                    return;
                                }
                                if len == 0 {
                                    return;
                                }
                                let first = (offset + 32 * 1024).min(offset + len);
                                if server_out.write_all(&media[offset..first]).await.is_err() {
                                    return;
                                }
                                if hold_get.load(Ordering::SeqCst) {
                                    tokio::select! {
                                        () = watch_cancel.cancelled() => return,
                                        () = tokio::time::sleep(Duration::from_secs(30)) => return,
                                    }
                                }
                                let _ = server_out.write_all(&media[first..offset + len]).await;
                            }
                            "put" => {
                                let path = req["path"].as_str().unwrap().to_owned();
                                let size = req["size"].as_u64().unwrap() as usize;
                                let fail_if_exists =
                                    req.get("if_exists").and_then(Value::as_str) == Some("fail");
                                if fail_if_exists && uploaded.lock().unwrap().contains_key(&path) {
                                    let err = json!({
                                        "error": ErrorBody::new(
                                            ProtoCode::Exists,
                                            format!("{path} already exists."),
                                        )
                                    });
                                    let _ =
                                        server_out.write_all(format!("{err}\n").as_bytes()).await;
                                    return;
                                }
                                let mut got = partials
                                    .lock()
                                    .unwrap()
                                    .get(&path)
                                    .cloned()
                                    .unwrap_or_default();
                                let offset = got.len();
                                put_offsets.lock().unwrap().push(offset);
                                let ready = json!({ "offset": offset });
                                if server_out
                                    .write_all(format!("{ready}\n").as_bytes())
                                    .await
                                    .is_err()
                                {
                                    return;
                                }
                                let pause_mid = hold_put_mid.swap(false, Ordering::SeqCst);
                                let mut buf = vec![0u8; 16 * 1024];
                                while got.len() < size {
                                    tokio::select! {
                                        biased;
                                        () = watch_cancel.cancelled() => {
                                            partials.lock().unwrap().insert(path, got);
                                            return;
                                        }
                                        res = reader.read(&mut buf) => match res {
                                            Ok(0) | Err(_) => {
                                                partials.lock().unwrap().insert(path, got);
                                                return;
                                            }
                                            Ok(n) => {
                                                got.extend_from_slice(&buf[..n]);
                                                if pause_mid && got.len() >= 32 * 1024 && got.len() < size {
                                                    partials.lock().unwrap().insert(path.clone(), got.clone());
                                                    put_mid_entered.notify_waiters();
                                                    // Hold open until updater close cancels the
                                                    // stream (mirroring a mid-transfer updater exit).
                                                    tokio::select! {
                                                        () = watch_cancel.cancelled() => return,
                                                        () = tokio::time::sleep(Duration::from_secs(30)) => return,
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                                partials.lock().unwrap().remove(&path);
                                let hash = format!("blake3:{}", blake3::hash(&got).to_hex());
                                uploaded.lock().unwrap().insert(path, got);
                                if hold_put_after_commit.swap(false, Ordering::SeqCst) {
                                    // Committed and renamed on target `studio`, then held right
                                    // before sending the final `ok` so the updater's close path
                                    // interrupts the app with a lost final reply.
                                    put_committed_entered.notify_waiters();
                                    tokio::select! {
                                        () = watch_cancel.cancelled() => return,
                                        () = tokio::time::sleep(Duration::from_secs(30)) => return,
                                    }
                                }
                                let done = json!({ "ok": true, "size": size, "hash": hash });
                                let _ = server_out.write_all(format!("{done}\n").as_bytes()).await;
                            }
                            other => panic!("unexpected relayed stream op: {other}"),
                        }
                    });
                    Ok(RawStream {
                        reader: Box::new(client_in),
                        writer: Box::new(client_out),
                        stderr: None,
                        cancel: stream_cancel,
                        park: None,
                        guard: None,
                    })
                })
            }
        }

        struct UpdaterCloseRelayBackend {
            link: Link,
            invalidations: Arc<AtomicUsize>,
        }

        impl Backend for UpdaterCloseRelayBackend {
            fn state(&self) -> BoxFuture<'_, Result<AppState, AppError>> {
                Box::pin(async { Err(AppError::not_paired()) })
            }
            fn tools(&self, _on: Option<Value>) -> BoxFuture<'_, Result<Vec<Value>, AppError>> {
                Box::pin(async { Err(AppError::not_paired()) })
            }
            fn call(
                &self,
                _request: CallRequest,
                _on_event: Option<tauri::ipc::Channel<Value>>,
            ) -> BoxFuture<'_, Result<Value, AppError>> {
                Box::pin(async { Err(AppError::not_paired()) })
            }
            fn cancel(&self, _id: CallId) -> BoxFuture<'_, Result<CancelResult, AppError>> {
                Box::pin(async { Err(AppError::not_paired()) })
            }
            fn pair(
                &self,
                _invite: String,
                _name: String,
            ) -> BoxFuture<'_, Result<AppState, AppError>> {
                Box::pin(async { Err(AppError::not_paired()) })
            }
            fn unpair(&self, _node: String) -> BoxFuture<'_, Result<UnpairResult, AppError>> {
                Box::pin(async { Err(AppError::not_paired()) })
            }
            fn select_node(&self, _node: String) -> BoxFuture<'_, Result<AppState, AppError>> {
                Box::pin(async { Err(AppError::not_paired()) })
            }
            fn repair_store(&self) -> BoxFuture<'_, Result<RepairResult, AppError>> {
                Box::pin(async { Err(AppError::not_paired()) })
            }
            fn invalidate_sessions(&self) -> BoxFuture<'_, Result<(), AppError>> {
                let link = self.link.clone();
                let invalidations = Arc::clone(&self.invalidations);
                Box::pin(async move {
                    invalidations.fetch_add(1, Ordering::SeqCst);
                    link.close().await;
                    Ok(())
                })
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
            fn dismiss_identity_change(
                &self,
                _node: String,
            ) -> BoxFuture<'_, Result<AppState, AppError>> {
                Box::pin(async { Err(AppError::not_paired()) })
            }
            fn content_target(&self, machine: &str) -> Result<ContentTarget, AppError> {
                Ok(ContentTarget {
                    link: self.link.clone(),
                    link_node: machine.to_owned(),
                    machine: machine.to_owned(),
                    principal: "device:pixel".to_owned(),
                    holder: "fleet:1".to_owned(),
                    learned_identity: None,
                })
            }
            fn is_pairing_holder_live(&self, _holder: &str) -> bool {
                true
            }
            fn node_route_identity(&self, node: &str) -> Option<NodeRouteIdentity> {
                let entry = self.link.config().node(node).cloned()?;
                Some(NodeRouteIdentity {
                    node: entry.node,
                    is_paired: entry.origin == NodeOrigin::Paired,
                    holder: format!("pairing:{node}:1"),
                    host_keys: entry.target.map(|t| t.host_keys).unwrap_or_default(),
                })
            }
        }

        let opener: SessionOpener = {
            let media = Arc::clone(&studio_media);
            let uploaded = Arc::clone(&studio_uploaded);
            let partials = Arc::clone(&studio_partials);
            let put_offsets = Arc::clone(&observed_put_offsets);
            let live_streams = Arc::clone(&live_relayed_streams);
            let hold_get = Arc::clone(&hold_get_stream);
            let hold_put_mid = Arc::clone(&hold_put_mid);
            let hold_put_after_commit = Arc::clone(&hold_put_after_commit);
            let put_mid_entered = Arc::clone(&put_mid_entered);
            let put_committed_entered = Arc::clone(&put_committed_entered);
            Arc::new(move |entry, _, cancel| {
                let media = Arc::clone(&media);
                let uploaded = Arc::clone(&uploaded);
                let partials = Arc::clone(&partials);
                let put_offsets = Arc::clone(&put_offsets);
                let live_streams = Arc::clone(&live_streams);
                let hold_get = Arc::clone(&hold_get);
                let hold_put_mid = Arc::clone(&hold_put_mid);
                let hold_put_after_commit = Arc::clone(&hold_put_after_commit);
                let put_mid_entered = Arc::clone(&put_mid_entered);
                let put_committed_entered = Arc::clone(&put_committed_entered);
                Box::pin(async move {
                    assert_eq!(entry.node, "tower");
                    let (client_in, mut server_out) = tokio::io::duplex(32 * 1024);
                    let (server_in, client_out) = tokio::io::duplex(32 * 1024);
                    let uploaded_rpc = Arc::clone(&uploaded);
                    tokio::spawn(async move {
                        let mut reader = BufReader::new(server_in);
                        let mut line = String::new();
                        while reader.read_line(&mut line).await.unwrap_or(0) > 0 {
                            let req: legion_link::proto::Request =
                                serde_json::from_str(&line).unwrap();
                            let reply = match req.method.as_str() {
                                "hello" => ProtoResp::ok(
                                    req.id,
                                    serde_json::to_value(HelloResult {
                                        node: "tower".to_owned(),
                                        node_explicit: None,
                                        node_id: None,
                                        version: VERSION.to_owned(),
                                        build: None,
                                        protocol: PROTOCOL,
                                        os: "linux".to_owned(),
                                        arch: "x86_64".to_owned(),
                                        systems: vec![],
                                        system: None,
                                        boot_id: None,
                                    })
                                    .unwrap(),
                                ),
                                "call" => {
                                    let tool = req.params["tool"].as_str().unwrap_or("");
                                    let path = req
                                        .params
                                        .pointer("/args/path")
                                        .and_then(Value::as_str)
                                        .unwrap_or("")
                                        .to_owned();
                                    let st = uploaded_rpc.lock().unwrap();
                                    match tool {
                                        "fs.stat" => {
                                            if let Some(bytes) = st.get(&path) {
                                                ProtoResp::ok(
                                                    req.id,
                                                    json!({
                                                        "path": path,
                                                        "kind": "file",
                                                        "size": bytes.len(),
                                                    }),
                                                )
                                            } else {
                                                ProtoResp::error(
                                                    req.id,
                                                    RpcError::new(
                                                        -32000,
                                                        ErrorBody::new(
                                                            ProtoCode::NotFound,
                                                            "missing path",
                                                        )
                                                        .with_machine("studio"),
                                                    ),
                                                )
                                            }
                                        }
                                        "fs.hash" => {
                                            if let Some(bytes) = st.get(&path) {
                                                let digest = format!(
                                                    "blake3:{}",
                                                    blake3::hash(bytes).to_hex()
                                                );
                                                ProtoResp::ok(
                                                    req.id,
                                                    json!({
                                                        "path": path,
                                                        "size": bytes.len(),
                                                        "hash": digest,
                                                    }),
                                                )
                                            } else {
                                                ProtoResp::error(
                                                    req.id,
                                                    RpcError::new(
                                                        -32000,
                                                        ErrorBody::new(
                                                            ProtoCode::NotFound,
                                                            "missing path",
                                                        )
                                                        .with_machine("studio"),
                                                    ),
                                                )
                                            }
                                        }
                                        other => ProtoResp::error(
                                            req.id,
                                            RpcError::new(
                                                -32000,
                                                ErrorBody::new(
                                                    ProtoCode::UnknownTool,
                                                    format!("unexpected tool {other}"),
                                                ),
                                            ),
                                        ),
                                    }
                                }
                                other => ProtoResp::error(
                                    req.id,
                                    RpcError::new(
                                        -32000,
                                        ErrorBody::new(
                                            ProtoCode::Unsupported,
                                            format!("unexpected method {other}"),
                                        ),
                                    ),
                                ),
                            };
                            let mut bytes = serde_json::to_vec(&reply).unwrap();
                            bytes.push(b'\n');
                            if server_out.write_all(&bytes).await.is_err() {
                                break;
                            }
                            line.clear();
                        }
                    });
                    let stream_opener = Arc::new(UpdaterCloseRelayOpener {
                        media,
                        uploaded,
                        partials,
                        put_offsets,
                        live_streams,
                        hold_get,
                        hold_put_mid,
                        hold_put_after_commit,
                        put_mid_entered,
                        put_committed_entered,
                    });
                    Session::open_transport(
                        client_in,
                        client_out,
                        Box::new(FakeTransport {
                            opener: stream_opener,
                        }),
                        &entry.node,
                        None,
                        Duration::from_secs(5),
                        cancel,
                    )
                    .await
                    .map_err(|e| e.into_error())
                })
            })
        };

        let mut cfg = LinkConfig {
            device_key: Some("dummy".to_owned()),
            ..LinkConfig::default()
        };
        cfg.nodes.push(NodeLink::ssh(
            "tower",
            SshTarget {
                hosts: vec!["tower.example.net".to_owned()],
                port: 22,
                user: "me".to_owned(),
                host_keys: vec![tower_key.to_owned()],
            },
            Some("device:pixel".to_owned()),
        ));
        let link = Link::with_opener(cfg, LinkOptions::default(), opener);
        let backend: Arc<dyn Backend> = Arc::new(UpdaterCloseRelayBackend {
            link: link.clone(),
            invalidations: Arc::clone(&invalidations),
        });
        let platform = Arc::new(MockPlatform::new(dir.path()));

        // Case A & B: Run an active relayed content stream AND a mid-transfer relayed upload
        // concurrently, then invoke the updater's real close path (`quiesce_for_update_exit`).
        let content_a = Arc::new(AppContentServer::with_config(
            Arc::clone(&backend),
            ContentMountMode::Loopback,
            TicketConfig::default(),
            Duration::from_secs(30),
            vec!["http://tauri.localhost".to_owned()],
        ));
        let mgr_a = TransferManager::with_options(
            dir.path(),
            Arc::clone(&backend),
            platform.clone(),
            2,
            Duration::from_secs(5),
        );

        let minted = content_a
            .mint_url("studio", "/home/me/video.mp4", false)
            .await
            .unwrap();
        let lb_port = content_a.loopback_port().unwrap();
        let req_path = minted
            .url
            .strip_prefix(&format!("http://127.0.0.1:{lb_port}"))
            .unwrap()
            .to_owned();
        let http_socket = TcpSocket::new_v4().unwrap();
        let _ = http_socket.set_recv_buffer_size(16 * 1024);
        let mut http_sock = http_socket
            .connect(std::net::SocketAddr::V4(std::net::SocketAddrV4::new(
                std::net::Ipv4Addr::LOCALHOST,
                lb_port,
            )))
            .await
            .unwrap();
        http_sock
            .write_all(
                format!(
                    "GET {req_path} HTTP/1.1\r\nHost: 127.0.0.1:{lb_port}\r\nOrigin: http://tauri.localhost\r\nConnection: close\r\n\r\n"
                )
                .as_bytes(),
            )
            .await
            .unwrap();
        let mut head_buf = vec![0u8; 4096];
        let n = http_sock.read(&mut head_buf).await.unwrap();
        assert!(
            String::from_utf8_lossy(&head_buf[..n]).starts_with("HTTP/1.1 200 OK"),
            "expected live 200 OK relayed stream"
        );
        assert_eq!(content_a.active_streams(), 1);

        let payload_b: Vec<u8> = (0..180_000).map(|i| ((i * 7) % 251) as u8).collect();
        let src_b = dir.path().join("updater-resume.bin");
        fs::write(&src_b, &payload_b).unwrap();
        platform
            .picked_files
            .lock()
            .unwrap()
            .push(PickedUploadFile {
                local_path: src_b.to_string_lossy().into_owned(),
                name: "updater-resume.bin".into(),
                size: payload_b.len() as u64,
                mime: None,
            });
        hold_put_mid.store(true, Ordering::SeqCst);
        let mid_wait = put_mid_entered.notified();
        let id_b = mgr_a
            .enqueue_upload("studio".into(), "/home/me/Uploads".into(), None)
            .await
            .unwrap()
            .unwrap()
            .id;
        tokio::time::timeout(Duration::from_secs(5), mid_wait)
            .await
            .expect("upload should reach mid-stream hold");
        assert!(live_relayed_streams.load(Ordering::SeqCst) >= 2);

        // Invoke the updater's desktop close path while BOTH the relayed content stream and
        // relayed upload are in flight.
        crate::commands::quiesce_for_update_exit(
            &mgr_a,
            &content_a,
            &backend,
            Duration::from_secs(3),
        )
        .await;
        drop(http_sock);

        // Both relayed streams must have ended cleanly (identical to cancel/revoke) and the
        // SSH session pool must have been closed.
        assert_eq!(content_a.active_streams(), 0);
        assert!(invalidations.load(Ordering::SeqCst) >= 1);
        let deadline = Instant::now() + Duration::from_secs(3);
        while live_relayed_streams.load(Ordering::SeqCst) > 0 && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(
            live_relayed_streams.load(Ordering::SeqCst),
            0,
            "updater close path must tear down both in-flight relayed streams cleanly"
        );

        // Simulate app restart after update swap: reload persisted queue on the same `data_dir`.
        let mgr_b = TransferManager::with_options(
            dir.path(),
            Arc::clone(&backend),
            platform.clone(),
            2,
            Duration::from_secs(5),
        );
        mgr_b.start();
        let done_b = wait_for_state(&mgr_b, &id_b, |s| s.is_terminal()).await;
        assert_eq!(
            done_b.state,
            TransferState::Done,
            "interrupted relayed upload must resume after updater restart and finish Done, got: {:?}",
            done_b.error
        );
        assert_eq!(
            studio_uploaded
                .lock()
                .unwrap()
                .get("/home/me/Uploads/updater-resume.bin")
                .map(Vec::as_slice),
            Some(payload_b.as_slice())
        );
        let offsets = observed_put_offsets.lock().unwrap().clone();
        assert_eq!(
            offsets.len(),
            2,
            "expected initial put + single resumed put: {offsets:?}"
        );
        assert_eq!(offsets[0], 0);
        assert!(
            offsets[1] >= 32 * 1024,
            "resumed upload after updater restart must continue from saved partial offset: {offsets:?}"
        );

        // Case C ("checked, not assumed" across updater restart): target `studio` commits and
        // renames the file, then the updater's close path terminates the app before the final `ok`
        // reply arrives. On restart, the app MUST verify `(size, blake3)` on the locked path and
        // finish `Done` with ZERO new `put` streams and ZERO duplicate `name (2).ext`.
        observed_put_offsets.lock().unwrap().clear();
        let payload_c: Vec<u8> = (0..96_000).map(|i| ((i * 19) % 251) as u8).collect();
        let src_c = dir.path().join("updater-committed.bin");
        fs::write(&src_c, &payload_c).unwrap();
        {
            let mut picked = platform.picked_files.lock().unwrap();
            picked.clear();
            picked.push(PickedUploadFile {
                local_path: src_c.to_string_lossy().into_owned(),
                name: "updater-committed.bin".into(),
                size: payload_c.len() as u64,
                mime: None,
            });
        }
        hold_put_after_commit.store(true, Ordering::SeqCst);
        let committed_wait = put_committed_entered.notified();
        let id_c = mgr_b
            .enqueue_upload("studio".into(), "/home/me/Uploads".into(), None)
            .await
            .unwrap()
            .unwrap()
            .id;
        tokio::time::timeout(Duration::from_secs(5), committed_wait)
            .await
            .expect("upload should commit on studio before final ok");
        assert_eq!(observed_put_offsets.lock().unwrap().len(), 1);

        let content_b = Arc::new(AppContentServer::with_config(
            Arc::clone(&backend),
            ContentMountMode::Loopback,
            TicketConfig::default(),
            Duration::from_secs(30),
            vec!["http://tauri.localhost".to_owned()],
        ));
        crate::commands::quiesce_for_update_exit(
            &mgr_b,
            &content_b,
            &backend,
            Duration::from_secs(3),
        )
        .await;

        // Restart into the updated app process (`mgr_c`): must check destination hash first and
        // complete `Done` without opening another `put` or creating `updater-committed (2).bin`.
        let mgr_c = TransferManager::with_options(
            dir.path(),
            Arc::clone(&backend),
            platform,
            2,
            Duration::from_secs(5),
        );
        mgr_c.start();
        let done_c = wait_for_state(&mgr_c, &id_c, |s| s.is_terminal()).await;
        assert_eq!(
            done_c.state,
            TransferState::Done,
            "committed upload whose final ok was lost to updater close must verify Done on restart, got: {:?}",
            done_c.error
        );
        assert_eq!(done_c.name, "updater-committed.bin");
        assert_eq!(done_c.bytes_done, payload_c.len() as u64);
        assert_eq!(
            observed_put_offsets.lock().unwrap().len(),
            1,
            "restarted app must not open a second put once destination hash matches"
        );
        assert!(
            !studio_uploaded
                .lock()
                .unwrap()
                .contains_key("/home/me/Uploads/updater-committed (2).bin"),
            "must never create a duplicate sibling after updater restart"
        );
        assert_eq!(
            studio_uploaded
                .lock()
                .unwrap()
                .get("/home/me/Uploads/updater-committed.bin")
                .map(Vec::as_slice),
            Some(payload_c.as_slice())
        );

        mgr_c.shutdown();
        link.close().await;
    }
}
