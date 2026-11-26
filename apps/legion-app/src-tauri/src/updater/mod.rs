//! Fleet-sourced native app self-updater.
//!
//! Queries every reachable paired machine with `update.list`, verifies metadata against compiled-in
//! Ed25519 keys, monotonic sequence rules, and signed byte sizes, downloads the matching artifact
//! from any answering node with automatic multi-node fallback and hostile-node bounds, and
//! installs via the platform mechanism:
//!
//! - Android: `PackageInstaller` via [`android`] (`AppUpdaterPlugin`), keeping background installs
//!   strictly silent (never foregrounding Legion or an installer prompt).
//! - Linux AppImage / macOS `.app`: copy-while-hashing + detached atomic swap-and-rollback
//!   supervisor via [`desktop`].
//! - Windows NSIS: copy-while-hashing + backup + detached System32 PowerShell silent installer
//!   and rollback supervisor via [`desktop`].
//! - Linux `.deb`: copies-while-hashing into a private user-only folder (`0o700`) and prints the
//!   exact `sudo apt-get install` command plus verified SHA-256 digest without replacing itself.
//! - Linux pacman / standalone / iOS / sequence-0 builds: never download unusable artifacts and
//!   never replace themselves.

pub mod android;
#[cfg(not(any(target_os = "android", target_os = "ios")))]
pub mod desktop;

use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::future::Future;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::Poll;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use legion_link::Link;
use legion_update::{
    Arch, Artifact, CHECKSUMS_FILENAME, Channel, InstallKind, MANIFEST_FILENAME,
    MAX_ARTIFACT_BYTES, MAX_MANIFEST_BYTES, MAX_SIG_BYTES, MAX_SUMS_BYTES, Os, Program,
    SIGNATURE_FILENAME, SequenceFloorState, SignerKind, TargetPlatform, TrustAnchors,
    VerifiedSetMeta, compiled_preview_key_marker, compiled_sequence_marker, read_sequence_floor,
    recover_sequence_floor, select_verified_artifact, verify_artifact_file, verify_set_metadata,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tauri::ipc::Channel as IpcChannel;
use tempfile::NamedTempFile;
use tokio::sync::watch;
use tokio::time::Instant;

use crate::backend::{AppError, Backend, BoxFuture, ErrorCode};
#[cfg(not(any(target_os = "android", target_os = "ios")))]
use crate::updater::desktop::{
    DEFAULT_STARTUP_WAIT_SECS, DesktopInstall, copy_and_verify_sha256, detect_desktop_install,
    ensure_private_dir,
};

/// Chunk size requested per `update.read` call (512 KiB).
const DOWNLOAD_CHUNK_BYTES: u64 = 512 * 1024;

/// Upper bound on a single downloaded app installer (512 MiB).
const MAX_DOWNLOAD_BYTES: u64 = MAX_ARTIFACT_BYTES;

/// Maximum artifact size downloaded automatically on a metered connection without an explicit tap
/// on "Update now" (5 MiB). Metadata checks remain allowed; larger bundles pause with a plain note.
pub const METERED_AUTO_DOWNLOAD_MAX_BYTES: u64 = 5 * 1024 * 1024;

/// Maximum distinct update candidates accepted from any single node's `update.list`.
pub const MAX_CANDIDATES_PER_NODE: usize = 16;

/// Maximum distinct update candidates examined across all nodes in a single check.
pub const MAX_CANDIDATES_TOTAL: usize = 32;

/// Maximum metadata bytes (`SHA256SUMS`, `SHA256SUMS.sig`, `update.json`) downloaded from one node
/// during a single check (2 MiB).
pub const MAX_METADATA_BYTES_PER_NODE: usize = 2 * 1024 * 1024;

/// Maximum `update.read` chunks accepted for one small metadata file.
const MAX_SMALL_FILE_CHUNKS: usize = 32;

/// Minimum assumed chunk size when bounding chunk count for an artifact download (4 KiB),
/// stopping a hostile node from trickling 1 byte per RPC call.
const MIN_EXPECTED_CHUNK_BYTES: u64 = 4096;

/// Minimum sustained download progress rate (16 KiB/s) enforced after [`PROGRESS_GRACE_WINDOW`].
pub const MIN_PROGRESS_BYTES_PER_SEC: u64 = 16 * 1024;

/// Grace window before enforcing [`MIN_PROGRESS_BYTES_PER_SEC`].
const PROGRESS_GRACE_WINDOW: Duration = Duration::from_secs(5);

/// Minimum sustained metadata progress rate (4 KiB/s) enforced after [`METADATA_PROGRESS_GRACE`].
const MIN_METADATA_BYTES_PER_SEC: u64 = 4 * 1024;

/// Grace window before enforcing [`MIN_METADATA_BYTES_PER_SEC`] during metadata reads.
const METADATA_PROGRESS_GRACE: Duration = Duration::from_secs(2);

/// Per-node timeout for concurrent `update.list` calls.
const LIST_NODE_TIMEOUT: Duration = Duration::from_secs(10);

/// Shorter `update.list` timeout for nodes already deprioritized from an earlier timeout or bad set.
const BAD_NODE_LIST_TIMEOUT: Duration = Duration::from_secs(3);

/// Per-RPC timeout for metadata `update.read` calls (`SHA256SUMS`, `SHA256SUMS.sig`, `update.json`).
const METADATA_CALL_TIMEOUT: Duration = Duration::from_secs(8);

/// Per-(candidate, node) wall-clock budget for reading and verifying a candidate's metadata files.
const METADATA_CANDIDATE_TIMEOUT: Duration = Duration::from_secs(15);

/// Total metadata wall-clock budget spent probing any single node within one update check.
const METADATA_NODE_TOTAL_TIMEOUT: Duration = Duration::from_secs(20);

/// Maximum metadata verification/timeout failures tolerated from one node within a single check
/// before skipping that node's remaining candidates in that check.
const MAX_METADATA_FAILURES_PER_NODE_PER_CHECK: usize = 4;

/// How long a node or `(channel, sequence, node)` source stays deprioritized before decay forgets it.
pub const BAD_NODE_TTL: Duration = Duration::from_secs(20 * 60);

/// Minimum wall-clock deadline for an artifact download attempt.
const MIN_DOWNLOAD_DEADLINE: Duration = Duration::from_secs(180);

/// Extra slack added on top of `remaining_bytes / MIN_PROGRESS_BYTES_PER_SEC` for an artifact download.
const DOWNLOAD_DEADLINE_SLACK: Duration = Duration::from_secs(60);

/// Upper cap on a single artifact download deadline (10 hours; > 512 MiB at 16 KiB/s).
const MAX_DOWNLOAD_DEADLINE: Duration = Duration::from_secs(10 * 3600);

/// Maximum time after launch during which a `"startup"` automatic install may run (and only
/// before the interface has shown its first full screen).
pub const STARTUP_AUTO_INSTALL_WINDOW: Duration = Duration::from_secs(15);

/// Minimum interval enforced natively between page-initiated `update_maybe_auto_install` requests.
pub const AUTO_INSTALL_RATE_LIMIT: Duration = Duration::from_secs(5);

/// Per-chunk timeout for artifact `update.read` calls.
const CHUNK_CALL_TIMEOUT: Duration = Duration::from_secs(30);

/// Minimum cooldown (1 hour) after a first desktop rollback before retrying the same sequence once
/// automatically at a later natural install point.
pub const ROLLBACK_RETRY_COOLDOWN_SECS: i64 = 60 * 60;

/// Hard floor under all automatic install triggers: at most one automatic install attempt per hour
/// per install, regardless of sequence (manual "Update now" and "Try again" are exempt from this
/// floor but still record their attempt).
pub const AUTO_INSTALL_HOURLY_FLOOR_SECS: i64 = 60 * 60;

/// Number of rollbacks or started attempts on the same sequence before Legion skips automatic
/// installation until a newer sequence arrives or the user taps "Try again".
pub const MAX_AUTO_ROLLBACK_ATTEMPTS: u32 = 2;

/// Plain-language headline shown when a new desktop build failed to start and was rolled back.
pub const ROLLBACK_REASON: &str =
    "The new version did not start cleanly, so Legion kept the previous copy.";

/// Computes the download deadline scaled to `remaining_bytes` and [`MIN_PROGRESS_BYTES_PER_SEC`],
/// guaranteeing an honest node sustaining at least the rate floor can always finish.
pub fn download_deadline_for_bytes(remaining_bytes: u64) -> Duration {
    let at_floor_secs = remaining_bytes.div_ceil(MIN_PROGRESS_BYTES_PER_SEC);
    let total_secs = at_floor_secs.saturating_add(DOWNLOAD_DEADLINE_SLACK.as_secs());
    Duration::from_secs(total_secs.clamp(
        MIN_DOWNLOAD_DEADLINE.as_secs(),
        MAX_DOWNLOAD_DEADLINE.as_secs(),
    ))
}

/// Build-time app semantic version from `Cargo.toml`.
pub const APP_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Build-time short revision embedded by `build.rs`.
pub const APP_BUILD_ID: &str = env!("LEGION_APP_BUILD_ID");

/// Parses the build-time update sequence emitted by `build.rs` (`0` for dev builds) and retains the
/// static `LEGION_UPDATE_SEQUENCE=<seq>\0` marker in compiled binaries for packaging inspection.
pub fn current_app_sequence() -> u64 {
    let _ = std::hint::black_box(compiled_sequence_marker());
    let _ = std::hint::black_box(compiled_preview_key_marker());
    env!("LEGION_APP_SEQUENCE").parse::<u64>().unwrap_or(0)
}

/// Identifies the currently running app build and platform.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CurrentBuildInfo {
    /// Semantic version string (`Cargo.toml`).
    pub version: String,
    /// Short revision string embedded at build time.
    pub build: String,
    /// Strictly growing update sequence embedded at build time (`0` for dev builds; disables updates).
    pub sequence: u64,
    /// Operating system name (`android`, `ios`, `linux`, `macos`, `windows`).
    pub os: String,
    /// CPU architecture (`aarch64`, `x86_64`).
    pub arch: String,
    /// Installation kind label (`apk`, `appimage`, `deb`, `app-tar`, `nsis`, `standalone`, `ios`).
    pub install_kind: String,
    /// Whether this build compiled in a preview update signing key.
    pub preview_enabled: bool,
    /// `SHA256:<base64>` fingerprint of the compiled-in release signing key.
    pub release_key_fingerprint: String,
    /// `SHA256:<base64>` fingerprint of the compiled-in preview key, when enabled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preview_key_fingerprint: Option<String>,
}

/// Details of the newest verified update available from the fleet.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AvailableUpdateInfo {
    /// Update channel (`"stable"` or `"preview"`).
    pub channel: Channel,
    /// Semantic version string.
    pub version: String,
    /// Short revision string.
    pub build: String,
    /// Strictly growing sequence number.
    pub sequence: u64,
    /// RFC 3339 UTC publication timestamp.
    pub published_at: String,
    /// Plain-language release notes from `update.json`.
    pub notes: String,
    /// Which compiled-in trusted key verified this update (`"release"` or `"preview"`).
    pub signer: SignerKind,
    /// `SHA256:<base64>` fingerprint of the key that verified this update.
    pub key_fingerprint: String,
    /// Fleet machine that served this update set.
    pub source_node: String,
    /// Bare filename of the selected artifact.
    pub artifact_name: String,
    /// Expected SHA-256 digest of the selected artifact.
    pub artifact_sha256: String,
    /// Signed exact artifact size in bytes from `update.json`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact_size: Option<u64>,
    /// Whether the artifact has been downloaded and verified on disk.
    pub downloaded: bool,
}

/// Live progress while downloading an artifact via `update.read`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DownloadProgress {
    /// Bytes downloaded so far.
    pub downloaded_bytes: u64,
    /// Total artifact size in bytes, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_bytes: Option<u64>,
}

/// One-time notice surfaced on the first start after an update succeeds.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct JustUpdatedInfo {
    /// Channel of the installed update.
    pub channel: Channel,
    /// Version installed.
    pub version: String,
    /// Short revision string.
    pub build: String,
    /// Sequence installed.
    pub sequence: u64,
    /// Plain-language release notes for "What is new".
    pub notes: String,
    /// RFC 3339 UTC timestamp when the new build confirmed startup.
    pub updated_at: String,
}

/// Notice surfaced when a desktop update crashed or failed to start and the previous copy returned.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RollbackInfo {
    /// Version that failed to start.
    pub version: String,
    /// Sequence that failed to start.
    pub sequence: u64,
    /// Plain-language explanation.
    pub reason: String,
    /// Optional captured startup output or supervisor exit reason shown ONLY under Technical details.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// Exact install directory state recorded by the supervisor (`"restored_previous"` or
    /// `"new_kept_files_in_use"`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub install_state: Option<String>,
}

/// Marker written by a desktop supervisor when it deferred cleanly before touching the install
/// because another process held the install open past the pre-swap wait bound.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct DeferredInstallRecord {
    sequence: u64,
    #[serde(default)]
    reason: String,
}

fn default_failed_attempts() -> u32 {
    1
}

fn default_failed_skipped() -> bool {
    true
}

/// Persisted record of an update sequence that failed to install or start on this device.
///
/// After a first desktop rollback, the same sequence is retried once automatically at a later
/// natural install point at least [`ROLLBACK_RETRY_COOLDOWN_SECS`] (1 hour) later. After a second
/// rollback (or a non-rollback install error), `skipped` becomes `true` and the sequence is skipped
/// until a strictly newer sequence arrives or the user taps "Try again".
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FailedUpdateRecord {
    /// Update channel of the failed set.
    pub channel: Channel,
    /// Semantic version that failed.
    pub version: String,
    /// Sequence number that failed.
    pub sequence: u64,
    /// Calm plain-language reason shown to the user.
    pub reason: String,
    /// RFC 3339 UTC timestamp when the failure was recorded.
    pub failed_at: String,
    /// Number of failed attempts recorded for this sequence.
    #[serde(default = "default_failed_attempts")]
    pub attempts: u32,
    /// True once automatic installation of this sequence is skipped until a newer sequence arrives
    /// or the user taps "Try again".
    #[serde(default = "default_failed_skipped")]
    pub skipped: bool,
    /// RFC 3339 UTC timestamp after which a single automatic retry is permitted (`None` when skipped).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_after: Option<String>,
    /// Captured first-start output or supervisor diagnostic shown ONLY under Technical details.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// Complete updater state exposed to the interface via `update_state` and `update_watch`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpdateState {
    /// Lifecycle phase: `"up_to_date"`, `"checking"`, `"available"`, `"downloading"`, `"ready"`,
    /// `"installing"`, `"manual"`, `"needs_permission"`, `"unsupported"`, or `"error"`.
    pub phase: String,
    /// Current running build metadata.
    pub current: CurrentBuildInfo,
    /// Highest verified sequence floor persisted on this device.
    pub sequence_floor: u64,
    /// Whether automatic updates (download when seen, install on start or leaving the app) are on.
    pub auto_update: bool,
    /// Whether this installation can replace itself in place.
    pub can_self_install: bool,
    /// Plain explanation when in-place replacement is not supported on this install.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unsupported_reason: Option<String>,
    /// Exact shell command to run for package-managed installs (such as `.deb`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub manual_command: Option<String>,
    /// Verified SHA-256 hex digest of the private copy referenced by `manual_command` (`.deb`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub manual_sha256: Option<String>,
    /// Newest verified update available, when one exists.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub available: Option<AvailableUpdateInfo>,
    /// Download progress while `phase == "downloading"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub progress: Option<DownloadProgress>,
    /// True when automatic download paused because the artifact exceeds the metered-network cap.
    #[serde(default)]
    pub metered_paused: bool,
    /// RFC 3339 UTC timestamp of the last check attempt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_checked_at: Option<String>,
    /// Fleet machines queried during the last check.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub checked_nodes: Vec<String>,
    /// One-time "Updated to …" notice until dismissed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub just_updated: Option<JustUpdatedInfo>,
    /// Notice shown when a desktop update rolled back to the previous copy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rolled_back: Option<RollbackInfo>,
    /// Latest failed update sequence (if it matches the newest available update), shown with "Try again".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failed_update: Option<FailedUpdateRecord>,
    /// Android capability details when running on Android.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub android: Option<android::AndroidCapabilities>,
    /// Plain-language app headline error (never raw node or OS error text).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    /// Raw diagnostic detail shown ONLY under Technical details.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error_detail: Option<String>,
}

/// Subset of `update.list` returned by a node.
#[derive(Clone, Debug, Deserialize)]
pub struct NodeUpdateList {
    /// Node name answering.
    #[serde(default)]
    pub node: String,
    /// Update sets held on that node.
    #[serde(default)]
    pub sets: Vec<NodeListedSet>,
}

/// One update set entry from a node's `update.list`.
#[derive(Clone, Debug, Deserialize)]
pub struct NodeListedSet {
    /// Update channel.
    pub channel: Channel,
    /// Sequence number.
    pub sequence: u64,
    /// Artifacts reported by the node (used only as a fast pre-filter before full verification).
    #[serde(default)]
    pub artifacts: Vec<NodeListedArtifact>,
}

/// Artifact hint from `update.list`.
#[derive(Clone, Debug, Deserialize)]
pub struct NodeListedArtifact {
    /// Bare filename.
    pub name: String,
    /// Target program.
    pub program: Program,
    /// Target OS.
    pub os: Os,
    /// Target architecture.
    pub arch: Arch,
    /// Install kind.
    pub kind: InstallKind,
    /// Optional size in bytes.
    #[serde(default)]
    pub size: Option<u64>,
}

/// One chunk returned by `update.read`.
#[derive(Clone, Debug, Deserialize)]
pub struct UpdateReadChunk {
    /// Total file size in bytes.
    pub size: u64,
    /// Byte offset where this chunk starts.
    pub offset: u64,
    /// Raw byte length encoded in `content`.
    pub length: u64,
    /// Base64-encoded chunk bytes.
    pub content: String,
    /// Whether this chunk reached end of file.
    pub eof: bool,
}

/// Transport abstraction for querying `update.list` and `update.read` from paired machines.
pub trait UpdateSourceClient: Send + Sync {
    /// Calls `update.list` on `node`.
    fn list_sets(&self, node: &str) -> BoxFuture<'_, Result<NodeUpdateList, AppError>>;

    /// Calls `update.read` on `node`.
    fn read_chunk(
        &self,
        node: &str,
        channel: Channel,
        sequence: u64,
        name: &str,
        offset: u64,
        length: u64,
    ) -> BoxFuture<'_, Result<UpdateReadChunk, AppError>>;
}

/// Production [`UpdateSourceClient`] backed by [`legion_link::Link`].
struct LinkUpdateClient {
    link: Link,
}

impl UpdateSourceClient for LinkUpdateClient {
    fn list_sets(&self, node: &str) -> BoxFuture<'_, Result<NodeUpdateList, AppError>> {
        let link = self.link.clone();
        let node = node.to_owned();
        Box::pin(async move {
            let routed = tokio::time::timeout(
                LIST_NODE_TIMEOUT,
                link.call_machine(&node, "update.list", json!({}), |_| {}),
            )
            .await
            .map_err(|_| {
                AppError::new(
                    ErrorCode::Timeout,
                    format!("Timed out asking {node} for updates."),
                )
            })?
            .map_err(AppError::from)?;
            serde_json::from_value(routed.value).map_err(|err| {
                AppError::new(
                    ErrorCode::Failed,
                    format!("Could not read update list from {node}: {err}"),
                )
            })
        })
    }

    fn read_chunk(
        &self,
        node: &str,
        channel: Channel,
        sequence: u64,
        name: &str,
        offset: u64,
        length: u64,
    ) -> BoxFuture<'_, Result<UpdateReadChunk, AppError>> {
        let link = self.link.clone();
        let node = node.to_owned();
        let name = name.to_owned();
        Box::pin(async move {
            let args = json!({
                "channel": channel,
                "sequence": sequence,
                "name": name,
                "offset": offset,
                "length": length,
            });
            let routed = tokio::time::timeout(
                CHUNK_CALL_TIMEOUT,
                link.call_machine(&node, "update.read", args, |_| {}),
            )
            .await
            .map_err(|_| {
                AppError::new(
                    ErrorCode::Timeout,
                    format!("Timed out downloading update from {node}."),
                )
            })?
            .map_err(AppError::from)?;
            serde_json::from_value(routed.value).map_err(|err| {
                AppError::new(
                    ErrorCode::Failed,
                    format!("Could not read update chunk from {node}: {err}"),
                )
            })
        })
    }
}

/// Platform installation environment resolved at startup.
#[derive(Clone, Debug)]
pub enum PlatformEnv {
    /// Android APK self-installer.
    Android,
    /// iOS (updates delivered via App Store / TestFlight only).
    Ios,
    /// Desktop installation target.
    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    Desktop(DesktopInstall),
}

impl PlatformEnv {
    /// Detects the active platform environment.
    pub fn detect() -> Self {
        #[cfg(target_os = "android")]
        {
            Self::Android
        }
        #[cfg(target_os = "ios")]
        {
            Self::Ios
        }
        #[cfg(not(any(target_os = "android", target_os = "ios")))]
        {
            Self::Desktop(detect_desktop_install())
        }
    }

    /// Returns the `legion-update` [`TargetPlatform`] for this platform/install, or `None` on iOS.
    pub fn target_platform(&self, os: Os, arch: Arch) -> Option<TargetPlatform> {
        let _ = os;
        match self {
            Self::Android => Some(TargetPlatform {
                program: Program::App,
                os: Os::Android,
                arch,
                kind: InstallKind::Apk,
            }),
            Self::Ios => None,
            #[cfg(not(any(target_os = "android", target_os = "ios")))]
            Self::Desktop(desk) => Some(TargetPlatform {
                program: Program::App,
                os,
                arch,
                kind: desk.artifact_kind(),
            }),
        }
    }

    fn install_label(&self) -> &'static str {
        match self {
            Self::Android => "apk",
            Self::Ios => "ios",
            #[cfg(not(any(target_os = "android", target_os = "ios")))]
            Self::Desktop(desk) => desk.label(),
        }
    }

    fn can_self_install(&self) -> bool {
        match self {
            Self::Android => true,
            Self::Ios => false,
            #[cfg(not(any(target_os = "android", target_os = "ios")))]
            Self::Desktop(desk) => desk.can_self_install(),
        }
    }

    fn should_download_artifact(&self) -> bool {
        match self {
            Self::Android => true,
            Self::Ios => false,
            #[cfg(not(any(target_os = "android", target_os = "ios")))]
            Self::Desktop(desk) => desk.should_download_artifact(),
        }
    }

    fn is_deb(&self) -> bool {
        #[cfg(not(any(target_os = "android", target_os = "ios")))]
        {
            matches!(self, Self::Desktop(DesktopInstall::LinuxDeb { .. }))
        }
        #[cfg(any(target_os = "android", target_os = "ios"))]
        {
            false
        }
    }

    fn unsupported_reason(&self) -> Option<String> {
        match self {
            Self::Android => None,
            Self::Ios => {
                Some("Updates on iOS are delivered through TestFlight or the App Store.".to_owned())
            }
            #[cfg(not(any(target_os = "android", target_os = "ios")))]
            Self::Desktop(DesktopInstall::Standalone { reason, .. }) => Some(reason.clone()),
            #[cfg(not(any(target_os = "android", target_os = "ios")))]
            Self::Desktop(_) => None,
        }
    }

    fn manual_command_for(&self, artifact_path: Option<&Path>) -> Option<String> {
        let _ = artifact_path;
        match self {
            Self::Android | Self::Ios => None,
            #[cfg(not(any(target_os = "android", target_os = "ios")))]
            Self::Desktop(desk) => desk.manual_command_for(artifact_path),
        }
    }
}

fn default_pending_stage() -> String {
    "swapped".to_owned()
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct PendingInstallRecord {
    channel: Channel,
    version: String,
    build: String,
    sequence: u64,
    notes: String,
    from_sequence: u64,
    started_at: String,
    /// Lifecycle stage reached by the install: `"staged"`, `"swapping"`, `"swapped"`, or
    /// `"started"`. Legacy records without `stage` default to `"swapped"`.
    #[serde(default = "default_pending_stage")]
    stage: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct StartedAttemptRecord {
    sequence: u64,
    attempts: u32,
    last_started_at: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct PersistedPrefs {
    #[serde(default = "default_auto_update")]
    auto_update: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_checked_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pending_install: Option<PendingInstallRecord>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    just_updated: Option<JustUpdatedInfo>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    rolled_back: Option<RollbackInfo>,
    #[serde(default)]
    failed_sequences: Vec<u64>,
    #[serde(default)]
    failed_updates: Vec<FailedUpdateRecord>,
    #[serde(default)]
    started_attempts: Vec<StartedAttemptRecord>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_install_attempt_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    stale_update_notice: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    cached_available: Option<AvailableUpdateInfo>,
}

fn default_auto_update() -> bool {
    true
}

impl Default for PersistedPrefs {
    fn default() -> Self {
        Self {
            auto_update: true,
            last_checked_at: None,
            pending_install: None,
            just_updated: None,
            rolled_back: None,
            failed_sequences: Vec::new(),
            failed_updates: Vec::new(),
            started_attempts: Vec::new(),
            last_install_attempt_at: None,
            stale_update_notice: None,
            cached_available: None,
        }
    }
}

impl PersistedPrefs {
    fn has_failed(&self, sequence: u64) -> bool {
        self.failed_sequences.contains(&sequence)
            || self.failed_updates.iter().any(|r| r.sequence == sequence)
            || self.started_attempts_count(sequence) >= MAX_AUTO_ROLLBACK_ATTEMPTS
    }

    fn started_attempts_count(&self, sequence: u64) -> u32 {
        self.started_attempts
            .iter()
            .find(|r| r.sequence == sequence)
            .map_or(0, |r| r.attempts)
    }

    fn record_started_attempt(&mut self, sequence: u64, now_unix: i64) -> u32 {
        let now_str = format_rfc3339_utc(now_unix);
        self.last_install_attempt_at = Some(now_str.clone());
        let next = self.started_attempts_count(sequence).saturating_add(1);
        self.started_attempts.retain(|r| r.sequence != sequence);
        self.started_attempts.push(StartedAttemptRecord {
            sequence,
            attempts: next,
            last_started_at: now_str,
        });
        if self.started_attempts.len() > 16 {
            let drain = self.started_attempts.len() - 16;
            self.started_attempts.drain(0..drain);
        }
        next
    }

    fn decrement_started_attempt(&mut self, sequence: u64) {
        if let Some(rec) = self
            .started_attempts
            .iter_mut()
            .find(|r| r.sequence == sequence)
        {
            rec.attempts = rec.attempts.saturating_sub(1);
        }
        self.started_attempts.retain(|r| r.attempts > 0);
    }

    fn ensure_started_attempts_at_least(
        &mut self,
        sequence: u64,
        min_attempts: u32,
        now_unix: i64,
    ) {
        if min_attempts == 0 {
            return;
        }
        let now_str = format_rfc3339_utc(now_unix);
        if let Some(rec) = self
            .started_attempts
            .iter_mut()
            .find(|r| r.sequence == sequence)
        {
            if rec.attempts < min_attempts {
                rec.attempts = min_attempts;
            }
            return;
        }
        self.started_attempts.push(StartedAttemptRecord {
            sequence,
            attempts: min_attempts,
            last_started_at: now_str,
        });
        if self.started_attempts.len() > 16 {
            let drain = self.started_attempts.len() - 16;
            self.started_attempts.drain(0..drain);
        }
    }

    fn failed_record(&self, sequence: u64) -> Option<FailedUpdateRecord> {
        self.failed_updates
            .iter()
            .find(|r| r.sequence == sequence)
            .cloned()
            .or_else(|| {
                let started = self.started_attempts_count(sequence);
                if self.failed_sequences.contains(&sequence)
                    || started >= MAX_AUTO_ROLLBACK_ATTEMPTS
                {
                    let version = self
                        .cached_available
                        .as_ref()
                        .filter(|a| a.sequence == sequence)
                        .map_or_else(String::new, |a| a.version.clone());
                    let channel = self
                        .cached_available
                        .as_ref()
                        .filter(|a| a.sequence == sequence)
                        .map_or(Channel::Stable, |a| a.channel);
                    let failed_at = self
                        .started_attempts
                        .iter()
                        .find(|r| r.sequence == sequence)
                        .map_or_else(now_rfc3339_utc, |r| r.last_started_at.clone());
                    Some(FailedUpdateRecord {
                        channel,
                        version,
                        sequence,
                        reason: ROLLBACK_REASON.to_owned(),
                        failed_at,
                        attempts: started.max(MAX_AUTO_ROLLBACK_ATTEMPTS),
                        skipped: true,
                        retry_after: None,
                        detail: None,
                    })
                } else {
                    None
                }
            })
    }

    /// Returns whether automatic installation of `sequence` itself is blocked due to prior failures,
    /// rollback cooldown, or reaching [`MAX_AUTO_ROLLBACK_ATTEMPTS`] started attempts.
    fn sequence_failure_block_reason_at(
        &self,
        sequence: u64,
        now_unix: i64,
    ) -> Option<&'static str> {
        if self.started_attempts_count(sequence) >= MAX_AUTO_ROLLBACK_ATTEMPTS {
            return Some("sequence_skipped");
        }
        if let Some(rec) = self.failed_updates.iter().find(|r| r.sequence == sequence) {
            if rec.skipped || rec.attempts >= MAX_AUTO_ROLLBACK_ATTEMPTS {
                return Some("sequence_skipped");
            }
            if let Some(retry_at) = rec.retry_after.as_deref().and_then(parse_rfc3339_utc) {
                if now_unix < retry_at {
                    return Some("rollback_retry_cooldown");
                }
            } else {
                return Some("sequence_skipped");
            }
        }
        if self.failed_sequences.contains(&sequence) {
            return Some("sequence_skipped");
        }
        if let Some(started) = self
            .started_attempts
            .iter()
            .find(|r| r.sequence == sequence && r.attempts > 0)
            && let Some(last_ts) = parse_rfc3339_utc(&started.last_started_at)
            && last_ts <= now_unix.saturating_add(300)
            && now_unix.saturating_sub(last_ts) < ROLLBACK_RETRY_COOLDOWN_SECS
        {
            return Some("rollback_retry_cooldown");
        }
        None
    }

    /// Returns `Some("auto_install_hourly_floor")` whenever any install attempt started within the
    /// last [`AUTO_INSTALL_HOURLY_FLOOR_SECS`] (1 hour), regardless of sequence.
    fn hourly_floor_block_reason_at(&self, now_unix: i64) -> Option<&'static str> {
        if let Some(last_ts) = self
            .last_install_attempt_at
            .as_deref()
            .and_then(parse_rfc3339_utc)
            && last_ts <= now_unix.saturating_add(300)
            && now_unix.saturating_sub(last_ts) < AUTO_INSTALL_HOURLY_FLOOR_SECS
        {
            return Some("auto_install_hourly_floor");
        }
        None
    }

    fn auto_block_reason_at(&self, sequence: u64, now_unix: i64) -> Option<&'static str> {
        self.sequence_failure_block_reason_at(sequence, now_unix)
            .or_else(|| self.hourly_floor_block_reason_at(now_unix))
    }

    fn is_sequence_blocked_at(&self, sequence: u64, now_unix: i64) -> bool {
        self.sequence_failure_block_reason_at(sequence, now_unix)
            .is_some()
    }

    fn record_failure(&mut self, record: FailedUpdateRecord) {
        if record.skipped {
            if !self.failed_sequences.contains(&record.sequence) {
                self.failed_sequences.push(record.sequence);
            }
        } else {
            self.failed_sequences.retain(|&s| s != record.sequence);
        }
        self.failed_updates
            .retain(|r| r.sequence != record.sequence);
        self.failed_updates.push(record);
        // Bound persisted failure list.
        if self.failed_updates.len() > 16 {
            let drain = self.failed_updates.len() - 16;
            self.failed_updates.drain(0..drain);
        }
        if self.failed_sequences.len() > 32 {
            let drain = self.failed_sequences.len() - 32;
            self.failed_sequences.drain(0..drain);
        }
    }

    fn clear_failure(&mut self, sequence: u64) {
        self.failed_sequences.retain(|&s| s != sequence);
        self.failed_updates.retain(|r| r.sequence != sequence);
        self.started_attempts.retain(|r| r.sequence != sequence);
        if self
            .rolled_back
            .as_ref()
            .is_some_and(|r| r.sequence == sequence)
        {
            self.rolled_back = None;
        }
    }
}

#[derive(Clone, Debug)]
struct CandidateSource {
    channel: Channel,
    sequence: u64,
    nodes: Vec<String>,
}

struct VerifiedCandidate {
    meta: VerifiedSetMeta,
    artifact: Artifact,
    expected_sha256: String,
    sums_bytes: Vec<u8>,
    sig_text: String,
    manifest_bytes: Vec<u8>,
    source_node: String,
    all_nodes: Vec<String>,
}

/// One contiguous byte range `[start, end)` written into a `.part` file by `node`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct PartRange {
    start: u64,
    end: u64,
    node: String,
}

/// Sidecar provenance persisted beside `<artifact>.part` (`<artifact>.part.meta.json`) recording
/// which paired node contributed each byte range.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct PartMetadata {
    sha256: String,
    expected_size: u64,
    #[serde(default)]
    ranges: Vec<PartRange>,
}

enum CandidateFetchError {
    /// Set metadata verified cleanly, but the signed manifest carries no artifact for this OS/arch/kind.
    NotForThisPlatform {
        bytes_read: usize,
        elapsed: Duration,
    },
    /// Check was cancelled by a newer generation.
    Cancelled,
    /// Metadata read timed out, stalled, or failed cryptographic/manifest verification.
    Failed {
        err: AppError,
        bytes_read: usize,
        elapsed: Duration,
    },
}

enum DownloadFailure {
    /// Download was cancelled by a newer generation (`.part` preserved).
    Cancelled(AppError),
    /// Network disconnect, timeout, rate floor, or protocol error (`.part` preserved).
    Interrupted(AppError),
    /// Complete `.part` reached signed size but failed final SHA-256 verification (`.part` discarded).
    HashMismatch {
        err: AppError,
        contributors: Vec<String>,
    },
}

/// Shared managed state holding the app's [`UpdateManager`].
pub struct SharedUpdater(pub Arc<UpdateManager>);

/// RAII guard for native UI/command activity outside [`Backend`] (such as Android QR scanner
/// activity) so automatic updates never fire while it is active.
pub struct ExternalOpGuard(Arc<AtomicUsize>);

impl Drop for ExternalOpGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

/// Native app update coordinator persisted under `<updates_base>/updates/`.
pub struct UpdateManager {
    updates_dir: PathBuf,
    anchors: TrustAnchors,
    os: Os,
    arch: Arch,
    platform: PlatformEnv,
    current_version: String,
    current_build: String,
    current_sequence: u64,
    state: Mutex<UpdateState>,
    prefs: Mutex<PersistedPrefs>,
    watchers: Mutex<Vec<IpcChannel<UpdateState>>>,
    /// Tracks `(channel, sequence, node)` sources that failed or stalled on a specific update set,
    /// with timestamps so entries decay after [`BAD_NODE_TTL`].
    bad_sources: Mutex<BTreeMap<(Channel, u64, String), Instant>>,
    /// Tracks nodes that timed out or served unverifiable sets across checks so they are tried last
    /// on subsequent checks and automatically forgotten after [`BAD_NODE_TTL`].
    bad_nodes: Mutex<BTreeMap<String, Instant>>,
    /// Generation counter used to immediately cancel a stalled background check whenever a manual
    /// check or install starts (no mutex is ever held across network waits).
    cancel_tx: watch::Sender<u64>,
    /// Monotonic launch timestamp used to enforce [`STARTUP_AUTO_INSTALL_WINDOW`] natively.
    launched_at: Instant,
    /// Set once `commands::ready` reports that the interface painted its first full screen,
    /// permanently closing the `"startup"` auto-install window for this session.
    first_screen_shown: AtomicBool,
    /// Last page-initiated `update_maybe_auto_install` request timestamp for native rate limiting.
    last_auto_install_request: Mutex<Option<Instant>>,
    /// Count of in-flight native operations tracked directly on the updater (for example Android
    /// QR camera scanning during pairing or future content streams).
    external_operations: Arc<AtomicUsize>,
}

impl UpdateManager {
    /// Creates the production update manager for this app using compiled-in trusted keys, the
    /// bundle's configured version string, and the detected platform/install kind.
    pub fn for_app(updates_base_dir: impl AsRef<Path>, bundle_version: Option<String>) -> Self {
        let version = bundle_version
            .map(|v| v.trim().to_owned())
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| APP_VERSION.to_owned());
        Self::with_config(
            updates_base_dir.as_ref().join("updates"),
            TrustAnchors::compiled_in(),
            current_os(),
            current_arch(),
            PlatformEnv::detect(),
            version,
            APP_BUILD_ID.to_owned(),
            current_app_sequence(),
        )
    }

    /// Creates an update manager with explicit trust anchors, platform, and build identity (used by
    /// unit tests). Never performs synchronous SHA-256 hashing of cached artifacts on the setup
    /// thread; full cryptographic verification runs asynchronously before install or during check.
    #[allow(clippy::too_many_arguments)]
    pub fn with_config(
        updates_dir: PathBuf,
        anchors: TrustAnchors,
        os: Os,
        arch: Arch,
        platform: PlatformEnv,
        current_version: String,
        current_build: String,
        current_sequence: u64,
    ) -> Self {
        Self::with_config_at(
            updates_dir,
            anchors,
            os,
            arch,
            platform,
            current_version,
            current_build,
            current_sequence,
            now_unix_secs(),
        )
    }

    /// Creates an update manager evaluating persisted rollback retry windows at `now_unix`
    /// (seconds since Unix epoch), used directly by deterministic retry tests.
    #[allow(clippy::too_many_arguments)]
    pub fn with_config_at(
        updates_dir: PathBuf,
        anchors: TrustAnchors,
        os: Os,
        arch: Arch,
        platform: PlatformEnv,
        current_version: String,
        current_build: String,
        current_sequence: u64,
        now_unix: i64,
    ) -> Self {
        let floor_path = updates_dir.join("floor.json");
        let sequence_floor = read_floor_value(&floor_path).max(current_sequence);
        let mut prefs = load_prefs(&updates_dir.join("prefs.json")).unwrap_or_default();

        // Reconcile any unread `rollback-*.json`, `deferred-*.json`, `stale-lock.json`, and
        // interrupted `pending_install` records at construction time BEFORE anything can ask the
        // auto-install gate, and clean interrupted staging leftovers when no update lock is held.
        let (reconciled_dirty, saw_rollback, rollback_detail) = reconcile_supervisor_records_at(
            &updates_dir,
            current_sequence,
            &platform,
            &mut prefs,
            now_unix,
        );
        if reconciled_dirty {
            let _ = save_prefs(&updates_dir.join("prefs.json"), &prefs);
        }

        let current = CurrentBuildInfo {
            version: current_version.clone(),
            build: current_build.clone(),
            sequence: current_sequence,
            os: os.as_str().to_owned(),
            arch: arch.as_str().to_owned(),
            install_kind: platform.install_label().to_owned(),
            preview_enabled: anchors.has_preview(),
            release_key_fingerprint: anchors.release_fingerprint(),
            preview_key_fingerprint: anchors.preview_fingerprint(),
        };

        let seq_zero_disabled = current_sequence == 0;
        let can_self_install = !seq_zero_disabled && platform.can_self_install();
        let unsupported_reason = if seq_zero_disabled {
            Some("This build has sequence 0, so self-updates are disabled.".to_owned())
        } else {
            platform.unsupported_reason()
        };
        let mut initial_phase = if seq_zero_disabled || platform.target_platform(os, arch).is_none()
        {
            "unsupported".to_owned()
        } else {
            "up_to_date".to_owned()
        };

        // Fast presence + size-only rehydration of any previously verified cached update (zero
        // synchronous SHA-256 hashing on the setup thread). Full signature & digest verification
        // always runs asynchronously before installing.
        let mut available = None;
        let mut manual_command = None;
        let mut manual_sha256 = None;
        let mut failed_update = None;
        let mut initial_last_error = None;
        let mut initial_last_error_detail = None;
        if !seq_zero_disabled
            && let Some(cached) = &prefs.cached_available
            && cached.sequence > current_sequence
            && cached.sequence > sequence_floor
            && platform.target_platform(os, arch).is_some()
        {
            let set_dir = download_dir_for(&updates_dir, cached.channel, cached.sequence);
            let art_path = set_dir.join(&cached.artifact_name);
            let metadata_present = set_dir.join(CHECKSUMS_FILENAME).is_file()
                && set_dir.join(SIGNATURE_FILENAME).is_file()
                && set_dir.join(MANIFEST_FILENAME).is_file();
            let size_ok = fs::symlink_metadata(&art_path)
                .ok()
                .filter(|m| m.file_type().is_file())
                .is_some_and(|m| cached.artifact_size.is_none_or(|sz| m.len() == sz));

            if metadata_present && size_ok {
                let mut item = cached.clone();
                item.downloaded = true;
                if platform.is_deb() {
                    let verified_copy = updates_dir.join("verified").join(&cached.artifact_name);
                    if verified_copy.is_file() {
                        manual_command = platform.manual_command_for(Some(&verified_copy));
                        manual_sha256 = Some(cached.artifact_sha256.clone());
                    }
                } else {
                    manual_command = platform.manual_command_for(Some(&art_path));
                }

                if prefs.is_sequence_blocked_at(cached.sequence, now_unix)
                    && let Some(fail_rec) = prefs.failed_record(cached.sequence)
                {
                    initial_last_error = Some(fail_rec.reason.clone());
                    initial_last_error_detail = fail_rec.detail.clone();
                    failed_update = Some(fail_rec);
                    initial_phase = "error".to_owned();
                } else if manual_command.is_some() {
                    initial_phase = "manual".to_owned();
                } else if can_self_install {
                    initial_phase = "ready".to_owned();
                } else {
                    initial_phase = "available".to_owned();
                }
                available = Some(item);
            }
        }
        if failed_update.is_none()
            && let Some(last_fail) = prefs
                .failed_updates
                .iter()
                .filter(|r| {
                    r.sequence > sequence_floor
                        && prefs.is_sequence_blocked_at(r.sequence, now_unix)
                })
                .max_by_key(|r| r.sequence)
                .cloned()
        {
            if initial_last_error_detail.is_none() {
                initial_last_error_detail = last_fail.detail.clone();
            }
            failed_update = Some(last_fail);
        } else if saw_rollback && initial_last_error_detail.is_none() {
            initial_last_error_detail = rollback_detail;
        }
        if initial_last_error.is_none() && prefs.stale_update_notice.is_some() {
            initial_last_error = prefs.stale_update_notice.clone();
        }

        let initial_state = UpdateState {
            phase: initial_phase,
            current,
            sequence_floor,
            auto_update: prefs.auto_update,
            can_self_install,
            unsupported_reason,
            manual_command,
            manual_sha256,
            available,
            progress: None,
            metered_paused: false,
            last_checked_at: prefs.last_checked_at.clone(),
            checked_nodes: Vec::new(),
            just_updated: prefs.just_updated.clone(),
            rolled_back: prefs.rolled_back.clone(),
            failed_update,
            android: None,
            last_error: initial_last_error,
            last_error_detail: initial_last_error_detail,
        };

        let (cancel_tx, _rx) = watch::channel(0u64);

        Self {
            updates_dir,
            anchors,
            os,
            arch,
            platform,
            current_version,
            current_build,
            current_sequence,
            state: Mutex::new(initial_state),
            prefs: Mutex::new(prefs),
            watchers: Mutex::new(Vec::new()),
            bad_sources: Mutex::new(BTreeMap::new()),
            bad_nodes: Mutex::new(BTreeMap::new()),
            cancel_tx,
            launched_at: Instant::now(),
            first_screen_shown: AtomicBool::new(false),
            last_auto_install_request: Mutex::new(None),
            external_operations: Arc::new(AtomicUsize::new(0)),
        }
    }

    /// Returns a snapshot of the current update state.
    pub fn snapshot(&self) -> UpdateState {
        self.state.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }

    /// Registers a Tauri IPC channel to receive live state updates and immediately pushes the
    /// current state.
    pub fn watch(&self, channel: IpcChannel<UpdateState>) {
        let current = self.snapshot();
        let _ = channel.send(current);
        let mut guard = self.watchers.lock().unwrap_or_else(|p| p.into_inner());
        if guard.len() >= 8 {
            guard.remove(0);
        }
        guard.push(channel);
    }

    fn bump_generation(&self) -> (u64, watch::Receiver<u64>) {
        let mut rx = self.cancel_tx.subscribe();
        let mut next = 0u64;
        self.cancel_tx.send_modify(|generation| {
            *generation = generation.wrapping_add(1);
            next = *generation;
        });
        // Mark `next` as seen only if no newer caller has already superseded it; if another
        // caller bumped again before we got here, leave the newer value unseen so our very first
        // `cancel_rx.changed().await` returns immediately.
        if *rx.borrow() == next {
            let _ = rx.borrow_and_update();
        }
        (next, rx)
    }

    fn persist_diagnostic_state(&self, st: &UpdateState) {
        if let Ok(bytes) = serde_json::to_vec_pretty(st) {
            let _ = write_atomic(&self.updates_dir.join("last-state.json"), &bytes);
        }
    }

    fn publish_state(&self, next: UpdateState) -> UpdateState {
        {
            let mut guard = self.state.lock().unwrap_or_else(|p| p.into_inner());
            *guard = next.clone();
        }
        self.persist_diagnostic_state(&next);
        let mut watchers = self.watchers.lock().unwrap_or_else(|p| p.into_inner());
        watchers.retain(|ch| ch.send(next.clone()).is_ok());
        next
    }

    fn update_state<F>(&self, f: F) -> UpdateState
    where
        F: FnOnce(&mut UpdateState),
    {
        let next = {
            let mut guard = self.state.lock().unwrap_or_else(|p| p.into_inner());
            f(&mut guard);
            guard.clone()
        };
        self.persist_diagnostic_state(&next);
        let mut watchers = self.watchers.lock().unwrap_or_else(|p| p.into_inner());
        watchers.retain(|ch| ch.send(next.clone()).is_ok());
        next
    }

    /// Confirms startup after the UI paints its first frame (or immediately in desktop verify-only
    /// mode on window close).
    ///
    /// - Writes `started-<sequence>.ok` and cleans older `started-*.ok` markers.
    /// - Writes `current-build.json`.
    /// - Recovers or advances the monotonic `floor.json` at `current_sequence`, surfacing any
    ///   rebuilt corruption truth in `last_error` / `last_error_detail`.
    /// - Transitions a matching `pending_install` into `just_updated`, or records a failed update
    ///   when a `pending_install` did not take effect (`current_sequence != pending.sequence`) or
    ///   when the desktop watchdog left a rollback marker.
    pub fn on_app_ready(&self) {
        self.on_app_ready_at(now_unix_secs());
    }

    /// Returns `true` once the first full screen has painted (`on_app_ready` has run).
    pub fn is_first_screen_shown(&self) -> bool {
        self.first_screen_shown.load(Ordering::SeqCst)
    }

    /// Deterministic variant of [`Self::on_app_ready`] evaluating rollback retry timestamps at
    /// `now_unix` (seconds since Unix epoch).
    pub fn on_app_ready_at(&self, now_unix: i64) {
        self.first_screen_shown.store(true, Ordering::SeqCst);
        if ensure_updates_root(&self.updates_dir).is_err() {
            return;
        }

        if self.current_sequence > 0 {
            let started_marker = self
                .updates_dir
                .join(format!("started-{}.ok", self.current_sequence));
            let _ = write_atomic(&started_marker, b"ok\n");
            cleanup_old_started_markers(&self.updates_dir, self.current_sequence);
        }

        let current_json = json!({
            "version": self.current_version,
            "build": self.current_build,
            "sequence": self.current_sequence,
            "os": self.os.as_str(),
            "arch": self.arch.as_str(),
            "install_kind": self.platform.install_label(),
        });
        if let Ok(bytes) = serde_json::to_vec_pretty(&current_json) {
            let _ = write_atomic(&self.updates_dir.join("current-build.json"), &bytes);
        }

        let floor_path = self.updates_dir.join("floor.json");
        let mut floor_recovery_detail: Option<String> = None;
        let new_floor = if self.current_sequence > 0 {
            match recover_sequence_floor(&floor_path, self.current_sequence) {
                Ok((seq, recovered_detail)) => {
                    floor_recovery_detail = recovered_detail;
                    seq
                }
                Err(err) => {
                    floor_recovery_detail = Some(err.message);
                    read_floor_value(&floor_path).max(self.current_sequence)
                }
            }
        } else {
            read_floor_value(&floor_path)
        };

        let mut prefs = self.prefs.lock().unwrap_or_else(|p| p.into_inner()).clone();
        let (mut prefs_dirty, saw_rollback, rollback_detail) = reconcile_supervisor_records_at(
            &self.updates_dir,
            self.current_sequence,
            &self.platform,
            &mut prefs,
            now_unix,
        );

        // Confirm a matching pending update once the new build renders its first frame (or
        // verifies in headless close-update mode).
        if let Some(pending) = prefs.pending_install.clone()
            && self.current_sequence > 0
            && !saw_rollback
            && pending.sequence == self.current_sequence
        {
            #[cfg(not(any(target_os = "android", target_os = "ios")))]
            desktop::clear_pending_stage(&self.updates_dir, pending.sequence);
            prefs.pending_install = None;
            prefs.rolled_back = None;
            prefs.stale_update_notice = None;
            prefs.cached_available = None;
            prefs
                .failed_sequences
                .retain(|&s| s > self.current_sequence);
            prefs
                .failed_updates
                .retain(|r| r.sequence > self.current_sequence);
            prefs
                .started_attempts
                .retain(|r| r.sequence > self.current_sequence);
            prefs.just_updated = Some(JustUpdatedInfo {
                channel: pending.channel,
                version: pending.version,
                build: pending.build,
                sequence: pending.sequence,
                notes: pending.notes,
                updated_at: format_rfc3339_utc(now_unix),
            });
            prefs_dirty = true;
            cleanup_old_downloads(&self.updates_dir, self.current_sequence);
            #[cfg(not(any(target_os = "android", target_os = "ios")))]
            {
                let _ = fs::remove_file(self.updates_dir.join(desktop::NEW_START_LOG_FILENAME));
                let _ = fs::remove_file(self.updates_dir.join(desktop::ROLLBACK_PS_FILENAME));
            }
        }

        if prefs_dirty {
            let _ = save_prefs(&self.updates_dir.join("prefs.json"), &prefs);
            let mut guard = self.prefs.lock().unwrap_or_else(|p| p.into_inner());
            *guard = prefs.clone();
        }

        let mut st = self.snapshot();
        st.sequence_floor = new_floor.max(self.current_sequence);
        st.just_updated = prefs.just_updated.clone();
        st.rolled_back = prefs.rolled_back.clone();
        if let Some(detail) = floor_recovery_detail.as_ref() {
            st.last_error = Some(
                "Legion rebuilt a damaged update floor file at the running version.".to_owned(),
            );
            st.last_error_detail = Some(detail.clone());
        } else if prefs.stale_update_notice.is_some() && st.last_error.is_none() {
            st.last_error = prefs.stale_update_notice.clone();
        }
        if let Some(avail) = &st.available {
            if avail.sequence <= st.sequence_floor {
                st.available = None;
                st.manual_command = None;
                st.manual_sha256 = None;
                st.failed_update = None;
                if st.phase != "unsupported" {
                    st.phase = "up_to_date".to_owned();
                }
            } else if prefs.is_sequence_blocked_at(avail.sequence, now_unix)
                && let Some(fail) = prefs.failed_record(avail.sequence)
            {
                st.last_error = Some(fail.reason.clone());
                st.last_error_detail = fail.detail.clone().or(floor_recovery_detail);
                st.failed_update = Some(fail);
                if st.phase != "unsupported" {
                    st.phase = "error".to_owned();
                }
            }
        } else if let Some(last_fail) = prefs
            .failed_updates
            .iter()
            .filter(|r| {
                r.sequence > st.sequence_floor && prefs.is_sequence_blocked_at(r.sequence, now_unix)
            })
            .max_by_key(|r| r.sequence)
            .cloned()
        {
            st.last_error_detail = last_fail.detail.clone().or(floor_recovery_detail);
            st.failed_update = Some(last_fail);
        } else if saw_rollback && let Some(detail) = rollback_detail {
            st.last_error_detail = Some(detail);
        }
        self.publish_state(st);
    }

    /// Toggles the "Update automatically" setting (default `true`).
    pub fn set_auto_update(&self, enabled: bool) -> Result<UpdateState, AppError> {
        let mut prefs = self.prefs.lock().unwrap_or_else(|p| p.into_inner()).clone();
        prefs.auto_update = enabled;
        save_prefs(&self.updates_dir.join("prefs.json"), &prefs)?;
        {
            let mut guard = self.prefs.lock().unwrap_or_else(|p| p.into_inner());
            *guard = prefs;
        }
        Ok(self.update_state(|st| {
            st.auto_update = enabled;
        }))
    }

    /// Dismisses the one-time `just_updated`, `rolled_back`, and/or stale-lock notice.
    pub fn dismiss_notice(&self) -> Result<UpdateState, AppError> {
        let mut prefs = self.prefs.lock().unwrap_or_else(|p| p.into_inner()).clone();
        let cleared_stale = prefs.stale_update_notice.take();
        prefs.just_updated = None;
        prefs.rolled_back = None;
        save_prefs(&self.updates_dir.join("prefs.json"), &prefs)?;
        {
            let mut guard = self.prefs.lock().unwrap_or_else(|p| p.into_inner());
            *guard = prefs;
        }
        Ok(self.update_state(|st| {
            st.just_updated = None;
            st.rolled_back = None;
            if cleared_stale.is_some() && st.last_error == cleared_stale {
                st.last_error = None;
            }
        }))
    }

    /// Updates cached Android capability metadata on state.
    pub fn set_android_capabilities(&self, caps: android::AndroidCapabilities) -> UpdateState {
        self.update_state(|st| {
            st.android = Some(caps);
        })
    }

    /// Checks every reachable paired machine in `backend` for newer verified updates.
    ///
    /// When `auto_update` is enabled (or `force_download` is true), downloads and verifies the
    /// selected artifact unless paused by the metered-network cap or an unsupported install kind.
    pub async fn check_with_backend(
        &self,
        backend: &dyn Backend,
        force_download: bool,
        is_metered: bool,
    ) -> Result<UpdateState, AppError> {
        let Some((link, nodes)) = backend.update_sources()? else {
            return Ok(self.snapshot());
        };
        let client = LinkUpdateClient { link };
        self.check_with_client(&client, &nodes, force_download, is_metered)
            .await
    }

    /// Core check + multi-node fallback download logic using `client` across `nodes`.
    ///
    /// Cancels any older in-flight check immediately via generation watch channel so a slow or
    /// hostile paired machine can never freeze the UI or block a manual check.
    pub async fn check_with_client(
        &self,
        client: &dyn UpdateSourceClient,
        nodes: &[String],
        force_download: bool,
        is_metered: bool,
    ) -> Result<UpdateState, AppError> {
        self.check_with_client_at(client, nodes, force_download, is_metered, now_unix_secs())
            .await
    }

    /// Deterministic variant of [`Self::check_with_client`] evaluating rollback retry windows at
    /// `now_unix` (seconds since Unix epoch).
    pub async fn check_with_client_at(
        &self,
        client: &dyn UpdateSourceClient,
        nodes: &[String],
        force_download: bool,
        is_metered: bool,
        now_unix: i64,
    ) -> Result<UpdateState, AppError> {
        if self.current_sequence == 0 {
            return Ok(self.update_state(|st| {
                st.phase = "unsupported".to_owned();
                st.can_self_install = false;
                st.unsupported_reason =
                    Some("This build has sequence 0, so self-updates are disabled.".to_owned());
                st.last_checked_at = Some(now_rfc3339_utc());
            }));
        }

        let Some(target) = self.platform.target_platform(self.os, self.arch) else {
            return Ok(self.update_state(|st| {
                st.phase = "unsupported".to_owned();
                st.last_checked_at = Some(now_rfc3339_utc());
            }));
        };

        let (my_gen, mut cancel_rx) = self.bump_generation();
        let floor_path = self.updates_dir.join("floor.json");
        let _ = ensure_updates_root(&self.updates_dir);

        let (floor, floor_recovery_detail) =
            match recover_sequence_floor(&floor_path, self.current_sequence) {
                Ok(pair) => pair,
                Err(err) => {
                    return Ok(self.update_state(|st| {
                        st.phase = "error".to_owned();
                        st.last_error =
                            Some("Legion could not read its update floor file.".to_owned());
                        st.last_error_detail = Some(err.message);
                    }));
                }
            };

        let prev_state = self.snapshot();
        self.update_state(|st| {
            st.phase = "checking".to_owned();
            st.sequence_floor = floor;
            st.metered_paused = false;
            st.last_error = None;
            st.last_error_detail = None;
        });

        let checked_at = now_rfc3339_utc();
        let mut answered_nodes = Vec::new();
        let mut last_query_detail: Option<String> = None;
        let mut had_timeout = false;

        // Ask all paired nodes concurrently during listing, each with its own short time budget
        // completely separate from the download deadline. Order clean nodes ahead of previously
        // deprioritized nodes so fair candidate merging favors healthy machines first.
        let ordered_list_nodes = self.order_nodes_general(nodes);
        let list_futs: Vec<ListNodeFuture<'_>> = ordered_list_nodes
            .iter()
            .map(|node| {
                let node_name = node.clone();
                let timeout = if self.is_node_deprioritized(node) {
                    BAD_NODE_LIST_TIMEOUT
                } else {
                    LIST_NODE_TIMEOUT
                };
                Box::pin(async move {
                    let res = tokio::time::timeout(timeout, client.list_sets(&node_name))
                        .await
                        .map_err(|_| ());
                    (node_name, res)
                }) as ListNodeFuture<'_>
            })
            .collect();

        let list_results = tokio::select! {
            biased;
            _ = cancel_rx.changed() => {
                return Ok(self.snapshot());
            }
            res = join_all_futures(list_futs) => res,
        };

        if *cancel_rx.borrow() != my_gen {
            return Ok(self.snapshot());
        }

        // Cap eligible candidates per node BEFORE merging round-robin across nodes so two flooding
        // nodes can never fill `MAX_CANDIDATES_TOTAL` and crowd out an honest node listed later.
        let mut per_node_candidates: Vec<(String, Vec<(Channel, u64)>)> = Vec::new();
        for (node, res) in list_results {
            match res {
                Ok(Ok(listed)) => {
                    answered_nodes.push(node.clone());
                    let mut sets = listed.sets;
                    sets.sort_by(|a, b| {
                        b.sequence
                            .cmp(&a.sequence)
                            .then_with(|| a.channel.cmp(&b.channel))
                    });
                    let mut node_cands: Vec<(Channel, u64)> = Vec::new();
                    for set in sets {
                        if node_cands.len() >= MAX_CANDIDATES_PER_NODE {
                            break;
                        }
                        if set.sequence <= self.current_sequence || set.sequence <= floor {
                            continue;
                        }
                        if set.channel == Channel::Preview && !self.anchors.has_preview() {
                            continue;
                        }
                        if legion_update::check_sequence_plausibility(
                            set.sequence,
                            self.current_sequence,
                        )
                        .is_err()
                        {
                            continue;
                        }
                        let key = (set.channel, set.sequence);
                        if !node_cands.contains(&key) {
                            node_cands.push(key);
                        }
                    }
                    per_node_candidates.push((node, node_cands));
                }
                Ok(Err(err)) => {
                    if err.code == ErrorCode::Timeout {
                        had_timeout = true;
                        self.mark_bad_node(&node);
                    }
                    if err.code != ErrorCode::UnknownTool {
                        last_query_detail = Some(format!("{node}: {}", err.message));
                    }
                }
                Err(()) => {
                    had_timeout = true;
                    self.mark_bad_node(&node);
                    last_query_detail =
                        Some(format!("{node}: Timed out asking {node} for updates."));
                }
            }
        }

        let mut by_key: BTreeMap<(Channel, u64), CandidateSource> = BTreeMap::new();
        for rank in 0..MAX_CANDIDATES_PER_NODE {
            for (node, node_cands) in &per_node_candidates {
                let Some(&(channel, sequence)) = node_cands.get(rank) else {
                    continue;
                };
                if let Some(entry) = by_key.get_mut(&(channel, sequence)) {
                    if !entry.nodes.contains(node) {
                        entry.nodes.push(node.clone());
                    }
                } else if by_key.len() < MAX_CANDIDATES_TOTAL {
                    by_key.insert(
                        (channel, sequence),
                        CandidateSource {
                            channel,
                            sequence,
                            nodes: vec![node.clone()],
                        },
                    );
                }
            }
        }

        // Order merged candidates newest sequence first (preferring stable on an exact tie).
        let mut candidates: Vec<CandidateSource> = by_key.into_values().collect();
        candidates.sort_by(|a, b| {
            b.sequence
                .cmp(&a.sequence)
                .then_with(|| a.channel.cmp(&b.channel))
        });

        let prefs_snapshot = self.prefs.lock().unwrap_or_else(|p| p.into_inner()).clone();
        let mut metadata_budget_by_node: BTreeMap<String, usize> = BTreeMap::new();
        let mut metadata_time_by_node: BTreeMap<String, Duration> = BTreeMap::new();
        let mut metadata_failures_by_node: BTreeMap<String, usize> = BTreeMap::new();
        let mut chosen: Option<VerifiedCandidate> = None;
        let mut newest_failed_verified: Option<(VerifiedCandidate, FailedUpdateRecord)> = None;
        let mut refusal_detail: Option<String> = None;
        let mut had_unverifiable_candidate = false;

        'outer: for cand in candidates {
            let ordered_nodes = self.order_nodes_for(cand.channel, cand.sequence, &cand.nodes);
            let eligible_nodes: Vec<String> = ordered_nodes
                .iter()
                .filter(|node| {
                    let used_bytes = metadata_budget_by_node.get(*node).copied().unwrap_or(0);
                    let used_time = metadata_time_by_node
                        .get(*node)
                        .copied()
                        .unwrap_or(Duration::ZERO);
                    let failures = metadata_failures_by_node.get(*node).copied().unwrap_or(0);
                    used_bytes < MAX_METADATA_BYTES_PER_NODE
                        && used_time < METADATA_NODE_TOTAL_TIMEOUT
                        && failures < MAX_METADATA_FAILURES_PER_NODE_PER_CHECK
                })
                .cloned()
                .collect();
            if eligible_nodes.is_empty() {
                continue;
            }

            // Probe clean nodes concurrently first (so a slow sibling node holding the same
            // candidate costs nothing), then fall back to previously deprioritized nodes.
            let (clean_wave, bad_wave): (Vec<String>, Vec<String>) = eligible_nodes
                .into_iter()
                .partition(|n| !self.is_source_or_node_bad(cand.channel, cand.sequence, n));

            for wave in [clean_wave, bad_wave] {
                if wave.is_empty() {
                    continue;
                }
                let outcome = self
                    .probe_candidate_nodes_concurrent(
                        client,
                        &cand,
                        &ordered_nodes,
                        &wave,
                        target,
                        &floor_path,
                        &metadata_budget_by_node,
                        &metadata_time_by_node,
                        my_gen,
                        &mut cancel_rx,
                    )
                    .await;

                match outcome {
                    WaveOutcome::Cancelled => {
                        return Ok(self.snapshot());
                    }
                    WaveOutcome::NotForThisPlatform {
                        node,
                        bytes_read,
                        elapsed,
                        errors,
                    } => {
                        *metadata_budget_by_node.entry(node.clone()).or_insert(0) += bytes_read;
                        *metadata_time_by_node.entry(node).or_insert(Duration::ZERO) += elapsed;
                        for (bad_node, err, b_read, b_elapsed) in errors {
                            *metadata_budget_by_node.entry(bad_node.clone()).or_insert(0) += b_read;
                            *metadata_time_by_node
                                .entry(bad_node.clone())
                                .or_insert(Duration::ZERO) += b_elapsed;
                            *metadata_failures_by_node
                                .entry(bad_node.clone())
                                .or_insert(0) += 1;
                            if err.code == ErrorCode::Timeout {
                                had_timeout = true;
                            }
                            self.mark_bad_source(cand.channel, cand.sequence, &bad_node);
                        }
                        continue 'outer;
                    }
                    WaveOutcome::Verified {
                        verified,
                        bytes_read,
                        elapsed,
                        errors,
                    } => {
                        *metadata_budget_by_node
                            .entry(verified.source_node.clone())
                            .or_insert(0) += bytes_read;
                        *metadata_time_by_node
                            .entry(verified.source_node.clone())
                            .or_insert(Duration::ZERO) += elapsed;
                        for (bad_node, err, b_read, b_elapsed) in errors {
                            *metadata_budget_by_node.entry(bad_node.clone()).or_insert(0) += b_read;
                            *metadata_time_by_node
                                .entry(bad_node.clone())
                                .or_insert(Duration::ZERO) += b_elapsed;
                            *metadata_failures_by_node
                                .entry(bad_node.clone())
                                .or_insert(0) += 1;
                            if err.code == ErrorCode::Timeout {
                                had_timeout = true;
                            }
                            self.mark_bad_source(cand.channel, cand.sequence, &bad_node);
                        }
                        self.clear_bad_node(cand.channel, cand.sequence, &verified.source_node);
                        let seq = verified.meta.manifest.sequence;
                        if !force_download
                            && prefs_snapshot.is_sequence_blocked_at(seq, now_unix)
                            && let Some(fail_rec) = prefs_snapshot.failed_record(seq)
                        {
                            // Skip sequences currently blocked by rollback cooldown or skip limit
                            // so a newer/alternate valid set can still be chosen, while remembering
                            // the newest blocked one so the user can tap "Try again".
                            if newest_failed_verified.is_none() {
                                newest_failed_verified = Some((*verified, fail_rec));
                            }
                            continue 'outer;
                        }
                        chosen = Some(*verified);
                        break 'outer;
                    }
                    WaveOutcome::AllFailed(errors) => {
                        for (bad_node, err, b_read, b_elapsed) in errors {
                            *metadata_budget_by_node.entry(bad_node.clone()).or_insert(0) += b_read;
                            *metadata_time_by_node
                                .entry(bad_node.clone())
                                .or_insert(Duration::ZERO) += b_elapsed;
                            *metadata_failures_by_node
                                .entry(bad_node.clone())
                                .or_insert(0) += 1;
                            if err.code == ErrorCode::Timeout {
                                had_timeout = true;
                            } else {
                                had_unverifiable_candidate = true;
                            }
                            self.mark_bad_source(cand.channel, cand.sequence, &bad_node);
                            refusal_detail = Some(format!("{bad_node}: {}", err.message));
                        }
                    }
                }
            }
        }

        if *cancel_rx.borrow() != my_gen {
            return Ok(self.snapshot());
        }

        let auto_update = self
            .prefs
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .auto_update;

        // If every newer verified update previously failed, surface the newest failed one with
        // "Try again" instead of pretending we are up to date or auto-retrying it.
        if chosen.is_none()
            && let Some((failed_cand, fail_rec)) = newest_failed_verified
        {
            let set_dir = download_dir_for(
                &self.updates_dir,
                failed_cand.meta.manifest.channel,
                failed_cand.meta.manifest.sequence,
            );
            let already_downloaded = verify_downloaded_set_async(
                set_dir,
                failed_cand.artifact.name.clone(),
                self.anchors,
                target,
                self.current_sequence,
                floor_path,
            )
            .await
            .is_ok();
            let avail = AvailableUpdateInfo {
                channel: failed_cand.meta.manifest.channel,
                version: failed_cand.meta.manifest.version.clone(),
                build: failed_cand.meta.manifest.build.clone(),
                sequence: failed_cand.meta.manifest.sequence,
                published_at: failed_cand.meta.manifest.published_at.clone(),
                notes: failed_cand.meta.manifest.notes.clone(),
                signer: failed_cand.meta.signer,
                key_fingerprint: failed_cand.meta.key_fingerprint.clone(),
                source_node: failed_cand.source_node.clone(),
                artifact_name: failed_cand.artifact.name.clone(),
                artifact_sha256: failed_cand.expected_sha256.clone(),
                artifact_size: Some(failed_cand.artifact.size),
                downloaded: already_downloaded,
            };
            self.record_last_checked(&checked_at, Some(avail.clone()));
            return Ok(self.update_state(|st| {
                st.phase = "error".to_owned();
                st.available = Some(avail);
                st.failed_update = Some(fail_rec.clone());
                st.manual_command = None;
                st.manual_sha256 = None;
                st.progress = None;
                st.metered_paused = false;
                st.last_checked_at = Some(checked_at);
                st.checked_nodes = answered_nodes;
                st.last_error = Some(fail_rec.reason);
                st.last_error_detail = fail_rec.detail.or(floor_recovery_detail);
            }));
        }

        let Some(chosen) = chosen else {
            self.record_last_checked(&checked_at, None);
            let had_no_answers = answered_nodes.is_empty() && !nodes.is_empty();
            let check_incomplete = had_timeout || had_unverifiable_candidate;
            if (had_no_answers || check_incomplete)
                && let Some(existing) = prev_state.available.as_ref()
                && existing.downloaded
                && existing.sequence > floor
            {
                return Ok(self.update_state(|st| {
                    st.phase = prev_state.phase.clone();
                    st.last_checked_at = Some(checked_at);
                    st.checked_nodes = answered_nodes;
                    st.last_error = floor_recovery_detail.as_ref().map(|_| {
                        "Legion rebuilt a damaged update floor file at the running version."
                            .to_owned()
                    });
                    st.last_error_detail = floor_recovery_detail
                        .or(refusal_detail)
                        .or(last_query_detail);
                }));
            }
            if check_incomplete {
                return Ok(self.update_state(|st| {
                    st.phase = "error".to_owned();
                    st.available = None;
                    st.manual_command = None;
                    st.manual_sha256 = None;
                    st.progress = None;
                    st.metered_paused = false;
                    st.failed_update = None;
                    st.last_checked_at = Some(checked_at);
                    st.checked_nodes = answered_nodes;
                    st.last_error = Some("Could not check for updates right now.".to_owned());
                    st.last_error_detail = floor_recovery_detail
                        .or(refusal_detail)
                        .or(last_query_detail);
                }));
            }
            return Ok(self.update_state(|st| {
                st.phase = "up_to_date".to_owned();
                st.available = None;
                st.manual_command = None;
                st.manual_sha256 = None;
                st.progress = None;
                st.metered_paused = false;
                st.failed_update = None;
                st.last_checked_at = Some(checked_at);
                st.checked_nodes = answered_nodes;
                st.last_error = floor_recovery_detail.as_ref().map(|_| {
                    "Legion rebuilt a damaged update floor file at the running version.".to_owned()
                });
                st.last_error_detail =
                    floor_recovery_detail
                        .or(refusal_detail)
                        .or(if had_no_answers {
                            last_query_detail
                        } else {
                            None
                        });
            }));
        };

        let set_dir = download_dir_for(
            &self.updates_dir,
            chosen.meta.manifest.channel,
            chosen.meta.manifest.sequence,
        );
        let art_path = set_dir.join(&chosen.artifact.name);

        let mut avail = AvailableUpdateInfo {
            channel: chosen.meta.manifest.channel,
            version: chosen.meta.manifest.version.clone(),
            build: chosen.meta.manifest.build.clone(),
            sequence: chosen.meta.manifest.sequence,
            published_at: chosen.meta.manifest.published_at.clone(),
            notes: chosen.meta.manifest.notes.clone(),
            signer: chosen.meta.signer,
            key_fingerprint: chosen.meta.key_fingerprint.clone(),
            source_node: chosen.source_node.clone(),
            artifact_name: chosen.artifact.name.clone(),
            artifact_sha256: chosen.expected_sha256.clone(),
            artifact_size: Some(chosen.artifact.size),
            downloaded: false,
        };

        // Standalone and pacman installs never download artifacts they cannot install.
        if !self.platform.should_download_artifact() {
            let manual_cmd = self.platform.manual_command_for(None);
            let phase = if manual_cmd.is_some() {
                "manual"
            } else {
                "available"
            };
            self.record_last_checked(&checked_at, None);
            return Ok(self.update_state(|st| {
                st.phase = phase.to_owned();
                st.available = Some(avail);
                st.manual_command = manual_cmd;
                st.manual_sha256 = None;
                st.progress = None;
                st.metered_paused = false;
                st.failed_update = None;
                st.last_checked_at = Some(checked_at);
                st.checked_nodes = answered_nodes;
                st.last_error = None;
                st.last_error_detail = floor_recovery_detail;
            }));
        }

        let already_downloaded = verify_downloaded_set_async(
            set_dir.clone(),
            chosen.artifact.name.clone(),
            self.anchors,
            target,
            self.current_sequence,
            floor_path.clone(),
        )
        .await
        .is_ok();

        if already_downloaded {
            avail.downloaded = true;
            let (manual_cmd, manual_sha) = self.finalize_manual_copy_if_needed(
                &art_path,
                &chosen.artifact.name,
                &chosen.expected_sha256,
                chosen.artifact.size,
            )?;
            self.record_last_checked(&checked_at, Some(avail.clone()));
            let phase = if manual_cmd.is_some() {
                "manual"
            } else if self.platform.can_self_install() {
                "ready"
            } else {
                "available"
            };
            return Ok(self.update_state(|st| {
                st.phase = phase.to_owned();
                st.available = Some(avail);
                st.manual_command = manual_cmd;
                st.manual_sha256 = manual_sha;
                st.progress = None;
                st.metered_paused = false;
                st.failed_update = None;
                st.last_checked_at = Some(checked_at);
                st.checked_nodes = answered_nodes;
                st.last_error = None;
                st.last_error_detail = floor_recovery_detail;
            }));
        }

        // Pause automatic downloads larger than 5 MiB on metered connections until user taps
        // "Update now" (`force_download = true`).
        if !force_download && is_metered && chosen.artifact.size > METERED_AUTO_DOWNLOAD_MAX_BYTES {
            self.record_last_checked(&checked_at, None);
            return Ok(self.update_state(|st| {
                st.phase = "available".to_owned();
                st.available = Some(avail);
                st.manual_command = None;
                st.manual_sha256 = None;
                st.progress = None;
                st.metered_paused = true;
                st.failed_update = None;
                st.last_checked_at = Some(checked_at);
                st.checked_nodes = answered_nodes;
                st.last_error = None;
                st.last_error_detail = floor_recovery_detail;
            }));
        }

        let should_download = force_download || auto_update;
        if !should_download {
            self.record_last_checked(&checked_at, None);
            return Ok(self.update_state(|st| {
                st.phase = "available".to_owned();
                st.available = Some(avail);
                st.manual_command = None;
                st.manual_sha256 = None;
                st.progress = None;
                st.metered_paused = false;
                st.failed_update = None;
                st.last_checked_at = Some(checked_at);
                st.checked_nodes = answered_nodes;
                st.last_error = None;
                st.last_error_detail = floor_recovery_detail;
            }));
        }

        self.update_state(|st| {
            st.phase = "downloading".to_owned();
            st.available = Some(avail.clone());
            st.progress = Some(DownloadProgress {
                downloaded_bytes: 0,
                total_bytes: Some(chosen.artifact.size),
            });
            st.metered_paused = false;
            st.last_checked_at = Some(checked_at.clone());
            st.checked_nodes = answered_nodes.clone();
            st.last_error = None;
            st.last_error_detail = None;
        });

        match self
            .download_verified_candidate_with_fallback(client, &chosen, my_gen, &mut cancel_rx)
            .await
        {
            Ok((final_size, winning_node)) => {
                if *cancel_rx.borrow() != my_gen {
                    return Ok(self.snapshot());
                }
                avail.downloaded = true;
                avail.artifact_size = Some(final_size);
                avail.source_node = winning_node;
                let (manual_cmd, manual_sha) = self.finalize_manual_copy_if_needed(
                    &art_path,
                    &chosen.artifact.name,
                    &chosen.expected_sha256,
                    chosen.artifact.size,
                )?;
                self.record_last_checked(&checked_at, Some(avail.clone()));
                cleanup_other_downloads(
                    &self.updates_dir,
                    chosen.meta.manifest.channel,
                    chosen.meta.manifest.sequence,
                );
                let phase = if manual_cmd.is_some() {
                    "manual"
                } else if self.platform.can_self_install() {
                    "ready"
                } else {
                    "available"
                };
                Ok(self.update_state(|st| {
                    st.phase = phase.to_owned();
                    st.available = Some(avail);
                    st.manual_command = manual_cmd;
                    st.manual_sha256 = manual_sha;
                    st.progress = None;
                    st.metered_paused = false;
                    st.failed_update = None;
                    st.last_error = None;
                    st.last_error_detail = floor_recovery_detail;
                }))
            }
            Err(err) => {
                if *cancel_rx.borrow() != my_gen {
                    return Ok(self.snapshot());
                }
                self.record_last_checked(&checked_at, None);
                Ok(self.update_state(|st| {
                    st.phase = "error".to_owned();
                    st.available = Some(avail);
                    st.progress = None;
                    st.metered_paused = false;
                    // Headline is always the app's own calm sentence; raw node/OS error stays in
                    // Technical details only.
                    st.last_error =
                        Some("Could not finish downloading the update right now.".to_owned());
                    st.last_error_detail = Some(err.message);
                }))
            }
        }
    }

    fn finalize_manual_copy_if_needed(
        &self,
        art_path: &Path,
        artifact_name: &str,
        expected_sha256: &str,
        expected_size: u64,
    ) -> Result<(Option<String>, Option<String>), AppError> {
        #[cfg(not(any(target_os = "android", target_os = "ios")))]
        if self.platform.is_deb() {
            let verified_dir = self.updates_dir.join("verified");
            ensure_private_dir(&verified_dir)?;
            let verified_copy = verified_dir.join(artifact_name);
            copy_and_verify_sha256(
                art_path,
                &verified_copy,
                expected_sha256,
                expected_size,
                0o600,
            )?;
            let cmd = self.platform.manual_command_for(Some(&verified_copy));
            return Ok((cmd, Some(expected_sha256.to_owned())));
        }
        let _ = (artifact_name, expected_sha256, expected_size);
        Ok((self.platform.manual_command_for(Some(art_path)), None))
    }

    fn prune_expired_reputation(&self, now: Instant) {
        {
            let mut bad = self.bad_sources.lock().unwrap_or_else(|p| p.into_inner());
            bad.retain(|_, ts| now.saturating_duration_since(*ts) < BAD_NODE_TTL);
        }
        {
            let mut bad = self.bad_nodes.lock().unwrap_or_else(|p| p.into_inner());
            bad.retain(|_, ts| now.saturating_duration_since(*ts) < BAD_NODE_TTL);
        }
    }

    fn is_node_deprioritized(&self, node: &str) -> bool {
        let now = Instant::now();
        let mut bad = self.bad_nodes.lock().unwrap_or_else(|p| p.into_inner());
        bad.retain(|_, ts| now.saturating_duration_since(*ts) < BAD_NODE_TTL);
        bad.contains_key(node)
    }

    fn is_source_or_node_bad(&self, channel: Channel, sequence: u64, node: &str) -> bool {
        let now = Instant::now();
        self.prune_expired_reputation(now);
        let src_bad = self
            .bad_sources
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .contains_key(&(channel, sequence, node.to_owned()));
        if src_bad {
            return true;
        }
        self.bad_nodes
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .contains_key(node)
    }

    fn order_nodes_general(&self, nodes: &[String]) -> Vec<String> {
        let now = Instant::now();
        self.prune_expired_reputation(now);
        let bad = self.bad_nodes.lock().unwrap_or_else(|p| p.into_inner());
        let mut good_list = Vec::new();
        let mut bad_list = Vec::new();
        for n in nodes {
            if bad.contains_key(n) {
                bad_list.push(n.clone());
            } else {
                good_list.push(n.clone());
            }
        }
        good_list.extend(bad_list);
        good_list
    }

    fn order_nodes_for(&self, channel: Channel, sequence: u64, nodes: &[String]) -> Vec<String> {
        let now = Instant::now();
        self.prune_expired_reputation(now);
        let bad_src = self.bad_sources.lock().unwrap_or_else(|p| p.into_inner());
        let bad_nodes = self.bad_nodes.lock().unwrap_or_else(|p| p.into_inner());
        let mut clean_list = Vec::new();
        let mut node_bad_list = Vec::new();
        let mut src_bad_list = Vec::new();
        for n in nodes {
            if bad_src.contains_key(&(channel, sequence, n.clone())) {
                src_bad_list.push(n.clone());
            } else if bad_nodes.contains_key(n) {
                node_bad_list.push(n.clone());
            } else {
                clean_list.push(n.clone());
            }
        }
        clean_list.extend(node_bad_list);
        clean_list.extend(src_bad_list);
        clean_list
    }

    fn mark_bad_node(&self, node: &str) {
        let now = Instant::now();
        let mut bad = self.bad_nodes.lock().unwrap_or_else(|p| p.into_inner());
        bad.retain(|_, ts| now.saturating_duration_since(*ts) < BAD_NODE_TTL);
        if bad.len() >= 128 {
            bad.clear();
        }
        bad.insert(node.to_owned(), now);
    }

    fn mark_bad_source(&self, channel: Channel, sequence: u64, node: &str) {
        let now = Instant::now();
        {
            let mut bad = self.bad_sources.lock().unwrap_or_else(|p| p.into_inner());
            bad.retain(|_, ts| now.saturating_duration_since(*ts) < BAD_NODE_TTL);
            if bad.len() >= 128 {
                bad.clear();
            }
            bad.insert((channel, sequence, node.to_owned()), now);
        }
        self.mark_bad_node(node);
    }

    fn clear_bad_node(&self, channel: Channel, sequence: u64, node: &str) {
        self.bad_sources
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(&(channel, sequence, node.to_owned()));
        self.bad_nodes
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(node);
    }

    #[allow(clippy::too_many_arguments)]
    async fn probe_candidate_nodes_concurrent(
        &self,
        client: &dyn UpdateSourceClient,
        cand: &CandidateSource,
        ordered_nodes: &[String],
        wave_nodes: &[String],
        target: TargetPlatform,
        floor_path: &Path,
        metadata_budget_by_node: &BTreeMap<String, usize>,
        metadata_time_by_node: &BTreeMap<String, Duration>,
        my_gen: u64,
        cancel_rx: &mut watch::Receiver<u64>,
    ) -> WaveOutcome {
        let mut futs: Vec<(String, Option<CandidateProbeFuture<'_>>)> = wave_nodes
            .iter()
            .map(|node| {
                let used_bytes = metadata_budget_by_node.get(node).copied().unwrap_or(0);
                let budget_left = MAX_METADATA_BYTES_PER_NODE.saturating_sub(used_bytes);
                let used_time = metadata_time_by_node
                    .get(node)
                    .copied()
                    .unwrap_or(Duration::ZERO);
                let time_left = METADATA_NODE_TOTAL_TIMEOUT.saturating_sub(used_time);
                let deprioritized = self.is_source_or_node_bad(cand.channel, cand.sequence, node);
                let cand_budget = if deprioritized {
                    Duration::from_secs(5).min(time_left)
                } else {
                    METADATA_CANDIDATE_TIMEOUT.min(time_left)
                };
                let call_budget = if deprioritized {
                    Duration::from_secs(3)
                } else {
                    METADATA_CALL_TIMEOUT
                };
                let node_deadline = Instant::now() + cand_budget;
                let node_cancel = cancel_rx.clone();
                let fut: CandidateProbeFuture<'_> = Box::pin(self.fetch_and_verify_candidate(
                    client,
                    node,
                    cand,
                    ordered_nodes,
                    target,
                    floor_path,
                    budget_left,
                    node_deadline,
                    call_budget,
                    my_gen,
                    node_cancel,
                ));
                (node.clone(), Some(fut))
            })
            .collect();

        let race_fut = async {
            let mut errors: Vec<(String, AppError, usize, Duration)> = Vec::new();
            let mut pending = futs.len();
            std::future::poll_fn(|cx| {
                for (node, slot) in &mut futs {
                    let Some(fut) = slot.as_mut() else {
                        continue;
                    };
                    if let Poll::Ready(res) = fut.as_mut().poll(cx) {
                        *slot = None;
                        pending -= 1;
                        match res {
                            Ok((verified, bytes_read, elapsed)) => {
                                return Poll::Ready(WaveOutcome::Verified {
                                    verified: Box::new(verified),
                                    bytes_read,
                                    elapsed,
                                    errors: std::mem::take(&mut errors),
                                });
                            }
                            Err(CandidateFetchError::NotForThisPlatform {
                                bytes_read,
                                elapsed,
                            }) => {
                                return Poll::Ready(WaveOutcome::NotForThisPlatform {
                                    node: node.clone(),
                                    bytes_read,
                                    elapsed,
                                    errors: std::mem::take(&mut errors),
                                });
                            }
                            Err(CandidateFetchError::Cancelled) => {
                                return Poll::Ready(WaveOutcome::Cancelled);
                            }
                            Err(CandidateFetchError::Failed {
                                err,
                                bytes_read,
                                elapsed,
                            }) => {
                                errors.push((node.clone(), err, bytes_read, elapsed));
                            }
                        }
                    }
                }
                if pending == 0 {
                    Poll::Ready(WaveOutcome::AllFailed(std::mem::take(&mut errors)))
                } else {
                    Poll::Pending
                }
            })
            .await
        };

        tokio::select! {
            biased;
            _ = cancel_rx.changed() => WaveOutcome::Cancelled,
            outcome = race_fut => outcome,
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn fetch_and_verify_candidate(
        &self,
        client: &dyn UpdateSourceClient,
        node: &str,
        cand: &CandidateSource,
        ordered_nodes: &[String],
        target: TargetPlatform,
        floor_path: &Path,
        budget_left: usize,
        deadline: Instant,
        call_timeout: Duration,
        my_gen: u64,
        mut cancel_rx: watch::Receiver<u64>,
    ) -> Result<(VerifiedCandidate, usize, Duration), CandidateFetchError> {
        let started = Instant::now();
        let mut read_total = 0usize;

        let map_read_err = |err: AppError, read_total: usize, rx: &watch::Receiver<u64>| {
            if *rx.borrow() != my_gen || err.code == ErrorCode::Cancelled {
                CandidateFetchError::Cancelled
            } else {
                CandidateFetchError::Failed {
                    err,
                    bytes_read: read_total,
                    elapsed: started.elapsed(),
                }
            }
        };

        let sums_bytes = read_small_file(
            client,
            node,
            cand.channel,
            cand.sequence,
            CHECKSUMS_FILENAME,
            MAX_SUMS_BYTES.min(budget_left),
            deadline,
            call_timeout,
            &mut cancel_rx,
        )
        .await
        .map_err(|e| map_read_err(e, read_total, &cancel_rx))?;
        read_total += sums_bytes.len();

        let sig_budget = MAX_SIG_BYTES.min(budget_left.saturating_sub(read_total));
        let sig_bytes = read_small_file(
            client,
            node,
            cand.channel,
            cand.sequence,
            SIGNATURE_FILENAME,
            sig_budget,
            deadline,
            call_timeout,
            &mut cancel_rx,
        )
        .await
        .map_err(|e| map_read_err(e, read_total, &cancel_rx))?;
        read_total += sig_bytes.len();

        let sig_text = String::from_utf8(sig_bytes).map_err(|err| CandidateFetchError::Failed {
            err: AppError::new(
                ErrorCode::Denied,
                format!("{SIGNATURE_FILENAME} from {node} is not valid UTF-8: {err}"),
            ),
            bytes_read: read_total,
            elapsed: started.elapsed(),
        })?;
        // Verify signature before reading update.json so untrusted sets are refused immediately.
        self.anchors
            .verify_for_channel(cand.channel, &sums_bytes, &sig_text)
            .map_err(|e| CandidateFetchError::Failed {
                err: AppError::new(ErrorCode::Denied, e.message),
                bytes_read: read_total,
                elapsed: started.elapsed(),
            })?;

        let manifest_budget = MAX_MANIFEST_BYTES.min(budget_left.saturating_sub(read_total));
        let manifest_bytes = read_small_file(
            client,
            node,
            cand.channel,
            cand.sequence,
            MANIFEST_FILENAME,
            manifest_budget,
            deadline,
            call_timeout,
            &mut cancel_rx,
        )
        .await
        .map_err(|e| map_read_err(e, read_total, &cancel_rx))?;
        read_total += manifest_bytes.len();

        let meta = verify_set_metadata(&sums_bytes, &sig_text, &manifest_bytes, &self.anchors)
            .map_err(|e| CandidateFetchError::Failed {
                err: AppError::new(e.code.into(), e.message),
                bytes_read: read_total,
                elapsed: started.elapsed(),
            })?;
        let artifact =
            match select_verified_artifact(&meta, self.current_sequence, floor_path, target) {
                Ok(art) => art.clone(),
                Err(e) if e.code == legion_link::proto::ErrorCode::NotFound => {
                    // Valid signed set that simply carries no artifact for this OS/arch/kind (for
                    // example a node-only update set). Do not blame the node or flag an error.
                    return Err(CandidateFetchError::NotForThisPlatform {
                        bytes_read: read_total,
                        elapsed: started.elapsed(),
                    });
                }
                Err(e) => {
                    return Err(CandidateFetchError::Failed {
                        err: AppError::new(e.code.into(), e.message),
                        bytes_read: read_total,
                        elapsed: started.elapsed(),
                    });
                }
            };

        if meta.manifest.channel != cand.channel || meta.manifest.sequence != cand.sequence {
            return Err(CandidateFetchError::Failed {
                err: AppError::new(
                    ErrorCode::Denied,
                    format!(
                        "Update set from {node} claims {}/{} instead of {}/{}.",
                        meta.manifest.channel, meta.manifest.sequence, cand.channel, cand.sequence
                    ),
                ),
                bytes_read: read_total,
                elapsed: started.elapsed(),
            });
        }

        let expected_sha256 = meta
            .sums
            .get(&artifact.name)
            .ok_or_else(|| CandidateFetchError::Failed {
                err: AppError::new(
                    ErrorCode::Denied,
                    format!(
                        "Artifact {} is missing from {CHECKSUMS_FILENAME}.",
                        artifact.name
                    ),
                ),
                bytes_read: read_total,
                elapsed: started.elapsed(),
            })?
            .to_owned();

        // Put the node that passed metadata verification first, followed by all other nodes.
        let mut all_nodes = vec![node.to_owned()];
        for other in ordered_nodes {
            if other != node && !all_nodes.contains(other) {
                all_nodes.push(other.clone());
            }
        }

        Ok((
            VerifiedCandidate {
                meta,
                artifact,
                expected_sha256,
                sums_bytes,
                sig_text,
                manifest_bytes,
                source_node: node.to_owned(),
                all_nodes,
            },
            read_total,
            started.elapsed(),
        ))
    }

    async fn download_verified_candidate_with_fallback(
        &self,
        client: &dyn UpdateSourceClient,
        cand: &VerifiedCandidate,
        my_gen: u64,
        cancel_rx: &mut watch::Receiver<u64>,
    ) -> Result<(u64, String), AppError> {
        let ordered = self.order_nodes_for(
            cand.meta.manifest.channel,
            cand.meta.manifest.sequence,
            &cand.all_nodes,
        );
        let mut last_err = AppError::new(
            ErrorCode::Failed,
            "No paired machine could serve the update artifact.",
        );
        // Overall download budget is separate from listing/metadata and scales with signed size
        // and the rate floor, with headroom for multi-node fallback.
        let overall_deadline =
            Instant::now() + download_deadline_for_bytes(cand.artifact.size).saturating_mul(2);

        for node in ordered {
            if *cancel_rx.borrow() != my_gen {
                return Err(AppError::new(
                    ErrorCode::Cancelled,
                    "Update check was superseded by a newer request.",
                ));
            }
            match self
                .download_from_single_node(client, &node, cand, false, overall_deadline, cancel_rx)
                .await
            {
                Ok(final_size) => {
                    self.clear_bad_node(
                        cand.meta.manifest.channel,
                        cand.meta.manifest.sequence,
                        &node,
                    );
                    return Ok((final_size, node));
                }
                Err(DownloadFailure::Cancelled(err)) => return Err(err),
                Err(DownloadFailure::Interrupted(err)) => {
                    if *cancel_rx.borrow() != my_gen {
                        return Err(err);
                    }
                    self.mark_bad_source(
                        cand.meta.manifest.channel,
                        cand.meta.manifest.sequence,
                        &node,
                    );
                    last_err = AppError::new(err.code, format!("{node}: {}", err.message));
                }
                Err(DownloadFailure::HashMismatch { err, contributors }) => {
                    if *cancel_rx.borrow() != my_gen {
                        return Err(err);
                    }
                    // Final SHA-256 verification failed: `.part` was already discarded. Mark every
                    // contributing node bad so we remember who wrote bytes into the failed file.
                    let had_other_contributor = contributors.iter().any(|c| c != &node);
                    for contributor in &contributors {
                        self.mark_bad_source(
                            cand.meta.manifest.channel,
                            cand.meta.manifest.sequence,
                            contributor,
                        );
                    }
                    if !contributors.contains(&node) {
                        self.mark_bad_source(
                            cand.meta.manifest.channel,
                            cand.meta.manifest.sequence,
                            &node,
                        );
                    }
                    last_err = AppError::new(err.code, format!("{node}: {}", err.message));

                    // If this node only resumed partial bytes written by another node, retry THIS
                    // node once from offset 0 before blaming it so a poisoned prefix from an
                    // earlier node never permanently condemns a healthy node.
                    if had_other_contributor {
                        match self
                            .download_from_single_node(
                                client,
                                &node,
                                cand,
                                true,
                                overall_deadline,
                                cancel_rx,
                            )
                            .await
                        {
                            Ok(final_size) => {
                                self.clear_bad_node(
                                    cand.meta.manifest.channel,
                                    cand.meta.manifest.sequence,
                                    &node,
                                );
                                return Ok((final_size, node));
                            }
                            Err(DownloadFailure::Cancelled(retry_err)) => return Err(retry_err),
                            Err(
                                DownloadFailure::Interrupted(retry_err)
                                | DownloadFailure::HashMismatch { err: retry_err, .. },
                            ) => {
                                if *cancel_rx.borrow() != my_gen {
                                    return Err(retry_err);
                                }
                                self.mark_bad_source(
                                    cand.meta.manifest.channel,
                                    cand.meta.manifest.sequence,
                                    &node,
                                );
                                last_err = AppError::new(
                                    retry_err.code,
                                    format!("{node}: {}", retry_err.message),
                                );
                            }
                        }
                    }
                }
            }
        }
        Err(last_err)
    }

    async fn download_from_single_node(
        &self,
        client: &dyn UpdateSourceClient,
        node: &str,
        cand: &VerifiedCandidate,
        reset_part: bool,
        overall_deadline: Instant,
        cancel_rx: &mut watch::Receiver<u64>,
    ) -> Result<u64, DownloadFailure> {
        let expected_size = cand.artifact.size;
        if expected_size == 0 || expected_size > MAX_DOWNLOAD_BYTES {
            return Err(DownloadFailure::Interrupted(AppError::new(
                ErrorCode::Denied,
                format!(
                    "Artifact {} declares invalid signed size {expected_size}.",
                    cand.artifact.name
                ),
            )));
        }

        let set_dir = download_dir_for(
            &self.updates_dir,
            cand.meta.manifest.channel,
            cand.meta.manifest.sequence,
        );
        ensure_updates_root(&self.updates_dir).map_err(DownloadFailure::Interrupted)?;
        ensure_updates_root(&self.updates_dir.join("downloads"))
            .map_err(DownloadFailure::Interrupted)?;
        ensure_updates_root(&set_dir).map_err(DownloadFailure::Interrupted)?;

        write_atomic(&set_dir.join(CHECKSUMS_FILENAME), &cand.sums_bytes)
            .map_err(DownloadFailure::Interrupted)?;
        write_atomic(&set_dir.join(SIGNATURE_FILENAME), cand.sig_text.as_bytes())
            .map_err(DownloadFailure::Interrupted)?;
        write_atomic(&set_dir.join(MANIFEST_FILENAME), &cand.manifest_bytes)
            .map_err(DownloadFailure::Interrupted)?;

        let final_path = set_dir.join(&cand.artifact.name);
        let part_path = set_dir.join(format!("{}.part", cand.artifact.name));
        let part_meta_path = set_dir.join(format!("{}.part.meta.json", cand.artifact.name));
        let legacy_hash_marker = set_dir.join(format!("{}.part.sha256", cand.artifact.name));
        let _ = fs::remove_file(&legacy_hash_marker);

        if reset_part {
            let _ = fs::remove_file(&part_path);
            let _ = fs::remove_file(&part_meta_path);
        }

        let (mut offset, mut part_meta) = load_or_init_part_state(
            &part_path,
            &part_meta_path,
            &cand.expected_sha256,
            expected_size,
        )
        .map_err(DownloadFailure::Interrupted)?;
        save_part_metadata(&part_meta_path, &part_meta).map_err(DownloadFailure::Interrupted)?;

        let mut file = if offset > 0 {
            OpenOptions::new()
                .append(true)
                .open(&part_path)
                .map_err(|err| {
                    DownloadFailure::Interrupted(AppError::new(
                        ErrorCode::Failed,
                        format!("Could not resume download {}: {err}", part_path.display()),
                    ))
                })?
        } else {
            fs::File::create(&part_path).map_err(|err| {
                DownloadFailure::Interrupted(AppError::new(
                    ErrorCode::Failed,
                    format!("Could not create download {}: {err}", part_path.display()),
                ))
            })?
        };

        if offset > 0 {
            self.update_state(|st| {
                st.progress = Some(DownloadProgress {
                    downloaded_bytes: offset,
                    total_bytes: Some(expected_size),
                });
            });
        }

        // Scale the attempt deadline with the remaining bytes and rate floor so an honest slow
        // link (at or above MIN_PROGRESS_BYTES_PER_SEC) can always finish.
        let remaining_at_start = expected_size.saturating_sub(offset);
        let attempt_deadline = (Instant::now() + download_deadline_for_bytes(remaining_at_start))
            .min(overall_deadline);

        let max_chunks = (remaining_at_start / MIN_EXPECTED_CHUNK_BYTES)
            .saturating_add(64)
            .min(usize::MAX as u64) as usize;
        let started_at = Instant::now();
        let initial_offset = offset;
        let mut chunks_seen = 0usize;
        let mut bytes_since_meta_save = 0u64;

        let persist_progress = |file: &mut fs::File, meta: &PartMetadata| {
            let _ = file.flush();
            let _ = save_part_metadata(&part_meta_path, meta);
        };

        while offset < expected_size {
            let remaining = expected_size - offset;
            let req_len = DOWNLOAD_CHUNK_BYTES.min(remaining);
            let call_deadline = (Instant::now() + CHUNK_CALL_TIMEOUT).min(attempt_deadline);
            let reply = match run_cancellable(
                cancel_rx,
                call_deadline,
                client.read_chunk(
                    node,
                    cand.meta.manifest.channel,
                    cand.meta.manifest.sequence,
                    &cand.artifact.name,
                    offset,
                    req_len,
                ),
            )
            .await
            {
                Ok(Ok(reply)) => reply,
                Ok(Err(err)) => {
                    persist_progress(&mut file, &part_meta);
                    return Err(DownloadFailure::Interrupted(err));
                }
                Err(err) if err.code == ErrorCode::Cancelled => {
                    persist_progress(&mut file, &part_meta);
                    return Err(DownloadFailure::Cancelled(err));
                }
                Err(err) => {
                    persist_progress(&mut file, &part_meta);
                    return Err(DownloadFailure::Interrupted(err));
                }
            };

            if reply.offset != offset {
                persist_progress(&mut file, &part_meta);
                return Err(DownloadFailure::Interrupted(AppError::new(
                    ErrorCode::Failed,
                    format!(
                        "Unexpected chunk offset {} (expected {offset}) for {}.",
                        reply.offset, cand.artifact.name
                    ),
                )));
            }
            if reply.size != expected_size {
                persist_progress(&mut file, &part_meta);
                return Err(DownloadFailure::Interrupted(AppError::new(
                    ErrorCode::Failed,
                    format!(
                        "Reported size {} for {} does not match signed size {expected_size}.",
                        reply.size, cand.artifact.name
                    ),
                )));
            }
            let chunk = match BASE64.decode(reply.content.as_bytes()) {
                Ok(c) => c,
                Err(err) => {
                    persist_progress(&mut file, &part_meta);
                    return Err(DownloadFailure::Interrupted(AppError::new(
                        ErrorCode::Failed,
                        format!("Invalid base64 chunk for {}: {err}", cand.artifact.name),
                    )));
                }
            };
            if chunk.len() as u64 != reply.length {
                persist_progress(&mut file, &part_meta);
                return Err(DownloadFailure::Interrupted(AppError::new(
                    ErrorCode::Failed,
                    format!("Inconsistent chunk length for {}.", cand.artifact.name),
                )));
            }
            if chunk.len() as u64 > req_len {
                persist_progress(&mut file, &part_meta);
                return Err(DownloadFailure::Interrupted(AppError::new(
                    ErrorCode::Failed,
                    format!(
                        "Chunk length {} exceeded requested {req_len} bytes for {}.",
                        chunk.len(),
                        cand.artifact.name
                    ),
                )));
            }
            if offset.saturating_add(chunk.len() as u64) > expected_size {
                persist_progress(&mut file, &part_meta);
                return Err(DownloadFailure::Interrupted(AppError::new(
                    ErrorCode::Failed,
                    format!(
                        "Download exceeded signed size ({expected_size} bytes) for {}.",
                        cand.artifact.name
                    ),
                )));
            }
            if !chunk.is_empty() {
                let write_start = offset;
                if let Err(err) = file.write_all(&chunk) {
                    persist_progress(&mut file, &part_meta);
                    return Err(DownloadFailure::Interrupted(AppError::new(
                        ErrorCode::Failed,
                        format!("Could not write downloaded update: {err}"),
                    )));
                }
                offset = offset.saturating_add(chunk.len() as u64);
                record_part_range(&mut part_meta, write_start, offset, node);
                bytes_since_meta_save = bytes_since_meta_save.saturating_add(chunk.len() as u64);
                if bytes_since_meta_save >= DOWNLOAD_CHUNK_BYTES {
                    let _ = save_part_metadata(&part_meta_path, &part_meta);
                    bytes_since_meta_save = 0;
                }
                self.update_state(|st| {
                    st.progress = Some(DownloadProgress {
                        downloaded_bytes: offset,
                        total_bytes: Some(expected_size),
                    });
                });
            }

            chunks_seen += 1;
            if offset == expected_size {
                break;
            }
            if reply.eof {
                persist_progress(&mut file, &part_meta);
                return Err(DownloadFailure::Interrupted(AppError::new(
                    ErrorCode::Failed,
                    format!("Download ended early at {offset} bytes (expected {expected_size})."),
                )));
            }
            if chunk.is_empty() {
                persist_progress(&mut file, &part_meta);
                return Err(DownloadFailure::Interrupted(AppError::new(
                    ErrorCode::Failed,
                    "Download stopped making progress.",
                )));
            }
            if chunks_seen >= max_chunks {
                persist_progress(&mut file, &part_meta);
                return Err(DownloadFailure::Interrupted(AppError::new(
                    ErrorCode::Failed,
                    format!(
                        "Download used too many tiny chunks ({chunks_seen}) for {}.",
                        cand.artifact.name
                    ),
                )));
            }
            let elapsed = started_at.elapsed();
            if elapsed >= PROGRESS_GRACE_WINDOW {
                let received = offset.saturating_sub(initial_offset);
                let min_bytes = (elapsed.as_secs_f64() * MIN_PROGRESS_BYTES_PER_SEC as f64) as u64;
                if received < min_bytes {
                    persist_progress(&mut file, &part_meta);
                    return Err(DownloadFailure::Interrupted(AppError::new(
                        ErrorCode::Timeout,
                        format!(
                            "Download rate fell below {} KiB/s.",
                            MIN_PROGRESS_BYTES_PER_SEC / 1024
                        ),
                    )));
                }
            }
        }

        if let Err(err) = file.sync_all() {
            persist_progress(&mut file, &part_meta);
            return Err(DownloadFailure::Interrupted(AppError::new(
                ErrorCode::Failed,
                format!("Could not flush downloaded update: {err}"),
            )));
        }
        drop(file);
        let _ = save_part_metadata(&part_meta_path, &part_meta);

        // Stream-verify the complete `.part` file against signed size + SHA-256 on a blocking
        // thread before renaming into place. Only discard `.part` when this final verification fails.
        let meta_clone = cand.meta.clone();
        let art_name = cand.artifact.name.clone();
        let part_check = part_path.clone();
        let verify_res = tokio::task::spawn_blocking(move || {
            verify_artifact_file(&meta_clone, &art_name, &part_check)
        })
        .await
        .map_err(|err| {
            DownloadFailure::Interrupted(AppError::new(
                ErrorCode::Internal,
                format!("Artifact verification stopped unexpectedly: {err}"),
            ))
        })?;

        if let Err(err) = verify_res {
            let mut contributors = Vec::new();
            for range in &part_meta.ranges {
                if !contributors.contains(&range.node) {
                    contributors.push(range.node.clone());
                }
            }
            let _ = fs::remove_file(&part_path);
            let _ = fs::remove_file(&part_meta_path);
            return Err(DownloadFailure::HashMismatch {
                err: AppError::new(ErrorCode::Denied, err.message),
                contributors,
            });
        }

        fs::rename(&part_path, &final_path).map_err(|err| {
            DownloadFailure::Interrupted(AppError::new(
                ErrorCode::Failed,
                format!("Could not finalize downloaded update: {err}"),
            ))
        })?;
        let _ = fs::remove_file(&part_meta_path);
        Ok(offset)
    }

    fn record_last_checked(&self, checked_at: &str, cached_available: Option<AvailableUpdateInfo>) {
        let mut prefs = self.prefs.lock().unwrap_or_else(|p| p.into_inner()).clone();
        prefs.last_checked_at = Some(checked_at.to_owned());
        if let Some(avail) = cached_available {
            prefs.cached_available = Some(avail);
        } else if prefs
            .cached_available
            .as_ref()
            .is_some_and(|a| a.sequence <= self.current_sequence)
        {
            prefs.cached_available = None;
        }
        if save_prefs(&self.updates_dir.join("prefs.json"), &prefs).is_ok() {
            let mut guard = self.prefs.lock().unwrap_or_else(|p| p.into_inner());
            *guard = prefs;
        }
    }

    #[cfg(any(target_os = "android", test))]
    fn record_pending_install(&self, avail: &AvailableUpdateInfo) -> Result<(), AppError> {
        self.record_started_attempt_and_pending_at(avail, "swapped", now_unix_secs())
    }

    /// Atomically persists (with fsync) that attempt N of `avail.sequence` started at `now_unix`,
    /// updates the global `last_install_attempt_at` timestamp for the 1-hour automatic floor, and
    /// records `pending_install` at `stage` (`"staged"` on desktop before spawning a supervisor,
    /// `"swapped"` on Android).
    fn record_started_attempt_and_pending_at(
        &self,
        avail: &AvailableUpdateInfo,
        stage: &str,
        now_unix: i64,
    ) -> Result<(), AppError> {
        let mut prefs = self.prefs.lock().unwrap_or_else(|p| p.into_inner()).clone();
        prefs.record_started_attempt(avail.sequence, now_unix);
        prefs.pending_install = Some(PendingInstallRecord {
            channel: avail.channel,
            version: avail.version.clone(),
            build: avail.build.clone(),
            sequence: avail.sequence,
            notes: avail.notes.clone(),
            from_sequence: self.current_sequence,
            started_at: format_rfc3339_utc(now_unix),
            stage: stage.to_owned(),
        });
        save_prefs(&self.updates_dir.join("prefs.json"), &prefs)?;
        #[cfg(not(any(target_os = "android", target_os = "ios")))]
        desktop::write_pending_stage(&self.updates_dir, avail.sequence, stage)?;
        let mut guard = self.prefs.lock().unwrap_or_else(|p| p.into_inner());
        *guard = prefs;
        Ok(())
    }

    fn clear_pending_install(&self) {
        let mut prefs = self.prefs.lock().unwrap_or_else(|p| p.into_inner()).clone();
        if let Some(pending) = prefs.pending_install.take() {
            #[cfg(not(any(target_os = "android", target_os = "ios")))]
            desktop::clear_pending_stage(&self.updates_dir, pending.sequence);
            #[cfg(any(target_os = "android", target_os = "ios"))]
            let _ = pending;
            if save_prefs(&self.updates_dir.join("prefs.json"), &prefs).is_ok() {
                let mut guard = self.prefs.lock().unwrap_or_else(|p| p.into_inner());
                *guard = prefs;
            }
        }
    }

    #[cfg(target_os = "android")]
    fn clear_pending_and_refund_attempt(&self, sequence: u64) {
        let mut prefs = self.prefs.lock().unwrap_or_else(|p| p.into_inner()).clone();
        prefs.pending_install = None;
        prefs.decrement_started_attempt(sequence);
        if save_prefs(&self.updates_dir.join("prefs.json"), &prefs).is_ok() {
            let mut guard = self.prefs.lock().unwrap_or_else(|p| p.into_inner());
            *guard = prefs;
        }
    }

    /// Persists a failed update attempt on any platform so automatic updates never retry it in a
    /// loop while still allowing manual "Try again" or automatically moving on to a newer sequence.
    pub fn record_failed_update(
        &self,
        channel: Channel,
        version: &str,
        sequence: u64,
        reason: &str,
    ) -> FailedUpdateRecord {
        let mut prefs = self.prefs.lock().unwrap_or_else(|p| p.into_inner()).clone();
        let prev_attempts = prefs
            .failed_updates
            .iter()
            .find(|r| r.sequence == sequence)
            .map_or(0, |r| r.attempts);
        let record = FailedUpdateRecord {
            channel,
            version: version.to_owned(),
            sequence,
            reason: reason.to_owned(),
            failed_at: now_rfc3339_utc(),
            attempts: prev_attempts.saturating_add(1),
            skipped: true,
            retry_after: None,
            detail: None,
        };
        prefs.pending_install = None;
        prefs.record_failure(record.clone());
        if save_prefs(&self.updates_dir.join("prefs.json"), &prefs).is_ok() {
            let mut guard = self.prefs.lock().unwrap_or_else(|p| p.into_inner());
            *guard = prefs;
        }
        record
    }

    /// Clears any recorded failure/skip state for `sequence` (invoked when the user taps "Try
    /// again") and restores `phase = "ready"` if that sequence is already downloaded on disk.
    pub fn clear_failed_sequence(&self, sequence: u64) {
        let mut prefs = self.prefs.lock().unwrap_or_else(|p| p.into_inner()).clone();
        let had_entry = prefs.has_failed(sequence)
            || prefs
                .rolled_back
                .as_ref()
                .is_some_and(|r| r.sequence == sequence);
        if had_entry {
            prefs.clear_failure(sequence);
            if save_prefs(&self.updates_dir.join("prefs.json"), &prefs).is_ok() {
                let mut guard = self.prefs.lock().unwrap_or_else(|p| p.into_inner());
                *guard = prefs;
            }
        }
        self.update_state(|st| {
            if st
                .failed_update
                .as_ref()
                .is_some_and(|f| f.sequence == sequence)
            {
                st.failed_update = None;
                st.last_error = None;
                st.last_error_detail = None;
            }
            if st
                .rolled_back
                .as_ref()
                .is_some_and(|r| r.sequence == sequence)
            {
                st.rolled_back = None;
            }
            if st.phase == "error"
                && st
                    .available
                    .as_ref()
                    .is_some_and(|a| a.sequence == sequence && a.downloaded)
                && st.can_self_install
            {
                st.phase = "ready".to_owned();
            }
        });
    }

    /// Returns whether `sequence` has any failure record on this device.
    pub fn sequence_failed_before(&self, sequence: u64) -> bool {
        self.prefs
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .has_failed(sequence)
    }

    /// Returns the number of currently active tracked external operations (such as camera scanning).
    pub fn external_operations_count(&self) -> usize {
        self.external_operations.load(Ordering::Acquire)
    }

    /// Returns the reason automatic installation of `sequence` is currently blocked at `now_unix`,
    /// or `None` when automatic installation (including a single retry >= 1 hour after a first
    /// rollback) is permitted.
    pub fn auto_install_block_reason_at(
        &self,
        sequence: u64,
        now_unix: i64,
    ) -> Option<&'static str> {
        if has_unread_supervisor_records(&self.updates_dir) {
            return Some("unread_supervisor_record");
        }
        self.prefs
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .auto_block_reason_at(sequence, now_unix)
    }

    /// Re-verifies the currently ready downloaded update on disk and returns its verified info,
    /// signed exact byte size, expected SHA-256, and artifact path.
    pub fn prepare_verified_install(
        &self,
    ) -> Result<(AvailableUpdateInfo, u64, String, PathBuf), AppError> {
        if self.current_sequence == 0 {
            return Err(AppError::new(
                ErrorCode::Unsupported,
                "This build has sequence 0, so self-updates are disabled.",
            ));
        }
        let target = self
            .platform
            .target_platform(self.os, self.arch)
            .ok_or_else(|| {
                AppError::new(
                    ErrorCode::Unsupported,
                    "In-place updates are not supported on this platform.",
                )
            })?;
        let st = self.snapshot();
        let avail = st.available.ok_or_else(|| {
            AppError::new(ErrorCode::NotFound, "No update is waiting to be installed.")
        })?;
        if !avail.downloaded {
            return Err(AppError::new(
                ErrorCode::Failed,
                "The update has not finished downloading yet.",
            ));
        }
        let floor_path = self.updates_dir.join("floor.json");
        let set_dir = download_dir_for(&self.updates_dir, avail.channel, avail.sequence);
        let (verified_art, expected_size, expected_sha256) = verify_downloaded_set(
            &set_dir,
            &avail.artifact_name,
            &self.anchors,
            target,
            self.current_sequence,
            &floor_path,
        )?;
        Ok((avail, expected_size, expected_sha256, verified_art))
    }

    /// Tracks an in-flight native operation outside [`Backend`] (such as Android QR camera
    /// scanning during pairing) for the lifetime of the returned guard so automatic updates do not
    /// fire while it is active.
    pub fn track_external_operation(&self) -> ExternalOpGuard {
        self.external_operations.fetch_add(1, Ordering::AcqRel);
        ExternalOpGuard(Arc::clone(&self.external_operations))
    }

    /// Evaluates natively whether an automatic update install is allowed right now.
    ///
    /// The native side—never the web page—enforces all safety rules:
    /// - Page-initiated requests are rate-limited to at most once every [`AUTO_INSTALL_RATE_LIMIT`].
    /// - Any in-flight native call, pairing, transfer, or content stream (`active_ops > 0`) refuses
    ///   automatic installation across every trigger (`"startup"`, `"background"`, and `"close"`).
    /// - `"startup"` is allowed only within [`STARTUP_AUTO_INSTALL_WINDOW`] of launch and strictly
    ///   before `on_app_ready` has recorded the first full screen.
    /// - `"background"` is allowed only on Android.
    /// - `"close"` is accepted only from the native desktop window-close handler (`from_page == false`).
    pub fn evaluate_auto_install_gate(
        &self,
        trigger: &str,
        active_ops: usize,
        from_page: bool,
    ) -> Result<(), &'static str> {
        self.evaluate_auto_install_gate_at(trigger, active_ops, from_page, now_unix_secs())
    }

    /// Deterministic variant of [`Self::evaluate_auto_install_gate`] evaluating rollback retry
    /// cooldowns at `now_unix` (seconds since Unix epoch).
    pub fn evaluate_auto_install_gate_at(
        &self,
        trigger: &str,
        active_ops: usize,
        from_page: bool,
        now_unix: i64,
    ) -> Result<(), &'static str> {
        let now = Instant::now();
        if from_page {
            let mut last = self
                .last_auto_install_request
                .lock()
                .unwrap_or_else(|p| p.into_inner());
            if let Some(prev) = *last
                && now.saturating_duration_since(prev) < AUTO_INSTALL_RATE_LIMIT
            {
                return Err("rate_limited");
            }
            *last = Some(now);
        }

        let total_active_ops =
            active_ops.saturating_add(self.external_operations.load(Ordering::Acquire));
        if total_active_ops > 0 {
            return Err("native_activity_in_flight");
        }

        match trigger {
            "startup" => {
                if self.first_screen_shown.load(Ordering::SeqCst) {
                    return Err("first_screen_already_shown");
                }
                if now.saturating_duration_since(self.launched_at) > STARTUP_AUTO_INSTALL_WINDOW {
                    return Err("startup_window_elapsed");
                }
            }
            "background" => {
                if self.os != Os::Android {
                    return Err("background_android_only");
                }
            }
            "close" => {
                if from_page {
                    return Err("close_native_only");
                }
            }
            _ => return Err("unsupported_trigger"),
        }

        if self.current_sequence == 0 {
            return Err("sequence_zero");
        }
        if has_unread_supervisor_records(&self.updates_dir) {
            return Err("unread_supervisor_record");
        }
        let st = self.snapshot();
        let ready_or_checking_downloaded = st.phase == "ready"
            || (st.phase == "checking" && st.available.as_ref().is_some_and(|a| a.downloaded));
        if !st.auto_update || !st.can_self_install || !ready_or_checking_downloaded {
            return Err("not_ready");
        }
        if let Some(avail) = st.available.as_ref()
            && let Some(reason) = self.auto_install_block_reason_at(avail.sequence, now_unix)
        {
            return Err(reason);
        }
        if let Some(reason) = self
            .prefs
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .hourly_floor_block_reason_at(now_unix)
        {
            return Err(reason);
        }
        Ok(())
    }

    /// Synchronous hook called when the last desktop window closes: if automatic updates are on,
    /// no native operations are in flight, and a verified update is ready (and not blocked by
    /// unread supervisor records, rollback cooldown, skip limit, or the 1-hour floor), stages it
    /// via copy-while-hashing and spawns the detached swapper in `verify_only` mode so the new
    /// build verifies startup and exits cleanly without popping a window open.
    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    pub fn try_spawn_desktop_close_install(&self, active_ops: usize) -> bool {
        self.try_spawn_desktop_close_install_at(active_ops, now_unix_secs())
    }

    /// Deterministic variant of [`Self::try_spawn_desktop_close_install`] evaluating attempt floors
    /// and cooldowns at `now_unix`.
    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    pub fn try_spawn_desktop_close_install_at(&self, active_ops: usize, now_unix: i64) -> bool {
        if self
            .evaluate_auto_install_gate_at("close", active_ops, false, now_unix)
            .is_err()
        {
            return false;
        }
        let Ok((avail, expected_size, expected_sha256, artifact_path)) =
            self.prepare_verified_install()
        else {
            return false;
        };
        if self
            .auto_install_block_reason_at(avail.sequence, now_unix)
            .is_some()
        {
            return false;
        }
        matches!(
            self.spawn_desktop_install_inner_at(
                &avail,
                &artifact_path,
                &expected_sha256,
                expected_size,
                true,
                now_unix,
            ),
            Ok(true)
        )
    }

    /// Installs the verified update on desktop hosts and returns `true` when the caller should exit
    /// so the detached swapper can replace the running app and relaunch it.
    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    pub async fn install_desktop(
        &self,
        backend: &dyn Backend,
        auto_triggered: bool,
    ) -> Result<(UpdateState, bool), AppError> {
        if self.current_sequence == 0 {
            return Err(AppError::new(
                ErrorCode::Unsupported,
                "This build has sequence 0, so self-updates are disabled.",
            ));
        }

        // Cancel any stalled background check immediately.
        let _ = self.bump_generation();

        if let Some(avail) = self.snapshot().available {
            if !auto_triggered {
                self.clear_failed_sequence(avail.sequence);
            }
            if !avail.downloaded {
                if auto_triggered {
                    return Ok((self.snapshot(), false));
                }
                let after = self.check_with_backend(backend, true, false).await?;
                if after.phase != "ready" {
                    return Ok((after, false));
                }
            }
        }

        let (avail, expected_size, expected_sha256, artifact_path) = {
            let updates_dir = self.updates_dir.clone();
            let anchors = self.anchors;
            let os = self.os;
            let arch = self.arch;
            let platform = self.platform.clone();
            let current_sequence = self.current_sequence;
            let st = self.snapshot();
            tokio::task::spawn_blocking(move || {
                let target = platform.target_platform(os, arch).ok_or_else(|| {
                    AppError::new(
                        ErrorCode::Unsupported,
                        "In-place updates are not supported on this platform.",
                    )
                })?;
                let avail = st.available.ok_or_else(|| {
                    AppError::new(ErrorCode::NotFound, "No update is waiting to be installed.")
                })?;
                if !avail.downloaded {
                    return Err(AppError::new(
                        ErrorCode::Failed,
                        "The update has not finished downloading yet.",
                    ));
                }
                let floor_path = updates_dir.join("floor.json");
                let set_dir = download_dir_for(&updates_dir, avail.channel, avail.sequence);
                let (path, size, sha) = verify_downloaded_set(
                    &set_dir,
                    &avail.artifact_name,
                    &anchors,
                    target,
                    current_sequence,
                    &floor_path,
                )?;
                Ok::<_, AppError>((avail, size, sha, path))
            })
            .await
            .map_err(|err| {
                AppError::new(
                    ErrorCode::Internal,
                    format!("Update verification stopped unexpectedly: {err}"),
                )
            })??
        };

        if auto_triggered
            && self
                .auto_install_block_reason_at(avail.sequence, now_unix_secs())
                .is_some()
        {
            return Ok((self.snapshot(), false));
        }

        match &self.platform {
            PlatformEnv::Desktop(DesktopInstall::LinuxDeb { .. })
            | PlatformEnv::Desktop(DesktopInstall::LinuxPacman { .. }) => {
                let (cmd, sha) = self.finalize_manual_copy_if_needed(
                    &artifact_path,
                    &avail.artifact_name,
                    &expected_sha256,
                    expected_size,
                )?;
                let next = self.update_state(|st| {
                    st.phase = "manual".to_owned();
                    st.manual_command = cmd;
                    st.manual_sha256 = sha;
                });
                Ok((next, false))
            }
            PlatformEnv::Desktop(DesktopInstall::Standalone { reason, .. }) => {
                Err(AppError::new(ErrorCode::Unsupported, reason.clone()))
            }
            _ => {
                let spawned = self.spawn_desktop_install_inner(
                    &avail,
                    &artifact_path,
                    &expected_sha256,
                    expected_size,
                    false,
                )?;
                Ok((self.snapshot(), spawned))
            }
        }
    }

    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    fn spawn_desktop_install_inner(
        &self,
        avail: &AvailableUpdateInfo,
        artifact_path: &Path,
        expected_sha256: &str,
        expected_size: u64,
        verify_only: bool,
    ) -> Result<bool, AppError> {
        self.spawn_desktop_install_inner_at(
            avail,
            artifact_path,
            expected_sha256,
            expected_size,
            verify_only,
            now_unix_secs(),
        )
    }

    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    fn spawn_desktop_install_inner_at(
        &self,
        avail: &AvailableUpdateInfo,
        artifact_path: &Path,
        expected_sha256: &str,
        expected_size: u64,
        verify_only: bool,
        now_unix: i64,
    ) -> Result<bool, AppError> {
        // Exclusive update lock: only one instance may stage, record pending_install, and spawn
        // the supervisor for an update. A second instance seeing an active lock does nothing.
        let Some(lock_guard) =
            desktop::try_acquire_update_lock(&self.updates_dir, avail.sequence, verify_only)?
        else {
            return Ok(false);
        };

        let ready_marker = self
            .updates_dir
            .join(format!("started-{}.ok", avail.sequence));
        let rollback_marker = self
            .updates_dir
            .join(format!("rollback-{}.json", avail.sequence));
        let deferred_marker = self
            .updates_dir
            .join(format!("deferred-{}.json", avail.sequence));
        let rollback_json = serde_json::to_string(&RollbackInfo {
            version: avail.version.clone(),
            sequence: avail.sequence,
            reason: ROLLBACK_REASON.to_owned(),
            detail: None,
            install_state: Some("restored_previous".to_owned()),
        })
        .unwrap_or_else(|_| "{}".to_owned());
        let rollback_kept_new_json = serde_json::to_string(&RollbackInfo {
            version: avail.version.clone(),
            sequence: avail.sequence,
            reason: ROLLBACK_REASON.to_owned(),
            detail: None,
            install_state: Some("new_kept_files_in_use".to_owned()),
        })
        .unwrap_or_else(|_| "{}".to_owned());
        let deferred_json = serde_json::to_string(&DeferredInstallRecord {
            sequence: avail.sequence,
            reason: "Install files were still in use; deferred without changing the install."
                .to_owned(),
        })
        .unwrap_or_else(|_| "{}".to_owned());
        let verified_dir = self.updates_dir.join("verified");

        match &self.platform {
            #[cfg(unix)]
            PlatformEnv::Desktop(DesktopInstall::LinuxAppImage { appimage_path }) => {
                self.record_started_attempt_and_pending_at(avail, "staged", now_unix)?;
                self.update_state(|st| {
                    st.phase = "installing".to_owned();
                    st.failed_update = None;
                    st.last_error = None;
                    st.last_error_detail = None;
                });
                let sup_pid = match desktop::stage_and_spawn_linux_appimage(
                    artifact_path,
                    expected_sha256,
                    expected_size,
                    appimage_path,
                    &ready_marker,
                    &rollback_marker,
                    rollback_json,
                    rollback_kept_new_json,
                    &deferred_marker,
                    deferred_json,
                    avail.sequence,
                    DEFAULT_STARTUP_WAIT_SECS,
                    verify_only,
                ) {
                    Ok(pid) => pid,
                    Err(err) => {
                        self.clear_pending_install();
                        let rec = self.record_failed_update(
                            avail.channel,
                            &avail.version,
                            avail.sequence,
                            "Legion could not start the update helper.",
                        );
                        self.update_state(|st| {
                            st.phase = "error".to_owned();
                            st.failed_update = Some(rec);
                            st.last_error = Some("Legion could not start the update.".to_owned());
                            st.last_error_detail = Some(err.message.clone());
                        });
                        return Err(err);
                    }
                };
                let _ = lock_guard.hand_over_to_supervisor(sup_pid);
                Ok(true)
            }
            #[cfg(unix)]
            PlatformEnv::Desktop(DesktopInstall::MacosAppBundle {
                bundle_dir,
                executable_rel,
            }) => {
                self.record_started_attempt_and_pending_at(avail, "staged", now_unix)?;
                self.update_state(|st| {
                    st.phase = "installing".to_owned();
                    st.failed_update = None;
                    st.last_error = None;
                    st.last_error_detail = None;
                });
                let sup_pid = match desktop::stage_and_spawn_macos_bundle(
                    artifact_path,
                    expected_sha256,
                    expected_size,
                    &verified_dir,
                    bundle_dir,
                    executable_rel,
                    &ready_marker,
                    &rollback_marker,
                    rollback_json,
                    rollback_kept_new_json,
                    &deferred_marker,
                    deferred_json,
                    avail.sequence,
                    DEFAULT_STARTUP_WAIT_SECS,
                    verify_only,
                ) {
                    Ok(pid) => pid,
                    Err(err) => {
                        self.clear_pending_install();
                        let rec = self.record_failed_update(
                            avail.channel,
                            &avail.version,
                            avail.sequence,
                            "Legion could not unpack the macOS update.",
                        );
                        self.update_state(|st| {
                            st.phase = "error".to_owned();
                            st.failed_update = Some(rec);
                            st.last_error = Some("Legion could not start the update.".to_owned());
                            st.last_error_detail = Some(err.message.clone());
                        });
                        return Err(err);
                    }
                };
                let _ = lock_guard.hand_over_to_supervisor(sup_pid);
                Ok(true)
            }
            #[cfg(windows)]
            PlatformEnv::Desktop(DesktopInstall::WindowsNsis {
                install_dir,
                exe_path,
            }) => {
                self.record_started_attempt_and_pending_at(avail, "staged", now_unix)?;
                self.update_state(|st| {
                    st.phase = "installing".to_owned();
                    st.failed_update = None;
                    st.last_error = None;
                    st.last_error_detail = None;
                });
                let backup_dir = self
                    .updates_dir
                    .join(format!("backup-{}", self.current_sequence));
                let sup_pid = match desktop::stage_and_spawn_windows_nsis(
                    artifact_path,
                    expected_sha256,
                    expected_size,
                    &verified_dir,
                    install_dir,
                    exe_path,
                    &backup_dir,
                    &ready_marker,
                    &rollback_marker,
                    rollback_json,
                    rollback_kept_new_json,
                    &deferred_marker,
                    deferred_json,
                    avail.sequence,
                    DEFAULT_STARTUP_WAIT_SECS,
                    verify_only,
                ) {
                    Ok(pid) => pid,
                    Err(err) => {
                        self.clear_pending_install();
                        let rec = self.record_failed_update(
                            avail.channel,
                            &avail.version,
                            avail.sequence,
                            "Legion could not start the Windows installer.",
                        );
                        self.update_state(|st| {
                            st.phase = "error".to_owned();
                            st.failed_update = Some(rec);
                            st.last_error = Some("Legion could not start the update.".to_owned());
                            st.last_error_detail = Some(err.message.clone());
                        });
                        return Err(err);
                    }
                };
                let _ = lock_guard.hand_over_to_supervisor(sup_pid);
                Ok(true)
            }
            _ => Err(AppError::new(
                ErrorCode::Unsupported,
                "In-place installation is not available for this build.",
            )),
        }
    }

    /// Installs the verified APK on Android via `AppUpdaterPlugin` (`PackageInstaller`).
    ///
    /// When `background` is `true`, Android sessions that ask for user confirmation are abandoned
    /// silently so leaving the app never brings Legion or a prompt back to the foreground.
    #[cfg(target_os = "android")]
    pub async fn install_android(
        &self,
        backend: &dyn Backend,
        plugin: &android::AndroidUpdaterHandle,
        auto_triggered: bool,
        background: bool,
    ) -> Result<UpdateState, AppError> {
        if self.current_sequence == 0 {
            return Err(AppError::new(
                ErrorCode::Unsupported,
                "This build has sequence 0, so self-updates are disabled.",
            ));
        }

        let _ = self.bump_generation();

        if let Some(avail) = self.snapshot().available {
            if !auto_triggered {
                self.clear_failed_sequence(avail.sequence);
            }
            if !avail.downloaded {
                if auto_triggered {
                    return Ok(self.snapshot());
                }
                let after = self.check_with_backend(backend, true, false).await?;
                if after.phase != "ready" {
                    return Ok(after);
                }
            }
        }

        let (avail, _size, _sha, apk_path) = {
            let updates_dir = self.updates_dir.clone();
            let anchors = self.anchors;
            let os = self.os;
            let arch = self.arch;
            let platform = self.platform.clone();
            let current_sequence = self.current_sequence;
            let st = self.snapshot();
            tokio::task::spawn_blocking(move || {
                let target = platform.target_platform(os, arch).ok_or_else(|| {
                    AppError::new(
                        ErrorCode::Unsupported,
                        "In-place updates are not supported on this platform.",
                    )
                })?;
                let avail = st.available.ok_or_else(|| {
                    AppError::new(ErrorCode::NotFound, "No update is waiting to be installed.")
                })?;
                if !avail.downloaded {
                    return Err(AppError::new(
                        ErrorCode::Failed,
                        "The update has not finished downloading yet.",
                    ));
                }
                let floor_path = updates_dir.join("floor.json");
                let set_dir = download_dir_for(&updates_dir, avail.channel, avail.sequence);
                let (path, size, sha) = verify_downloaded_set(
                    &set_dir,
                    &avail.artifact_name,
                    &anchors,
                    target,
                    current_sequence,
                    &floor_path,
                )?;
                Ok::<_, AppError>((avail, size, sha, path))
            })
            .await
            .map_err(|err| {
                AppError::new(
                    ErrorCode::Internal,
                    format!("Update verification stopped unexpectedly: {err}"),
                )
            })??
        };

        if auto_triggered
            && self
                .auto_install_block_reason_at(avail.sequence, now_unix_secs())
                .is_some()
        {
            return Ok(self.snapshot());
        }

        let mut caps = android::capabilities(plugin).await?;
        self.set_android_capabilities(caps.clone());

        if auto_triggered && !caps.supports_silent_self_update {
            return Ok(self.snapshot());
        }

        if !caps.can_request_package_installs {
            if background {
                return Ok(self.snapshot());
            }
            caps = android::request_install_permission(plugin).await?;
            self.set_android_capabilities(caps.clone());
            if !caps.can_request_package_installs {
                return Ok(self.update_state(|st| {
                    st.phase = "needs_permission".to_owned();
                    st.last_error = Some(
                        "Allow Legion to install updates in Android settings, then tap Update now again."
                            .to_owned(),
                    );
                    st.last_error_detail = None;
                }));
            }
        }

        self.record_pending_install(&avail)?;
        self.update_state(|st| {
            st.phase = "installing".to_owned();
            st.failed_update = None;
            st.last_error = None;
            st.last_error_detail = None;
        });

        let outcome = match android::install_apk(plugin, &apk_path, background).await {
            Ok(o) => o,
            Err(err) => {
                self.clear_pending_install();
                let rec = self.record_failed_update(
                    avail.channel,
                    &avail.version,
                    avail.sequence,
                    "Android could not finish installing this update.",
                );
                return Ok(self.update_state(|st| {
                    st.phase = "error".to_owned();
                    st.failed_update = Some(rec);
                    st.last_error =
                        Some("Android could not finish installing this update.".to_owned());
                    st.last_error_detail = Some(err.message);
                }));
            }
        };

        match outcome.status.as_str() {
            "installed" => Ok(self.snapshot()),
            "cancelled" | "deferred_background" => {
                self.clear_pending_and_refund_attempt(avail.sequence);
                Ok(self.update_state(|st| {
                    st.phase = "ready".to_owned();
                    st.last_error = None;
                    st.last_error_detail = None;
                }))
            }
            "needs_permission" => {
                self.clear_pending_and_refund_attempt(avail.sequence);
                Ok(self.update_state(|st| {
                    st.phase = "needs_permission".to_owned();
                    st.last_error = Some(
                        "Allow Legion to install updates in Android settings, then tap Update now again."
                            .to_owned(),
                    );
                    st.last_error_detail = if outcome.message.is_empty() {
                        None
                    } else {
                        Some(outcome.message)
                    };
                }))
            }
            _ => {
                self.clear_pending_install();
                let rec = self.record_failed_update(
                    avail.channel,
                    &avail.version,
                    avail.sequence,
                    "Android could not finish installing this update.",
                );
                Ok(self.update_state(|st| {
                    st.phase = "error".to_owned();
                    st.failed_update = Some(rec);
                    st.last_error =
                        Some("Android could not finish installing this update.".to_owned());
                    st.last_error_detail = if outcome.message.is_empty() {
                        None
                    } else {
                        Some(outcome.message)
                    };
                }))
            }
        }
    }
}

type ListNodeOutcome = (String, Result<Result<NodeUpdateList, AppError>, ()>);
type ListNodeFuture<'a> = BoxFuture<'a, ListNodeOutcome>;
type CandidateProbeFuture<'a> =
    BoxFuture<'a, Result<(VerifiedCandidate, usize, Duration), CandidateFetchError>>;

enum WaveOutcome {
    Verified {
        verified: Box<VerifiedCandidate>,
        bytes_read: usize,
        elapsed: Duration,
        errors: Vec<(String, AppError, usize, Duration)>,
    },
    NotForThisPlatform {
        node: String,
        bytes_read: usize,
        elapsed: Duration,
        errors: Vec<(String, AppError, usize, Duration)>,
    },
    AllFailed(Vec<(String, AppError, usize, Duration)>),
    Cancelled,
}

async fn join_all_futures<T>(mut futs: Vec<BoxFuture<'_, T>>) -> Vec<T> {
    let mut results: Vec<Option<T>> = (0..futs.len()).map(|_| None).collect();
    let mut remaining = futs.len();
    std::future::poll_fn(|cx| {
        for (i, fut) in futs.iter_mut().enumerate() {
            if results[i].is_none()
                && let Poll::Ready(val) = fut.as_mut().poll(cx)
            {
                results[i] = Some(val);
                remaining -= 1;
            }
        }
        if remaining == 0 {
            Poll::Ready(())
        } else {
            Poll::Pending
        }
    })
    .await;
    results.into_iter().flatten().collect()
}

fn load_or_init_part_state(
    part_path: &Path,
    meta_path: &Path,
    expected_sha256: &str,
    expected_size: u64,
) -> Result<(u64, PartMetadata), AppError> {
    let fresh = || PartMetadata {
        sha256: expected_sha256.to_owned(),
        expected_size,
        ranges: Vec::new(),
    };
    if !part_path.is_file() {
        let _ = fs::remove_file(meta_path);
        return Ok((0, fresh()));
    }
    let Ok(raw_meta) = fs::read(meta_path) else {
        let _ = fs::remove_file(part_path);
        let _ = fs::remove_file(meta_path);
        return Ok((0, fresh()));
    };
    let Ok(mut meta) = serde_json::from_slice::<PartMetadata>(&raw_meta) else {
        let _ = fs::remove_file(part_path);
        let _ = fs::remove_file(meta_path);
        return Ok((0, fresh()));
    };
    if meta.sha256 != expected_sha256 || meta.expected_size != expected_size {
        let _ = fs::remove_file(part_path);
        let _ = fs::remove_file(meta_path);
        return Ok((0, fresh()));
    }
    let file_len = fs::symlink_metadata(part_path)
        .ok()
        .filter(|m| m.file_type().is_file())
        .map_or(0, |m| m.len());
    if file_len > expected_size {
        let _ = fs::remove_file(part_path);
        let _ = fs::remove_file(meta_path);
        return Ok((0, fresh()));
    }
    // Validate contiguous provenance ranges starting at 0.
    let mut meta_end = 0u64;
    for r in &meta.ranges {
        if r.start != meta_end || r.end < r.start || r.end > expected_size {
            let _ = fs::remove_file(part_path);
            let _ = fs::remove_file(meta_path);
            return Ok((0, fresh()));
        }
        meta_end = r.end;
    }
    if file_len > meta_end {
        // Truncate unattributed tail bytes after an abrupt crash so every byte has provenance.
        let f = OpenOptions::new()
            .write(true)
            .open(part_path)
            .map_err(|err| {
                AppError::new(
                    ErrorCode::Failed,
                    format!(
                        "Could not align partial download {}: {err}",
                        part_path.display()
                    ),
                )
            })?;
        f.set_len(meta_end).map_err(|err| {
            AppError::new(
                ErrorCode::Failed,
                format!(
                    "Could not align partial download {}: {err}",
                    part_path.display()
                ),
            )
        })?;
        return Ok((meta_end, meta));
    }
    if file_len < meta_end {
        // Clamp provenance ranges to actual on-disk length.
        let mut clamped = Vec::new();
        for mut r in meta.ranges {
            if r.start >= file_len {
                break;
            }
            if r.end > file_len {
                r.end = file_len;
            }
            if r.end > r.start {
                clamped.push(r);
            }
        }
        meta.ranges = clamped;
    }
    Ok((file_len, meta))
}

fn record_part_range(meta: &mut PartMetadata, start: u64, end: u64, node: &str) {
    if end <= start {
        return;
    }
    if let Some(last) = meta.ranges.last_mut()
        && last.node == node
        && last.end == start
    {
        last.end = end;
        return;
    }
    meta.ranges.push(PartRange {
        start,
        end,
        node: node.to_owned(),
    });
}

fn save_part_metadata(path: &Path, meta: &PartMetadata) -> Result<(), AppError> {
    let bytes = serde_json::to_vec(meta).map_err(|err| {
        AppError::new(
            ErrorCode::Internal,
            format!("Could not serialize partial download metadata: {err}"),
        )
    })?;
    write_atomic(path, &bytes)
}

async fn run_cancellable<T, F>(
    cancel_rx: &mut watch::Receiver<u64>,
    deadline: Instant,
    fut: F,
) -> Result<T, AppError>
where
    F: Future<Output = T>,
{
    let now = Instant::now();
    if now >= deadline {
        return Err(AppError::new(
            ErrorCode::Timeout,
            "Update request exceeded its time budget.",
        ));
    }
    let remaining = deadline.saturating_duration_since(now);
    tokio::select! {
        biased;
        _ = cancel_rx.changed() => Err(AppError::new(
            ErrorCode::Cancelled,
            "Update check was cancelled by a newer request.",
        )),
        res = tokio::time::timeout(remaining, fut) => res.map_err(|_| {
            AppError::new(
                ErrorCode::Timeout,
                "Update request exceeded its time budget.",
            )
        }),
    }
}

#[allow(clippy::too_many_arguments)]
async fn read_small_file(
    client: &dyn UpdateSourceClient,
    node: &str,
    channel: Channel,
    sequence: u64,
    name: &str,
    max_bytes: usize,
    deadline: Instant,
    call_timeout: Duration,
    cancel_rx: &mut watch::Receiver<u64>,
) -> Result<Vec<u8>, AppError> {
    if max_bytes == 0 {
        return Err(AppError::new(
            ErrorCode::Denied,
            format!("Metadata budget exceeded while reading {name} from {node}."),
        ));
    }
    let started_at = Instant::now();
    let mut out = Vec::new();
    let mut offset = 0u64;
    let mut chunks = 0usize;
    loop {
        let req_len = DOWNLOAD_CHUNK_BYTES.min((max_bytes as u64).saturating_sub(offset));
        let call_deadline = (Instant::now() + call_timeout).min(deadline);
        let reply = run_cancellable(
            cancel_rx,
            call_deadline,
            client.read_chunk(node, channel, sequence, name, offset, req_len),
        )
        .await??;
        if reply.offset != offset {
            return Err(AppError::new(
                ErrorCode::Failed,
                format!(
                    "Unexpected offset {} (expected {offset}) for {name} from {node}.",
                    reply.offset
                ),
            ));
        }
        if reply.size > max_bytes as u64 {
            return Err(AppError::new(
                ErrorCode::Denied,
                format!("{name} from {node} exceeds the {max_bytes}-byte limit."),
            ));
        }
        let chunk = BASE64.decode(reply.content.as_bytes()).map_err(|err| {
            AppError::new(
                ErrorCode::Failed,
                format!("Invalid base64 for {name} from {node}: {err}"),
            )
        })?;
        if chunk.len() as u64 != reply.length {
            return Err(AppError::new(
                ErrorCode::Failed,
                format!("Inconsistent length for {name} from {node}."),
            ));
        }
        if chunk.len() as u64 > req_len {
            return Err(AppError::new(
                ErrorCode::Denied,
                format!("{name} from {node} returned more bytes than requested."),
            ));
        }
        if out.len().saturating_add(chunk.len()) > max_bytes {
            return Err(AppError::new(
                ErrorCode::Denied,
                format!("{name} from {node} exceeds the {max_bytes}-byte limit."),
            ));
        }
        out.extend_from_slice(&chunk);
        offset = offset.saturating_add(chunk.len() as u64);
        chunks += 1;
        if reply.eof {
            if offset != reply.size {
                return Err(AppError::new(
                    ErrorCode::Failed,
                    format!(
                        "{name} from {node} ended at {offset} bytes (claimed {}).",
                        reply.size
                    ),
                ));
            }
            break;
        }
        if chunk.is_empty() {
            return Err(AppError::new(
                ErrorCode::Failed,
                format!("Reading {name} from {node} stopped making progress."),
            ));
        }
        if chunks >= MAX_SMALL_FILE_CHUNKS {
            return Err(AppError::new(
                ErrorCode::Denied,
                format!("Reading {name} from {node} exceeded the small-file chunk cap."),
            ));
        }
        let elapsed = started_at.elapsed();
        if elapsed >= METADATA_PROGRESS_GRACE {
            let min_bytes = (elapsed.as_secs_f64() * MIN_METADATA_BYTES_PER_SEC as f64) as u64;
            if offset < min_bytes {
                return Err(AppError::new(
                    ErrorCode::Timeout,
                    format!(
                        "Metadata download rate for {name} from {node} fell below {} KiB/s.",
                        MIN_METADATA_BYTES_PER_SEC / 1024
                    ),
                ));
            }
        }
    }
    Ok(out)
}

fn read_floor_value(floor_path: &Path) -> u64 {
    match read_sequence_floor(floor_path) {
        SequenceFloorState::Recorded(seq) => seq,
        SequenceFloorState::Missing | SequenceFloorState::Invalid(_) => 0,
    }
}

async fn verify_downloaded_set_async(
    set_dir: PathBuf,
    expected_artifact_name: String,
    anchors: TrustAnchors,
    target: TargetPlatform,
    running_sequence: u64,
    floor_path: PathBuf,
) -> Result<(PathBuf, u64, String), AppError> {
    tokio::task::spawn_blocking(move || {
        verify_downloaded_set(
            &set_dir,
            &expected_artifact_name,
            &anchors,
            target,
            running_sequence,
            &floor_path,
        )
    })
    .await
    .map_err(|err| {
        AppError::new(
            ErrorCode::Internal,
            format!("Verification worker stopped unexpectedly: {err}"),
        )
    })?
}

fn verify_downloaded_set(
    set_dir: &Path,
    expected_artifact_name: &str,
    anchors: &TrustAnchors,
    target: TargetPlatform,
    running_sequence: u64,
    floor_path: &Path,
) -> Result<(PathBuf, u64, String), AppError> {
    let sums_bytes = read_bounded(set_dir.join(CHECKSUMS_FILENAME), MAX_SUMS_BYTES)?;
    let sig_bytes = read_bounded(set_dir.join(SIGNATURE_FILENAME), MAX_SIG_BYTES)?;
    let sig_text = String::from_utf8(sig_bytes).map_err(|err| {
        AppError::new(
            ErrorCode::Denied,
            format!("Stored {SIGNATURE_FILENAME} is not valid UTF-8: {err}"),
        )
    })?;
    let manifest_bytes = read_bounded(set_dir.join(MANIFEST_FILENAME), MAX_MANIFEST_BYTES)?;
    let meta = verify_set_metadata(&sums_bytes, &sig_text, &manifest_bytes, anchors)
        .map_err(|e| AppError::new(e.code.into(), e.message))?;
    let artifact = select_verified_artifact(&meta, running_sequence, floor_path, target)
        .map_err(|e| AppError::new(e.code.into(), e.message))?;

    if artifact.name != expected_artifact_name {
        return Err(AppError::new(
            ErrorCode::Denied,
            format!(
                "Downloaded artifact mismatch: expected {}, verified {}.",
                expected_artifact_name, artifact.name
            ),
        ));
    }
    let artifact_path = set_dir.join(&artifact.name);
    let sha = verify_artifact_file(&meta, &artifact.name, &artifact_path)
        .map_err(|e| AppError::new(e.code.into(), e.message))?;
    Ok((artifact_path, artifact.size, sha))
}

fn read_bounded(path: PathBuf, max_bytes: usize) -> Result<Vec<u8>, AppError> {
    let meta = fs::symlink_metadata(&path).map_err(|err| {
        AppError::new(
            ErrorCode::NotFound,
            format!("Missing update file {}: {err}", path.display()),
        )
    })?;
    if !meta.file_type().is_file() || meta.len() > max_bytes as u64 {
        return Err(AppError::new(
            ErrorCode::Denied,
            format!("Update file {} is invalid or too large.", path.display()),
        ));
    }
    fs::read(&path).map_err(|err| {
        AppError::new(
            ErrorCode::Failed,
            format!("Could not read {}: {err}", path.display()),
        )
    })
}

fn ensure_updates_root(dir: &Path) -> Result<(), AppError> {
    fs::create_dir_all(dir).map_err(|err| {
        AppError::new(
            ErrorCode::Failed,
            format!("Could not create directory {}: {err}", dir.display()),
        )
    })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(dir, fs::Permissions::from_mode(0o700));
    }
    Ok(())
}

fn download_dir_for(updates_dir: &Path, channel: Channel, sequence: u64) -> PathBuf {
    updates_dir
        .join("downloads")
        .join(format!("{}-{sequence}", channel.as_str()))
}

fn cleanup_old_started_markers(updates_dir: &Path, current_sequence: u64) {
    let Ok(entries) = fs::read_dir(updates_dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(text) = name.to_str() else { continue };
        if let Some(rest) = text.strip_prefix("started-")
            && let Some(seq_str) = rest.strip_suffix(".ok")
            && let Ok(seq) = seq_str.parse::<u64>()
            && seq < current_sequence
        {
            let _ = fs::remove_file(entry.path());
        }
    }
}

fn cleanup_old_downloads(updates_dir: &Path, current_sequence: u64) {
    let dir = updates_dir.join("downloads");
    let Ok(entries) = fs::read_dir(&dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(text) = name.to_str() else { continue };
        let seq = text.rsplit('-').next().and_then(|s| s.parse::<u64>().ok());
        if seq.is_some_and(|s| s <= current_sequence) {
            let _ = fs::remove_dir_all(entry.path());
        }
    }
}

fn cleanup_other_downloads(updates_dir: &Path, keep_channel: Channel, keep_sequence: u64) {
    let keep = format!("{}-{keep_sequence}", keep_channel.as_str());
    let dir = updates_dir.join("downloads");
    let Ok(entries) = fs::read_dir(&dir) else {
        return;
    };
    for entry in entries.flatten() {
        if entry.file_name().to_str() != Some(keep.as_str()) {
            let _ = fs::remove_dir_all(entry.path());
        }
    }
}

fn has_unread_supervisor_records(updates_dir: &Path) -> bool {
    let Ok(entries) = fs::read_dir(updates_dir) else {
        return false;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(s) = name.to_str() else { continue };
        if (s.starts_with("rollback-") || s.starts_with("deferred-")) && s.ends_with(".json") {
            return true;
        }
    }
    false
}

/// Reconciles supervisor outcome markers (`stale-lock.json`, `deferred-*.json`, `rollback-*.json`),
/// interrupted `pending_install` records, and leftover staging files.
///
/// Called both in [`UpdateManager::with_config_at`] (before anything can ask the auto-install gate)
/// and again in [`UpdateManager::on_app_ready_at`]. Leaves a matching `pending_install`
/// (`pending.sequence == current_sequence`) intact so `on_app_ready_at` confirms it into
/// `just_updated` only after first paint.
fn reconcile_supervisor_records_at(
    updates_dir: &Path,
    current_sequence: u64,
    platform: &PlatformEnv,
    prefs: &mut PersistedPrefs,
    now_unix: i64,
) -> (bool, bool, Option<String>) {
    let mut prefs_dirty = false;
    let mut saw_rollback = false;
    let mut rollback_detail: Option<String> = None;

    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    {
        if desktop::take_stale_lock_notice(updates_dir).is_some() {
            prefs.stale_update_notice = Some(desktop::STALE_UPDATE_NOTICE.to_owned());
            prefs_dirty = true;
        }
        if let PlatformEnv::Desktop(desk) = platform {
            desktop::cleanup_interrupted_staging_files(updates_dir, desk);
        }
    }
    #[cfg(any(target_os = "android", target_os = "ios"))]
    let _ = platform;

    // 0. Consume all clean pre-swap deferral records left by a desktop supervisor.
    while let Some((defer_path, deferred)) = read_and_take_deferred(updates_dir) {
        let _ = fs::remove_file(defer_path);
        #[cfg(not(any(target_os = "android", target_os = "ios")))]
        desktop::clear_pending_stage(updates_dir, deferred.sequence);
        if prefs
            .pending_install
            .as_ref()
            .is_some_and(|p| p.sequence == deferred.sequence)
        {
            prefs.pending_install = None;
            prefs.decrement_started_attempt(deferred.sequence);
            prefs_dirty = true;
        }
    }

    // 1. Consume all desktop rollback markers left by a detached supervisor.
    while let Some((rb_path, rb)) = read_and_take_rollback(updates_dir) {
        let _ = fs::remove_file(rb_path);
        #[cfg(not(any(target_os = "android", target_os = "ios")))]
        desktop::clear_pending_stage(updates_dir, rb.sequence);

        // If the running copy IS the sequence named by the rollback record (for example
        // `new_kept_files_in_use` or `new_kept_backup_missing` where the new copy is running fine),
        // the install succeeded: never show a rollback notice about keeping a previous copy.
        if current_sequence > 0 && rb.sequence == current_sequence {
            prefs.rolled_back = None;
            prefs.stale_update_notice = None;
            prefs.failed_sequences.retain(|&s| s > current_sequence);
            prefs
                .failed_updates
                .retain(|r| r.sequence > current_sequence);
            prefs
                .started_attempts
                .retain(|r| r.sequence > current_sequence);
            #[cfg(not(any(target_os = "android", target_os = "ios")))]
            {
                let _ = fs::remove_file(updates_dir.join(desktop::NEW_START_LOG_FILENAME));
                let _ = fs::remove_file(updates_dir.join(desktop::ROLLBACK_PS_FILENAME));
            }
            prefs_dirty = true;
            continue;
        }

        let channel = prefs
            .pending_install
            .as_ref()
            .map_or(Channel::Stable, |p| p.channel);
        #[cfg(not(any(target_os = "android", target_os = "ios")))]
        let rb = {
            let captured = desktop::read_captured_startup_detail(updates_dir);
            let state_note = match rb.install_state.as_deref() {
                Some("new_kept_files_in_use") => Some(
                    "Previous copy could not be restored because install files were in use; kept new copy intact."
                        .to_owned(),
                ),
                Some("new_kept_backup_missing") => Some(
                    "Previous copy backup was missing; kept new copy in place.".to_owned(),
                ),
                Some("restore_incomplete") => Some(
                    "Previous copy could not be fully restored after a rollback error.".to_owned(),
                ),
                _ => None,
            };
            let detail = match (rb.detail.or(state_note), captured) {
                (Some(a), Some(b)) if !a.contains(&b) => Some(format!("{a}\n{b}")),
                (Some(a), _) => Some(a),
                (None, Some(b)) => Some(b),
                (None, None) => None,
            };
            RollbackInfo { detail, ..rb }
        };
        let prev_attempts = prefs
            .failed_updates
            .iter()
            .find(|r| r.sequence == rb.sequence)
            .map_or(0, |r| r.attempts);
        let started_count = prefs.started_attempts_count(rb.sequence);
        let attempts = prev_attempts.saturating_add(1).max(started_count);
        prefs.ensure_started_attempts_at_least(rb.sequence, attempts, now_unix);
        let skipped = attempts >= MAX_AUTO_ROLLBACK_ATTEMPTS;
        let retry_after = if skipped {
            None
        } else {
            Some(format_rfc3339_utc(
                now_unix.saturating_add(ROLLBACK_RETRY_COOLDOWN_SECS),
            ))
        };
        rollback_detail = rb.detail.clone();
        prefs.record_failure(FailedUpdateRecord {
            channel,
            version: rb.version.clone(),
            sequence: rb.sequence,
            reason: if rb.reason.trim().is_empty() {
                ROLLBACK_REASON.to_owned()
            } else {
                rb.reason.clone()
            },
            failed_at: format_rfc3339_utc(now_unix),
            attempts,
            skipped,
            retry_after,
            detail: rb.detail.clone(),
        });
        prefs.pending_install = None;
        prefs.rolled_back = Some(rb);
        prefs_dirty = true;
        saw_rollback = true;
    }

    // Clear any stale persisted rollback notice if the running copy now IS that sequence.
    if current_sequence > 0
        && prefs
            .rolled_back
            .as_ref()
            .is_some_and(|r| r.sequence == current_sequence)
    {
        prefs.rolled_back = None;
        prefs.failed_sequences.retain(|&s| s > current_sequence);
        prefs
            .failed_updates
            .retain(|r| r.sequence > current_sequence);
        prefs
            .started_attempts
            .retain(|r| r.sequence > current_sequence);
        prefs_dirty = true;
    }

    // 2. Reconcile a mismatched `pending_install` when no rollback/deferral file existed and no
    //    active update lock is held. If the install never progressed past `"staged"` or
    //    `"swapping"` (old copy is still running), treat it as a clean deferral (decrementing the
    //    started-attempt count and recording NO failure); only `"swapped"`/`"started"` without a
    //    confirmed new sequence counts as a failed start.
    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    let lock_active = desktop::inspect_active_update_lock(updates_dir).is_some();
    #[cfg(any(target_os = "android", target_os = "ios"))]
    let lock_active = false;

    if !lock_active
        && !saw_rollback
        && current_sequence > 0
        && let Some(pending) = prefs.pending_install.clone()
        && pending.sequence != current_sequence
    {
        #[cfg(not(any(target_os = "android", target_os = "ios")))]
        let effective_stage = {
            let s = desktop::read_pending_stage(updates_dir, pending.sequence)
                .unwrap_or_else(|| pending.stage.clone());
            desktop::clear_pending_stage(updates_dir, pending.sequence);
            s
        };
        #[cfg(any(target_os = "android", target_os = "ios"))]
        let effective_stage = pending.stage.clone();

        if matches!(effective_stage.as_str(), "staged" | "swapping") {
            prefs.pending_install = None;
            prefs.decrement_started_attempt(pending.sequence);
            prefs_dirty = true;
        } else {
            let reason = format!(
                "Version {} did not take effect after installation, so Legion will not retry it automatically.",
                pending.version
            );
            let prev_attempts = prefs
                .failed_updates
                .iter()
                .find(|r| r.sequence == pending.sequence)
                .map_or(0, |r| r.attempts);
            let started_count = prefs.started_attempts_count(pending.sequence);
            let attempts = prev_attempts.saturating_add(1).max(started_count);
            prefs.ensure_started_attempts_at_least(pending.sequence, attempts, now_unix);
            prefs.record_failure(FailedUpdateRecord {
                channel: pending.channel,
                version: pending.version,
                sequence: pending.sequence,
                reason,
                failed_at: format_rfc3339_utc(now_unix),
                attempts,
                skipped: true,
                retry_after: None,
                detail: None,
            });
            prefs.pending_install = None;
            prefs_dirty = true;
        }
    }

    (prefs_dirty, saw_rollback, rollback_detail)
}

fn read_and_take_rollback(updates_dir: &Path) -> Option<(PathBuf, RollbackInfo)> {
    let entries = fs::read_dir(updates_dir).ok()?;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(s) = name.to_str() else { continue };
        if s.starts_with("rollback-") && s.ends_with(".json") {
            let path = entry.path();
            if let Ok(bytes) = fs::read(&path) {
                let slice = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(&bytes);
                if let Ok(info) = serde_json::from_slice::<RollbackInfo>(slice) {
                    return Some((path, info));
                }
            }
            let _ = fs::remove_file(&path);
        }
    }
    None
}

fn read_and_take_deferred(updates_dir: &Path) -> Option<(PathBuf, DeferredInstallRecord)> {
    let entries = fs::read_dir(updates_dir).ok()?;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(s) = name.to_str() else { continue };
        if s.starts_with("deferred-") && s.ends_with(".json") {
            let path = entry.path();
            if let Ok(bytes) = fs::read(&path) {
                let slice = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(&bytes);
                if let Ok(info) = serde_json::from_slice::<DeferredInstallRecord>(slice) {
                    return Some((path, info));
                }
            }
            let _ = fs::remove_file(&path);
        }
    }
    None
}

fn load_prefs(path: &Path) -> Result<PersistedPrefs, AppError> {
    let bytes = match fs::read(path) {
        Ok(b) => b,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Ok(PersistedPrefs::default());
        }
        Err(err) => {
            return Err(AppError::new(
                ErrorCode::Internal,
                format!("Could not read update preferences: {err}"),
            ));
        }
    };
    serde_json::from_slice(&bytes).map_err(|err| {
        AppError::new(
            ErrorCode::Internal,
            format!("Update preferences file is corrupt: {err}"),
        )
    })
}

fn save_prefs(path: &Path, prefs: &PersistedPrefs) -> Result<(), AppError> {
    let bytes = serde_json::to_vec_pretty(prefs).map_err(|err| {
        AppError::new(
            ErrorCode::Internal,
            format!("Could not serialize update preferences: {err}"),
        )
    })?;
    write_atomic(path, &bytes)
}

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), AppError> {
    let parent = path.parent().ok_or_else(|| {
        AppError::new(
            ErrorCode::Internal,
            format!("Path {} has no parent directory.", path.display()),
        )
    })?;
    ensure_updates_root(parent)?;
    let mut tmp = NamedTempFile::new_in(parent).map_err(|err| {
        AppError::new(
            ErrorCode::Failed,
            format!("Could not create temp file in {}: {err}", parent.display()),
        )
    })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = tmp
            .as_file()
            .set_permissions(fs::Permissions::from_mode(0o600));
    }
    tmp.write_all(bytes)
        .and_then(|()| tmp.as_file().sync_all())
        .map_err(|err| {
            AppError::new(
                ErrorCode::Failed,
                format!("Could not write {}: {err}", path.display()),
            )
        })?;
    tmp.persist(path).map_err(|err| {
        AppError::new(
            ErrorCode::Failed,
            format!("Could not save {}: {}", path.display(), err.error),
        )
    })?;
    Ok(())
}

/// Returns the current [`legion_update::Os`].
pub fn current_os() -> Os {
    #[cfg(target_os = "android")]
    {
        Os::Android
    }
    #[cfg(target_os = "ios")]
    {
        Os::Ios
    }
    #[cfg(target_os = "macos")]
    {
        Os::Macos
    }
    #[cfg(target_os = "windows")]
    {
        Os::Windows
    }
    #[cfg(not(any(
        target_os = "android",
        target_os = "ios",
        target_os = "macos",
        target_os = "windows"
    )))]
    {
        Os::Linux
    }
}

/// Returns the current [`legion_update::Arch`].
pub fn current_arch() -> Arch {
    #[cfg(target_arch = "aarch64")]
    {
        Arch::Aarch64
    }
    #[cfg(not(target_arch = "aarch64"))]
    {
        Arch::X86_64
    }
}

fn now_unix_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

fn format_rfc3339_utc(unix_secs: i64) -> String {
    let secs = unix_secs.max(0) as u64;
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

fn parse_rfc3339_utc(s: &str) -> Option<i64> {
    let bytes = s.as_bytes();
    if bytes.len() < 20
        || bytes[4] != b'-'
        || bytes[7] != b'-'
        || (bytes[10] != b'T' && bytes[10] != b't')
        || bytes[13] != b':'
        || bytes[16] != b':'
        || !s.ends_with('Z')
    {
        return None;
    }
    let year: i64 = s.get(0..4)?.parse().ok()?;
    let month: u32 = s.get(5..7)?.parse().ok()?;
    let day: u32 = s.get(8..10)?.parse().ok()?;
    let hour: i64 = s.get(11..13)?.parse().ok()?;
    let minute: i64 = s.get(14..16)?.parse().ok()?;
    let second: i64 = s.get(17..19)?.parse().ok()?;
    if !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
        || !(0..=23).contains(&hour)
        || !(0..=59).contains(&minute)
        || !(0..=60).contains(&second)
    {
        return None;
    }
    let y = year - i64::from(month <= 2);
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = (y - era * 400) as u64;
    let mp = if month > 2 { month - 3 } else { month + 9 } as u64;
    let doy = (153 * mp + 2) / 5 + (day as u64) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + (doe as i64) - 719_468;
    Some(days * 86_400 + hour * 3_600 + minute * 60 + second)
}

fn now_rfc3339_utc() -> String {
    format_rfc3339_utc(now_unix_secs())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use std::collections::HashMap;
    use std::fmt::Write as _;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use ed25519_dalek::{Signer, SigningKey};
    use legion_update::{encode_signature_bytes, sha256_bytes};

    use super::*;

    fn keypair(seed: u8) -> (SigningKey, [u8; 32]) {
        let sk = SigningKey::from_bytes(&[seed; 32]);
        let pk = sk.verifying_key().to_bytes();
        (sk, pk)
    }

    #[derive(Clone)]
    struct FakeSet {
        channel: Channel,
        sequence: u64,
        artifacts: Vec<NodeListedArtifact>,
        files: HashMap<String, Vec<u8>>,
    }

    #[derive(Clone, Copy)]
    enum NodeBehavior {
        Normal,
        TrickleOneByte,
        WrongOffset,
        ShortRead,
        StallListForever,
    }

    struct FakeFleet {
        nodes: HashMap<String, Vec<FakeSet>>,
        behaviors: HashMap<String, NodeBehavior>,
        chunk_cap: usize,
        fail_after_reads: Option<usize>,
        reads: AtomicUsize,
        artifact_reads_by_node: Mutex<HashMap<String, usize>>,
        stalled_list_entered: Arc<tokio::sync::Notify>,
        stalled_list_entered_count: Arc<AtomicUsize>,
        stalled_list_dropped: Arc<AtomicBool>,
    }

    impl FakeFleet {
        fn new() -> Self {
            Self {
                nodes: HashMap::new(),
                behaviors: HashMap::new(),
                chunk_cap: 16 * 1024,
                fail_after_reads: None,
                reads: AtomicUsize::new(0),
                artifact_reads_by_node: Mutex::new(HashMap::new()),
                stalled_list_entered: Arc::new(tokio::sync::Notify::new()),
                stalled_list_entered_count: Arc::new(AtomicUsize::new(0)),
                stalled_list_dropped: Arc::new(AtomicBool::new(false)),
            }
        }

        async fn wait_stalled_list_entered(&self) {
            loop {
                let notified = self.stalled_list_entered.notified();
                tokio::pin!(notified);
                notified.as_mut().enable();
                if self.stalled_list_entered_count.load(Ordering::SeqCst) > 0 {
                    return;
                }
                notified.await;
            }
        }

        fn stalled_list_dropped(&self) -> bool {
            self.stalled_list_dropped.load(Ordering::SeqCst)
        }

        fn add_set(&mut self, node: &str, set: FakeSet) {
            self.nodes.entry(node.to_owned()).or_default().push(set);
        }

        fn set_behavior(&mut self, node: &str, behavior: NodeBehavior) {
            self.behaviors.insert(node.to_owned(), behavior);
        }

        fn artifact_reads(&self, node: &str) -> usize {
            self.artifact_reads_by_node
                .lock()
                .unwrap()
                .get(node)
                .copied()
                .unwrap_or(0)
        }
    }

    impl UpdateSourceClient for FakeFleet {
        fn list_sets(&self, node: &str) -> BoxFuture<'_, Result<NodeUpdateList, AppError>> {
            if matches!(
                self.behaviors.get(node),
                Some(NodeBehavior::StallListForever)
            ) {
                let entered = Arc::clone(&self.stalled_list_entered);
                let entered_count = Arc::clone(&self.stalled_list_entered_count);
                let dropped = Arc::clone(&self.stalled_list_dropped);
                struct DropFlag(Arc<AtomicBool>);
                impl Drop for DropFlag {
                    fn drop(&mut self) {
                        self.0.store(true, Ordering::SeqCst);
                    }
                }
                return Box::pin(async move {
                    let _drop_guard = DropFlag(dropped);
                    entered_count.fetch_add(1, Ordering::SeqCst);
                    entered.notify_waiters();
                    std::future::pending::<()>().await;
                    unreachable!()
                });
            }
            let res = match self.nodes.get(node) {
                Some(sets) => Ok(NodeUpdateList {
                    node: node.to_owned(),
                    sets: sets
                        .iter()
                        .map(|s| NodeListedSet {
                            channel: s.channel,
                            sequence: s.sequence,
                            artifacts: s.artifacts.clone(),
                        })
                        .collect(),
                }),
                None => Err(AppError::new(
                    ErrorCode::Unreachable,
                    format!("{node} is offline"),
                )),
            };
            Box::pin(async move { res })
        }

        fn read_chunk(
            &self,
            node: &str,
            channel: Channel,
            sequence: u64,
            name: &str,
            offset: u64,
            length: u64,
        ) -> BoxFuture<'_, Result<UpdateReadChunk, AppError>> {
            let count = self.reads.fetch_add(1, Ordering::SeqCst);
            if !matches!(
                name,
                CHECKSUMS_FILENAME | SIGNATURE_FILENAME | MANIFEST_FILENAME
            ) {
                *self
                    .artifact_reads_by_node
                    .lock()
                    .unwrap()
                    .entry(node.to_owned())
                    .or_insert(0) += 1;
            }
            if let Some(limit) = self.fail_after_reads
                && count == limit
            {
                return Box::pin(async {
                    Err(AppError::new(
                        ErrorCode::Unreachable,
                        "Simulated disconnect mid-download",
                    ))
                });
            }
            let behavior = self
                .behaviors
                .get(node)
                .copied()
                .unwrap_or(NodeBehavior::Normal);
            let is_artifact = !matches!(
                name,
                CHECKSUMS_FILENAME | SIGNATURE_FILENAME | MANIFEST_FILENAME
            );
            let res = self
                .nodes
                .get(node)
                .and_then(|sets| {
                    sets.iter()
                        .find(|s| s.channel == channel && s.sequence == sequence)
                })
                .and_then(|s| s.files.get(name))
                .map(|bytes| {
                    let size = bytes.len() as u64;
                    let start = (offset as usize).min(bytes.len());
                    let cap = if is_artifact && matches!(behavior, NodeBehavior::TrickleOneByte) {
                        1
                    } else {
                        self.chunk_cap
                    };
                    let take = (length as usize)
                        .min(cap)
                        .min(bytes.len().saturating_sub(start));
                    let slice = &bytes[start..start + take];
                    match behavior {
                        NodeBehavior::WrongOffset if is_artifact => UpdateReadChunk {
                            size,
                            offset: offset.saturating_add(3),
                            length: slice.len() as u64,
                            content: BASE64.encode(slice),
                            eof: start + take >= bytes.len(),
                        },
                        NodeBehavior::ShortRead if is_artifact => {
                            let short = &slice[..slice.len().min(4)];
                            UpdateReadChunk {
                                size,
                                offset,
                                length: short.len() as u64,
                                content: BASE64.encode(short),
                                eof: true,
                            }
                        }
                        _ => UpdateReadChunk {
                            size,
                            offset,
                            length: slice.len() as u64,
                            content: BASE64.encode(slice),
                            eof: start + take >= bytes.len(),
                        },
                    }
                })
                .ok_or_else(|| AppError::new(ErrorCode::NotFound, format!("missing {name}")));
            Box::pin(async move { res })
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn build_fake_set(
        channel: Channel,
        version: &str,
        sequence: u64,
        artifact_name: &str,
        os: Os,
        arch: Arch,
        kind: InstallKind,
        payload: &[u8],
        sk: &SigningKey,
    ) -> FakeSet {
        let manifest_bytes = serde_json::to_vec_pretty(&json!({
            "channel": channel,
            "version": version,
            "build": "abcdef123456",
            "sequence": sequence,
            "published_at": "2026-11-25T02:00:00Z",
            "notes": format!("Notes for {version}"),
            "artifacts": [{
                "name": artifact_name,
                "program": "app",
                "os": os,
                "arch": arch,
                "kind": kind,
                "size": payload.len(),
            }],
        }))
        .unwrap();
        let mut entries = vec![
            (MANIFEST_FILENAME.to_owned(), sha256_bytes(&manifest_bytes)),
            (artifact_name.to_owned(), sha256_bytes(payload)),
        ];
        entries.sort_by(|a, b| a.0.cmp(&b.0));
        let mut sums = String::new();
        for (name, h) in entries {
            let _ = writeln!(&mut sums, "{h}  {name}");
        }
        let sig = encode_signature_bytes(&sk.sign(sums.as_bytes()).to_bytes());
        let mut files = HashMap::new();
        files.insert(CHECKSUMS_FILENAME.to_owned(), sums.into_bytes());
        files.insert(SIGNATURE_FILENAME.to_owned(), sig.into_bytes());
        files.insert(MANIFEST_FILENAME.to_owned(), manifest_bytes);
        files.insert(artifact_name.to_owned(), payload.to_vec());
        FakeSet {
            channel,
            sequence,
            artifacts: vec![NodeListedArtifact {
                name: artifact_name.to_owned(),
                program: Program::App,
                os,
                arch,
                kind,
                size: Some(payload.len() as u64),
            }],
            files,
        }
    }

    #[tokio::test]
    async fn selects_newest_across_nodes_resumes_chunked_download_and_confirms_on_restart() {
        let dir = tempfile::tempdir().unwrap();
        let updates_dir = dir.path().join("updates");
        let (rel_sk, rel_pk) = keypair(0x61);
        let (prev_sk, prev_pk) = keypair(0x62);
        let anchors = TrustAnchors::with_keys(rel_pk, Some(prev_pk));

        let payload = vec![0x7Au8; 40 * 1024];
        let mut fleet = FakeFleet::new();
        fleet.add_set(
            "tower",
            build_fake_set(
                Channel::Stable,
                "0.2.0",
                202611250201,
                "Legion.AppImage",
                Os::Linux,
                Arch::X86_64,
                InstallKind::AppImage,
                b"older",
                &rel_sk,
            ),
        );
        fleet.add_set(
            "pi",
            build_fake_set(
                Channel::Preview,
                "0.2.1",
                202611250202,
                "Legion.AppImage",
                Os::Linux,
                Arch::X86_64,
                InstallKind::AppImage,
                &payload,
                &prev_sk,
            ),
        );
        fleet.fail_after_reads = Some(4);

        let mgr = UpdateManager::with_config(
            updates_dir.clone(),
            anchors,
            Os::Linux,
            Arch::X86_64,
            PlatformEnv::Desktop(DesktopInstall::LinuxAppImage {
                appimage_path: dir.path().join("Legion.AppImage"),
            }),
            "0.1.0".to_owned(),
            "oldbuild".to_owned(),
            202611250200,
        );
        let nodes = vec!["offline".to_owned(), "tower".to_owned(), "pi".to_owned()];

        let st1 = mgr
            .check_with_client(&fleet, &nodes, false, false)
            .await
            .unwrap();
        assert_eq!(st1.phase, "error", "{st1:?}");
        // Headline error is the app's own calm sentence; raw disconnect text stays in detail.
        assert!(!st1.last_error.as_deref().unwrap().contains("Simulated"));
        assert!(
            st1.last_error_detail
                .as_deref()
                .unwrap()
                .contains("Simulated")
        );
        let part = download_dir_for(&updates_dir, Channel::Preview, 202611250202)
            .join("Legion.AppImage.part");
        assert_eq!(fs::metadata(&part).unwrap().len(), 16 * 1024);

        let st2 = mgr
            .check_with_client(&fleet, &nodes, false, false)
            .await
            .unwrap();
        assert_eq!(st2.phase, "ready", "{st2:?}");
        let avail = st2.available.clone().unwrap();
        assert_eq!(avail.sequence, 202611250202);
        assert_eq!(avail.source_node, "pi");
        assert!(avail.downloaded);
        assert!(!part.exists());

        // Drop an old started marker to verify cleanup_old_started_markers removes it.
        fs::write(updates_dir.join("started-202611250200.ok"), b"ok\n").unwrap();

        mgr.record_pending_install(&avail).unwrap();
        let mgr_after = UpdateManager::with_config(
            updates_dir.clone(),
            anchors,
            Os::Linux,
            Arch::X86_64,
            PlatformEnv::Desktop(DesktopInstall::LinuxAppImage {
                appimage_path: dir.path().join("Legion.AppImage"),
            }),
            "0.2.1".to_owned(),
            "abcdef123456".to_owned(),
            202611250202,
        );
        mgr_after.on_app_ready();
        let ready_state = mgr_after.snapshot();
        assert_eq!(ready_state.sequence_floor, 202611250202);
        let notice = ready_state.just_updated.unwrap();
        assert_eq!(notice.version, "0.2.1");
        assert_eq!(notice.notes, "Notes for 0.2.1");
        assert!(updates_dir.join("started-202611250202.ok").is_file());
        assert!(!updates_dir.join("started-202611250200.ok").exists());

        let dismissed = mgr_after.dismiss_notice().unwrap();
        assert!(dismissed.just_updated.is_none());
    }

    #[tokio::test]
    async fn falls_back_across_hostile_nodes_for_garbage_trickle_wrong_offset_and_short_read() {
        let (rel_sk, rel_pk) = keypair(0x81);
        let anchors = TrustAnchors::with_keys(rel_pk, None);
        let payload = vec![0x42u8; 32 * 1024];

        for (label, behavior, corrupt_bytes) in [
            ("garbage-bytes", NodeBehavior::Normal, true),
            ("one-byte-trickle", NodeBehavior::TrickleOneByte, false),
            ("wrong-offset", NodeBehavior::WrongOffset, false),
            ("short-read", NodeBehavior::ShortRead, false),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let good_set = build_fake_set(
                Channel::Stable,
                "0.2.0",
                202611250210,
                "Legion.apk",
                Os::Android,
                Arch::Universal,
                InstallKind::Apk,
                &payload,
                &rel_sk,
            );
            let mut bad_set = good_set.clone();
            if corrupt_bytes {
                let mut corrupted = payload.clone();
                corrupted[0] ^= 0xFF;
                bad_set.files.insert("Legion.apk".to_owned(), corrupted);
            }

            let mut fleet = FakeFleet::new();
            fleet.add_set("hostile", bad_set);
            fleet.add_set("healthy", good_set);
            fleet.set_behavior("hostile", behavior);

            let mgr = UpdateManager::with_config(
                dir.path().join("updates"),
                anchors,
                Os::Android,
                Arch::Aarch64,
                PlatformEnv::Android,
                "0.1.0".to_owned(),
                "old".to_owned(),
                202611250200,
            );
            let nodes = vec!["hostile".to_owned(), "healthy".to_owned()];
            let st = mgr
                .check_with_client(&fleet, &nodes, false, false)
                .await
                .unwrap();
            assert_eq!(st.phase, "ready", "case={label}: {st:?}");
            let avail = st.available.unwrap();
            assert_eq!(avail.source_node, "healthy", "case={label}");
            assert!(avail.downloaded, "case={label}");
            // Verify hostile node is now deprioritized behind healthy.
            let ordered = mgr.order_nodes_for(Channel::Stable, 202611250210, &nodes);
            assert_eq!(ordered[0], "healthy", "case={label}");
            assert_eq!(ordered[1], "hostile", "case={label}");
        }
    }

    #[tokio::test]
    async fn caps_fake_sequence_flood_and_still_finds_real_update() {
        let dir = tempfile::tempdir().unwrap();
        let (rel_sk, rel_pk) = keypair(0x82);
        let (bad_sk, _) = keypair(0x83);
        let anchors = TrustAnchors::with_keys(rel_pk, None);

        let mut fleet = FakeFleet::new();
        // Hostile node advertises 2,000 bogus untrusted sequences.
        for i in 0..2000u64 {
            let seq = 202611250300 + i;
            // Make valid calendar timestamps by offsetting minutes safely.
            let hh = ((seq - 202611250300) / 60) % 20;
            let mm = (seq - 202611250300) % 60;
            let dd = 1 + ((seq - 202611250300) / (20 * 60));
            let valid_seq = 202612000000 + dd * 10_000 + hh * 100 + mm;
            fleet.add_set(
                "flooder",
                build_fake_set(
                    Channel::Stable,
                    "9.9.9",
                    valid_seq,
                    "Legion.apk",
                    Os::Android,
                    Arch::Universal,
                    InstallKind::Apk,
                    b"evil",
                    &bad_sk,
                ),
            );
        }
        // Healthy node advertises the real signed update.
        fleet.add_set(
            "healthy",
            build_fake_set(
                Channel::Stable,
                "0.2.0",
                202611250210,
                "Legion.apk",
                Os::Android,
                Arch::Universal,
                InstallKind::Apk,
                b"real-apk-bytes",
                &rel_sk,
            ),
        );

        let mgr = UpdateManager::with_config(
            dir.path().join("updates"),
            anchors,
            Os::Android,
            Arch::Aarch64,
            PlatformEnv::Android,
            "0.1.0".to_owned(),
            "old".to_owned(),
            202611250200,
        );
        let st = mgr
            .check_with_client(
                &fleet,
                &["flooder".to_owned(), "healthy".to_owned()],
                false,
                false,
            )
            .await
            .unwrap();
        assert_eq!(st.phase, "ready", "{st:?}");
        assert_eq!(st.available.unwrap().sequence, 202611250210);
        // Proves flooder was capped (at most MAX_CANDIDATES_PER_NODE * 2 metadata reads, nowhere
        // near 2,000 * 2).
        assert!(fleet.reads.load(Ordering::SeqCst) < 100);
    }

    #[tokio::test]
    async fn metered_connection_pauses_auto_download_over_5mib_and_manual_override_succeeds() {
        let dir = tempfile::tempdir().unwrap();
        let (rel_sk, rel_pk) = keypair(0x84);
        let anchors = TrustAnchors::with_keys(rel_pk, None);
        // 5 MiB + 1 KiB payload exceeds METERED_AUTO_DOWNLOAD_MAX_BYTES.
        let big_payload = vec![0x55u8; (METERED_AUTO_DOWNLOAD_MAX_BYTES as usize) + 1024];

        let mut fleet = FakeFleet::new();
        fleet.chunk_cap = 512 * 1024;
        fleet.add_set(
            "tower",
            build_fake_set(
                Channel::Stable,
                "0.2.0",
                202611250210,
                "Legion.apk",
                Os::Android,
                Arch::Universal,
                InstallKind::Apk,
                &big_payload,
                &rel_sk,
            ),
        );

        let mgr = UpdateManager::with_config(
            dir.path().join("updates"),
            anchors,
            Os::Android,
            Arch::Aarch64,
            PlatformEnv::Android,
            "0.1.0".to_owned(),
            "old".to_owned(),
            202611250200,
        );

        let nodes = vec!["tower".to_owned()];
        // Auto check on metered connection pauses download without pulling artifact bytes.
        let st_metered = mgr
            .check_with_client(&fleet, &nodes, false, true)
            .await
            .unwrap();
        assert_eq!(st_metered.phase, "available", "{st_metered:?}");
        assert!(st_metered.metered_paused);
        assert!(!st_metered.available.unwrap().downloaded);
        assert_eq!(fleet.artifact_reads("tower"), 0);

        // Explicit manual tap (`force_download = true`) proceeds even on metered connection.
        let st_forced = mgr
            .check_with_client(&fleet, &nodes, true, true)
            .await
            .unwrap();
        assert_eq!(st_forced.phase, "ready", "{st_forced:?}");
        assert!(!st_forced.metered_paused);
        assert!(st_forced.available.unwrap().downloaded);
        assert!(fleet.artifact_reads("tower") > 0);
    }

    #[tokio::test]
    async fn standalone_and_pacman_never_download_artifacts_and_deb_stages_verified_private_copy() {
        let (rel_sk, rel_pk) = keypair(0x85);
        let anchors = TrustAnchors::with_keys(rel_pk, None);
        let nodes = vec!["tower".to_owned()];

        for (label, install, kind, name) in [
            (
                "standalone",
                DesktopInstall::Standalone {
                    check_kind: InstallKind::AppImage,
                    reason: "standalone".to_owned(),
                },
                InstallKind::AppImage,
                "Legion.AppImage",
            ),
            (
                "pacman",
                DesktopInstall::LinuxPacman {
                    package: "legion-app".to_owned(),
                },
                InstallKind::AppImage,
                "Legion.AppImage",
            ),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let mut fleet = FakeFleet::new();
            fleet.add_set(
                "tower",
                build_fake_set(
                    Channel::Stable,
                    "0.2.0",
                    202611250210,
                    name,
                    Os::Linux,
                    Arch::X86_64,
                    kind,
                    b"should-never-be-downloaded",
                    &rel_sk,
                ),
            );
            let mgr = UpdateManager::with_config(
                dir.path().join("updates"),
                anchors,
                Os::Linux,
                Arch::X86_64,
                PlatformEnv::Desktop(install),
                "0.1.0".to_owned(),
                "old".to_owned(),
                202611250200,
            );
            let st = mgr
                .check_with_client(&fleet, &nodes, false, false)
                .await
                .unwrap();
            assert!(!st.available.unwrap().downloaded, "{label}");
            assert_eq!(
                fleet.artifact_reads("tower"),
                0,
                "{label} must never download artifact bytes"
            );
        }

        // .deb installs exist only on Linux (`sudo apt-get install` with POSIX quoting and Unix
        // `0700` private directory permissions), so test the live staging path on Unix hosts.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;

            let dir = tempfile::tempdir().unwrap();
            let deb_bytes = b"fake-deb-package-bytes";
            let mut fleet = FakeFleet::new();
            fleet.add_set(
                "tower",
                build_fake_set(
                    Channel::Stable,
                    "0.2.0",
                    202611250210,
                    "legion-app_0.2.0_amd64.deb",
                    Os::Linux,
                    Arch::X86_64,
                    InstallKind::Deb,
                    deb_bytes,
                    &rel_sk,
                ),
            );
            let mgr = UpdateManager::with_config(
                dir.path().join("updates"),
                anchors,
                Os::Linux,
                Arch::X86_64,
                PlatformEnv::Desktop(DesktopInstall::LinuxDeb {
                    package: "legion-app".to_owned(),
                }),
                "0.1.0".to_owned(),
                "old".to_owned(),
                202611250200,
            );
            let st = mgr
                .check_with_client(&fleet, &nodes, false, false)
                .await
                .unwrap();
            assert_eq!(st.phase, "manual", "{st:?}");
            let cmd = st.manual_command.unwrap();
            assert!(cmd.starts_with("sudo apt-get install "));
            assert!(
                cmd.contains("/verified/legion-app_0.2.0_amd64.deb"),
                "{cmd}"
            );
            assert_eq!(
                st.manual_sha256.as_deref(),
                Some(sha256_bytes(deb_bytes).as_str())
            );
            let vdir_mode = fs::metadata(dir.path().join("updates/verified"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(vdir_mode, 0o700);
        }
    }

    #[tokio::test]
    async fn sequence_zero_build_disables_self_updates() {
        let dir = tempfile::tempdir().unwrap();
        let (rel_sk, rel_pk) = keypair(0x86);
        let anchors = TrustAnchors::with_keys(rel_pk, None);
        let mut fleet = FakeFleet::new();
        fleet.add_set(
            "tower",
            build_fake_set(
                Channel::Stable,
                "0.2.0",
                202611250210,
                "Legion.apk",
                Os::Android,
                Arch::Universal,
                InstallKind::Apk,
                b"apk",
                &rel_sk,
            ),
        );

        let mgr = UpdateManager::with_config(
            dir.path().join("updates"),
            anchors,
            Os::Android,
            Arch::Aarch64,
            PlatformEnv::Android,
            "0.1.0".to_owned(),
            "dev".to_owned(),
            0,
        );
        assert_eq!(mgr.snapshot().phase, "unsupported");
        assert!(!mgr.snapshot().can_self_install);

        let st = mgr
            .check_with_client(&fleet, &["tower".to_owned()], true, false)
            .await
            .unwrap();
        assert_eq!(st.phase, "unsupported");
        assert!(st.available.is_none());
        assert_eq!(fleet.reads.load(Ordering::SeqCst), 0);
        assert_eq!(
            mgr.prepare_verified_install().unwrap_err().code,
            ErrorCode::Unsupported
        );
    }

    #[tokio::test]
    async fn corrupt_floor_recovers_and_surfaces_diagnostic_instead_of_freezing_up_to_date() {
        let dir = tempfile::tempdir().unwrap();
        let updates_dir = dir.path().join("updates");
        fs::create_dir_all(&updates_dir).unwrap();
        fs::write(updates_dir.join("floor.json"), "{corrupt").unwrap();

        let (rel_sk, rel_pk) = keypair(0x87);
        let anchors = TrustAnchors::with_keys(rel_pk, None);
        let mut fleet = FakeFleet::new();
        fleet.add_set(
            "tower",
            build_fake_set(
                Channel::Stable,
                "0.2.0",
                202611250210,
                "Legion.apk",
                Os::Android,
                Arch::Universal,
                InstallKind::Apk,
                b"apk-bytes",
                &rel_sk,
            ),
        );

        let mgr = UpdateManager::with_config(
            updates_dir.clone(),
            anchors,
            Os::Android,
            Arch::Aarch64,
            PlatformEnv::Android,
            "0.1.0".to_owned(),
            "old".to_owned(),
            202611250200,
        );
        mgr.on_app_ready();
        let after_ready = mgr.snapshot();
        assert_eq!(after_ready.sequence_floor, 202611250200);
        assert!(after_ready.last_error.is_some());
        assert!(after_ready.last_error_detail.is_some());

        // Subsequent update check succeeds instead of staying frozen at up_to_date.
        let st = mgr
            .check_with_client(&fleet, &["tower".to_owned()], false, false)
            .await
            .unwrap();
        assert_eq!(st.phase, "ready", "{st:?}");
        assert_eq!(st.available.unwrap().sequence, 202611250210);
    }

    #[tokio::test]
    async fn post_install_sequence_mismatch_and_failure_skip_auto_retry_and_move_on_to_newer() {
        let dir = tempfile::tempdir().unwrap();
        let updates_dir = dir.path().join("updates");
        let (rel_sk, rel_pk) = keypair(0x88);
        let anchors = TrustAnchors::with_keys(rel_pk, None);

        let mut fleet = FakeFleet::new();
        fleet.add_set(
            "tower",
            build_fake_set(
                Channel::Stable,
                "0.2.0",
                202611250210,
                "Legion.apk",
                Os::Android,
                Arch::Universal,
                InstallKind::Apk,
                b"bad-seq-apk",
                &rel_sk,
            ),
        );

        let mgr = UpdateManager::with_config(
            updates_dir.clone(),
            anchors,
            Os::Android,
            Arch::Aarch64,
            PlatformEnv::Android,
            "0.1.0".to_owned(),
            "old".to_owned(),
            202611250200,
        );
        let st = mgr
            .check_with_client(&fleet, &["tower".to_owned()], false, false)
            .await
            .unwrap();
        assert_eq!(st.phase, "ready");
        mgr.record_pending_install(&st.available.unwrap()).unwrap();

        // Simulate restart where the binary STILL has sequence 202611250200 instead of 202611250210!
        let mgr_restarted = UpdateManager::with_config(
            updates_dir.clone(),
            anchors,
            Os::Android,
            Arch::Aarch64,
            PlatformEnv::Android,
            "0.1.0".to_owned(),
            "old".to_owned(),
            202611250200,
        );
        mgr_restarted.on_app_ready();
        assert!(mgr_restarted.sequence_failed_before(202611250210));
        let after = mgr_restarted.snapshot();
        assert!(after.just_updated.is_none());
        assert_eq!(after.failed_update.as_ref().unwrap().sequence, 202611250210);

        // Next automatic check surfaces the failed update with "error" (for manual "Try again")
        // rather than auto-installing it in a loop.
        let st_retry = mgr_restarted
            .check_with_client(&fleet, &["tower".to_owned()], false, false)
            .await
            .unwrap();
        assert_eq!(st_retry.phase, "error", "{st_retry:?}");
        assert_eq!(
            st_retry.failed_update.as_ref().unwrap().sequence,
            202611250210
        );

        // Once a strictly newer set (202611250211) appears, Legion moves on automatically!
        fleet.add_set(
            "tower",
            build_fake_set(
                Channel::Stable,
                "0.2.1",
                202611250211,
                "Legion.apk",
                Os::Android,
                Arch::Universal,
                InstallKind::Apk,
                b"good-newer-apk",
                &rel_sk,
            ),
        );
        let st_newer = mgr_restarted
            .check_with_client(&fleet, &["tower".to_owned()], false, false)
            .await
            .unwrap();
        assert_eq!(st_newer.phase, "ready", "{st_newer:?}");
        assert_eq!(st_newer.available.unwrap().sequence, 202611250211);
        assert!(st_newer.failed_update.is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn manual_check_cancels_stalled_background_check_immediately() {
        let dir = tempfile::tempdir().unwrap();
        let (rel_sk, rel_pk) = keypair(0x89);
        let anchors = TrustAnchors::with_keys(rel_pk, None);

        let mut slow_fleet = FakeFleet::new();
        slow_fleet.set_behavior("slow", NodeBehavior::StallListForever);
        let slow_fleet = Arc::new(slow_fleet);

        let mut fast_fleet = FakeFleet::new();
        fast_fleet.add_set(
            "fast",
            build_fake_set(
                Channel::Stable,
                "0.2.0",
                202611250210,
                "Legion.apk",
                Os::Android,
                Arch::Universal,
                InstallKind::Apk,
                b"fast-apk",
                &rel_sk,
            ),
        );

        let mgr = Arc::new(UpdateManager::with_config(
            dir.path().join("updates"),
            anchors,
            Os::Android,
            Arch::Aarch64,
            PlatformEnv::Android,
            "0.1.0".to_owned(),
            "old".to_owned(),
            202611250200,
        ));

        let mgr_bg = Arc::clone(&mgr);
        let slow_clone = Arc::clone(&slow_fleet);
        let bg_handle = tokio::spawn(async move {
            mgr_bg
                .check_with_client(slow_clone.as_ref(), &["slow".to_owned()], false, false)
                .await
        });

        // Wait deterministically until the background check is parked inside `list_sets("slow")`.
        slow_fleet.wait_stalled_list_entered().await;
        assert!(
            !slow_fleet.stalled_list_dropped(),
            "stalled background list_sets must still be active before manual check starts"
        );

        let started = Instant::now();
        let st = mgr
            .check_with_client(&fast_fleet, &["fast".to_owned()], true, false)
            .await
            .unwrap();
        assert_eq!(
            started.elapsed(),
            Duration::ZERO,
            "manual check must cancel stalled background check immediately without waiting on LIST_NODE_TIMEOUT"
        );
        assert_eq!(st.phase, "ready", "{st:?}");

        // Background task exits immediately via generation cancellation rather than waiting on
        // `LIST_NODE_TIMEOUT`, dropping the stalled `list_sets` future.
        let bg_res = bg_handle.await.unwrap();
        assert!(bg_res.is_ok());
        assert!(
            slow_fleet.stalled_list_dropped(),
            "cancelled background check must drop its stalled list_sets future"
        );
        assert_eq!(
            started.elapsed(),
            Duration::ZERO,
            "cancelled background check must finish without advancing virtual time to LIST_NODE_TIMEOUT"
        );
    }

    #[tokio::test]
    async fn refuses_tampered_artifact_tampered_manifest_unknown_key_and_preview_without_key() {
        let dir = tempfile::tempdir().unwrap();
        let (rel_sk, rel_pk) = keypair(0x71);
        let (prev_sk, _prev_pk) = keypair(0x72);
        let (bad_sk, _bad_pk) = keypair(0x73);
        let release_only = TrustAnchors::with_keys(rel_pk, None);
        let nodes = vec!["tower".to_owned()];

        let make_mgr = |sub: &str| {
            UpdateManager::with_config(
                dir.path().join(sub),
                release_only,
                Os::Android,
                Arch::Aarch64,
                PlatformEnv::Android,
                "0.1.0".to_owned(),
                "old".to_owned(),
                202611250200,
            )
        };

        let mut f1 = FakeFleet::new();
        f1.add_set(
            "tower",
            build_fake_set(
                Channel::Preview,
                "0.2.0",
                202611250210,
                "Legion.apk",
                Os::Android,
                Arch::Universal,
                InstallKind::Apk,
                b"apk",
                &prev_sk,
            ),
        );
        let st1 = make_mgr("m1")
            .check_with_client(&f1, &nodes, false, false)
            .await
            .unwrap();
        assert_eq!(st1.phase, "up_to_date");
        assert!(st1.available.is_none());

        let mut f2 = FakeFleet::new();
        f2.add_set(
            "tower",
            build_fake_set(
                Channel::Stable,
                "0.2.0",
                202611250210,
                "Legion.apk",
                Os::Android,
                Arch::Universal,
                InstallKind::Apk,
                b"apk",
                &bad_sk,
            ),
        );
        let st2 = make_mgr("m2")
            .check_with_client(&f2, &nodes, false, false)
            .await
            .unwrap();
        assert_eq!(
            st2.phase, "error",
            "unverifiable candidate must not report up_to_date: {st2:?}"
        );
        assert!(st2.available.is_none());
        assert_eq!(
            st2.last_error.as_deref(),
            Some("Could not check for updates right now.")
        );

        let mut f3 = FakeFleet::new();
        let mut set3 = build_fake_set(
            Channel::Stable,
            "0.2.0",
            202611250210,
            "Legion.apk",
            Os::Android,
            Arch::Universal,
            InstallKind::Apk,
            b"apk",
            &rel_sk,
        );
        set3.files.insert(
            MANIFEST_FILENAME.to_owned(),
            b"{\"tampered\":true}".to_vec(),
        );
        f3.add_set("tower", set3);
        let st3 = make_mgr("m3")
            .check_with_client(&f3, &nodes, false, false)
            .await
            .unwrap();
        assert_eq!(
            st3.phase, "error",
            "tampered manifest must not report up_to_date: {st3:?}"
        );
        assert!(st3.available.is_none());
        assert_eq!(
            st3.last_error.as_deref(),
            Some("Could not check for updates right now.")
        );

        let mut f4 = FakeFleet::new();
        let mut set4 = build_fake_set(
            Channel::Stable,
            "0.2.0",
            202611250210,
            "Legion.apk",
            Os::Android,
            Arch::Universal,
            InstallKind::Apk,
            b"good-apk-bytes",
            &rel_sk,
        );
        set4.files
            .insert("Legion.apk".to_owned(), b"evil-apk-bytes".to_vec());
        f4.add_set("tower", set4);
        let st4 = make_mgr("m4")
            .check_with_client(&f4, &nodes, false, false)
            .await
            .unwrap();
        assert_eq!(st4.phase, "error", "{st4:?}");
        assert!(!st4.available.unwrap().downloaded);
        assert!(
            !download_dir_for(&dir.path().join("m4"), Channel::Stable, 202611250210)
                .join("Legion.apk")
                .exists()
        );
    }

    struct DelayedFleet {
        sets: HashMap<String, Vec<FakeSet>>,
        list_delay: HashMap<String, Duration>,
        /// `(chunk_size, delay_per_chunk)` for metadata files per node.
        meta_throttle: HashMap<String, (usize, Duration)>,
        /// `(chunk_size, delay_per_chunk)` for artifact files per node.
        art_throttle: HashMap<String, (usize, Duration)>,
        /// Optional byte limit after which `node` disconnects on artifact reads.
        disconnect_after_offset: HashMap<String, u64>,
        disconnect_used: Mutex<HashMap<String, bool>>,
        meta_reads_by_node: Mutex<HashMap<String, usize>>,
    }

    impl DelayedFleet {
        fn new() -> Self {
            Self {
                sets: HashMap::new(),
                list_delay: HashMap::new(),
                meta_throttle: HashMap::new(),
                art_throttle: HashMap::new(),
                disconnect_after_offset: HashMap::new(),
                disconnect_used: Mutex::new(HashMap::new()),
                meta_reads_by_node: Mutex::new(HashMap::new()),
            }
        }

        fn add_set(&mut self, node: &str, set: FakeSet) {
            self.sets.entry(node.to_owned()).or_default().push(set);
        }

        fn meta_reads(&self, node: &str) -> usize {
            self.meta_reads_by_node
                .lock()
                .unwrap()
                .get(node)
                .copied()
                .unwrap_or(0)
        }
    }

    impl UpdateSourceClient for DelayedFleet {
        fn list_sets(&self, node: &str) -> BoxFuture<'_, Result<NodeUpdateList, AppError>> {
            let delay = self.list_delay.get(node).copied();
            let listed = self.sets.get(node).map(|sets| NodeUpdateList {
                node: node.to_owned(),
                sets: sets
                    .iter()
                    .map(|s| NodeListedSet {
                        channel: s.channel,
                        sequence: s.sequence,
                        artifacts: s.artifacts.clone(),
                    })
                    .collect(),
            });
            let node_owned = node.to_owned();
            Box::pin(async move {
                if let Some(d) = delay {
                    tokio::time::sleep(d).await;
                }
                listed.ok_or_else(|| {
                    AppError::new(ErrorCode::Unreachable, format!("{node_owned} offline"))
                })
            })
        }

        fn read_chunk(
            &self,
            node: &str,
            channel: Channel,
            sequence: u64,
            name: &str,
            offset: u64,
            length: u64,
        ) -> BoxFuture<'_, Result<UpdateReadChunk, AppError>> {
            let is_meta = matches!(
                name,
                CHECKSUMS_FILENAME | SIGNATURE_FILENAME | MANIFEST_FILENAME
            );
            if is_meta {
                *self
                    .meta_reads_by_node
                    .lock()
                    .unwrap()
                    .entry(node.to_owned())
                    .or_insert(0) += 1;
            } else if let Some(&limit) = self.disconnect_after_offset.get(node)
                && offset >= limit
            {
                let mut used = self.disconnect_used.lock().unwrap();
                if !used.get(node).copied().unwrap_or(false) {
                    used.insert(node.to_owned(), true);
                    return Box::pin(async {
                        Err(AppError::new(
                            ErrorCode::Unreachable,
                            "Simulated mid-stream disconnect",
                        ))
                    });
                }
            }
            let throttle = if is_meta {
                self.meta_throttle.get(node).copied()
            } else {
                self.art_throttle.get(node).copied()
            };
            let bytes = self
                .sets
                .get(node)
                .and_then(|sets| {
                    sets.iter()
                        .find(|s| s.channel == channel && s.sequence == sequence)
                })
                .and_then(|s| s.files.get(name))
                .cloned();
            let name_owned = name.to_owned();
            Box::pin(async move {
                let (cap, delay) = throttle.unwrap_or((512 * 1024, Duration::ZERO));
                if !delay.is_zero() {
                    tokio::time::sleep(delay).await;
                }
                let bytes = bytes.ok_or_else(|| {
                    AppError::new(ErrorCode::NotFound, format!("missing {name_owned}"))
                })?;
                let start = (offset as usize).min(bytes.len());
                let take = (length as usize)
                    .min(cap)
                    .min(bytes.len().saturating_sub(start));
                let slice = &bytes[start..start + take];
                Ok(UpdateReadChunk {
                    size: bytes.len() as u64,
                    offset,
                    length: slice.len() as u64,
                    content: BASE64.encode(slice),
                    eof: start + take >= bytes.len(),
                })
            })
        }
    }

    #[tokio::test(start_paused = true)]
    async fn slow_metadata_from_one_node_does_not_freeze_updates() {
        let dir = tempfile::tempdir().unwrap();
        let (rel_sk, rel_pk) = keypair(0x91);
        let (evil_sk, _) = keypair(0x92);
        let anchors = TrustAnchors::with_keys(rel_pk, None);

        let mut fleet = DelayedFleet::new();
        // Hostile node advertises higher bogus sequence and stalls 25s per 1-byte metadata chunk.
        fleet.add_set(
            "evil",
            build_fake_set(
                Channel::Stable,
                "9.9.9",
                202611250300,
                "Legion.apk",
                Os::Android,
                Arch::Universal,
                InstallKind::Apk,
                b"evil",
                &evil_sk,
            ),
        );
        fleet
            .meta_throttle
            .insert("evil".to_owned(), (1, Duration::from_secs(25)));
        // Honest node holds genuine update at lower sequence.
        fleet.add_set(
            "honest",
            build_fake_set(
                Channel::Stable,
                "0.2.0",
                202611250210,
                "Legion.apk",
                Os::Android,
                Arch::Universal,
                InstallKind::Apk,
                b"real-apk-payload",
                &rel_sk,
            ),
        );

        let mgr = UpdateManager::with_config(
            dir.path().join("updates"),
            anchors,
            Os::Android,
            Arch::Aarch64,
            PlatformEnv::Android,
            "0.1.0".to_owned(),
            "old".to_owned(),
            202611250200,
        );

        let t0 = Instant::now();
        let st = mgr
            .check_with_client(
                &fleet,
                &["evil".to_owned(), "honest".to_owned()],
                false,
                false,
            )
            .await
            .unwrap();
        let elapsed = t0.elapsed();
        assert_eq!(st.phase, "ready", "{st:?}");
        assert_eq!(st.available.as_ref().unwrap().sequence, 202611250210);
        assert_eq!(st.available.as_ref().unwrap().source_node, "honest");
        assert!(
            elapsed <= METADATA_CANDIDATE_TIMEOUT,
            "slow metadata node must be cut off within candidate budget, took {elapsed:?}"
        );
        assert!(mgr.is_node_deprioritized("evil"));
    }

    #[tokio::test(start_paused = true)]
    async fn slow_metadata_on_genuine_set_raced_concurrently_wins_immediately() {
        let dir = tempfile::tempdir().unwrap();
        let (rel_sk, rel_pk) = keypair(0x93);
        let anchors = TrustAnchors::with_keys(rel_pk, None);

        let genuine = build_fake_set(
            Channel::Stable,
            "0.2.0",
            202611250210,
            "Legion.apk",
            Os::Android,
            Arch::Universal,
            InstallKind::Apk,
            b"genuine-apk-payload",
            &rel_sk,
        );
        let mut fleet = DelayedFleet::new();
        fleet.add_set("evil", genuine.clone());
        fleet.add_set("honest", genuine);
        fleet
            .meta_throttle
            .insert("evil".to_owned(), (1, Duration::from_secs(25)));

        let mgr = UpdateManager::with_config(
            dir.path().join("updates"),
            anchors,
            Os::Android,
            Arch::Aarch64,
            PlatformEnv::Android,
            "0.1.0".to_owned(),
            "old".to_owned(),
            202611250200,
        );

        let t0 = Instant::now();
        let st = mgr
            .check_with_client(
                &fleet,
                &["evil".to_owned(), "honest".to_owned()],
                false,
                false,
            )
            .await
            .unwrap();
        let elapsed = t0.elapsed();
        assert_eq!(st.phase, "ready", "{st:?}");
        assert_eq!(st.available.as_ref().unwrap().source_node, "honest");
        assert!(
            elapsed < Duration::from_secs(1),
            "concurrent metadata probe should finish immediately via honest node, took {elapsed:?}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn slow_metadata_rate_floor_aborts_trickle_and_never_reports_up_to_date() {
        let dir = tempfile::tempdir().unwrap();
        let (rel_sk, rel_pk) = keypair(0x94);
        let anchors = TrustAnchors::with_keys(rel_pk, None);

        let mut fleet = DelayedFleet::new();
        fleet.add_set(
            "trickler",
            build_fake_set(
                Channel::Stable,
                "0.2.0",
                202611250210,
                "Legion.apk",
                Os::Android,
                Arch::Universal,
                InstallKind::Apk,
                b"payload",
                &rel_sk,
            ),
        );
        // 1 byte per second is faster than METADATA_CALL_TIMEOUT (8s) so only the metadata rate
        // floor catches it promptly.
        fleet
            .meta_throttle
            .insert("trickler".to_owned(), (1, Duration::from_secs(1)));

        let mgr = UpdateManager::with_config(
            dir.path().join("updates"),
            anchors,
            Os::Android,
            Arch::Aarch64,
            PlatformEnv::Android,
            "0.1.0".to_owned(),
            "old".to_owned(),
            202611250200,
        );

        let t0 = Instant::now();
        let st = mgr
            .check_with_client(&fleet, &["trickler".to_owned()], false, false)
            .await
            .unwrap();
        let elapsed = t0.elapsed();
        assert_eq!(
            st.phase, "error",
            "timed-out check must never report up_to_date: {st:?}"
        );
        assert_eq!(
            st.last_error.as_deref(),
            Some("Could not check for updates right now.")
        );
        assert!(
            elapsed <= Duration::from_secs(5),
            "metadata rate floor should abort 1 B/s trickle quickly, took {elapsed:?}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn bad_node_is_deprioritized_across_checks_and_forgotten_after_ttl() {
        let dir = tempfile::tempdir().unwrap();
        let (rel_sk, rel_pk) = keypair(0x95);
        let anchors = TrustAnchors::with_keys(rel_pk, None);

        let set1 = build_fake_set(
            Channel::Stable,
            "0.2.0",
            202611250210,
            "Legion.apk",
            Os::Android,
            Arch::Universal,
            InstallKind::Apk,
            b"payload-1",
            &rel_sk,
        );
        let mut fleet = DelayedFleet::new();
        fleet.add_set("slow", set1.clone());
        fleet.add_set("fast", set1);
        fleet
            .list_delay
            .insert("slow".to_owned(), Duration::from_secs(60));

        let mgr = UpdateManager::with_config(
            dir.path().join("updates"),
            anchors,
            Os::Android,
            Arch::Aarch64,
            PlatformEnv::Android,
            "0.1.0".to_owned(),
            "old".to_owned(),
            202611250200,
        );

        // Check 1: slow node times out on list_sets (concurrently with fast node), fast succeeds.
        let t0 = Instant::now();
        let st1 = mgr
            .check_with_client(
                &fleet,
                &["slow".to_owned(), "fast".to_owned()],
                false,
                false,
            )
            .await
            .unwrap();
        assert_eq!(st1.phase, "ready", "{st1:?}");
        assert!(
            t0.elapsed() <= LIST_NODE_TIMEOUT + Duration::from_secs(5),
            "concurrent check_with_client took too long: {:?}",
            t0.elapsed()
        );
        assert!(mgr.is_node_deprioritized("slow"));

        let ordered = mgr.order_nodes_for(
            Channel::Stable,
            202611250211,
            &["slow".to_owned(), "fast".to_owned()],
        );
        assert_eq!(ordered, vec!["fast".to_owned(), "slow".to_owned()]);

        // Check 2 (within TTL): slow recovers on listing, but clean wave tries fast FIRST without
        // touching slow's metadata!
        fleet.list_delay.remove("slow");
        let set2 = build_fake_set(
            Channel::Stable,
            "0.2.1",
            202611250211,
            "Legion.apk",
            Os::Android,
            Arch::Universal,
            InstallKind::Apk,
            b"payload-2",
            &rel_sk,
        );
        fleet.add_set("slow", set2.clone());
        fleet.add_set("fast", set2);
        let slow_reads_before = fleet.meta_reads("slow");
        let st2 = mgr
            .check_with_client(
                &fleet,
                &["slow".to_owned(), "fast".to_owned()],
                false,
                false,
            )
            .await
            .unwrap();
        assert_eq!(st2.phase, "ready", "{st2:?}");
        assert_eq!(st2.available.as_ref().unwrap().source_node, "fast");
        assert_eq!(
            fleet.meta_reads("slow"),
            slow_reads_before,
            "deprioritized node must not be queried ahead of a clean node"
        );

        // Advance past BAD_NODE_TTL: slow node is forgotten and treated normally again.
        tokio::time::advance(BAD_NODE_TTL + Duration::from_secs(1)).await;
        assert!(!mgr.is_node_deprioritized("slow"));
        let ordered_after_ttl = mgr.order_nodes_for(
            Channel::Stable,
            202611250212,
            &["slow".to_owned(), "fast".to_owned()],
        );
        assert_eq!(
            ordered_after_ttl,
            vec!["slow".to_owned(), "fast".to_owned()]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn slow_link_6mib_at_24kib_s_completes_with_one_and_two_nodes_and_resumes_across_checks()
    {
        let (rel_sk, rel_pk) = keypair(0x96);
        let anchors = TrustAnchors::with_keys(rel_pk, None);

        // Exact reviewer scenario: 6 MiB payload at 24 KiB/s (~256s > old 180s fixed deadline).
        let payload_6m: Vec<u8> = (0..(6 * 1024 * 1024))
            .map(|i| ((i * 31) & 0xFF) as u8)
            .collect();
        let set_6m = build_fake_set(
            Channel::Stable,
            "0.2.0",
            202611250210,
            "Legion.apk",
            Os::Android,
            Arch::Universal,
            InstallKind::Apk,
            &payload_6m,
            &rel_sk,
        );

        // Case 1: single slow node at 24 KiB/s (24 KiB per 1s) completes in one pass.
        {
            let dir = tempfile::tempdir().unwrap();
            let mut fleet = DelayedFleet::new();
            fleet.add_set("a", set_6m.clone());
            fleet
                .art_throttle
                .insert("a".to_owned(), (24 * 1024, Duration::from_secs(1)));

            let mgr = UpdateManager::with_config(
                dir.path().join("updates"),
                anchors,
                Os::Android,
                Arch::Aarch64,
                PlatformEnv::Android,
                "0.1.0".to_owned(),
                "old".to_owned(),
                202611250200,
            );
            let st = mgr
                .check_with_client(&fleet, &["a".to_owned()], false, false)
                .await
                .unwrap();
            assert_eq!(st.phase, "ready", "1-node 6 MiB @ 24 KiB/s failed: {st:?}");
            assert!(st.available.unwrap().downloaded);
        }

        // Case 2: two slow nodes at 24 KiB/s where `a` disconnects at 4 MiB; `b` resumes from
        // 4 MiB (preserving `.part` progress!) and finishes the remaining 2 MiB cleanly.
        {
            let dir = tempfile::tempdir().unwrap();
            let mut fleet = DelayedFleet::new();
            fleet.add_set("a", set_6m.clone());
            fleet.add_set("b", set_6m);
            fleet
                .art_throttle
                .insert("a".to_owned(), (24 * 1024, Duration::from_secs(1)));
            fleet
                .art_throttle
                .insert("b".to_owned(), (24 * 1024, Duration::from_secs(1)));
            fleet
                .disconnect_after_offset
                .insert("a".to_owned(), 4 * 1024 * 1024);

            let mgr = UpdateManager::with_config(
                dir.path().join("updates"),
                anchors,
                Os::Android,
                Arch::Aarch64,
                PlatformEnv::Android,
                "0.1.0".to_owned(),
                "old".to_owned(),
                202611250200,
            );
            let t0 = Instant::now();
            let st = mgr
                .check_with_client(&fleet, &["a".to_owned(), "b".to_owned()], false, false)
                .await
                .unwrap();
            let elapsed = t0.elapsed();
            assert_eq!(st.phase, "ready", "2-node 6 MiB @ 24 KiB/s failed: {st:?}");
            assert!(st.available.unwrap().downloaded);
            // Total transfer time stays ~256s (proving `b` resumed from 4 MiB instead of restarting!).
            assert!(
                elapsed < Duration::from_secs(300),
                "expected b to resume from 4 MiB without re-downloading, took {elapsed:?}"
            );
        }

        // Case 3: 11 MiB APK at 24 KiB/s interrupted mid-way completes across checks without losing `.part`.
        {
            let dir = tempfile::tempdir().unwrap();
            let updates_dir = dir.path().join("updates");
            let payload_11m = vec![0x3Cu8; 11 * 1024 * 1024];
            let set_11m = build_fake_set(
                Channel::Stable,
                "0.2.1",
                202611250211,
                "Legion.apk",
                Os::Android,
                Arch::Universal,
                InstallKind::Apk,
                &payload_11m,
                &rel_sk,
            );
            let mut fleet = DelayedFleet::new();
            fleet.add_set("a", set_11m);
            fleet
                .art_throttle
                .insert("a".to_owned(), (24 * 1024, Duration::from_secs(1)));
            fleet
                .disconnect_after_offset
                .insert("a".to_owned(), 5 * 1024 * 1024);

            let mgr = UpdateManager::with_config(
                updates_dir.clone(),
                anchors,
                Os::Android,
                Arch::Aarch64,
                PlatformEnv::Android,
                "0.1.0".to_owned(),
                "old".to_owned(),
                202611250200,
            );

            // Check 1 disconnects after 5 MiB -> `.part` and `.part.meta.json` stay intact!
            let st1 = mgr
                .check_with_client(&fleet, &["a".to_owned()], false, false)
                .await
                .unwrap();
            assert_eq!(st1.phase, "error", "{st1:?}");
            let set_dir = download_dir_for(&updates_dir, Channel::Stable, 202611250211);
            let part_len = fs::metadata(set_dir.join("Legion.apk.part")).unwrap().len();
            assert!(
                part_len >= 5 * 1024 * 1024,
                "expected >= 5 MiB preserved, got {part_len}"
            );
            let meta: PartMetadata = serde_json::from_slice(
                &fs::read(set_dir.join("Legion.apk.part.meta.json")).unwrap(),
            )
            .unwrap();
            assert_eq!(meta.ranges.len(), 1);
            assert_eq!(meta.ranges[0].node, "a");
            assert_eq!(meta.ranges[0].end, part_len);

            // Check 2 resumes from >= 5 MiB and finishes the remaining <= 6 MiB cleanly!
            let t_resume = Instant::now();
            let st2 = mgr
                .check_with_client(&fleet, &["a".to_owned()], false, false)
                .await
                .unwrap();
            assert_eq!(st2.phase, "ready", "{st2:?}");
            assert!(st2.available.unwrap().downloaded);
            assert!(
                t_resume.elapsed() < Duration::from_secs(290),
                "check 2 should only transfer remaining ~6 MiB, took {:?}",
                t_resume.elapsed()
            );
        }
    }

    #[tokio::test(start_paused = true)]
    async fn poisoned_partial_from_first_node_retries_second_node_clean_and_blames_only_poisoner() {
        let dir = tempfile::tempdir().unwrap();
        let (rel_sk, rel_pk) = keypair(0x97);
        let anchors = TrustAnchors::with_keys(rel_pk, None);

        let payload = vec![0x5Au8; 64 * 1024];
        let good_set = build_fake_set(
            Channel::Stable,
            "0.2.0",
            202611250210,
            "Legion.apk",
            Os::Android,
            Arch::Universal,
            InstallKind::Apk,
            &payload,
            &rel_sk,
        );
        let mut poisoned_set = good_set.clone();
        let mut poisoned_bytes = payload.clone();
        poisoned_bytes[0] ^= 0xFF;
        poisoned_set
            .files
            .insert("Legion.apk".to_owned(), poisoned_bytes);

        let mut fleet = DelayedFleet::new();
        fleet.add_set("poisoner", poisoned_set);
        fleet.add_set("honest", good_set);
        fleet
            .art_throttle
            .insert("poisoner".to_owned(), (16 * 1024, Duration::ZERO));
        fleet
            .art_throttle
            .insert("honest".to_owned(), (16 * 1024, Duration::ZERO));
        // Poisoner writes 32 KiB of corrupted bytes and then disconnects!
        fleet
            .disconnect_after_offset
            .insert("poisoner".to_owned(), 32 * 1024);

        let mgr = UpdateManager::with_config(
            dir.path().join("updates"),
            anchors,
            Os::Android,
            Arch::Aarch64,
            PlatformEnv::Android,
            "0.1.0".to_owned(),
            "old".to_owned(),
            202611250200,
        );

        let nodes = vec!["poisoner".to_owned(), "honest".to_owned()];
        let st = mgr
            .check_with_client(&fleet, &nodes, false, false)
            .await
            .unwrap();
        assert_eq!(st.phase, "ready", "{st:?}");
        assert_eq!(st.available.as_ref().unwrap().source_node, "honest");
        assert!(st.available.as_ref().unwrap().downloaded);
        // Honest node was exonerated after clean retry; poisoner remains deprioritized!
        assert!(mgr.is_node_deprioritized("poisoner"));
        assert!(!mgr.is_node_deprioritized("honest"));
        assert_eq!(
            mgr.order_nodes_for(Channel::Stable, 202611250210, &nodes),
            vec!["honest".to_owned(), "poisoner".to_owned()]
        );
    }

    #[tokio::test]
    async fn fair_candidate_merging_prevents_two_flooding_nodes_crowding_out_honest_third_node() {
        let dir = tempfile::tempdir().unwrap();
        let (rel_sk, rel_pk) = keypair(0x98);
        let (bad_sk, _) = keypair(0x99);
        let anchors = TrustAnchors::with_keys(rel_pk, None);

        let mut fleet = FakeFleet::new();
        // Two flooding nodes each advertise 16 distinct bogus higher sequences (32 total == cap).
        for (idx, flooder) in ["flood1", "flood2"].iter().enumerate() {
            for i in 0..20u64 {
                let seq = 202611260000 + (idx as u64) * 100 + i;
                fleet.add_set(
                    flooder,
                    build_fake_set(
                        Channel::Stable,
                        "9.9.9",
                        seq,
                        "Legion.apk",
                        Os::Android,
                        Arch::Universal,
                        InstallKind::Apk,
                        b"evil",
                        &bad_sk,
                    ),
                );
            }
        }
        // Third node is honest and listed LAST with the real signed update at a lower sequence.
        fleet.add_set(
            "honest",
            build_fake_set(
                Channel::Stable,
                "0.2.0",
                202611250210,
                "Legion.apk",
                Os::Android,
                Arch::Universal,
                InstallKind::Apk,
                b"real-apk-bytes",
                &rel_sk,
            ),
        );

        let mgr = UpdateManager::with_config(
            dir.path().join("updates"),
            anchors,
            Os::Android,
            Arch::Aarch64,
            PlatformEnv::Android,
            "0.1.0".to_owned(),
            "old".to_owned(),
            202611250200,
        );
        let st = mgr
            .check_with_client(
                &fleet,
                &[
                    "flood1".to_owned(),
                    "flood2".to_owned(),
                    "honest".to_owned(),
                ],
                false,
                false,
            )
            .await
            .unwrap();
        assert_eq!(st.phase, "ready", "{st:?}");
        assert_eq!(st.available.as_ref().unwrap().sequence, 202611250210);
        assert_eq!(st.available.as_ref().unwrap().source_node, "honest");
    }

    #[tokio::test(start_paused = true)]
    async fn auto_install_gate_enforces_startup_window_first_screen_in_flight_ops_and_rate_limit() {
        let dir = tempfile::tempdir().unwrap();
        let (rel_sk, rel_pk) = keypair(0x9A);
        let anchors = TrustAnchors::with_keys(rel_pk, None);

        let mut fleet = FakeFleet::new();
        fleet.add_set(
            "tower",
            build_fake_set(
                Channel::Stable,
                "0.2.0",
                202611250210,
                "Legion.AppImage",
                Os::Linux,
                Arch::X86_64,
                InstallKind::AppImage,
                b"ready-appimage",
                &rel_sk,
            ),
        );

        // Desktop manager with ready update right at launch.
        let mgr = UpdateManager::with_config(
            dir.path().join("desktop"),
            anchors,
            Os::Linux,
            Arch::X86_64,
            PlatformEnv::Desktop(DesktopInstall::LinuxAppImage {
                appimage_path: dir.path().join("Legion.AppImage"),
            }),
            "0.1.0".to_owned(),
            "old".to_owned(),
            202611250200,
        );
        let st = mgr
            .check_with_client(&fleet, &["tower".to_owned()], false, false)
            .await
            .unwrap();
        assert_eq!(st.phase, "ready");

        // 1. Within startup window, before first screen, zero in-flight ops -> startup allowed.
        assert_eq!(mgr.evaluate_auto_install_gate("startup", 0, true), Ok(()));

        // 2. Immediate repeat request from page is rate-limited natively.
        assert_eq!(
            mgr.evaluate_auto_install_gate("startup", 0, true),
            Err("rate_limited")
        );

        // 3. After rate-limit window, in-flight native work still blocks startup and close.
        tokio::time::advance(AUTO_INSTALL_RATE_LIMIT + Duration::from_millis(10)).await;
        assert_eq!(
            mgr.evaluate_auto_install_gate("startup", 1, true),
            Err("native_activity_in_flight")
        );
        assert_eq!(
            mgr.evaluate_auto_install_gate("close", 1, false),
            Err("native_activity_in_flight")
        );
        assert_eq!(mgr.evaluate_auto_install_gate("close", 0, false), Ok(()));
        // Page can never spoof desktop close or background or unknown triggers.
        tokio::time::advance(AUTO_INSTALL_RATE_LIMIT + Duration::from_millis(10)).await;
        assert_eq!(
            mgr.evaluate_auto_install_gate("close", 0, true),
            Err("close_native_only")
        );
        tokio::time::advance(AUTO_INSTALL_RATE_LIMIT + Duration::from_millis(10)).await;
        assert_eq!(
            mgr.evaluate_auto_install_gate("background", 0, true),
            Err("background_android_only")
        );
        tokio::time::advance(AUTO_INSTALL_RATE_LIMIT + Duration::from_millis(10)).await;
        assert_eq!(
            mgr.evaluate_auto_install_gate("idle", 0, true),
            Err("unsupported_trigger")
        );

        // 4. Once startup window elapses, startup is refused even if page never called `ready()`.
        tokio::time::advance(STARTUP_AUTO_INSTALL_WINDOW + Duration::from_secs(1)).await;
        assert_eq!(
            mgr.evaluate_auto_install_gate("startup", 0, true),
            Err("startup_window_elapsed")
        );

        // 5. Fresh manager: as soon as first full screen paints (`on_app_ready`), startup is closed
        //    permanently even 1ms after launch!
        let mgr2 = UpdateManager::with_config(
            dir.path().join("desktop"),
            anchors,
            Os::Linux,
            Arch::X86_64,
            PlatformEnv::Desktop(DesktopInstall::LinuxAppImage {
                appimage_path: dir.path().join("Legion.AppImage"),
            }),
            "0.1.0".to_owned(),
            "old".to_owned(),
            202611250200,
        );
        assert_eq!(mgr2.snapshot().phase, "ready");
        mgr2.on_app_ready();
        assert_eq!(
            mgr2.evaluate_auto_install_gate("startup", 0, true),
            Err("first_screen_already_shown")
        );

        // 6. Android manager allows `"background"` when idle, refuses when native work is in flight.
        let mut android_fleet = FakeFleet::new();
        android_fleet.add_set(
            "tower",
            build_fake_set(
                Channel::Stable,
                "0.2.0",
                202611250210,
                "Legion.apk",
                Os::Android,
                Arch::Universal,
                InstallKind::Apk,
                b"ready-apk",
                &rel_sk,
            ),
        );
        let android_mgr = UpdateManager::with_config(
            dir.path().join("android"),
            anchors,
            Os::Android,
            Arch::Aarch64,
            PlatformEnv::Android,
            "0.1.0".to_owned(),
            "old".to_owned(),
            202611250200,
        );
        android_mgr
            .check_with_client(&android_fleet, &["tower".to_owned()], false, false)
            .await
            .unwrap();
        android_mgr.on_app_ready();
        assert_eq!(
            android_mgr.evaluate_auto_install_gate("startup", 0, true),
            Err("first_screen_already_shown")
        );
        tokio::time::advance(AUTO_INSTALL_RATE_LIMIT + Duration::from_millis(10)).await;
        assert_eq!(
            android_mgr.evaluate_auto_install_gate("background", 1, true),
            Err("native_activity_in_flight")
        );
        tokio::time::advance(AUTO_INSTALL_RATE_LIMIT + Duration::from_millis(10)).await;
        let scan_guard = android_mgr.track_external_operation();
        assert_eq!(
            android_mgr.evaluate_auto_install_gate("background", 0, true),
            Err("native_activity_in_flight")
        );
        drop(scan_guard);
        tokio::time::advance(AUTO_INSTALL_RATE_LIMIT + Duration::from_millis(10)).await;
        assert_eq!(
            android_mgr.evaluate_auto_install_gate("background", 0, true),
            Ok(())
        );
    }

    #[test]
    fn rfc3339_utc_round_trip() {
        for ts in [
            0i64,
            1_700_000_000,
            1_795_593_600,
            1_893_456_000,
            2_000_000_000,
        ] {
            let formatted = format_rfc3339_utc(ts);
            assert_eq!(
                parse_rfc3339_utc(&formatted),
                Some(ts),
                "round-trip failed for {ts} -> {formatted}"
            );
        }
        assert!(parse_rfc3339_utc("not-a-timestamp").is_none());
    }

    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    #[tokio::test]
    async fn rollback_retries_once_after_one_hour_and_succeeds() {
        let dir = tempfile::tempdir().unwrap();
        let updates_dir = dir.path().join("updates");
        let appimage = dir.path().join("Legion.AppImage");
        let (rel_sk, rel_pk) = keypair(0x91);
        let anchors = TrustAnchors::with_keys(rel_pk, None);
        let t0: i64 = 1_795_593_600; // 2026-11-25T08:00:00Z

        let mut fleet = FakeFleet::new();
        fleet.add_set(
            "tower",
            build_fake_set(
                Channel::Stable,
                "0.2.0",
                202611250210,
                "Legion-linux-x86_64.AppImage",
                Os::Linux,
                Arch::X86_64,
                InstallKind::AppImage,
                b"v2-appimage",
                &rel_sk,
            ),
        );

        let platform = PlatformEnv::Desktop(DesktopInstall::LinuxAppImage {
            appimage_path: appimage.clone(),
        });

        // 1. Download v2 on seq1 and begin install.
        let mgr1 = UpdateManager::with_config_at(
            updates_dir.clone(),
            anchors,
            Os::Linux,
            Arch::X86_64,
            platform.clone(),
            "0.1.0".to_owned(),
            "old".to_owned(),
            202611250200,
            t0,
        );
        let st1 = mgr1
            .check_with_client_at(&fleet, &["tower".to_owned()], false, false, t0)
            .await
            .unwrap();
        assert_eq!(st1.phase, "ready");
        let avail_v2 = st1.available.unwrap();
        mgr1.record_started_attempt_and_pending_at(&avail_v2, "staged", t0)
            .unwrap();

        // 2. Simulate first rollback with captured startup log + supervisor log.
        fs::write(
            updates_dir.join("supervisor.log"),
            "supervisor: new_pid=9001 exited with rc=1 after 2 ticks\nsupervisor: ready marker missing after 34 ticks; rolling back\n",
        )
        .unwrap();
        fs::write(
            updates_dir.join(desktop::NEW_START_LOG_FILENAME),
            "transient display error\n",
        )
        .unwrap();
        fs::write(
            updates_dir.join("rollback-202611250210.json"),
            serde_json::to_vec(&RollbackInfo {
                version: "0.2.0".to_owned(),
                sequence: 202611250210,
                reason: ROLLBACK_REASON.to_owned(),
                detail: None,
                install_state: Some("restored_previous".to_owned()),
            })
            .unwrap(),
        )
        .unwrap();

        // Relaunched seq1 right after rollback (at t0 + 10s): records attempt 1 (not skipped),
        // surfaces captured detail under Technical details, and blocks immediate auto-retry.
        let mgr_after_rb1 = UpdateManager::with_config_at(
            updates_dir.clone(),
            anchors,
            Os::Linux,
            Arch::X86_64,
            platform.clone(),
            "0.1.0".to_owned(),
            "old".to_owned(),
            202611250200,
            t0 + 10,
        );
        mgr_after_rb1.on_app_ready_at(t0 + 10);
        let st_rb1 = mgr_after_rb1.snapshot();
        assert_eq!(st_rb1.phase, "error");
        let fail1 = st_rb1.failed_update.as_ref().unwrap();
        assert_eq!(fail1.attempts, 1);
        assert!(!fail1.skipped);
        assert!(
            st_rb1
                .last_error_detail
                .as_deref()
                .is_some_and(|d| d.contains("transient display error")),
            "expected captured startup error under Technical details: {:?}",
            st_rb1.last_error_detail
        );

        // 30 minutes later (< 1 hour): next app start MUST still respect the 1-hour cooldown.
        let mgr_too_soon = UpdateManager::with_config_at(
            updates_dir.clone(),
            anchors,
            Os::Linux,
            Arch::X86_64,
            platform.clone(),
            "0.1.0".to_owned(),
            "old".to_owned(),
            202611250200,
            t0 + 10 + 1800,
        );
        assert_eq!(mgr_too_soon.snapshot().phase, "error");
        assert!(
            mgr_too_soon
                .evaluate_auto_install_gate_at("startup", 0, true, t0 + 10 + 1800)
                .is_err()
        );

        // 3. Next app start >= 1 hour later: same sequence is eligible for its single automatic
        //    retry (phase == "ready", auto-install gate allows startup install).
        let retry_time = t0 + 10 + ROLLBACK_RETRY_COOLDOWN_SECS;
        let mgr_retry = UpdateManager::with_config_at(
            updates_dir.clone(),
            anchors,
            Os::Linux,
            Arch::X86_64,
            platform.clone(),
            "0.1.0".to_owned(),
            "old".to_owned(),
            202611250200,
            retry_time,
        );
        let st_retry = mgr_retry.snapshot();
        assert_eq!(st_retry.phase, "ready", "{st_retry:?}");
        assert!(st_retry.failed_update.is_none());
        assert_eq!(
            mgr_retry.evaluate_auto_install_gate_at("startup", 0, true, retry_time),
            Ok(())
        );
        mgr_retry
            .record_started_attempt_and_pending_at(&avail_v2, "staged", retry_time)
            .unwrap();

        // 4. Retry succeeds: new build boots at seq 202611250210 -> just_updated recorded,
        //    failure cleared, and new-start.log removed.
        let mgr_v2 = UpdateManager::with_config_at(
            updates_dir.clone(),
            anchors,
            Os::Linux,
            Arch::X86_64,
            platform,
            "0.2.0".to_owned(),
            "new".to_owned(),
            202611250210,
            retry_time + 5,
        );
        mgr_v2.on_app_ready_at(retry_time + 5);
        let st_v2 = mgr_v2.snapshot();
        assert_eq!(st_v2.phase, "up_to_date");
        assert_eq!(st_v2.just_updated.as_ref().unwrap().sequence, 202611250210);
        assert!(st_v2.rolled_back.is_none());
        assert!(st_v2.failed_update.is_none());
        assert!(!updates_dir.join(desktop::NEW_START_LOG_FILENAME).exists());
    }

    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    #[tokio::test]
    async fn two_rollbacks_skip_sequence_try_again_clears_and_newer_sequence_unaffected() {
        let dir = tempfile::tempdir().unwrap();
        let updates_dir = dir.path().join("updates");
        let appimage = dir.path().join("Legion.AppImage");
        let (rel_sk, rel_pk) = keypair(0x92);
        let anchors = TrustAnchors::with_keys(rel_pk, None);
        let t0: i64 = 1_795_593_600;

        let mut fleet = FakeFleet::new();
        fleet.add_set(
            "tower",
            build_fake_set(
                Channel::Stable,
                "0.2.0",
                202611250210,
                "Legion-linux-x86_64.AppImage",
                Os::Linux,
                Arch::X86_64,
                InstallKind::AppImage,
                b"broken-v2-appimage",
                &rel_sk,
            ),
        );

        let platform = PlatformEnv::Desktop(DesktopInstall::LinuxAppImage {
            appimage_path: appimage.clone(),
        });

        let mgr = UpdateManager::with_config_at(
            updates_dir.clone(),
            anchors,
            Os::Linux,
            Arch::X86_64,
            platform.clone(),
            "0.1.0".to_owned(),
            "old".to_owned(),
            202611250200,
            t0,
        );
        let st = mgr
            .check_with_client_at(&fleet, &["tower".to_owned()], false, false, t0)
            .await
            .unwrap();
        let avail_v2 = st.available.unwrap();

        // Rollback #1 at t0 + 10.
        mgr.record_started_attempt_and_pending_at(&avail_v2, "staged", t0)
            .unwrap();
        fs::write(
            updates_dir.join("rollback-202611250210.json"),
            serde_json::to_vec(&RollbackInfo {
                version: "0.2.0".to_owned(),
                sequence: 202611250210,
                reason: ROLLBACK_REASON.to_owned(),
                detail: None,
                install_state: Some("restored_previous".to_owned()),
            })
            .unwrap(),
        )
        .unwrap();
        let mgr_rb1 = UpdateManager::with_config_at(
            updates_dir.clone(),
            anchors,
            Os::Linux,
            Arch::X86_64,
            platform.clone(),
            "0.1.0".to_owned(),
            "old".to_owned(),
            202611250200,
            t0 + 10,
        );
        mgr_rb1.on_app_ready_at(t0 + 10);
        assert!(!mgr_rb1.snapshot().failed_update.as_ref().unwrap().skipped);

        // Rollback #2 at t0 + 10 + 1 hour (automatic retry fails again).
        let t_retry = t0 + 10 + ROLLBACK_RETRY_COOLDOWN_SECS;
        let mgr_retry = UpdateManager::with_config_at(
            updates_dir.clone(),
            anchors,
            Os::Linux,
            Arch::X86_64,
            platform.clone(),
            "0.1.0".to_owned(),
            "old".to_owned(),
            202611250200,
            t_retry,
        );
        assert_eq!(mgr_retry.snapshot().phase, "ready");
        mgr_retry
            .record_started_attempt_and_pending_at(&avail_v2, "staged", t_retry)
            .unwrap();
        fs::write(
            updates_dir.join(desktop::NEW_START_LOG_FILENAME),
            "fatal: missing libwebkit2gtk-4.1.so.0\n",
        )
        .unwrap();
        fs::write(
            updates_dir.join("rollback-202611250210.json"),
            serde_json::to_vec(&RollbackInfo {
                version: "0.2.0".to_owned(),
                sequence: 202611250210,
                reason: ROLLBACK_REASON.to_owned(),
                detail: None,
                install_state: Some("restored_previous".to_owned()),
            })
            .unwrap(),
        )
        .unwrap();

        let mgr_rb2 = UpdateManager::with_config_at(
            updates_dir.clone(),
            anchors,
            Os::Linux,
            Arch::X86_64,
            platform.clone(),
            "0.1.0".to_owned(),
            "old".to_owned(),
            202611250200,
            t_retry + 10,
        );
        mgr_rb2.on_app_ready_at(t_retry + 10);
        let st_rb2 = mgr_rb2.snapshot();
        assert_eq!(st_rb2.phase, "error");
        assert_eq!(st_rb2.last_error.as_deref(), Some(ROLLBACK_REASON));
        let fail2 = st_rb2.failed_update.as_ref().unwrap();
        assert_eq!(fail2.attempts, 2);
        assert!(fail2.skipped, "second rollback must mark sequence skipped");
        assert!(fail2.retry_after.is_none());
        assert!(
            st_rb2
                .last_error_detail
                .as_deref()
                .is_some_and(|d| d.contains("libwebkit2gtk-4.1.so.0")),
            "second rollback must surface captured startup error: {:?}",
            st_rb2.last_error_detail
        );

        // Even 24 hours later, seq 202611250210 remains skipped on launch AND on check!
        let t_next_day = t_retry + 24 * 3600;
        let mgr_next_day = UpdateManager::with_config_at(
            updates_dir.clone(),
            anchors,
            Os::Linux,
            Arch::X86_64,
            platform.clone(),
            "0.1.0".to_owned(),
            "old".to_owned(),
            202611250200,
            t_next_day,
        );
        assert_eq!(mgr_next_day.snapshot().phase, "error");
        assert_eq!(
            mgr_next_day.auto_install_block_reason_at(202611250210, t_next_day),
            Some("sequence_skipped")
        );
        let st_check_skipped = mgr_next_day
            .check_with_client_at(&fleet, &["tower".to_owned()], false, false, t_next_day)
            .await
            .unwrap();
        assert_eq!(st_check_skipped.phase, "error");
        assert_eq!(
            st_check_skipped.last_error.as_deref(),
            Some(ROLLBACK_REASON)
        );
        assert!(st_check_skipped.failed_update.as_ref().unwrap().skipped);
        assert!(
            st_check_skipped
                .last_error_detail
                .as_deref()
                .is_some_and(|d| d.contains("libwebkit2gtk-4.1.so.0"))
        );

        // "Try again" clears the skip for that sequence and restores phase == "ready".
        mgr_next_day.clear_failed_sequence(202611250210);
        let st_cleared = mgr_next_day.snapshot();
        assert_eq!(st_cleared.phase, "ready", "{st_cleared:?}");
        assert!(st_cleared.failed_update.is_none());
        assert!(st_cleared.rolled_back.is_none());
        assert_eq!(
            mgr_next_day.auto_install_block_reason_at(202611250210, t_next_day),
            None
        );
        let mgr_after_clear = UpdateManager::with_config_at(
            updates_dir.clone(),
            anchors,
            Os::Linux,
            Arch::X86_64,
            platform.clone(),
            "0.1.0".to_owned(),
            "old".to_owned(),
            202611250200,
            t_next_day + 5,
        );
        assert_eq!(mgr_after_clear.snapshot().phase, "ready");
        assert_eq!(
            mgr_after_clear.evaluate_auto_install_gate_at("startup", 0, true, t_next_day + 5),
            Ok(())
        );

        // Re-skip seq 202611250210, then publish strictly newer seq 202611250215:
        // the newer sequence MUST be completely unaffected by the older sequence's skip.
        mgr_after_clear.record_failed_update(
            Channel::Stable,
            "0.2.0",
            202611250210,
            ROLLBACK_REASON,
        );
        assert_eq!(
            mgr_after_clear.auto_install_block_reason_at(202611250210, t_next_day + 10),
            Some("sequence_skipped")
        );
        fleet.add_set(
            "tower",
            build_fake_set(
                Channel::Stable,
                "0.2.1",
                202611250215,
                "Legion-linux-x86_64.AppImage",
                Os::Linux,
                Arch::X86_64,
                InstallKind::AppImage,
                b"fixed-v2.1-appimage",
                &rel_sk,
            ),
        );
        let st_newer = mgr_after_clear
            .check_with_client_at(&fleet, &["tower".to_owned()], false, false, t_next_day + 10)
            .await
            .unwrap();
        assert_eq!(st_newer.phase, "ready", "{st_newer:?}");
        assert_eq!(st_newer.available.as_ref().unwrap().sequence, 202611250215);
        assert!(st_newer.failed_update.is_none());
        let mgr_newer_boot = UpdateManager::with_config_at(
            updates_dir,
            anchors,
            Os::Linux,
            Arch::X86_64,
            platform,
            "0.1.0".to_owned(),
            "old".to_owned(),
            202611250200,
            t_next_day + 20,
        );
        assert_eq!(mgr_newer_boot.snapshot().phase, "ready");
        assert_eq!(
            mgr_newer_boot.evaluate_auto_install_gate_at("startup", 0, true, t_next_day + 20),
            Ok(())
        );
    }

    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    #[tokio::test]
    async fn deferred_update_clears_pending_install_without_counting_as_failure() {
        let dir = tempfile::tempdir().unwrap();
        let updates_dir = dir.path().join("updates");
        let appimage = dir.path().join("Legion.AppImage");
        let (rel_sk, rel_pk) = keypair(0x93);
        let anchors = TrustAnchors::with_keys(rel_pk, None);
        let t0: i64 = 1_795_593_600;

        let mut fleet = FakeFleet::new();
        fleet.add_set(
            "tower",
            build_fake_set(
                Channel::Stable,
                "0.2.0",
                202611250220,
                "Legion-linux-x86_64.AppImage",
                Os::Linux,
                Arch::X86_64,
                InstallKind::AppImage,
                b"v2-appimage",
                &rel_sk,
            ),
        );

        let platform = PlatformEnv::Desktop(DesktopInstall::LinuxAppImage {
            appimage_path: appimage,
        });
        let mgr = UpdateManager::with_config_at(
            updates_dir.clone(),
            anchors,
            Os::Linux,
            Arch::X86_64,
            platform.clone(),
            "0.1.0".to_owned(),
            "old".to_owned(),
            202611250200,
            t0,
        );
        let st = mgr
            .check_with_client_at(&fleet, &["tower".to_owned()], false, false, t0)
            .await
            .unwrap();
        let avail = st.available.unwrap();
        mgr.record_started_attempt_and_pending_at(&avail, "staged", t0)
            .unwrap();

        // Simulate a supervisor deferring cleanly pre-swap (even with a UTF-8 BOM prefix).
        let mut deferred_bytes = vec![0xEF, 0xBB, 0xBF];
        deferred_bytes.extend_from_slice(
            br#"{"sequence":202611250220,"reason":"Install files were in use"}"#,
        );
        fs::write(
            updates_dir.join("deferred-202611250220.json"),
            deferred_bytes,
        )
        .unwrap();

        let mgr_next = UpdateManager::with_config_at(
            updates_dir.clone(),
            anchors,
            Os::Linux,
            Arch::X86_64,
            platform,
            "0.1.0".to_owned(),
            "old".to_owned(),
            202611250200,
            t0 + 20,
        );
        mgr_next.on_app_ready_at(t0 + 20);
        let st_next = mgr_next.snapshot();
        assert_eq!(st_next.phase, "ready", "deferred update must stay ready");
        assert!(
            st_next.failed_update.is_none(),
            "deferred update must NOT count as a failed update"
        );
        assert!(
            !mgr_next.sequence_failed_before(202611250220),
            "deferred update must NOT increment failed_updates or failed_sequences"
        );
        assert_eq!(
            mgr_next.auto_install_block_reason_at(202611250220, t0 + 20),
            Some("auto_install_hourly_floor"),
            "global 1-hour floor still caps automatic retries within 1 hour of any attempt"
        );
        assert_eq!(
            mgr_next
                .auto_install_block_reason_at(202611250220, t0 + AUTO_INSTALL_HOURLY_FLOOR_SECS),
            None,
            "deferred update has zero failures recorded once the 1-hour floor elapses"
        );
        assert!(
            !updates_dir.join("deferred-202611250220.json").exists(),
            "deferred marker should be consumed"
        );
    }

    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    #[tokio::test]
    async fn reviewer_probe_startup_gate_refuses_immediately_after_rollback_before_ready() {
        let dir = tempfile::tempdir().unwrap();
        let updates_dir = dir.path().join("updates");
        let appimage = dir.path().join("Legion.AppImage");
        let (rel_sk, rel_pk) = keypair(0x95);
        let anchors = TrustAnchors::with_keys(rel_pk, None);
        let t0: i64 = 1_795_593_600;

        let mut fleet = FakeFleet::new();
        fleet.add_set(
            "tower",
            build_fake_set(
                Channel::Stable,
                "0.2.0",
                202611250230,
                "Legion-linux-x86_64.AppImage",
                Os::Linux,
                Arch::X86_64,
                InstallKind::AppImage,
                b"broken-v2",
                &rel_sk,
            ),
        );
        let platform = PlatformEnv::Desktop(DesktopInstall::LinuxAppImage {
            appimage_path: appimage,
        });

        let mgr1 = UpdateManager::with_config_at(
            updates_dir.clone(),
            anchors,
            Os::Linux,
            Arch::X86_64,
            platform.clone(),
            "0.1.0".to_owned(),
            "old".to_owned(),
            202611250200,
            t0,
        );
        let st1 = mgr1
            .check_with_client_at(&fleet, &["tower".to_owned()], false, false, t0)
            .await
            .unwrap();
        let avail = st1.available.unwrap();
        mgr1.record_started_attempt_and_pending_at(&avail, "staged", t0)
            .unwrap();

        // Supervisor rolls back, writes rollback-202611250230.json, and relaunches v1.
        fs::write(
            updates_dir.join("rollback-202611250230.json"),
            serde_json::to_vec(&RollbackInfo {
                version: "0.2.0".to_owned(),
                sequence: 202611250230,
                reason: ROLLBACK_REASON.to_owned(),
                detail: None,
                install_state: Some("restored_previous".to_owned()),
            })
            .unwrap(),
        )
        .unwrap();

        // Reviewer's exact probe: construct UpdateManager on relaunch and IMMEDIATELY ask the
        // startup auto-install gate BEFORE `on_app_ready` has run!
        let mgr_relaunched = UpdateManager::with_config_at(
            updates_dir.clone(),
            anchors,
            Os::Linux,
            Arch::X86_64,
            platform.clone(),
            "0.1.0".to_owned(),
            "old".to_owned(),
            202611250200,
            t0 + 15,
        );
        assert!(
            !updates_dir.join("rollback-202611250230.json").exists(),
            "rollback record must be read and consumed during UpdateManager construction"
        );
        assert_eq!(mgr_relaunched.snapshot().phase, "error");
        assert!(
            mgr_relaunched
                .evaluate_auto_install_gate_at("startup", 0, true, t0 + 15)
                .is_err(),
            "startup gate MUST refuse immediately after rollback before on_app_ready"
        );

        // Second safeguard check: even if an unread rollback or deferred record appears on disk
        // AFTER construction (e.g. after the cooldown elapsed), the gate itself MUST refuse!
        let t_later = t0 + 15 + ROLLBACK_RETRY_COOLDOWN_SECS + 10;
        let mgr_later = UpdateManager::with_config_at(
            updates_dir.clone(),
            anchors,
            Os::Linux,
            Arch::X86_64,
            platform,
            "0.1.0".to_owned(),
            "old".to_owned(),
            202611250200,
            t_later,
        );
        assert_eq!(mgr_later.snapshot().phase, "ready");
        fs::write(
            updates_dir.join("deferred-202611250230.json"),
            br#"{"sequence":202611250230,"reason":"unread"}"#,
        )
        .unwrap();
        assert_eq!(
            mgr_later.evaluate_auto_install_gate_at("startup", 0, true, t_later),
            Err("unread_supervisor_record"),
            "gate must refuse while any unread supervisor record exists on disk"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn endless_loop_impossible_broken_update_spawns_at_most_two_supervisors() {
        use std::os::unix::fs::PermissionsExt;
        use std::time::Instant as StdInstant;

        let dir = tempfile::tempdir().unwrap();
        let updates_dir = dir.path().join("updates");
        let appimage = dir.path().join("Legion.AppImage");
        let spawn_counter = dir.path().join("v2-launches.txt");
        let (rel_sk, rel_pk) = keypair(0x96);
        let anchors = TrustAnchors::with_keys(rel_pk, None);
        let t0: i64 = 1_795_593_600;

        fs::write(&appimage, "#!/bin/sh\nexit 0\n").unwrap();
        fs::set_permissions(&appimage, fs::Permissions::from_mode(0o755)).unwrap();

        // Broken v2 AppImage: logs its launch and exits 1 without ever writing `started-*.ok`.
        let broken_v2 = format!(
            "#!/bin/sh\nprintf 'attempt\\n' >> '{}'\nexit 1\n",
            spawn_counter.display()
        );
        let mut fleet = FakeFleet::new();
        fleet.add_set(
            "tower",
            build_fake_set(
                Channel::Stable,
                "0.2.0",
                202611250235,
                "Legion-linux-x86_64.AppImage",
                Os::Linux,
                Arch::X86_64,
                InstallKind::AppImage,
                broken_v2.as_bytes(),
                &rel_sk,
            ),
        );

        let platform = PlatformEnv::Desktop(DesktopInstall::LinuxAppImage {
            appimage_path: appimage.clone(),
        });

        // Initial check & download on v1.
        let mgr_init = UpdateManager::with_config_at(
            updates_dir.clone(),
            anchors,
            Os::Linux,
            Arch::X86_64,
            platform.clone(),
            "0.1.0".to_owned(),
            "old".to_owned(),
            202611250200,
            t0,
        );
        let st_init = mgr_init
            .check_with_client_at(&fleet, &["tower".to_owned()], false, false, t0)
            .await
            .unwrap();
        assert_eq!(st_init.phase, "ready");

        // Drive 8 relaunches across immediate restarts, 30 min, 61 min, 2 hours, 6 hours, 24 hours,
        // asking the startup gate BEFORE `on_app_ready` on every single boot. Even if we delete
        // rollback files before one relaunch, the persisted started-attempt counter caps spawns
        // at EXACTLY 2!
        let boot_times = [
            t0,
            t0 + 15,
            t0 + 45,
            t0 + 1800,
            t0 + 3700,
            t0 + 3715,
            t0 + 7500,
            t0 + 86_400,
        ];
        let mut supervisors_spawned = 0usize;
        for (idx, &now) in boot_times.iter().enumerate() {
            let mgr = UpdateManager::with_config_at(
                updates_dir.clone(),
                anchors,
                Os::Linux,
                Arch::X86_64,
                platform.clone(),
                "0.1.0".to_owned(),
                "old".to_owned(),
                202611250200,
                now,
            );
            if mgr
                .evaluate_auto_install_gate_at("startup", 0, true, now)
                .is_ok()
            {
                let (avail, size, sha, art_path) = mgr.prepare_verified_install().unwrap();
                let mut exited_old = std::process::Command::new("/bin/sh")
                    .args(["-c", "exit 0"])
                    .spawn()
                    .unwrap();
                let dead_pid = exited_old.id();
                let _ = exited_old.wait();

                // Persist started attempt + pending install (exactly as production spawn does)
                // and run a real short-wait Unix swap supervisor with our broken v2.
                let lock = desktop::try_acquire_update_lock(&updates_dir, avail.sequence, true)
                    .unwrap()
                    .unwrap();
                mgr.record_started_attempt_and_pending_at(&avail, "staged", now)
                    .unwrap();
                let staged = dir.path().join(format!(".Legion.AppImage.new-loop-{idx}"));
                desktop::copy_and_verify_sha256(&art_path, &staged, &sha, size, 0o755).unwrap();
                let rb_path = updates_dir.join(format!("rollback-{}.json", avail.sequence));
                let sup_pid = desktop::spawn_unix_swap_supervisor(&desktop::UnixSwapPlan {
                    old_pid: dead_pid,
                    old_ppid: None,
                    target_path: appimage.clone(),
                    staged_new_path: staged,
                    backup_path: dir.path().join(".Legion.AppImage.prev"),
                    exec_path: appimage.clone(),
                    ready_marker_path: updates_dir.join(format!("started-{}.ok", avail.sequence)),
                    rollback_marker_path: rb_path.clone(),
                    rollback_json: serde_json::to_string(&RollbackInfo {
                        version: avail.version.clone(),
                        sequence: avail.sequence,
                        reason: ROLLBACK_REASON.to_owned(),
                        detail: None,
                        install_state: Some("restored_previous".to_owned()),
                    })
                    .unwrap(),
                    rollback_kept_new_json: "{}".to_owned(),
                    deferred_marker_path: None,
                    deferred_json: None,
                    target_sequence: avail.sequence,
                    update_lock_path: Some(updates_dir.join(desktop::UPDATE_LOCK_FILENAME)),
                    stage_file_path: Some(desktop::pending_stage_path(
                        &updates_dir,
                        avail.sequence,
                    )),
                    reopen_marker_path: None,
                    log_path: Some(updates_dir.join("supervisor.log")),
                    new_start_log_path: Some(updates_dir.join(desktop::NEW_START_LOG_FILENAME)),
                    rollback_ps_path: None,
                    single_instance_dbus_name: None,
                    single_instance_socket_path: None,
                    pre_swap_wait_secs: 4,
                    wait_secs: 4,
                    exit_grace_secs: 1,
                    rollback_retries: 4,
                    verify_only: true,
                })
                .unwrap();
                lock.hand_over_to_supervisor(sup_pid).unwrap();
                supervisors_spawned += 1;

                // Wait for the supervisor to finish rolling back and release update.lock.
                let deadline = StdInstant::now() + Duration::from_secs(30);
                while StdInstant::now() < deadline {
                    if rb_path.exists() && !updates_dir.join(desktop::UPDATE_LOCK_FILENAME).exists()
                    {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
                assert!(
                    rb_path.exists(),
                    "expected rollback on broken v2 attempt {idx}"
                );
            }
            mgr.on_app_ready_at(now);
        }

        assert_eq!(
            supervisors_spawned, 2,
            "broken update must spawn EXACTLY two supervisors across repeated relaunches"
        );
        let launched = fs::read_to_string(&spawn_counter).unwrap();
        assert_eq!(launched.lines().count(), 2);
    }

    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    #[tokio::test]
    async fn hourly_floor_limits_automatic_installs_across_different_sequences_while_manual_exempt()
    {
        let dir = tempfile::tempdir().unwrap();
        let updates_dir = dir.path().join("updates");
        let appimage = dir.path().join("Legion.AppImage");
        let (rel_sk, rel_pk) = keypair(0x97);
        let anchors = TrustAnchors::with_keys(rel_pk, None);
        let t0: i64 = 1_795_593_600;

        let mut fleet = FakeFleet::new();
        fleet.add_set(
            "tower",
            build_fake_set(
                Channel::Stable,
                "0.2.0",
                202611250240,
                "Legion-linux-x86_64.AppImage",
                Os::Linux,
                Arch::X86_64,
                InstallKind::AppImage,
                b"v2-payload",
                &rel_sk,
            ),
        );
        let platform = PlatformEnv::Desktop(DesktopInstall::LinuxAppImage {
            appimage_path: appimage,
        });

        let mgr = UpdateManager::with_config_at(
            updates_dir.clone(),
            anchors,
            Os::Linux,
            Arch::X86_64,
            platform.clone(),
            "0.1.0".to_owned(),
            "old".to_owned(),
            202611250200,
            t0,
        );
        let st1 = mgr
            .check_with_client_at(&fleet, &["tower".to_owned()], false, false, t0)
            .await
            .unwrap();
        mgr.record_started_attempt_and_pending_at(&st1.available.unwrap(), "staged", t0)
            .unwrap();

        // Rollback seq 240 at t0 + 10s, then publish a brand-new seq 245 at t0 + 20m.
        fs::write(
            updates_dir.join("rollback-202611250240.json"),
            serde_json::to_vec(&RollbackInfo {
                version: "0.2.0".to_owned(),
                sequence: 202611250240,
                reason: ROLLBACK_REASON.to_owned(),
                detail: None,
                install_state: Some("restored_previous".to_owned()),
            })
            .unwrap(),
        )
        .unwrap();
        fleet.add_set(
            "tower",
            build_fake_set(
                Channel::Stable,
                "0.2.1",
                202611250245,
                "Legion-linux-x86_64.AppImage",
                Os::Linux,
                Arch::X86_64,
                InstallKind::AppImage,
                b"v2.1-payload",
                &rel_sk,
            ),
        );

        let mgr_soon = UpdateManager::with_config_at(
            updates_dir.clone(),
            anchors,
            Os::Linux,
            Arch::X86_64,
            platform.clone(),
            "0.1.0".to_owned(),
            "old".to_owned(),
            202611250200,
            t0 + 1200,
        );
        let st_new = mgr_soon
            .check_with_client_at(&fleet, &["tower".to_owned()], false, false, t0 + 1200)
            .await
            .unwrap();
        assert_eq!(
            st_new.phase, "ready",
            "newer sequence downloads cleanly for manual install"
        );
        assert_eq!(st_new.available.as_ref().unwrap().sequence, 202611250245);
        assert_eq!(
            mgr_soon.evaluate_auto_install_gate_at("startup", 0, true, t0 + 1200),
            Err("auto_install_hourly_floor"),
            "1-hour floor must block automatic install even for a brand-new sequence"
        );

        // Once 1 full hour has elapsed since t0, automatic install of the new sequence is allowed.
        let mgr_after_hour = UpdateManager::with_config_at(
            updates_dir,
            anchors,
            Os::Linux,
            Arch::X86_64,
            platform,
            "0.1.0".to_owned(),
            "old".to_owned(),
            202611250200,
            t0 + AUTO_INSTALL_HOURLY_FLOOR_SECS,
        );
        assert_eq!(
            mgr_after_hour.evaluate_auto_install_gate_at(
                "startup",
                0,
                true,
                t0 + AUTO_INSTALL_HOURLY_FLOOR_SECS
            ),
            Ok(())
        );
    }

    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    #[tokio::test]
    async fn pending_install_stage_distinguishes_pre_swap_deferral_from_post_swap_failure() {
        let dir = tempfile::tempdir().unwrap();
        let updates_dir = dir.path().join("updates");
        let appimage = dir.path().join("Legion.AppImage");
        let (rel_sk, rel_pk) = keypair(0x98);
        let anchors = TrustAnchors::with_keys(rel_pk, None);
        let t0: i64 = 1_795_593_600;

        let mut fleet = FakeFleet::new();
        fleet.add_set(
            "tower",
            build_fake_set(
                Channel::Stable,
                "0.2.0",
                202611250250,
                "Legion-linux-x86_64.AppImage",
                Os::Linux,
                Arch::X86_64,
                InstallKind::AppImage,
                b"v2-payload",
                &rel_sk,
            ),
        );
        let platform = PlatformEnv::Desktop(DesktopInstall::LinuxAppImage {
            appimage_path: appimage,
        });

        let mgr = UpdateManager::with_config_at(
            updates_dir.clone(),
            anchors,
            Os::Linux,
            Arch::X86_64,
            platform.clone(),
            "0.1.0".to_owned(),
            "old".to_owned(),
            202611250200,
            t0,
        );
        let avail = mgr
            .check_with_client_at(&fleet, &["tower".to_owned()], false, false, t0)
            .await
            .unwrap()
            .available
            .unwrap();

        // 1. Supervisor stopped at `"staged"` (or `"swapping"`) before swap completed, leaving no
        //    deferred/rollback file: next start treats it as a deferral (zero failure, not skipped).
        for stage in ["staged", "swapping"] {
            mgr.record_started_attempt_and_pending_at(&avail, stage, t0)
                .unwrap();
            let mgr_next = UpdateManager::with_config_at(
                updates_dir.clone(),
                anchors,
                Os::Linux,
                Arch::X86_64,
                platform.clone(),
                "0.1.0".to_owned(),
                "old".to_owned(),
                202611250200,
                t0 + 10,
            );
            assert_eq!(
                mgr_next.snapshot().phase,
                "ready",
                "stage {stage} stopped before swap must stay ready"
            );
            assert!(
                !mgr_next.sequence_failed_before(202611250250),
                "stage {stage} stopped before swap must not record failure"
            );
            assert!(mgr_next.snapshot().failed_update.is_none());
        }

        // 2. Supervisor reached `"swapped"` and old sequence is running without rollback marker:
        //    next start records a real failed start with `skipped: true`.
        mgr.record_started_attempt_and_pending_at(&avail, "staged", t0 + 100)
            .unwrap();
        desktop::write_pending_stage(&updates_dir, 202611250250, "swapped").unwrap();
        let mgr_swapped = UpdateManager::with_config_at(
            updates_dir,
            anchors,
            Os::Linux,
            Arch::X86_64,
            platform,
            "0.1.0".to_owned(),
            "old".to_owned(),
            202611250200,
            t0 + 120,
        );
        assert_eq!(mgr_swapped.snapshot().phase, "error");
        let fail = mgr_swapped.snapshot().failed_update.unwrap();
        assert_eq!(fail.sequence, 202611250250);
        assert!(fail.skipped);
    }

    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    #[tokio::test]
    async fn rollback_kept_new_running_target_sequence_reports_success_with_no_rollback_notice() {
        let dir = tempfile::tempdir().unwrap();
        let updates_dir = dir.path().join("updates");
        let appimage = dir.path().join("Legion.AppImage");
        let (rel_sk, rel_pk) = keypair(0x99);
        let anchors = TrustAnchors::with_keys(rel_pk, None);
        let t0: i64 = 1_795_593_600;

        let mut fleet = FakeFleet::new();
        fleet.add_set(
            "tower",
            build_fake_set(
                Channel::Stable,
                "0.2.0",
                202611250255,
                "Legion-linux-x86_64.AppImage",
                Os::Linux,
                Arch::X86_64,
                InstallKind::AppImage,
                b"v2-payload",
                &rel_sk,
            ),
        );
        let platform = PlatformEnv::Desktop(DesktopInstall::LinuxAppImage {
            appimage_path: appimage,
        });

        let mgr1 = UpdateManager::with_config_at(
            updates_dir.clone(),
            anchors,
            Os::Linux,
            Arch::X86_64,
            platform.clone(),
            "0.1.0".to_owned(),
            "old".to_owned(),
            202611250200,
            t0,
        );
        let avail = mgr1
            .check_with_client_at(&fleet, &["tower".to_owned()], false, false, t0)
            .await
            .unwrap()
            .available
            .unwrap();
        mgr1.record_started_attempt_and_pending_at(&avail, "staged", t0)
            .unwrap();

        // Supervisor timed out and wrote `rollback-202611250255.json` with `new_kept_files_in_use`,
        // but the new build 202611250255 IS what is now running!
        fs::write(
            updates_dir.join("rollback-202611250255.json"),
            serde_json::to_vec(&RollbackInfo {
                version: "0.2.0".to_owned(),
                sequence: 202611250255,
                reason: ROLLBACK_REASON.to_owned(),
                detail: None,
                install_state: Some("new_kept_files_in_use".to_owned()),
            })
            .unwrap(),
        )
        .unwrap();

        let mgr_v2 = UpdateManager::with_config_at(
            updates_dir.clone(),
            anchors,
            Os::Linux,
            Arch::X86_64,
            platform,
            "0.2.0".to_owned(),
            "new".to_owned(),
            202611250255,
            t0 + 40,
        );
        assert!(
            mgr_v2.snapshot().rolled_back.is_none(),
            "running the target sequence must never show a rollback notice about a previous copy"
        );
        assert!(mgr_v2.snapshot().failed_update.is_none());
        mgr_v2.on_app_ready_at(t0 + 40);
        let st_v2 = mgr_v2.snapshot();
        assert_eq!(st_v2.phase, "up_to_date");
        assert!(st_v2.rolled_back.is_none());
        assert!(st_v2.failed_update.is_none());
        assert_eq!(st_v2.just_updated.as_ref().unwrap().sequence, 202611250255);
        assert!(!updates_dir.join("rollback-202611250255.json").exists());
    }

    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    #[test]
    fn stale_lock_notice_surfaces_under_updates_and_clears_on_dismiss() {
        let dir = tempfile::tempdir().unwrap();
        let updates_dir = dir.path().join("updates");
        let (_rel_sk, rel_pk) = keypair(0x9a);
        let anchors = TrustAnchors::with_keys(rel_pk, None);
        let platform = PlatformEnv::Desktop(DesktopInstall::LinuxAppImage {
            appimage_path: dir.path().join("Legion.AppImage"),
        });

        desktop::record_stale_lock_notice(&updates_dir, "lock exceeded 180s");
        let mgr = UpdateManager::with_config(
            updates_dir.clone(),
            anchors,
            Os::Linux,
            Arch::X86_64,
            platform,
            "0.1.0".to_owned(),
            "old".to_owned(),
            202611250200,
        );
        assert_eq!(
            mgr.snapshot().last_error.as_deref(),
            Some(desktop::STALE_UPDATE_NOTICE)
        );
        assert!(
            !updates_dir
                .join(desktop::STALE_LOCK_MARKER_FILENAME)
                .exists()
        );

        let dismissed = mgr.dismiss_notice().unwrap();
        assert!(dismissed.last_error.is_none());
    }

    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    #[tokio::test]
    async fn second_instance_cannot_stage_while_update_lock_is_held() {
        let dir = tempfile::tempdir().unwrap();
        let updates_dir = dir.path().join("updates");
        let appimage = dir.path().join("Legion.AppImage");
        fs::write(&appimage, b"v1-appimage").unwrap();
        let (rel_sk, rel_pk) = keypair(0x94);
        let anchors = TrustAnchors::with_keys(rel_pk, None);
        let t0: i64 = 1_795_593_600;

        let mut fleet = FakeFleet::new();
        fleet.add_set(
            "tower",
            build_fake_set(
                Channel::Stable,
                "0.2.0",
                202611250225,
                "Legion-linux-x86_64.AppImage",
                Os::Linux,
                Arch::X86_64,
                InstallKind::AppImage,
                b"v2-appimage",
                &rel_sk,
            ),
        );

        let platform = PlatformEnv::Desktop(DesktopInstall::LinuxAppImage {
            appimage_path: appimage.clone(),
        });
        let mgr = UpdateManager::with_config_at(
            updates_dir.clone(),
            anchors,
            Os::Linux,
            Arch::X86_64,
            platform,
            "0.1.0".to_owned(),
            "old".to_owned(),
            202611250200,
            t0,
        );
        let st = mgr
            .check_with_client_at(&fleet, &["tower".to_owned()], false, false, t0)
            .await
            .unwrap();
        assert_eq!(st.phase, "ready");
        let (avail, size, sha, art_path) = mgr.prepare_verified_install().unwrap();

        // Hold the exclusive update lock as Instance 1.
        let lock1 = desktop::try_acquire_update_lock(&updates_dir, avail.sequence, false)
            .unwrap()
            .expect("instance 1 must acquire lock");

        // Instance 2 attempting to stage/spawn MUST return Ok(false) without staging any copy,
        // without touching pending_install, and without spawning a second supervisor.
        let spawned = mgr
            .spawn_desktop_install_inner(&avail, &art_path, &sha, size, false)
            .unwrap();
        assert!(
            !spawned,
            "second instance must not stage or spawn a supervisor while update.lock is held"
        );
        assert!(
            !dir.path()
                .join(format!(".Legion.AppImage.new-{}", std::process::id()))
                .exists(),
            "second instance must not create a second staged copy"
        );
        assert!(
            !mgr.try_spawn_desktop_close_install(0),
            "close-update path must also skip when update.lock is held"
        );

        drop(lock1);
    }

    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    #[tokio::test]
    async fn desktop_close_update_waits_for_active_transfer_drain_and_installs_after_queue_empties()
    {
        use std::sync::atomic::AtomicBool;

        use crate::link_backend::LinkBackend;
        use crate::store::AppStore;
        use crate::transfers::{
            PickedUploadFile, QueueNotificationSummary, TransferControlAction, TransferManager,
            TransferPlatform,
        };
        use crate::{
            DesktopCloseOutcome, desktop_close_active_ops, handle_desktop_close_request,
            try_spawn_close_install_after_drain,
        };

        struct DrainTestPlatform {
            hidden_for_drain: AtomicBool,
            drain_notified: AtomicBool,
        }

        impl DrainTestPlatform {
            fn new() -> Self {
                Self {
                    hidden_for_drain: AtomicBool::new(false),
                    drain_notified: AtomicBool::new(false),
                }
            }
        }

        impl TransferPlatform for DrainTestPlatform {
            fn pick_upload_files(
                &self,
            ) -> crate::backend::BoxFuture<'_, Result<Vec<PickedUploadFile>, AppError>>
            {
                Box::pin(async { Ok(Vec::new()) })
            }

            fn finalize_download<'a>(
                &'a self,
                _id: &'a str,
                part_path: &'a Path,
                _file_name: &'a str,
                _mime: Option<&'a str>,
                _open_after_done: bool,
            ) -> crate::backend::BoxFuture<'a, Result<String, AppError>> {
                let out = part_path.to_string_lossy().into_owned();
                Box::pin(async move { Ok(out) })
            }

            fn open_local_file<'a>(
                &'a self,
                _local_path: &'a str,
                _mime: Option<&'a str>,
            ) -> crate::backend::BoxFuture<'a, Result<(), AppError>> {
                Box::pin(async { Ok(()) })
            }

            fn on_queue_updated(&self, summary: QueueNotificationSummary) {
                if summary.active_count == 0
                    && summary.cancelling_count == 0
                    && self.hidden_for_drain.swap(false, Ordering::SeqCst)
                {
                    self.drain_notified.store(true, Ordering::SeqCst);
                }
            }
        }

        let dir = tempfile::tempdir().unwrap();
        let updates_dir = dir.path().join("updates");
        let data_dir = dir.path().join("data");
        fs::create_dir_all(&data_dir).unwrap();
        let appimage = dir.path().join("Legion.AppImage");
        fs::write(&appimage, b"#!/bin/sh\nexit 0\n").unwrap();

        let (rel_sk, rel_pk) = keypair(0x95);
        let anchors = TrustAnchors::with_keys(rel_pk, None);
        let t0: i64 = 1_795_593_600;

        let mut fleet = FakeFleet::new();
        fleet.add_set(
            "tower",
            build_fake_set(
                Channel::Stable,
                "0.2.0",
                202611250225,
                "Legion-linux-x86_64.AppImage",
                Os::Linux,
                Arch::X86_64,
                InstallKind::AppImage,
                b"#!/bin/sh\nexit 0\n",
                &rel_sk,
            ),
        );

        let platform = PlatformEnv::Desktop(DesktopInstall::LinuxAppImage {
            appimage_path: appimage.clone(),
        });
        let mgr = UpdateManager::with_config_at(
            updates_dir.clone(),
            anchors,
            Os::Linux,
            Arch::X86_64,
            platform,
            "0.1.0".to_owned(),
            "old".to_owned(),
            202611250200,
            t0,
        );
        let st = mgr
            .check_with_client_at(&fleet, &["tower".to_owned()], false, false, t0)
            .await
            .unwrap();
        assert_eq!(st.phase, "ready");

        let store = AppStore::in_dir(&data_dir);
        let backend: Arc<dyn Backend> = Arc::new(LinkBackend::with_local_program(
            store,
            Some("/usr/bin/legion".to_owned()),
        ));
        let drain_platform = Arc::new(DrainTestPlatform::new());
        let transfers = TransferManager::load_or_new(
            &data_dir,
            Arc::clone(&backend),
            Arc::clone(&drain_platform) as Arc<dyn TransferPlatform>,
        );

        // Queue an active device transfer without starting workers so it stays active deterministically.
        let xfer_id = transfers
            .enqueue_download(
                "local".to_owned(),
                "/tmp/sample.bin".to_owned(),
                Some("sample.bin".to_owned()),
                Some(4096),
                None,
                false,
            )
            .await
            .unwrap();
        assert!(transfers.has_active_work());

        // 1. Closing the window while a transfer is active MUST hide for drain and NEVER spawn
        //    an update supervisor or acquire update.lock.
        assert_eq!(
            handle_desktop_close_request(&mgr, backend.as_ref(), &transfers),
            DesktopCloseOutcome::HideForDrain
        );
        assert!(
            !updates_dir.join(desktop::UPDATE_LOCK_FILENAME).exists(),
            "active transfer must prevent update.lock acquisition on window close"
        );
        assert!(
            !updates_dir.join("supervisor.log").exists(),
            "active transfer must prevent supervisor spawn on window close"
        );
        assert_eq!(mgr.snapshot().phase, "ready");

        // Defense in depth: passing active transfer count into the native updater gate also
        // refuses startup and close auto-installs while transfers are active.
        let active_ops = desktop_close_active_ops(backend.as_ref(), &transfers);
        assert!(active_ops >= 1);
        assert_eq!(
            mgr.evaluate_auto_install_gate_at("startup", active_ops, true, t0),
            Err("native_activity_in_flight")
        );
        assert_eq!(
            mgr.evaluate_auto_install_gate_at("close", active_ops, false, t0),
            Err("native_activity_in_flight")
        );
        assert!(!try_spawn_close_install_after_drain(
            &mgr,
            backend.as_ref(),
            &transfers
        ));

        // 2. Window is now hidden waiting for the queue to drain. Complete/drain the transfer so
        //    no active work remains and verify the drain callback fires.
        drain_platform
            .hidden_for_drain
            .store(true, Ordering::SeqCst);
        transfers
            .control(&xfer_id, TransferControlAction::Cancel)
            .await
            .unwrap();
        assert!(!transfers.has_active_work());
        assert!(
            drain_platform.drain_notified.load(Ordering::SeqCst),
            "draining the active transfer queue must notify the hidden desktop window"
        );

        // 3. Now that the queue has drained, the native gate permits close-time update install.
        let idle_ops = desktop_close_active_ops(backend.as_ref(), &transfers);
        assert_eq!(idle_ops, 0);
        assert_eq!(
            mgr.evaluate_auto_install_gate_at("close", idle_ops, false, t0),
            Ok(())
        );

        #[cfg(unix)]
        {
            assert!(
                try_spawn_close_install_after_drain(&mgr, backend.as_ref(), &transfers),
                "ready update must spawn verify-only close supervisor once hidden transfer queue drains"
            );
            assert_eq!(mgr.snapshot().phase, "installing");
            let lock = desktop::inspect_active_update_lock(&updates_dir)
                .expect("close supervisor must hold update.lock after drain");
            assert!(lock.verify_only);
            assert_eq!(lock.target_sequence, 202611250225);
            if let Some(sup_pid) = lock.supervisor_pid {
                let _ = std::process::Command::new("/bin/kill")
                    .args(["-TERM", &sup_pid.to_string()])
                    .status();
                std::thread::sleep(Duration::from_millis(100));
            }
        }
    }

    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    #[tokio::test]
    async fn cancelling_transfer_blocks_every_update_gate_until_cleanup_finishes() {
        use std::sync::atomic::AtomicBool;

        use crate::commands::{evaluate_and_quiesce_auto_install, has_active_native_work};
        use crate::content::AppContentServer;
        use crate::link_backend::LinkBackend;
        use crate::store::AppStore;
        use crate::transfers::{
            PickedUploadFile, QueueNotificationSummary, TransferControlAction, TransferManager,
            TransferPlatform, TransferState,
        };
        use crate::{
            DesktopCloseOutcome, desktop_close_active_ops, handle_desktop_close_request,
            try_spawn_close_install_after_drain,
        };

        struct BlockingCancelPlatform {
            block_delete_part: AtomicBool,
            hidden_for_drain: AtomicBool,
            drain_notified: AtomicBool,
            drain_notify: tokio::sync::Notify,
        }

        impl BlockingCancelPlatform {
            fn new() -> Self {
                Self {
                    block_delete_part: AtomicBool::new(true),
                    hidden_for_drain: AtomicBool::new(false),
                    drain_notified: AtomicBool::new(false),
                    drain_notify: tokio::sync::Notify::new(),
                }
            }
        }

        impl TransferPlatform for BlockingCancelPlatform {
            fn pick_upload_files(
                &self,
            ) -> crate::backend::BoxFuture<'_, Result<Vec<PickedUploadFile>, AppError>>
            {
                Box::pin(async { Ok(Vec::new()) })
            }

            fn finalize_download<'a>(
                &'a self,
                _id: &'a str,
                part_path: &'a Path,
                _file_name: &'a str,
                _mime: Option<&'a str>,
                _open_after_done: bool,
            ) -> crate::backend::BoxFuture<'a, Result<String, AppError>> {
                let out = part_path.to_string_lossy().into_owned();
                Box::pin(async move { Ok(out) })
            }

            fn delete_part_file<'a>(
                &'a self,
                part_path: &'a Path,
            ) -> crate::backend::BoxFuture<'a, Result<(), AppError>> {
                let blocked = self.block_delete_part.load(Ordering::SeqCst);
                let path = part_path.to_path_buf();
                Box::pin(async move {
                    if blocked {
                        return Err(AppError::new(
                            ErrorCode::Busy,
                            "Simulated file lock holding partial transfer file.",
                        ));
                    }
                    match fs::remove_file(&path) {
                        Ok(()) => Ok(()),
                        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
                        Err(err) => Err(AppError::new(ErrorCode::Failed, err.to_string())),
                    }
                })
            }

            fn open_local_file<'a>(
                &'a self,
                _local_path: &'a str,
                _mime: Option<&'a str>,
            ) -> crate::backend::BoxFuture<'a, Result<(), AppError>> {
                Box::pin(async { Ok(()) })
            }

            fn on_queue_updated(&self, summary: QueueNotificationSummary) {
                if summary.active_count == 0
                    && summary.cancelling_count == 0
                    && self.hidden_for_drain.swap(false, Ordering::SeqCst)
                {
                    self.drain_notified.store(true, Ordering::SeqCst);
                    self.drain_notify.notify_waiters();
                }
            }
        }

        let dir = tempfile::tempdir().unwrap();
        let updates_dir = dir.path().join("updates");
        let android_updates_dir = dir.path().join("android-updates");
        let data_dir = dir.path().join("data");
        fs::create_dir_all(&data_dir).unwrap();
        let appimage = dir.path().join("Legion.AppImage");
        fs::write(&appimage, b"#!/bin/sh\nexit 0\n").unwrap();

        let (rel_sk, rel_pk) = keypair(0x96);
        let anchors = TrustAnchors::with_keys(rel_pk, None);
        let t0: i64 = 1_795_593_600;

        let mut fleet = FakeFleet::new();
        fleet.add_set(
            "tower",
            build_fake_set(
                Channel::Stable,
                "0.2.0",
                202611250230,
                "Legion-linux-x86_64.AppImage",
                Os::Linux,
                Arch::X86_64,
                InstallKind::AppImage,
                b"#!/bin/sh\nexit 0\n",
                &rel_sk,
            ),
        );
        let mut android_fleet = FakeFleet::new();
        android_fleet.add_set(
            "tower",
            build_fake_set(
                Channel::Stable,
                "0.2.0",
                202611250230,
                "Legion-android.apk",
                Os::Android,
                Arch::Aarch64,
                InstallKind::Apk,
                b"fake-apk-bytes",
                &rel_sk,
            ),
        );

        let mgr = UpdateManager::with_config_at(
            updates_dir.clone(),
            anchors,
            Os::Linux,
            Arch::X86_64,
            PlatformEnv::Desktop(DesktopInstall::LinuxAppImage {
                appimage_path: appimage.clone(),
            }),
            "0.1.0".to_owned(),
            "old".to_owned(),
            202611250200,
            t0,
        );
        assert_eq!(
            mgr.check_with_client_at(&fleet, &["tower".to_owned()], false, false, t0)
                .await
                .unwrap()
                .phase,
            "ready"
        );

        let android_mgr = UpdateManager::with_config_at(
            android_updates_dir,
            anchors,
            Os::Android,
            Arch::Aarch64,
            PlatformEnv::Android,
            "0.1.0".to_owned(),
            "old".to_owned(),
            202611250200,
            t0,
        );
        assert_eq!(
            android_mgr
                .check_with_client_at(&android_fleet, &["tower".to_owned()], false, false, t0)
                .await
                .unwrap()
                .phase,
            "ready"
        );

        let store = AppStore::in_dir(&data_dir);
        let backend: Arc<dyn Backend> = Arc::new(LinkBackend::with_local_program(
            store,
            Some("/usr/bin/legion".to_owned()),
        ));
        let content = AppContentServer::new(Arc::clone(&backend));
        let platform = Arc::new(BlockingCancelPlatform::new());
        let transfers = TransferManager::with_cleanup_retry_interval(
            &data_dir,
            Arc::clone(&backend),
            Arc::clone(&platform) as Arc<dyn TransferPlatform>,
            2,
            Duration::from_secs(2),
            Duration::from_millis(20),
        );

        let xfer_id = transfers
            .enqueue_download(
                "local".to_owned(),
                "/tmp/large.bin".to_owned(),
                Some("large.bin".to_owned()),
                Some(8192),
                None,
                false,
            )
            .await
            .unwrap();
        let part_file = data_dir
            .join("transfers")
            .join("parts")
            .join(format!("{xfer_id}.part"));
        fs::write(&part_file, b"partial-download-bytes").unwrap();

        // Simulate window hidden for drain before the transfer enters `Cancelling`.
        platform.hidden_for_drain.store(true, Ordering::SeqCst);
        let cancelled_snapshot = transfers
            .control(&xfer_id, TransferControlAction::Cancel)
            .await
            .unwrap();
        assert_eq!(
            cancelled_snapshot.state,
            TransferState::Cancelling,
            "blocked part-file removal must hold the transfer in Cancelling"
        );
        assert!(part_file.exists());

        // While the transfer is in `Cancelling`, EVERY update gate MUST treat it as active work:
        assert!(
            transfers.has_active_work(),
            "Cancelling transfer must count as active work"
        );
        assert!(
            !platform.drain_notified.load(Ordering::SeqCst),
            "hidden-for-drain must not fire while a transfer is still Cancelling"
        );
        assert!(
            has_active_native_work(backend.as_ref(), &transfers, &content, &mgr),
            "manual Update now gate must treat Cancelling transfer as active work"
        );
        let active_ops = desktop_close_active_ops(backend.as_ref(), &transfers);
        assert_eq!(active_ops, 1);
        assert_eq!(
            mgr.evaluate_auto_install_gate_at("startup", active_ops, true, t0),
            Err("native_activity_in_flight")
        );
        assert_eq!(
            mgr.evaluate_auto_install_gate_at("close", active_ops, false, t0),
            Err("native_activity_in_flight")
        );
        assert_eq!(
            android_mgr.evaluate_auto_install_gate_at("background", active_ops, true, t0),
            Err("native_activity_in_flight")
        );
        assert!(
            !evaluate_and_quiesce_auto_install(
                &mgr,
                backend.as_ref(),
                &transfers,
                &content,
                "startup",
                Duration::from_millis(100),
            )
            .await
        );
        assert!(
            !evaluate_and_quiesce_auto_install(
                &android_mgr,
                backend.as_ref(),
                &transfers,
                &content,
                "background",
                Duration::from_millis(100),
            )
            .await
        );
        assert_eq!(
            handle_desktop_close_request(&mgr, backend.as_ref(), &transfers),
            DesktopCloseOutcome::HideForDrain
        );
        assert!(!try_spawn_close_install_after_drain(
            &mgr,
            backend.as_ref(),
            &transfers
        ));
        assert!(
            !updates_dir.join(desktop::UPDATE_LOCK_FILENAME).exists(),
            "Cancelling transfer must prevent update.lock acquisition and supervisor spawn"
        );

        // Release the simulated file lock and start the cleanup retry loop: once file removal
        // completes and the transfer reaches `Cancelled`, every gate unblocks cleanly.
        platform.block_delete_part.store(false, Ordering::SeqCst);
        transfers.start();
        tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                let notified = platform.drain_notify.notified();
                tokio::pin!(notified);
                notified.as_mut().enable();
                let state = transfers
                    .list()
                    .into_iter()
                    .find(|t| t.id == xfer_id)
                    .map(|t| t.state);
                if state == Some(TransferState::Cancelled)
                    && platform.drain_notified.load(Ordering::SeqCst)
                {
                    break;
                }
                notified.await;
            }
        })
        .await
        .expect("timed out waiting for Cancelling transfer to finish cleanup");
        transfers.shutdown();

        assert!(!part_file.exists());
        assert!(!transfers.has_active_work());
        assert!(
            platform.drain_notified.load(Ordering::SeqCst),
            "drain callback must fire once Cancelling finishes removing files"
        );
        assert!(!has_active_native_work(
            backend.as_ref(),
            &transfers,
            &content,
            &mgr
        ));
        let idle_ops = desktop_close_active_ops(backend.as_ref(), &transfers);
        assert_eq!(idle_ops, 0);
        assert_eq!(
            mgr.evaluate_auto_install_gate_at("startup", idle_ops, false, t0),
            Ok(())
        );
        assert_eq!(
            mgr.evaluate_auto_install_gate_at("close", idle_ops, false, t0),
            Ok(())
        );
        assert_eq!(
            android_mgr.evaluate_auto_install_gate_at("background", idle_ops, false, t0),
            Ok(())
        );
    }

    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    #[tokio::test(start_paused = true)]
    async fn overview_background_refreshes_do_not_block_updates_and_never_get_cut_off_mid_call() {
        use std::sync::atomic::AtomicBool;

        use legion_link::proto::{HelloResult, PROTOCOL, Request, Response, method};
        use serde_json::Value;
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        use tokio::sync::{Notify, Semaphore};

        use crate::backend::CallRequest;
        use crate::commands::{evaluate_and_quiesce_auto_install, has_active_native_work};
        use crate::content::AppContentServer;
        use crate::link_backend::{DrainClock, LinkBackend, ManualDrainClock};
        use crate::store::AppStore;
        use crate::transfers::{PickedUploadFile, TransferManager, TransferPlatform};
        use crate::{DesktopCloseOutcome, handle_desktop_close_request_with_timeout};

        struct NoopPlatform;
        impl TransferPlatform for NoopPlatform {
            fn pick_upload_files(
                &self,
            ) -> crate::backend::BoxFuture<'_, Result<Vec<PickedUploadFile>, AppError>>
            {
                Box::pin(async { Ok(Vec::new()) })
            }
            fn finalize_download<'a>(
                &'a self,
                _id: &'a str,
                part_path: &'a Path,
                _file_name: &'a str,
                _mime: Option<&'a str>,
                _open_after_done: bool,
            ) -> crate::backend::BoxFuture<'a, Result<String, AppError>> {
                let out = part_path.to_string_lossy().into_owned();
                Box::pin(async move { Ok(out) })
            }
            fn open_local_file<'a>(
                &'a self,
                _local_path: &'a str,
                _mime: Option<&'a str>,
            ) -> crate::backend::BoxFuture<'a, Result<(), AppError>> {
                Box::pin(async { Ok(()) })
            }
        }

        let dir = tempfile::tempdir().unwrap();
        let updates_dir = dir.path().join("updates");
        let data_dir = dir.path().join("data");
        fs::create_dir_all(&data_dir).unwrap();
        let appimage = dir.path().join("Legion.AppImage");
        fs::write(&appimage, b"#!/bin/sh\nexit 0\n").unwrap();

        let (rel_sk, rel_pk) = keypair(0x97);
        let anchors = TrustAnchors::with_keys(rel_pk, None);
        let t0: i64 = 1_795_593_600;
        let mut fleet = FakeFleet::new();
        fleet.add_set(
            "tower",
            build_fake_set(
                Channel::Stable,
                "0.2.0",
                202611250235,
                "Legion-linux-x86_64.AppImage",
                Os::Linux,
                Arch::X86_64,
                InstallKind::AppImage,
                b"#!/bin/sh\nexit 0\n",
                &rel_sk,
            ),
        );

        let mgr = Arc::new(UpdateManager::with_config_at(
            updates_dir.clone(),
            anchors,
            Os::Linux,
            Arch::X86_64,
            PlatformEnv::Desktop(DesktopInstall::LinuxAppImage {
                appimage_path: appimage.clone(),
            }),
            "0.1.0".to_owned(),
            "old".to_owned(),
            202611250200,
            t0,
        ));
        assert_eq!(
            mgr.check_with_client_at(&fleet, &["tower".to_owned()], false, false, t0)
                .await
                .unwrap()
                .phase,
            "ready"
        );

        let call_entered = Arc::new(AtomicUsize::new(0));
        let call_entered_notify = Arc::new(Notify::new());
        let release = Arc::new(Semaphore::new(0));
        let entered_tx = Arc::clone(&call_entered);
        let entered_notify_tx = Arc::clone(&call_entered_notify);
        let release_rx = Arc::clone(&release);
        let opener: legion_link::SessionOpener = Arc::new(move |entry, _, cancel| {
            let entered_tx = Arc::clone(&entered_tx);
            let entered_notify_tx = Arc::clone(&entered_notify_tx);
            let release_rx = Arc::clone(&release_rx);
            Box::pin(async move {
                let node_name = entry.node.clone();
                let (client, server) = tokio::io::duplex(64 * 1024);
                let (client_r, client_w) = tokio::io::split(client);
                tokio::spawn(async move {
                    let (server_r, mut server_w) = tokio::io::split(server);
                    let mut lines = BufReader::new(server_r).lines();
                    while let Ok(Some(line)) = lines.next_line().await {
                        let req: Request = serde_json::from_str(&line).unwrap_or_else(|_| {
                            Request::new(legion_link::proto::Id::Number(0), "", Value::Null)
                        });
                        let reply = if req.method == method::HELLO {
                            Response::ok(
                                req.id,
                                serde_json::to_value(HelloResult {
                                    node: node_name.clone(),
                                    node_explicit: None,
                                    node_id: None,
                                    version: "0.1.0".to_owned(),
                                    build: None,
                                    protocol: PROTOCOL,
                                    os: "linux".to_owned(),
                                    arch: "x86_64".to_owned(),
                                    systems: Vec::new(),
                                    system: None,
                                    boot_id: None,
                                })
                                .unwrap_or(Value::Null),
                            )
                        } else {
                            entered_tx.fetch_add(1, Ordering::SeqCst);
                            entered_notify_tx.notify_waiters();
                            let Ok(permit) = release_rx.acquire().await else {
                                break;
                            };
                            permit.forget();
                            Response::ok(req.id, serde_json::json!({ "ok": true }))
                        };
                        let mut out = serde_json::to_string(&reply).unwrap_or_default();
                        out.push('\n');
                        if server_w.write_all(out.as_bytes()).await.is_err() {
                            break;
                        }
                        let _ = server_w.flush().await;
                    }
                });
                legion_link::Session::open_io(
                    client_r,
                    client_w,
                    &entry.node,
                    None,
                    Duration::from_secs(5),
                    cancel,
                )
                .await
            })
        });

        let wait_for_call_entered = || {
            let call_entered = Arc::clone(&call_entered);
            let call_entered_notify = Arc::clone(&call_entered_notify);
            async move {
                loop {
                    let notified = call_entered_notify.notified();
                    tokio::pin!(notified);
                    notified.as_mut().enable();
                    if call_entered.load(Ordering::SeqCst) > 0 {
                        return;
                    }
                    notified.await;
                }
            }
        };

        let clock = ManualDrainClock::new();
        let store = AppStore::in_dir(&data_dir);
        let backend: Arc<LinkBackend> = Arc::new(LinkBackend::with_opener_and_clock(
            store,
            Some("/usr/bin/legion".to_owned()),
            opener,
            Arc::clone(&clock) as Arc<dyn DrainClock>,
        ));
        let backend_dyn: Arc<dyn Backend> = Arc::clone(&backend) as Arc<dyn Backend>;
        let content = Arc::new(AppContentServer::new(Arc::clone(&backend_dyn)));
        let transfers = Arc::new(TransferManager::load_or_new(
            &data_dir,
            Arc::clone(&backend_dyn),
            Arc::new(NoopPlatform),
        ));

        // 1. Start a Machine Overview `sys.procs` poll and pause it mid-flight.
        let b_poll = Arc::clone(&backend);
        let poll_finished = Arc::new(AtomicBool::new(false));
        let poll_finished_tx = Arc::clone(&poll_finished);
        let poll_task = tokio::spawn(async move {
            let res = b_poll
                .call(
                    CallRequest {
                        id: None,
                        tool: "sys.procs".to_owned(),
                        args: serde_json::json!({ "group": true, "limit": 12 }),
                        on: Some(Value::String("local".to_owned())),
                    },
                    None,
                )
                .await;
            poll_finished_tx.store(true, Ordering::SeqCst);
            res
        });

        wait_for_call_entered().await;
        assert_eq!(backend.active_operations(), 0);
        assert_eq!(backend.background_refreshes(), 1);
        assert!(
            !has_active_native_work(backend.as_ref(), &transfers, &content, &mgr),
            "Overview background poll must not make active native work appear busy"
        );

        // If the poll stays mid-flight past the grace timeout, desktop close defers cleanly
        // instead of cutting the RPC off mid-call (driven deterministically via ManualDrainClock).
        let m_close = Arc::clone(&mgr);
        let b_close = Arc::clone(&backend);
        let t_close = Arc::clone(&transfers);
        let deferred_task = tokio::task::spawn_blocking(move || {
            handle_desktop_close_request_with_timeout(
                &m_close,
                b_close.as_ref(),
                &t_close,
                Duration::from_secs(2),
            )
        });
        clock.wait_for_waiter().await;
        assert!(
            !poll_finished.load(Ordering::SeqCst),
            "poll must still be in flight while desktop close waits for grace timeout"
        );
        clock.advance(Duration::from_secs(2));
        let deferred = deferred_task.await.unwrap();
        assert_eq!(deferred, DesktopCloseOutcome::CloseNormally);
        assert!(!updates_dir.join(desktop::UPDATE_LOCK_FILENAME).exists());

        // Start a startup auto-install quiesce while the poll is mid-call, prove it waits behind
        // the poll, then release the poll: the install waits for the poll to finish (never cutting
        // it off) and succeeds right between poll ticks without relying on wall-clock budgets.
        let m_quiesce = Arc::clone(&mgr);
        let b_quiesce = Arc::clone(&backend);
        let t_quiesce = Arc::clone(&transfers);
        let c_quiesce = Arc::clone(&content);
        let quiesce_task = tokio::spawn(async move {
            evaluate_and_quiesce_auto_install(
                &m_quiesce,
                b_quiesce.as_ref(),
                &t_quiesce,
                &c_quiesce,
                "startup",
                Duration::from_secs(2),
            )
            .await
        });
        clock.wait_for_waiter().await;
        assert!(
            !poll_finished.load(Ordering::SeqCst),
            "poll must still be in flight until explicitly released"
        );
        release.add_permits(1);
        let allowed_after_poll = quiesce_task.await.unwrap();
        assert!(
            allowed_after_poll,
            "auto-install must wait for in-flight Overview poll and then proceed between ticks"
        );
        assert!(poll_task.await.unwrap().is_ok());
        assert!(
            poll_finished.load(Ordering::SeqCst),
            "Overview poll must finish cleanly before auto-install proceeds"
        );
        assert_eq!(backend.background_refreshes(), 0);

        // 2. Now start a user-started `exec.run` call and pause it mid-flight:
        //    it MUST block every update gate immediately without entering the refresh drain wait.
        call_entered.store(0, Ordering::SeqCst);
        let b_exec = Arc::clone(&backend);
        let exec_task = tokio::spawn(async move {
            b_exec
                .call(
                    CallRequest {
                        id: None,
                        tool: "exec.run".to_owned(),
                        args: serde_json::json!({ "cmd": "sleep 1" }),
                        on: Some(Value::String("local".to_owned())),
                    },
                    None,
                )
                .await
        });
        wait_for_call_entered().await;
        assert_eq!(backend.active_operations(), 1);
        assert!(has_active_native_work(
            backend.as_ref(),
            &transfers,
            &content,
            &mgr
        ));
        assert_eq!(
            mgr.evaluate_auto_install_gate("close", backend.active_operations(), false),
            Err("native_activity_in_flight")
        );
        let m_blocked = Arc::clone(&mgr);
        let b_blocked = Arc::clone(&backend);
        let t_blocked = Arc::clone(&transfers);
        let blocked_close = tokio::task::spawn_blocking(move || {
            handle_desktop_close_request_with_timeout(
                &m_blocked,
                b_blocked.as_ref(),
                &t_blocked,
                Duration::from_secs(2),
            )
        })
        .await
        .unwrap();
        assert_eq!(blocked_close, DesktopCloseOutcome::CloseNormally);
        assert_eq!(
            clock.pending_waiters(),
            0,
            "active user operation must refuse close-time install immediately without waiting on refresh drain"
        );
        assert!(!updates_dir.join(desktop::UPDATE_LOCK_FILENAME).exists());
        release.add_permits(1);
        assert!(exec_task.await.unwrap().is_ok());
        assert_eq!(backend.active_operations(), 0);

        // 3. Desktop close while a `sys.info` refresh is mid-flight waits cleanly for that call to
        //    finish rather than cutting it off, then spawns the close-time supervisor between ticks.
        #[cfg(unix)]
        {
            call_entered.store(0, Ordering::SeqCst);
            let sys_done = Arc::new(AtomicBool::new(false));
            let sys_done_tx = Arc::clone(&sys_done);
            let b_sys = Arc::clone(&backend);
            let sys_task = tokio::spawn(async move {
                let res = b_sys
                    .call(
                        CallRequest {
                            id: None,
                            tool: "sys.info".to_owned(),
                            args: serde_json::json!({}),
                            on: Some(Value::String("local".to_owned())),
                        },
                        None,
                    )
                    .await;
                sys_done_tx.store(true, Ordering::SeqCst);
                res
            });
            wait_for_call_entered().await;
            let m_sys_close = Arc::clone(&mgr);
            let b_sys_close = Arc::clone(&backend);
            let t_sys_close = Arc::clone(&transfers);
            let close_task = tokio::task::spawn_blocking(move || {
                handle_desktop_close_request_with_timeout(
                    &m_sys_close,
                    b_sys_close.as_ref(),
                    &t_sys_close,
                    Duration::from_secs(2),
                )
            });
            clock.wait_for_waiter().await;
            assert!(
                !sys_done.load(Ordering::SeqCst),
                "sys.info must still be in flight while desktop close waits"
            );
            release.add_permits(1);
            let outcome = close_task.await.unwrap();
            assert_eq!(
                outcome,
                DesktopCloseOutcome::CloseWithUpdate,
                "desktop close must wait for in-flight sys.info and install cleanly between ticks"
            );
            assert!(
                sys_task.await.unwrap().is_ok(),
                "sys.info call must complete cleanly before close-time update runs"
            );
            assert!(sys_done.load(Ordering::SeqCst));
            if let Some(lock) = desktop::inspect_active_update_lock(&updates_dir)
                && let Some(sup_pid) = lock.supervisor_pid
            {
                let _ = std::process::Command::new("/bin/kill")
                    .args(["-TERM", &sup_pid.to_string()])
                    .status();
            }
        }
    }
}
