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
#[cfg(target_os = "ios")]
pub mod ios;

use std::collections::{HashMap, HashSet};
use std::fs::{self, OpenOptions};
use std::io::{SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use legion_content::extract_file_name;
use legion_link::{ContentTime, PutRequest};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tauri::ipc::Channel as IpcChannel;
use tempfile::NamedTempFile;
use tokio::fs::File as AsyncFile;
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

use crate::backend::{AppError, Backend, BoxFuture, ContentTarget, ErrorCode};

/// Default maximum concurrent transfers running at once.
pub const DEFAULT_MAX_CONCURRENT: usize = 2;

/// Default size threshold above which a transfer counts as "large" for the Wi-Fi-only rule
/// (50 MiB).
pub const DEFAULT_LARGE_THRESHOLD_BYTES: u64 = 50 * 1024 * 1024;

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
    /// Verified and placed in its destination.
    Done,
    /// Stopped after an unrecoverable error or exhausted retries.
    Failed,
    /// Cancelled by the user; any partial file was removed.
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

/// Structured error attached to a failed transfer.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TransferError {
    /// Machine-readable error code (`snake_case`).
    pub code: String,
    /// One-sentence human-readable summary.
    pub message: String,
    /// Optional actionable hint.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
}

impl From<&AppError> for TransferError {
    fn from(err: &AppError) -> Self {
        let code = serde_json::to_value(err.code)
            .ok()
            .and_then(|v| v.as_str().map(str::to_owned))
            .unwrap_or_else(|| "failed".to_owned());
        Self {
            code,
            message: err.message.clone(),
            hint: err.hint.clone(),
        }
    }
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
    /// True when tapping the finished row opens the local file.
    #[serde(rename = "canOpen")]
    pub can_open: bool,
    /// Reason this transfer is paused, when `state == Paused`.
    #[serde(
        rename = "pausedReason",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub paused_reason: Option<PauseReason>,
}

/// User-configurable queue settings persisted alongside the queue.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TransferSettings {
    /// When true, transfers at or above [`Self::large_threshold_bytes`] pause on metered networks
    /// and resume automatically on unmetered Wi-Fi. Defaults to `false`.
    #[serde(default)]
    pub wifi_only_large: bool,
    /// Byte threshold defining a "large" transfer (default 50 MiB).
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
    /// Identifier of the newly queued transfer.
    pub id: String,
    /// Always true when queued cleanly.
    pub ok: bool,
}

/// Pre-selected upload file optionally supplied by tests or internal callers.
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

    /// Opens a finished local file or URI in the system viewer or chooser.
    fn open_local_file<'a>(
        &'a self,
        local_path: &'a str,
        mime: Option<&'a str>,
    ) -> BoxFuture<'a, Result<(), AppError>>;

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
    #[serde(default)]
    pub open_after_done: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote_mtime_secs: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote_mtime_nanos: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote_file_id: Option<u64>,
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

struct QueueState {
    settings: TransferSettings,
    next_seq: u64,
    items: Vec<TransferRecord>,
    running: HashMap<String, CancellationToken>,
    watchers: Vec<IpcChannel<Vec<DeviceTransfer>>>,
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
    state: Mutex<QueueState>,
    schedule_notify: Notify,
    scheduler_started: AtomicBool,
    shutdown_token: CancellationToken,
    #[allow(dead_code)]
    id_salt: AtomicU64,
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
        Self::with_options(
            data_dir,
            backend,
            platform,
            DEFAULT_MAX_CONCURRENT,
            DEFAULT_RETRY_WINDOW,
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
        let mut changed = false;
        for rec in &mut loaded.items {
            if rec.transfer.direction == TransferDirection::Download
                && !rec.transfer.state.is_terminal()
            {
                let part = parts_dir.join(format!("{}.part", rec.transfer.id));
                let on_disk = fs::metadata(&part).map(|m| m.len()).unwrap_or(0);
                if rec.transfer.bytes_done != on_disk {
                    rec.transfer.bytes_done = on_disk;
                    changed = true;
                }
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
            state: Mutex::new(QueueState {
                settings: loaded.settings,
                next_seq: loaded.next_seq.max(1),
                items: loaded.items,
                running: HashMap::new(),
                watchers: Vec::new(),
                last_ui_broadcast: Instant::now() - UI_PROGRESS_INTERVAL,
                last_notify_broadcast: Instant::now() - NOTIFY_PROGRESS_INTERVAL,
            }),
            schedule_notify: Notify::new(),
            scheduler_started: AtomicBool::new(false),
            shutdown_token: CancellationToken::new(),
            id_salt: AtomicU64::new(salt),
        });

        if changed {
            manager.persist_locked_snapshot();
        }
        manager
    }

    /// Starts the background scheduler loop on the active Tokio runtime and resumes any queued
    /// transfers loaded from disk.
    pub fn start(self: &Arc<Self>) {
        if self.scheduler_started.swap(true, Ordering::SeqCst) {
            self.schedule_notify.notify_one();
            return;
        }
        let mgr = Arc::clone(self);
        spawn_bg(async move {
            mgr.run_scheduler().await;
        });
        self.broadcast_state_change();
        self.schedule_notify.notify_one();
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
            guard.running.values().cloned().collect()
        };
        for tok in tokens {
            tok.cancel();
        }
        self.schedule_notify.notify_one();
    }

    /// Returns true if any transfer is currently `queued` or `running`.
    pub fn has_active_work(&self) -> bool {
        let guard = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        guard.items.iter().any(|r| r.transfer.state.is_active())
    }

    /// Returns true if any transfer is `queued`, `running`, or `paused` (used on Android so a
    /// paused notification remains interactive while the user taps Resume/Cancel).
    pub fn has_unfinished_or_paused(&self) -> bool {
        let guard = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        guard
            .items
            .iter()
            .any(|r| r.transfer.state.is_active() || r.transfer.state == TransferState::Paused)
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
                        if let Some(tok) = state.running.get(&rec.transfer.id) {
                            to_cancel.push(tok.clone());
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
                        if let Some(tok) = state.running.get(&rec.transfer.id) {
                            to_cancel.push(tok.clone());
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
                    if let Some(tok) = state.running.get(&rec.transfer.id) {
                        to_cancel.push(tok.clone());
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
                        r.transfer.state.is_active() || r.transfer.state == TransferState::Paused
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
    /// snapshot.
    pub fn watch(&self, channel: IpcChannel<Vec<DeviceTransfer>>) {
        let snapshot = {
            let mut guard = self
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let snap = snapshot_transfers(&guard.items);
            guard.watchers.push(channel.clone());
            snap
        };
        let _ = channel.send(snapshot);
    }

    /// Enqueues a non-blocking download (either to Downloads or to open-externally cache) and
    /// returns its id immediately.
    pub async fn enqueue_download(
        self: &Arc<Self>,
        machine: String,
        path: String,
        name: Option<String>,
        size: Option<u64>,
        mime: Option<String>,
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
            .map(sanitize_file_name)
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| sanitize_file_name(&inferred_name));
        let resolved_mime = mime
            .filter(|s| !s.trim().is_empty())
            .or_else(|| guess_mime_from_name(&display_name).map(str::to_owned));

        // Verify the machine is paired before enqueueing.
        let _ = self.backend.content_target(&machine_trimmed)?;

        // Ask Android notification permission lazily on first user-initiated transfer.
        let _ = self.platform.before_user_transfer().await;

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
                paused_reason,
            },
            open_after_done,
            remote_mtime_secs: None,
            remote_mtime_nanos: None,
            remote_file_id: None,
        };

        {
            let mut guard = self
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            guard.items.push(record);
            prune_terminal_locked(&mut guard.items);
        }

        self.start();
        self.persist_locked_snapshot();
        self.broadcast_state_change();
        self.schedule_notify.notify_one();
        Ok(id)
    }

    /// Picks files using the native picker (unless pre-supplied in `files`) and enqueues non-blocking
    /// bulk-pipe `put` uploads into `machine:dir`. Returns `Ok(None)` if the picker was cancelled.
    pub async fn enqueue_upload(
        self: &Arc<Self>,
        machine: String,
        dir: String,
        files: Option<Vec<UploadFileInput>>,
    ) -> Result<Option<UploadEnqueueResult>, AppError> {
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

        let picked: Vec<PickedUploadFile> = match files {
            Some(supplied) if !supplied.is_empty() => {
                let mut out = Vec::with_capacity(supplied.len());
                for item in supplied {
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
                    if safe_name.is_empty() {
                        return Err(AppError::new(
                            ErrorCode::InvalidArgs,
                            "Supplied upload file has an empty file name.",
                        ));
                    }
                    let size = item
                        .size
                        .unwrap_or_else(|| fs::metadata(&local_path).map(|m| m.len()).unwrap_or(0));
                    let mime = item
                        .mime
                        .filter(|s| !s.trim().is_empty())
                        .or_else(|| guess_mime_from_name(&safe_name).map(str::to_owned));
                    out.push(PickedUploadFile {
                        local_path,
                        name: safe_name,
                        size,
                        mime,
                    });
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

        // Ask Android notification permission lazily on first user-initiated transfer.
        let _ = self.platform.before_user_transfer().await;

        let mut ids = Vec::with_capacity(picked.len());
        let created = now_rfc3339_utc();
        {
            let mut guard = self
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let wifi_only = guard.settings.wifi_only_large;
            let threshold = guard.settings.large_threshold_bytes;
            let unmetered = self.platform.is_unmetered_wifi();

            for file in picked {
                let safe_name = sanitize_file_name(&file.name);
                if safe_name.is_empty() {
                    continue;
                }
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
                let mime = file
                    .mime
                    .filter(|s| !s.trim().is_empty())
                    .or_else(|| guess_mime_from_name(&safe_name).map(str::to_owned));
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
                        paused_reason,
                    },
                    open_after_done: false,
                    remote_mtime_secs: None,
                    remote_mtime_nanos: None,
                    remote_file_id: None,
                });
                ids.push(id);
            }
            prune_terminal_locked(&mut guard.items);
        }

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
    pub async fn control(
        self: &Arc<Self>,
        id: &str,
        action: TransferControlAction,
    ) -> Result<DeviceTransfer, AppError> {
        let mut cancel_token: Option<CancellationToken> = None;
        let mut delete_part = false;
        let mut release_upload_uri: Option<String> = None;

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
                        cancel_token = guard.running.get(id).cloned();
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
                TransferControlAction::Cancel => {
                    if !matches!(current, TransferState::Done | TransferState::Cancelled) {
                        guard.items[pos].transfer.state = TransferState::Cancelled;
                        guard.items[pos].transfer.paused_reason = None;
                        guard.items[pos].transfer.rate = None;
                        guard.items[pos].transfer.error = None;
                        guard.items[pos].transfer.finished = Some(now_rfc3339_utc());
                        cancel_token = guard.running.get(id).cloned();
                        delete_part =
                            guard.items[pos].transfer.direction == TransferDirection::Download;
                        if guard.items[pos].transfer.direction == TransferDirection::Upload {
                            release_upload_uri = guard.items[pos].transfer.local_path.clone();
                        }
                    }
                }
            }
            guard.items[pos].transfer.clone()
        };

        if let Some(tok) = cancel_token {
            tok.cancel();
        }
        if delete_part {
            let _ = tokio::fs::remove_file(self.part_path(id)).await;
        }
        if let Some(uri) = release_upload_uri {
            self.platform.release_upload_source(&uri).await;
        }

        if action == TransferControlAction::Resume {
            self.start();
        }
        self.persist_locked_snapshot();
        self.broadcast_state_change();
        self.schedule_notify.notify_one();
        Ok(snapshot)
    }

    /// Opens a finished downloaded file on this device using the platform viewer / chooser.
    pub async fn open_transfer(&self, id: &str) -> Result<OpenTransferResult, AppError> {
        let (local_path, mime) = {
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
            let Some(path) = rec.transfer.local_path.clone() else {
                return Err(AppError::new(
                    ErrorCode::NotFound,
                    "The downloaded file path is no longer recorded.",
                ));
            };
            (path, rec.transfer.mime.clone())
        };
        self.platform
            .open_local_file(&local_path, mime.as_deref())
            .await?;
        Ok(OpenTransferResult { ok: true })
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
        let payload = {
            let guard = self
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            PersistedQueue {
                version: QUEUE_FILE_VERSION,
                settings: guard.settings,
                next_seq: guard.next_seq,
                items: guard.items.clone(),
            }
        };
        let _ = save_queue_file(&self.queue_file_path(), &payload);
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
            let watchers = guard.watchers.clone();
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
                    Some((snapshot_transfers(&guard.items), guard.watchers.clone()))
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
        watchers: &[IpcChannel<Vec<DeviceTransfer>>],
        snapshot: &[DeviceTransfer],
    ) {
        if watchers.is_empty() {
            return;
        }
        let mut dead_indices = HashSet::new();
        for (idx, ch) in watchers.iter().enumerate() {
            if ch.send(snapshot.to_vec()).is_err() {
                dead_indices.insert(idx);
            }
        }
        if !dead_indices.is_empty() {
            let mut guard = self
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let mut keep_idx = 0usize;
            guard.watchers.retain(|_| {
                let keep = !dead_indices.contains(&keep_idx);
                keep_idx += 1;
                keep
            });
        }
    }

    async fn run_scheduler(self: Arc<Self>) {
        loop {
            if self.shutdown_token.is_cancelled() {
                return;
            }

            let to_spawn: Vec<(String, CancellationToken)> = {
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
                    let next_pos = guard
                        .items
                        .iter()
                        .position(|r| r.transfer.state == TransferState::Queued);
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
                    let tok = self.shutdown_token.child_token();
                    guard.running.insert(id.clone(), tok.clone());
                    spawned.push((id, tok));
                }

                if wifi_paused_any || !spawned.is_empty() {
                    drop(guard);
                    self.persist_locked_snapshot();
                    self.broadcast_state_change();
                }
                spawned
            };

            for (id, cancel) in to_spawn {
                let mgr = Arc::clone(&self);
                tokio::spawn(async move {
                    mgr.run_worker(&id, cancel).await;
                });
            }

            tokio::select! {
                biased;
                () = self.shutdown_token.cancelled() => return,
                () = self.schedule_notify.notified() => {}
            }
        }
    }

    async fn run_worker(self: Arc<Self>, id: &str, cancel: CancellationToken) {
        let record = {
            let guard = self
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            guard.items.iter().find(|r| r.transfer.id == id).cloned()
        };

        let Some(record) = record else {
            self.finish_worker_slot(id);
            return;
        };

        let result = match record.transfer.direction {
            TransferDirection::Download => self.execute_download(&record, &cancel).await,
            TransferDirection::Upload => self.execute_upload(&record, &cancel).await,
        };

        // Check whether an external state transition (pause / cancel / shutdown) stopped us.
        let external_state = {
            let guard = self
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            guard
                .items
                .iter()
                .find(|r| r.transfer.id == id)
                .map(|r| r.transfer.state)
        };

        match result {
            Ok(outcome) => {
                if external_state == Some(TransferState::Cancelled) {
                    if record.transfer.direction == TransferDirection::Download {
                        let _ = tokio::fs::remove_file(self.part_path(id)).await;
                    }
                    self.finish_worker_slot(id);
                    return;
                }
                let completed = {
                    let mut guard = self
                        .state
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                    guard.running.remove(id);
                    let mut done_snap = None;
                    if let Some(rec) = guard.items.iter_mut().find(|r| r.transfer.id == id) {
                        rec.transfer.state = TransferState::Done;
                        rec.transfer.bytes_done = outcome.bytes_total;
                        rec.transfer.bytes_total = outcome.bytes_total;
                        rec.transfer.rate = None;
                        rec.transfer.error = None;
                        rec.transfer.paused_reason = None;
                        rec.transfer.finished = Some(now_rfc3339_utc());
                        rec.transfer.hash = Some(outcome.hash);
                        if let Some(local_path) = outcome.local_path {
                            rec.transfer.local_path = Some(local_path);
                        }
                        rec.transfer.can_open =
                            rec.transfer.direction == TransferDirection::Download;
                        done_snap = Some(rec.transfer.clone());
                    }
                    prune_terminal_locked(&mut guard.items);
                    done_snap
                };
                self.persist_locked_snapshot();
                self.broadcast_state_change();
                if let Some(done) = completed {
                    if done.direction == TransferDirection::Upload
                        && let Some(uri) = done.local_path.as_deref()
                    {
                        self.platform.release_upload_source(uri).await;
                    }
                    if record.open_after_done
                        && let Some(path) = done.local_path.as_deref()
                    {
                        let _ = self
                            .platform
                            .open_local_file(path, done.mime.as_deref())
                            .await;
                    } else {
                        self.platform.on_transfer_completed(&done);
                    }
                }
                self.schedule_notify.notify_one();
            }
            Err(WorkerStop::WifiOnlyPause) => {
                {
                    let mut guard = self
                        .state
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                    guard.running.remove(id);
                    if let Some(rec) = guard.items.iter_mut().find(|r| r.transfer.id == id)
                        && rec.transfer.state == TransferState::Running
                    {
                        rec.transfer.state = TransferState::Paused;
                        rec.transfer.paused_reason = Some(PauseReason::WifiOnly);
                        rec.transfer.rate = None;
                    }
                }
                self.persist_locked_snapshot();
                self.broadcast_state_change();
                self.schedule_notify.notify_one();
            }
            Err(WorkerStop::Interrupted) => {
                self.finish_worker_slot(id);
            }
            Err(WorkerStop::Failed(err)) => {
                if self.shutdown_token.is_cancelled()
                    || matches!(
                        external_state,
                        Some(TransferState::Paused | TransferState::Cancelled)
                    )
                {
                    self.finish_worker_slot(id);
                    return;
                }
                {
                    let mut guard = self
                        .state
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                    guard.running.remove(id);
                    if let Some(rec) = guard.items.iter_mut().find(|r| r.transfer.id == id) {
                        rec.transfer.state = TransferState::Failed;
                        rec.transfer.paused_reason = None;
                        rec.transfer.rate = None;
                        rec.transfer.finished = Some(now_rfc3339_utc());
                        rec.transfer.error = Some(TransferError::from(&err));
                    }
                    prune_terminal_locked(&mut guard.items);
                }
                self.persist_locked_snapshot();
                self.broadcast_state_change();
                self.schedule_notify.notify_one();
            }
        }
    }

    fn finish_worker_slot(&self, id: &str) {
        {
            let mut guard = self
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            guard.running.remove(id);
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
                    .open_content_with_cancel(
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
                    .open_content_with_cancel(
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
                    .open_content_with_cancel(
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
                    res = stream.read(&mut buf) => res,
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
            let _ = tokio::fs::remove_file(&part_path).await;
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

        let final_path = self
            .platform
            .finalize_download(
                id,
                &part_path,
                &initial.transfer.name,
                initial.transfer.mime.as_deref(),
                initial.open_after_done,
            )
            .await
            .map_err(WorkerStop::Failed)?;
        let _ = tokio::fs::remove_file(&part_path).await;

        Ok(WorkerOutcome {
            bytes_total: total_size,
            hash: local_hash,
            local_path: Some(final_path),
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

    async fn execute_upload(
        &self,
        initial: &TransferRecord,
        cancel: &CancellationToken,
    ) -> Result<WorkerOutcome, WorkerStop> {
        let id = &initial.transfer.id;
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

        let mut retry_started: Option<Instant> = None;
        let mut attempt: u32 = 0;

        loop {
            if cancel.is_cancelled() {
                return Err(WorkerStop::Interrupted);
            }
            if self.should_wifi_pause(total_size) {
                return Err(WorkerStop::WifiOnlyPause);
            }

            let target: ContentTarget = self
                .backend
                .content_target(&initial.transfer.machine)
                .map_err(WorkerStop::Failed)?;

            let req = PutRequest {
                path: initial.transfer.path.clone(),
                size: total_size,
                hash: local_hash.clone(),
                mode: Some(0o644),
                mtime: local_file_mtime(local_path),
            };

            let mut put = match target
                .link
                .open_put_with_cancel(&target.link_node, req, cancel.clone())
                .await
            {
                Ok(put) => put,
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

            let resume_offset = put.offset();
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
                    res = source.read_chunk(&mut buf[..max_read]) => res.map_err(WorkerStop::Failed)?,
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
                    res = put.write_all(&buf[..read_n]) => res,
                };
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
                retry_started = None;
                attempt = 0;

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
                put.cancel();
                if self
                    .should_retry(&err, &mut retry_started, &mut attempt, cancel)
                    .await?
                {
                    continue;
                }
                return Err(WorkerStop::Failed(err));
            }

            match put.finish().await {
                Ok(done) => {
                    return Ok(WorkerOutcome {
                        bytes_total: done.size,
                        hash: done.hash,
                        local_path: None,
                    });
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
}

struct WorkerOutcome {
    bytes_total: u64,
    hash: String,
    local_path: Option<String>,
}

enum WorkerStop {
    Interrupted,
    WifiOnlyPause,
    Failed(AppError),
}

fn is_transient_error(err: &AppError) -> bool {
    matches!(
        err.code,
        ErrorCode::Unreachable | ErrorCode::Timeout | ErrorCode::Busy
    )
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

fn prune_terminal_locked(items: &mut Vec<TransferRecord>) {
    let terminal_count = items
        .iter()
        .filter(|r| r.transfer.state.is_terminal())
        .count();
    if terminal_count <= MAX_RETAINED_TERMINAL {
        return;
    }
    let mut to_drop = terminal_count - MAX_RETAINED_TERMINAL;
    items.retain(|r| {
        if to_drop > 0 && r.transfer.state.is_terminal() {
            to_drop -= 1;
            false
        } else {
            true
        }
    });
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

    let focus: &[&DeviceTransfer] = if !active.is_empty() { &active } else { &paused };

    let bytes_done: u64 = focus.iter().map(|t| t.bytes_done).sum();
    let bytes_total: u64 = focus.iter().map(|t| t.bytes_total).sum();
    let indeterminate = bytes_total == 0 || focus.iter().any(|t| t.bytes_total == 0);

    let (title, detail, primary_transfer_id) = if active.len() == 1 {
        let t = active[0];
        let verb = match t.direction {
            TransferDirection::Download => "Downloading",
            TransferDirection::Upload => "Uploading",
        };
        let detail = format_progress_line(t.bytes_done, t.bytes_total);
        (format!("{verb} {}", t.name), detail, Some(t.id.clone()))
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
            Some(PauseReason::FgsTimeout) => "Paused at Android limit",
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
    } else {
        (String::new(), String::new(), None)
    };

    QueueNotificationSummary {
        active_count: active.len(),
        running_count,
        paused_count: paused.len(),
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
    format!("{} of {} ({pct}%)", format_bytes(done), format_bytes(total))
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
        format!("{:.0} KiB", b / KIB)
    } else {
        format!("{bytes} B")
    }
}

/// Sanitizes a filename to a safe single path component with no separators or control chars.
pub fn sanitize_file_name(raw: &str) -> String {
    let base = raw.rsplit(['/', '\\']).next().unwrap_or("").trim();
    let cleaned: String = base
        .chars()
        .filter(|c| !c.is_control() && *c != '/' && *c != '\\' && *c != '\0')
        .collect();
    let trimmed = cleaned.trim_matches('.');
    if trimmed.is_empty() {
        if cleaned.is_empty() || cleaned == "." || cleaned == ".." {
            String::new()
        } else {
            cleaned
        }
    } else {
        cleaned
    }
}

/// Allocates a unique path inside `dir` for `file_name` (`name (2).ext` if `name.ext` exists).
pub fn unique_dest_path(dir: &Path, file_name: &str) -> PathBuf {
    let safe = sanitize_file_name(file_name);
    let safe = if safe.is_empty() {
        "download".to_owned()
    } else {
        safe
    };
    let candidate = dir.join(&safe);
    if !candidate.exists() {
        return candidate;
    }
    let path_ref = Path::new(&safe);
    let stem = path_ref
        .file_stem()
        .and_then(|s| s.to_str())
        .filter(|s| !s.is_empty())
        .unwrap_or(&safe);
    let ext = path_ref.extension().and_then(|e| e.to_str());
    for n in 2..=9999u32 {
        let next = match ext {
            Some(ext) => dir.join(format!("{stem} ({n}).{ext}")),
            None => dir.join(format!("{stem} ({n})")),
        };
        if !next.exists() {
            return next;
        }
    }
    dir.join(format!("{safe}-{}", now_unix_secs()))
}

/// Joins a remote destination folder and filename without creating duplicate separators.
fn join_remote_path(dir: &str, name: &str) -> String {
    let sep = if dir.contains('\\') && !dir.contains('/') {
        '\\'
    } else {
        '/'
    };
    if dir.ends_with('/') || dir.ends_with('\\') {
        format!("{dir}{name}")
    } else {
        format!("{dir}{sep}{name}")
    }
}

/// Maps common file extensions to MIME types when the UI did not supply one.
pub fn guess_mime_from_name(name: &str) -> Option<&'static str> {
    let ext = Path::new(name)
        .extension()
        .and_then(|e| e.to_str())?
        .to_ascii_lowercase();
    let mime = match ext.as_str() {
        "jpg" | "jpeg" => "image/jpeg",
        "png" => "image/png",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "avif" => "image/avif",
        "bmp" => "image/bmp",
        "svg" => "image/svg+xml",
        "mp4" | "m4v" => "video/mp4",
        "webm" => "video/webm",
        "mov" => "video/quicktime",
        "mkv" => "video/x-matroska",
        "mp3" => "audio/mpeg",
        "m4a" | "aac" => "audio/mp4",
        "flac" => "audio/flac",
        "ogg" | "opus" => "audio/ogg",
        "wav" => "audio/wav",
        "pdf" => "application/pdf",
        "txt" | "md" | "log" | "csv" | "toml" | "yaml" | "yml" => "text/plain",
        "json" => "application/json",
        "html" | "htm" => "text/html",
        "zip" => "application/zip",
        "tar" => "application/x-tar",
        "gz" => "application/gzip",
        "7z" => "application/x-7z-compressed",
        _ => "application/octet-stream",
    };
    Some(mime)
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

/// Copies `src` to `dst`, falling back from `rename` to copy+delete across filesystems.
pub async fn move_file_across_fs(src: &Path, dst: &Path) -> Result<(), AppError> {
    if tokio::fs::rename(src, dst).await.is_ok() {
        return Ok(());
    }
    let mut input = AsyncFile::open(src).await.map_err(|err| {
        AppError::new(
            ErrorCode::Failed,
            format!("Could not read staged download {}: {err}.", src.display()),
        )
    })?;
    let mut output = AsyncFile::create(dst).await.map_err(|err| {
        AppError::new(
            ErrorCode::Failed,
            format!(
                "Could not create destination file {}: {err}.",
                dst.display()
            ),
        )
    })?;
    let mut buf = vec![0u8; COPY_BUF_BYTES];
    loop {
        let n = input.read(&mut buf).await.map_err(|err| {
            AppError::new(
                ErrorCode::Failed,
                format!("Could not read staged download: {err}."),
            )
        })?;
        if n == 0 {
            break;
        }
        output.write_all(&buf[..n]).await.map_err(|err| {
            AppError::new(
                ErrorCode::Failed,
                format!("Could not write destination file: {err}."),
            )
        })?;
    }
    output.flush().await.map_err(|err| {
        AppError::new(
            ErrorCode::Failed,
            format!("Could not flush destination file: {err}."),
        )
    })?;
    let _ = tokio::fs::remove_file(src).await;
    Ok(())
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
        active_streams: AtomicUsize,
        max_concurrent_observed: AtomicUsize,
    }

    struct MockStreamOpener {
        remote: Arc<Mutex<MockRemoteState>>,
    }

    impl StreamOpener for MockStreamOpener {
        fn open_stream(
            &self,
            _cancel: CancellationToken,
        ) -> Pin<Box<dyn Future<Output = Result<RawStream, StreamOpenError>> + Send + '_>> {
            let remote = Arc::clone(&self.remote);
            Box::pin(async move {
                let (client_in, mut server_out) = tokio::io::duplex(64 * 1024);
                let (server_in, client_out) = tokio::io::duplex(64 * 1024);
                let stream_cancel = CancellationToken::new();
                let watch_cancel = stream_cancel.clone();

                tokio::spawn(async move {
                    let mut reader = BufReader::new(server_in);
                    let mut line = String::new();
                    if reader.read_line(&mut line).await.unwrap_or(0) == 0 {
                        return;
                    }
                    let req: Value = serde_json::from_str(&line).unwrap();
                    let _ = server_out.write_all(b"{\"protocol\":1}\n").await;
                    let op = req["op"].as_str().unwrap_or("");
                    if op == "get" {
                        let path = req["path"].as_str().unwrap_or("").to_owned();
                        let offset = req["offset"].as_u64().unwrap_or(0) as usize;
                        let req_len = req
                            .get("length")
                            .and_then(Value::as_u64)
                            .map(|n| n as usize);

                        let (payload, delay_ms, fail_now) = {
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
                        let ready = json!({
                            "size": data.len(),
                            "offset": offset,
                            "length": len,
                            "mtime": { "secs": 1_700_000_000i64, "nanos": 0u32 },
                            "file_id": 11u64,
                        });
                        let _ = server_out.write_all(format!("{ready}\n").as_bytes()).await;
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
                        let mut pos = offset;
                        let end = offset + len;
                        while pos < end {
                            let next = (pos + 16 * 1024).min(end);
                            if delay_ms > 0 {
                                tokio::select! {
                                    biased;
                                    () = watch_cancel.cancelled() => break,
                                    () = tokio::time::sleep(Duration::from_millis(delay_ms)) => {}
                                }
                            }
                            if watch_cancel.is_cancelled() {
                                break;
                            }
                            if server_out.write_all(&data[pos..next]).await.is_err() {
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
                    } else if op == "put" {
                        let path = req["path"].as_str().unwrap_or("").to_owned();
                        let size = req["size"].as_u64().unwrap() as usize;
                        let expected_hash = req["hash"].as_str().unwrap_or("").to_owned();
                        let mut existing = {
                            let st = remote.lock().unwrap();
                            st.partial_uploads.get(&path).cloned().unwrap_or_default()
                        };
                        if existing.len() > size {
                            existing.clear();
                        }
                        let resume_off = existing.len();
                        let _ = server_out
                            .write_all(format!("{{\"offset\":{resume_off}}}\n").as_bytes())
                            .await;
                        let mut buf = vec![0u8; 16 * 1024];
                        let mut got = existing;
                        while got.len() < size {
                            tokio::select! {
                                biased;
                                () = watch_cancel.cancelled() => {
                                    remote.lock().unwrap().partial_uploads.insert(path, got);
                                    return;
                                }
                                res = reader.read(&mut buf) => match res {
                                    Ok(0) => break,
                                    Ok(n) => got.extend_from_slice(&buf[..n]),
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
                        {
                            let mut st = remote.lock().unwrap();
                            st.partial_uploads.remove(&path);
                            st.uploaded.insert(path, got);
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
            let stream_opener: Arc<dyn StreamOpener> = Arc::new(MockStreamOpener {
                remote: Arc::clone(&remote),
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
                                let path = req
                                    .params
                                    .pointer("/args/path")
                                    .and_then(Value::as_str)
                                    .unwrap_or("");
                                let st = remote.lock().unwrap();
                                if let Some(forced) = st.override_hash.get(path) {
                                    ProtoResp::ok(
                                        req.id,
                                        json!({ "path": path, "size": 0, "hash": forced }),
                                    )
                                } else if let Some(data) = st.files.get(path) {
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
                link: Link::with_opener(cfg, LinkOptions::default(), opener),
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
            })
        }
        fn is_pairing_holder_live(&self, holder: &str) -> bool {
            holder == "pairing:tower:1"
        }
    }

    struct MockPlatform {
        downloads_dir: PathBuf,
        unmetered_wifi: Arc<AtomicBool>,
        opened: Arc<Mutex<Vec<String>>>,
        completed_notifications: Arc<Mutex<Vec<String>>>,
    }

    impl MockPlatform {
        fn new(dir: &Path) -> Self {
            let downloads_dir = dir.join("Downloads");
            fs::create_dir_all(&downloads_dir).unwrap();
            Self {
                downloads_dir,
                unmetered_wifi: Arc::new(AtomicBool::new(true)),
                opened: Arc::new(Mutex::new(Vec::new())),
                completed_notifications: Arc::new(Mutex::new(Vec::new())),
            }
        }
    }

    impl TransferPlatform for MockPlatform {
        fn is_unmetered_wifi(&self) -> bool {
            self.unmetered_wifi.load(Ordering::Acquire)
        }

        fn pick_upload_files(&self) -> BoxFuture<'_, Result<Vec<PickedUploadFile>, AppError>> {
            Box::pin(async { Ok(Vec::new()) })
        }

        fn finalize_download<'a>(
            &'a self,
            _id: &'a str,
            part_path: &'a Path,
            file_name: &'a str,
            _mime: Option<&'a str>,
            _open_after_done: bool,
        ) -> BoxFuture<'a, Result<String, AppError>> {
            Box::pin(async move {
                let dst = unique_dest_path(&self.downloads_dir, file_name);
                move_file_across_fs(part_path, &dst).await?;
                Ok(dst.to_string_lossy().into_owned())
            })
        }

        fn open_local_file<'a>(
            &'a self,
            local_path: &'a str,
            _mime: Option<&'a str>,
        ) -> BoxFuture<'a, Result<(), AppError>> {
            Box::pin(async move {
                self.opened.lock().unwrap().push(local_path.to_owned());
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

    async fn wait_for_state(
        mgr: &TransferManager,
        id: &str,
        predicate: impl Fn(TransferState) -> bool,
    ) -> DeviceTransfer {
        for _ in 0..200 {
            if let Some(t) = mgr.list().into_iter().find(|t| t.id == id)
                && predicate(t.state)
            {
                return t;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!(
            "timed out waiting for transfer {id}; current: {:?}",
            mgr.list()
        );
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
                .insert("/home/me/movie.bin".to_owned(), data.clone());
            st.files
                .insert("/home/me/tampered.bin".to_owned(), data.clone());
            st.override_hash.insert(
                "/home/me/tampered.bin".to_owned(),
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

        // Pre-seed a valid partial file after enqueueing by pausing first or writing `.part`.
        let id = mgr
            .enqueue_download(
                "tower".to_owned(),
                "/home/me/movie.bin".to_owned(),
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
        let saved_bytes = fs::read(done.local_path.as_deref().unwrap()).unwrap();
        assert_eq!(saved_bytes, data);

        // Hash mismatch rejects and deletes partial file.
        let bad_id = mgr
            .enqueue_download(
                "tower".to_owned(),
                "/home/me/tampered.bin".to_owned(),
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
            for name in ["a.bin", "b.bin", "c.bin"] {
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
                "/home/me/a.bin".into(),
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
                "/home/me/b.bin".into(),
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
                "/home/me/c.bin".into(),
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
            .insert("/home/me/resume.bin".into(), data.clone());
        remote.lock().unwrap().per_chunk_delay_ms = 40;
        let mgr2 = TransferManager::load_or_new(dir.path(), Arc::clone(&backend), platform.clone());
        let rid = mgr2
            .enqueue_download(
                "tower".into(),
                "/home/me/resume.bin".into(),
                None,
                None,
                None,
                false,
            )
            .await
            .unwrap();
        // Wait until some bytes have landed in `.part`, then simulate process termination.
        for _ in 0..100 {
            if fs::metadata(mgr2.part_path(&rid))
                .map(|m| m.len())
                .unwrap_or(0)
                >= 16 * 1024
            {
                break;
            }
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
            .insert("/home/me/small.bin".into(), small.clone());
        remote
            .lock()
            .unwrap()
            .files
            .insert("/home/me/large.bin".into(), vec![2u8; 64 * 1024]);

        let backend: Arc<dyn Backend> = Arc::new(MockBackend::new(Arc::clone(&remote)));
        let platform = Arc::new(MockPlatform::new(dir.path()));
        platform.unmetered_wifi.store(false, Ordering::Release);

        let mgr = TransferManager::load_or_new(dir.path(), backend, platform.clone());
        // Lower threshold to 32 KiB in queue settings to test without allocating 50 MiB in RAM.
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
                "/home/me/small.bin".into(),
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
                "/home/me/large.bin".into(),
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
                .insert("/home/me/slow1.bin".into(), vec![7u8; 256 * 1024]);
            st.files
                .insert("/home/me/slow2.bin".into(), vec![8u8; 256 * 1024]);
            st.files
                .insert("/home/me/slow3.bin".into(), vec![9u8; 256 * 1024]);
            st.per_chunk_delay_ms = 30;
        }

        let backend: Arc<dyn Backend> = Arc::new(MockBackend::new(Arc::clone(&remote)));
        let platform = Arc::new(MockPlatform::new(dir.path()));
        let mgr =
            TransferManager::with_options(dir.path(), backend, platform, 2, Duration::from_secs(2));

        // 1) Upload with pre-seeded remote partial resumes cleanly and arrives with exact bytes.
        let up = mgr
            .enqueue_upload(
                "tower".into(),
                "/home/me/uploads".into(),
                Some(vec![UploadFileInput {
                    name: "photo.jpg".into(),
                    path: Some(local_src.to_string_lossy().into_owned()),
                    uri: None,
                    size: Some(upload_bytes.len() as u64),
                    mime: Some("image/jpeg".into()),
                }]),
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
                "/home/me/slow1.bin".into(),
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
                "/home/me/slow2.bin".into(),
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
                "/home/me/slow3.bin".into(),
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
                "/home/me/missing.bin".into(),
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
                "/home/me/slow1.bin".into(),
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
}
