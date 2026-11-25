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

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, OpenOptions};
use std::future::Future;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use legion_link::Link;
use legion_update::{
    Arch, Artifact, CHECKSUMS_FILENAME, Channel, InstallKind, MANIFEST_FILENAME,
    MAX_ARTIFACT_BYTES, MAX_MANIFEST_BYTES, MAX_SIG_BYTES, MAX_SUMS_BYTES, Os, Program,
    SIGNATURE_FILENAME, SequenceFloorState, SignerKind, TargetPlatform, TrustAnchors,
    VerifiedSetMeta, compiled_sequence_marker, read_sequence_floor, recover_sequence_floor,
    select_verified_artifact, verify_artifact_file, verify_set_metadata,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tauri::ipc::Channel as IpcChannel;
use tempfile::NamedTempFile;
use tokio::sync::watch;

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
const MIN_PROGRESS_BYTES_PER_SEC: u64 = 16 * 1024;

/// Grace window before enforcing [`MIN_PROGRESS_BYTES_PER_SEC`].
const PROGRESS_GRACE_WINDOW: Duration = Duration::from_secs(5);

/// Overall wall-clock deadline for one check + download pass.
const OVERALL_CHECK_DEADLINE: Duration = Duration::from_secs(180);

/// Per-node timeout for `update.list` and metadata `update.read` calls.
const CHECK_CALL_TIMEOUT: Duration = Duration::from_secs(12);

/// Per-chunk timeout for artifact `update.read` calls.
const CHUNK_CALL_TIMEOUT: Duration = Duration::from_secs(30);

/// Build-time app semantic version from `Cargo.toml`.
pub const APP_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Build-time short revision embedded by `build.rs`.
pub const APP_BUILD_ID: &str = env!("LEGION_APP_BUILD_ID");

/// Parses the build-time update sequence emitted by `build.rs` (`0` for dev builds) and retains the
/// static `LEGION_UPDATE_SEQUENCE=<seq>\0` marker in compiled binaries for packaging inspection.
pub fn current_app_sequence() -> u64 {
    let _ = std::hint::black_box(compiled_sequence_marker());
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
}

/// Persisted record of an update sequence that failed to install or take effect on this device.
///
/// Failed sequences are never retried automatically; the user can tap "Try again" manually, and
/// automatic updates resume as soon as a strictly newer sequence is published.
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
                CHECK_CALL_TIMEOUT,
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

#[derive(Clone, Debug, Serialize, Deserialize)]
struct PendingInstallRecord {
    channel: Channel,
    version: String,
    build: String,
    sequence: u64,
    notes: String,
    from_sequence: u64,
    started_at: String,
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
            cached_available: None,
        }
    }
}

impl PersistedPrefs {
    fn has_failed(&self, sequence: u64) -> bool {
        self.failed_sequences.contains(&sequence)
            || self.failed_updates.iter().any(|r| r.sequence == sequence)
    }

    fn failed_record(&self, sequence: u64) -> Option<FailedUpdateRecord> {
        self.failed_updates
            .iter()
            .find(|r| r.sequence == sequence)
            .cloned()
            .or_else(|| {
                self.failed_sequences
                    .contains(&sequence)
                    .then(|| FailedUpdateRecord {
                        channel: Channel::Stable,
                        version: String::new(),
                        sequence,
                        reason: "A previous attempt to install this version failed.".to_owned(),
                        failed_at: now_rfc3339_utc(),
                    })
            })
    }

    fn record_failure(&mut self, record: FailedUpdateRecord) {
        if !self.failed_sequences.contains(&record.sequence) {
            self.failed_sequences.push(record.sequence);
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

/// Shared managed state holding the app's [`UpdateManager`].
pub struct SharedUpdater(pub Arc<UpdateManager>);

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
    /// Tracks `(channel, sequence, node)` sources that returned corrupt bytes, stalled, or errored
    /// so future checks and multi-node fallbacks try healthy nodes first.
    bad_sources: Mutex<BTreeSet<(Channel, u64, String)>>,
    /// Generation counter used to immediately cancel a stalled background check whenever a manual
    /// check or install starts (no mutex is ever held across network waits).
    cancel_tx: watch::Sender<u64>,
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
        let floor_path = updates_dir.join("floor.json");
        let sequence_floor = read_floor_value(&floor_path).max(current_sequence);
        let prefs = load_prefs(&updates_dir.join("prefs.json")).unwrap_or_default();

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

                if let Some(fail_rec) = prefs.failed_record(cached.sequence) {
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
            last_error: None,
            last_error_detail: None,
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
            bad_sources: Mutex::new(BTreeSet::new()),
            cancel_tx,
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
        let next = self.cancel_tx.borrow().wrapping_add(1);
        self.cancel_tx.send_replace(next);
        let mut rx = self.cancel_tx.subscribe();
        let _ = rx.borrow_and_update();
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
        let mut prefs_dirty = false;
        let mut saw_rollback = false;

        // 1. Check for a desktop rollback marker left by the detached supervisor.
        if let Some((rb_path, rb)) = read_and_take_rollback(&self.updates_dir) {
            let _ = fs::remove_file(rb_path);
            let channel = prefs
                .pending_install
                .as_ref()
                .map_or(Channel::Stable, |p| p.channel);
            prefs.record_failure(FailedUpdateRecord {
                channel,
                version: rb.version.clone(),
                sequence: rb.sequence,
                reason: rb.reason.clone(),
                failed_at: now_rfc3339_utc(),
            });
            prefs.pending_install = None;
            prefs.rolled_back = Some(rb);
            prefs_dirty = true;
            saw_rollback = true;
        }

        // 2. Check whether a pending update completed or failed to take effect on this start.
        if let Some(pending) = prefs.pending_install.clone()
            && self.current_sequence > 0
            && !saw_rollback
        {
            if pending.sequence == self.current_sequence {
                prefs.pending_install = None;
                prefs.rolled_back = None;
                prefs.cached_available = None;
                prefs
                    .failed_sequences
                    .retain(|&s| s > self.current_sequence);
                prefs
                    .failed_updates
                    .retain(|r| r.sequence > self.current_sequence);
                prefs.just_updated = Some(JustUpdatedInfo {
                    channel: pending.channel,
                    version: pending.version,
                    build: pending.build,
                    sequence: pending.sequence,
                    notes: pending.notes,
                    updated_at: now_rfc3339_utc(),
                });
                prefs_dirty = true;
                cleanup_old_downloads(&self.updates_dir, self.current_sequence);
            } else {
                // Pending install was recorded, but the running build after restart does NOT have
                // the expected sequence (for example, an artifact with a mismatched compiled
                // sequence or an aborted/ineffective install). Record it as failed so Legion never
                // loops retrying it automatically.
                let reason = format!(
                    "Version {} did not take effect after installation, so Legion will not retry it automatically.",
                    pending.version
                );
                prefs.record_failure(FailedUpdateRecord {
                    channel: pending.channel,
                    version: pending.version,
                    sequence: pending.sequence,
                    reason,
                    failed_at: now_rfc3339_utc(),
                });
                prefs.pending_install = None;
                prefs_dirty = true;
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
        if let Some(detail) = floor_recovery_detail {
            st.last_error = Some(
                "Legion rebuilt a damaged update floor file at the running version.".to_owned(),
            );
            st.last_error_detail = Some(detail);
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
            } else if let Some(fail) = prefs.failed_record(avail.sequence) {
                st.last_error = Some(fail.reason.clone());
                st.failed_update = Some(fail);
                if st.phase != "unsupported" {
                    st.phase = "error".to_owned();
                }
            }
        } else if let Some(last_fail) = prefs
            .failed_updates
            .iter()
            .filter(|r| r.sequence > st.sequence_floor)
            .max_by_key(|r| r.sequence)
            .cloned()
        {
            st.failed_update = Some(last_fail);
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

    /// Dismisses the one-time `just_updated` and/or `rolled_back` notice.
    pub fn dismiss_notice(&self) -> Result<UpdateState, AppError> {
        let mut prefs = self.prefs.lock().unwrap_or_else(|p| p.into_inner()).clone();
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
        let deadline = Instant::now() + OVERALL_CHECK_DEADLINE;
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
        let mut by_key: BTreeMap<(Channel, u64), CandidateSource> = BTreeMap::new();
        let mut answered_nodes = Vec::new();
        let mut last_query_detail: Option<String> = None;

        for node in nodes {
            let list_res = run_cancellable(&mut cancel_rx, deadline, client.list_sets(node)).await;
            match list_res {
                Ok(Ok(listed)) => {
                    answered_nodes.push(node.clone());
                    let mut accepted_from_node = 0usize;
                    // Sort newest first BEFORE truncating per-node candidate cap so a flood of old
                    // or fake sequences cannot push valid new sets out (and a flood of fake high
                    // sequences is capped at MAX_CANDIDATES_PER_NODE).
                    let mut sets = listed.sets;
                    sets.sort_by(|a, b| {
                        b.sequence
                            .cmp(&a.sequence)
                            .then_with(|| a.channel.cmp(&b.channel))
                    });
                    for set in sets {
                        if accepted_from_node >= MAX_CANDIDATES_PER_NODE {
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
                        if !by_key.contains_key(&(set.channel, set.sequence))
                            && by_key.len() >= MAX_CANDIDATES_TOTAL
                        {
                            continue;
                        }
                        accepted_from_node += 1;
                        let entry =
                            by_key
                                .entry((set.channel, set.sequence))
                                .or_insert_with(|| CandidateSource {
                                    channel: set.channel,
                                    sequence: set.sequence,
                                    nodes: Vec::new(),
                                });
                        if !entry.nodes.contains(node) {
                            entry.nodes.push(node.clone());
                        }
                    }
                }
                Ok(Err(err)) => {
                    if err.code != ErrorCode::UnknownTool {
                        last_query_detail = Some(format!("{node}: {}", err.message));
                    }
                }
                Err(cancelled_or_timeout) => {
                    if *cancel_rx.borrow() != my_gen {
                        return Ok(self.snapshot());
                    }
                    last_query_detail = Some(cancelled_or_timeout.message);
                    break;
                }
            }
        }

        if *cancel_rx.borrow() != my_gen {
            return Ok(self.snapshot());
        }

        // Order candidates newest sequence first (preferring stable on an exact tie).
        let mut candidates: Vec<CandidateSource> = by_key.into_values().collect();
        candidates.sort_by(|a, b| {
            b.sequence
                .cmp(&a.sequence)
                .then_with(|| a.channel.cmp(&b.channel))
        });

        let prefs_snapshot = self.prefs.lock().unwrap_or_else(|p| p.into_inner()).clone();
        let mut metadata_budget_by_node: BTreeMap<String, usize> = BTreeMap::new();
        let mut chosen: Option<VerifiedCandidate> = None;
        let mut newest_failed_verified: Option<(VerifiedCandidate, FailedUpdateRecord)> = None;
        let mut refusal_detail: Option<String> = None;

        'outer: for cand in candidates {
            let ordered_nodes = self.order_nodes_for(cand.channel, cand.sequence, &cand.nodes);
            for node in &ordered_nodes {
                let used = metadata_budget_by_node.get(node).copied().unwrap_or(0);
                if used >= MAX_METADATA_BYTES_PER_NODE {
                    continue;
                }
                let budget_left = MAX_METADATA_BYTES_PER_NODE - used;
                let verify_fut = self.fetch_and_verify_candidate(
                    client,
                    node,
                    &cand,
                    &ordered_nodes,
                    target,
                    &floor_path,
                    budget_left,
                    deadline,
                    &mut cancel_rx,
                );
                match verify_fut.await {
                    Ok((verified, bytes_read)) => {
                        *metadata_budget_by_node.entry(node.clone()).or_insert(0) += bytes_read;
                        let seq = verified.meta.manifest.sequence;
                        if !force_download && let Some(fail_rec) = prefs_snapshot.failed_record(seq)
                        {
                            // Skip failed sequences automatically so a newer/alternate valid set
                            // can still be chosen, while remembering the newest failed one so the
                            // user can tap "Try again".
                            if newest_failed_verified.is_none() {
                                newest_failed_verified = Some((verified, fail_rec));
                            }
                            continue 'outer;
                        }
                        chosen = Some(verified);
                        break 'outer;
                    }
                    Err((err, bytes_read)) => {
                        *metadata_budget_by_node.entry(node.clone()).or_insert(0) += bytes_read;
                        if *cancel_rx.borrow() != my_gen {
                            return Ok(self.snapshot());
                        }
                        self.mark_bad_source(cand.channel, cand.sequence, node);
                        refusal_detail = Some(format!("{node}: {}", err.message));
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
                st.last_error_detail = floor_recovery_detail;
            }));
        }

        let Some(chosen) = chosen else {
            self.record_last_checked(&checked_at, None);
            if answered_nodes.is_empty()
                && !nodes.is_empty()
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
                    st.last_error_detail = floor_recovery_detail.or(last_query_detail);
                }));
            }
            let had_no_answers = answered_nodes.is_empty() && !nodes.is_empty();
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
            .download_verified_candidate_with_fallback(
                client,
                &chosen,
                my_gen,
                &mut cancel_rx,
                deadline,
            )
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

    fn order_nodes_for(&self, channel: Channel, sequence: u64, nodes: &[String]) -> Vec<String> {
        let bad = self.bad_sources.lock().unwrap_or_else(|p| p.into_inner());
        let mut good_list = Vec::new();
        let mut bad_list = Vec::new();
        for n in nodes {
            if bad.contains(&(channel, sequence, n.clone())) {
                bad_list.push(n.clone());
            } else {
                good_list.push(n.clone());
            }
        }
        good_list.extend(bad_list);
        good_list
    }

    fn mark_bad_source(&self, channel: Channel, sequence: u64, node: &str) {
        let mut bad = self.bad_sources.lock().unwrap_or_else(|p| p.into_inner());
        if bad.len() >= 128 {
            bad.clear();
        }
        bad.insert((channel, sequence, node.to_owned()));
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
        cancel_rx: &mut watch::Receiver<u64>,
    ) -> Result<(VerifiedCandidate, usize), (AppError, usize)> {
        let mut read_total = 0usize;

        let sums_bytes = read_small_file(
            client,
            node,
            cand.channel,
            cand.sequence,
            CHECKSUMS_FILENAME,
            MAX_SUMS_BYTES.min(budget_left),
            deadline,
            cancel_rx,
        )
        .await
        .map_err(|e| (e, read_total))?;
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
            cancel_rx,
        )
        .await
        .map_err(|e| (e, read_total))?;
        read_total += sig_bytes.len();

        let sig_text = String::from_utf8(sig_bytes).map_err(|err| {
            (
                AppError::new(
                    ErrorCode::Denied,
                    format!("{SIGNATURE_FILENAME} from {node} is not valid UTF-8: {err}"),
                ),
                read_total,
            )
        })?;
        // Verify signature before reading update.json so untrusted sets are refused immediately.
        self.anchors
            .verify_for_channel(cand.channel, &sums_bytes, &sig_text)
            .map_err(|e| (AppError::new(ErrorCode::Denied, e.message), read_total))?;

        let manifest_budget = MAX_MANIFEST_BYTES.min(budget_left.saturating_sub(read_total));
        let manifest_bytes = read_small_file(
            client,
            node,
            cand.channel,
            cand.sequence,
            MANIFEST_FILENAME,
            manifest_budget,
            deadline,
            cancel_rx,
        )
        .await
        .map_err(|e| (e, read_total))?;
        read_total += manifest_bytes.len();

        let meta = verify_set_metadata(&sums_bytes, &sig_text, &manifest_bytes, &self.anchors)
            .map_err(|e| (AppError::new(e.code.into(), e.message), read_total))?;
        let artifact = select_verified_artifact(&meta, self.current_sequence, floor_path, target)
            .map_err(|e| (AppError::new(e.code.into(), e.message), read_total))?
            .clone();

        if meta.manifest.channel != cand.channel || meta.manifest.sequence != cand.sequence {
            return Err((
                AppError::new(
                    ErrorCode::Denied,
                    format!(
                        "Update set from {node} claims {}/{} instead of {}/{}.",
                        meta.manifest.channel, meta.manifest.sequence, cand.channel, cand.sequence
                    ),
                ),
                read_total,
            ));
        }

        let expected_sha256 = meta
            .sums
            .get(&artifact.name)
            .ok_or_else(|| {
                (
                    AppError::new(
                        ErrorCode::Denied,
                        format!(
                            "Artifact {} is missing from {CHECKSUMS_FILENAME}.",
                            artifact.name
                        ),
                    ),
                    read_total,
                )
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
        ))
    }

    async fn download_verified_candidate_with_fallback(
        &self,
        client: &dyn UpdateSourceClient,
        cand: &VerifiedCandidate,
        my_gen: u64,
        cancel_rx: &mut watch::Receiver<u64>,
        deadline: Instant,
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
        let mut poisoned_part_from_earlier_node = false;

        for node in ordered {
            if *cancel_rx.borrow() != my_gen {
                return Err(AppError::new(
                    ErrorCode::Cancelled,
                    "Update check was superseded by a newer request.",
                ));
            }
            // If an earlier node failed partway through, start fresh on the next node so bytes
            // from a hostile first node can never poison the hash of a healthy second node.
            let reset_part = poisoned_part_from_earlier_node;
            match self
                .download_from_single_node(client, &node, cand, reset_part, deadline, cancel_rx)
                .await
            {
                Ok(final_size) => return Ok((final_size, node)),
                Err(err) => {
                    if *cancel_rx.borrow() != my_gen {
                        return Err(err);
                    }
                    self.mark_bad_source(
                        cand.meta.manifest.channel,
                        cand.meta.manifest.sequence,
                        &node,
                    );
                    last_err = AppError::new(err.code, format!("{node}: {}", err.message));
                    poisoned_part_from_earlier_node = true;
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
        deadline: Instant,
        cancel_rx: &mut watch::Receiver<u64>,
    ) -> Result<u64, AppError> {
        let expected_size = cand.artifact.size;
        if expected_size == 0 || expected_size > MAX_DOWNLOAD_BYTES {
            return Err(AppError::new(
                ErrorCode::Denied,
                format!(
                    "Artifact {} declares invalid signed size {expected_size}.",
                    cand.artifact.name
                ),
            ));
        }

        let set_dir = download_dir_for(
            &self.updates_dir,
            cand.meta.manifest.channel,
            cand.meta.manifest.sequence,
        );
        ensure_updates_root(&self.updates_dir)?;
        ensure_updates_root(&self.updates_dir.join("downloads"))?;
        ensure_updates_root(&set_dir)?;

        write_atomic(&set_dir.join(CHECKSUMS_FILENAME), &cand.sums_bytes)?;
        write_atomic(&set_dir.join(SIGNATURE_FILENAME), cand.sig_text.as_bytes())?;
        write_atomic(&set_dir.join(MANIFEST_FILENAME), &cand.manifest_bytes)?;

        let final_path = set_dir.join(&cand.artifact.name);
        let part_path = set_dir.join(format!("{}.part", cand.artifact.name));
        let part_hash_marker = set_dir.join(format!("{}.part.sha256", cand.artifact.name));

        if reset_part {
            let _ = fs::remove_file(&part_path);
        }

        // Resume any leftover `.part` file only if it matches the exact expected SHA-256 and fits
        // within the signed `expected_size`.
        let mut offset = 0u64;
        if part_path.is_file() {
            let prev_hash = fs::read_to_string(&part_hash_marker).unwrap_or_default();
            if prev_hash.trim() == cand.expected_sha256 {
                offset = fs::symlink_metadata(&part_path)
                    .ok()
                    .filter(|m| m.file_type().is_file())
                    .map_or(0, |m| m.len());
                if offset > expected_size {
                    offset = 0;
                    let _ = fs::remove_file(&part_path);
                }
            } else {
                let _ = fs::remove_file(&part_path);
            }
        }
        write_atomic(&part_hash_marker, cand.expected_sha256.as_bytes())?;

        let mut file = if offset > 0 {
            OpenOptions::new()
                .append(true)
                .open(&part_path)
                .map_err(|err| {
                    AppError::new(
                        ErrorCode::Failed,
                        format!("Could not resume download {}: {err}", part_path.display()),
                    )
                })?
        } else {
            fs::File::create(&part_path).map_err(|err| {
                AppError::new(
                    ErrorCode::Failed,
                    format!("Could not create download {}: {err}", part_path.display()),
                )
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

        let max_chunks = (expected_size / MIN_EXPECTED_CHUNK_BYTES)
            .saturating_add(64)
            .min(usize::MAX as u64) as usize;
        let started_at = Instant::now();
        let initial_offset = offset;
        let mut chunks_seen = 0usize;

        while offset < expected_size {
            let remaining = expected_size - offset;
            let req_len = DOWNLOAD_CHUNK_BYTES.min(remaining);
            let reply = run_cancellable(
                cancel_rx,
                deadline,
                client.read_chunk(
                    node,
                    cand.meta.manifest.channel,
                    cand.meta.manifest.sequence,
                    &cand.artifact.name,
                    offset,
                    req_len,
                ),
            )
            .await??;

            if reply.offset != offset {
                let _ = fs::remove_file(&part_path);
                return Err(AppError::new(
                    ErrorCode::Failed,
                    format!(
                        "Unexpected chunk offset {} (expected {offset}) for {}.",
                        reply.offset, cand.artifact.name
                    ),
                ));
            }
            if reply.size != expected_size {
                let _ = fs::remove_file(&part_path);
                return Err(AppError::new(
                    ErrorCode::Failed,
                    format!(
                        "Reported size {} for {} does not match signed size {expected_size}.",
                        reply.size, cand.artifact.name
                    ),
                ));
            }
            let chunk = BASE64.decode(reply.content.as_bytes()).map_err(|err| {
                let _ = fs::remove_file(&part_path);
                AppError::new(
                    ErrorCode::Failed,
                    format!("Invalid base64 chunk for {}: {err}", cand.artifact.name),
                )
            })?;
            if chunk.len() as u64 != reply.length {
                let _ = fs::remove_file(&part_path);
                return Err(AppError::new(
                    ErrorCode::Failed,
                    format!("Inconsistent chunk length for {}.", cand.artifact.name),
                ));
            }
            if chunk.len() as u64 > req_len {
                let _ = fs::remove_file(&part_path);
                return Err(AppError::new(
                    ErrorCode::Failed,
                    format!(
                        "Chunk length {} exceeded requested {req_len} bytes for {}.",
                        chunk.len(),
                        cand.artifact.name
                    ),
                ));
            }
            if offset.saturating_add(chunk.len() as u64) > expected_size {
                let _ = fs::remove_file(&part_path);
                return Err(AppError::new(
                    ErrorCode::Failed,
                    format!(
                        "Download exceeded signed size ({expected_size} bytes) for {}.",
                        cand.artifact.name
                    ),
                ));
            }
            if !chunk.is_empty() {
                file.write_all(&chunk).map_err(|err| {
                    AppError::new(
                        ErrorCode::Failed,
                        format!("Could not write downloaded update: {err}"),
                    )
                })?;
                offset = offset.saturating_add(chunk.len() as u64);
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
                let _ = fs::remove_file(&part_path);
                return Err(AppError::new(
                    ErrorCode::Failed,
                    format!("Download ended early at {offset} bytes (expected {expected_size})."),
                ));
            }
            if chunk.is_empty() {
                return Err(AppError::new(
                    ErrorCode::Failed,
                    "Download stopped making progress.",
                ));
            }
            if chunks_seen >= max_chunks {
                let _ = fs::remove_file(&part_path);
                return Err(AppError::new(
                    ErrorCode::Failed,
                    format!(
                        "Download used too many tiny chunks ({chunks_seen}) for {}.",
                        cand.artifact.name
                    ),
                ));
            }
            let elapsed = started_at.elapsed();
            if elapsed >= PROGRESS_GRACE_WINDOW {
                let received = offset.saturating_sub(initial_offset);
                let min_bytes = (elapsed.as_secs_f64() * MIN_PROGRESS_BYTES_PER_SEC as f64) as u64;
                if received < min_bytes {
                    return Err(AppError::new(
                        ErrorCode::Timeout,
                        format!(
                            "Download rate fell below {} KiB/s.",
                            MIN_PROGRESS_BYTES_PER_SEC / 1024
                        ),
                    ));
                }
            }
        }

        file.sync_all().map_err(|err| {
            AppError::new(
                ErrorCode::Failed,
                format!("Could not flush downloaded update: {err}"),
            )
        })?;
        drop(file);

        // Stream-verify the complete `.part` file against signed size + SHA-256 on a blocking
        // thread before renaming into place.
        let meta_clone = cand.meta.clone();
        let art_name = cand.artifact.name.clone();
        let part_check = part_path.clone();
        let verify_res = tokio::task::spawn_blocking(move || {
            verify_artifact_file(&meta_clone, &art_name, &part_check)
        })
        .await
        .map_err(|err| {
            AppError::new(
                ErrorCode::Internal,
                format!("Artifact verification stopped unexpectedly: {err}"),
            )
        })?;

        if let Err(err) = verify_res {
            let _ = fs::remove_file(&part_path);
            let _ = fs::remove_file(&part_hash_marker);
            return Err(AppError::new(ErrorCode::Denied, err.message));
        }

        fs::rename(&part_path, &final_path).map_err(|err| {
            AppError::new(
                ErrorCode::Failed,
                format!("Could not finalize downloaded update: {err}"),
            )
        })?;
        let _ = fs::remove_file(&part_hash_marker);
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

    fn record_pending_install(&self, avail: &AvailableUpdateInfo) -> Result<(), AppError> {
        let mut prefs = self.prefs.lock().unwrap_or_else(|p| p.into_inner()).clone();
        prefs.pending_install = Some(PendingInstallRecord {
            channel: avail.channel,
            version: avail.version.clone(),
            build: avail.build.clone(),
            sequence: avail.sequence,
            notes: avail.notes.clone(),
            from_sequence: self.current_sequence,
            started_at: now_rfc3339_utc(),
        });
        save_prefs(&self.updates_dir.join("prefs.json"), &prefs)?;
        let mut guard = self.prefs.lock().unwrap_or_else(|p| p.into_inner());
        *guard = prefs;
        Ok(())
    }

    fn clear_pending_install(&self) {
        let mut prefs = self.prefs.lock().unwrap_or_else(|p| p.into_inner()).clone();
        if prefs.pending_install.take().is_some()
            && save_prefs(&self.updates_dir.join("prefs.json"), &prefs).is_ok()
        {
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
        let record = FailedUpdateRecord {
            channel,
            version: version.to_owned(),
            sequence,
            reason: reason.to_owned(),
            failed_at: now_rfc3339_utc(),
        };
        let mut prefs = self.prefs.lock().unwrap_or_else(|p| p.into_inner()).clone();
        prefs.pending_install = None;
        prefs.record_failure(record.clone());
        if save_prefs(&self.updates_dir.join("prefs.json"), &prefs).is_ok() {
            let mut guard = self.prefs.lock().unwrap_or_else(|p| p.into_inner());
            *guard = prefs;
        }
        record
    }

    fn clear_failed_sequence(&self, sequence: u64) {
        let mut prefs = self.prefs.lock().unwrap_or_else(|p| p.into_inner()).clone();
        if prefs.has_failed(sequence) {
            prefs.clear_failure(sequence);
            if save_prefs(&self.updates_dir.join("prefs.json"), &prefs).is_ok() {
                let mut guard = self.prefs.lock().unwrap_or_else(|p| p.into_inner());
                *guard = prefs;
            }
        }
    }

    /// Returns whether `sequence` already failed previously on this device.
    pub fn sequence_failed_before(&self, sequence: u64) -> bool {
        self.prefs
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .has_failed(sequence)
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

    /// Synchronous hook called when the last desktop window closes: if automatic updates are on
    /// and a verified update is ready (and has not failed before), stages it via copy-while-hashing
    /// and spawns the detached swapper in `verify_only` mode so the new build verifies startup and
    /// exits cleanly without popping a window open.
    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    pub fn try_spawn_desktop_close_install(&self) -> bool {
        let st = self.snapshot();
        let ready_or_checking_downloaded = st.phase == "ready"
            || (st.phase == "checking" && st.available.as_ref().is_some_and(|a| a.downloaded));
        if !st.auto_update
            || !ready_or_checking_downloaded
            || !st.can_self_install
            || self.current_sequence == 0
        {
            return false;
        }
        let Ok((avail, expected_size, expected_sha256, artifact_path)) =
            self.prepare_verified_install()
        else {
            return false;
        };
        if self.sequence_failed_before(avail.sequence) {
            return false;
        }
        self.spawn_desktop_install_inner(
            &avail,
            &artifact_path,
            &expected_sha256,
            expected_size,
            true,
        )
        .is_ok()
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

        if auto_triggered && self.sequence_failed_before(avail.sequence) {
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
                self.spawn_desktop_install_inner(
                    &avail,
                    &artifact_path,
                    &expected_sha256,
                    expected_size,
                    false,
                )?;
                Ok((self.snapshot(), true))
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
    ) -> Result<(), AppError> {
        let ready_marker = self
            .updates_dir
            .join(format!("started-{}.ok", avail.sequence));
        let rollback_marker = self
            .updates_dir
            .join(format!("rollback-{}.json", avail.sequence));
        let rollback_json = serde_json::to_string(&RollbackInfo {
            version: avail.version.clone(),
            sequence: avail.sequence,
            reason: "The new version did not start cleanly, so Legion kept the previous copy."
                .to_owned(),
        })
        .unwrap_or_else(|_| "{}".to_owned());
        let verified_dir = self.updates_dir.join("verified");

        match &self.platform {
            #[cfg(unix)]
            PlatformEnv::Desktop(DesktopInstall::LinuxAppImage { appimage_path }) => {
                self.record_pending_install(avail)?;
                self.update_state(|st| {
                    st.phase = "installing".to_owned();
                    st.failed_update = None;
                    st.last_error = None;
                    st.last_error_detail = None;
                });
                if let Err(err) = desktop::stage_and_spawn_linux_appimage(
                    artifact_path,
                    expected_sha256,
                    expected_size,
                    appimage_path,
                    &ready_marker,
                    &rollback_marker,
                    rollback_json,
                    DEFAULT_STARTUP_WAIT_SECS,
                    verify_only,
                ) {
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
                Ok(())
            }
            #[cfg(unix)]
            PlatformEnv::Desktop(DesktopInstall::MacosAppBundle {
                bundle_dir,
                executable_rel,
            }) => {
                self.record_pending_install(avail)?;
                self.update_state(|st| {
                    st.phase = "installing".to_owned();
                    st.failed_update = None;
                    st.last_error = None;
                    st.last_error_detail = None;
                });
                if let Err(err) = desktop::stage_and_spawn_macos_bundle(
                    artifact_path,
                    expected_sha256,
                    expected_size,
                    &verified_dir,
                    bundle_dir,
                    executable_rel,
                    &ready_marker,
                    &rollback_marker,
                    rollback_json,
                    DEFAULT_STARTUP_WAIT_SECS,
                    verify_only,
                ) {
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
                Ok(())
            }
            #[cfg(windows)]
            PlatformEnv::Desktop(DesktopInstall::WindowsNsis {
                install_dir,
                exe_path,
            }) => {
                self.record_pending_install(avail)?;
                self.update_state(|st| {
                    st.phase = "installing".to_owned();
                    st.failed_update = None;
                    st.last_error = None;
                    st.last_error_detail = None;
                });
                let backup_dir = self
                    .updates_dir
                    .join(format!("backup-{}", self.current_sequence));
                if let Err(err) = desktop::stage_and_spawn_windows_nsis(
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
                    DEFAULT_STARTUP_WAIT_SECS,
                    verify_only,
                ) {
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
                Ok(())
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

        if auto_triggered && self.sequence_failed_before(avail.sequence) {
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
                self.clear_pending_install();
                Ok(self.update_state(|st| {
                    st.phase = "ready".to_owned();
                    st.last_error = None;
                    st.last_error_detail = None;
                }))
            }
            "needs_permission" => {
                self.clear_pending_install();
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
            "Update check exceeded its overall time budget.",
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
                "Update check exceeded its overall time budget.",
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
    cancel_rx: &mut watch::Receiver<u64>,
) -> Result<Vec<u8>, AppError> {
    if max_bytes == 0 {
        return Err(AppError::new(
            ErrorCode::Denied,
            format!("Metadata budget exceeded while reading {name} from {node}."),
        ));
    }
    let mut out = Vec::new();
    let mut offset = 0u64;
    let mut chunks = 0usize;
    loop {
        let req_len = DOWNLOAD_CHUNK_BYTES.min((max_bytes as u64).saturating_sub(offset));
        let reply = run_cancellable(
            cancel_rx,
            deadline,
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

fn read_and_take_rollback(updates_dir: &Path) -> Option<(PathBuf, RollbackInfo)> {
    let entries = fs::read_dir(updates_dir).ok()?;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(s) = name.to_str() else { continue };
        if s.starts_with("rollback-") && s.ends_with(".json") {
            let path = entry.path();
            if let Ok(bytes) = fs::read(&path)
                && let Ok(info) = serde_json::from_slice::<RollbackInfo>(&bytes)
            {
                return Some((path, info));
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

fn now_rfc3339_utc() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
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
            }
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
                return Box::pin(async {
                    tokio::time::sleep(Duration::from_secs(60)).await;
                    Err(AppError::new(ErrorCode::Timeout, "stalled"))
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

        // .deb downloads and copies-while-hashing into private `<updates>/verified/` directory.
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
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
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

    #[tokio::test]
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

        // Give the background check a moment to enter the stalled call.
        tokio::time::sleep(Duration::from_millis(50)).await;

        let started = Instant::now();
        let st = mgr
            .check_with_client(&fast_fleet, &["fast".to_owned()], true, false)
            .await
            .unwrap();
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "manual check must not block behind stalled background check"
        );
        assert_eq!(st.phase, "ready", "{st:?}");
        // Background task also exits promptly when cancelled.
        let bg_res = tokio::time::timeout(Duration::from_secs(2), bg_handle)
            .await
            .expect("cancelled background check should exit promptly")
            .unwrap();
        assert!(bg_res.is_ok());
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
        assert_eq!(st2.phase, "up_to_date");
        assert!(st2.available.is_none());

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
        assert_eq!(st3.phase, "up_to_date");
        assert!(st3.available.is_none());

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
}
