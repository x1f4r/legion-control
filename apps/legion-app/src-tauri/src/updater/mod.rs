//! Fleet-sourced native app self-updater.
//!
//! Queries every reachable paired machine with `update.list`, picks the newest set that passes
//! [`legion_update::verify_candidate`] (trusted Ed25519 key, SHA-256 hashes, strictly newer
//! `sequence` than both the running build and the monotonic floor, matching OS/arch/install kind),
//! downloads the artifact in bounded-memory resumable chunks with `update.read`, verifies its
//! SHA-256 digest before renaming into place, and installs via the platform mechanism:
//!
//! - Android: `PackageInstaller` via [`android`] (`AppUpdaterPlugin`).
//! - Linux AppImage / macOS `.app`: detached atomic swap-and-rollback supervisor via [`desktop`].
//! - Windows NSIS: backup + detached silent installer and rollback supervisor via [`desktop`].
//! - Linux `.deb` / pacman: downloads and verifies the `.deb` without ever replacing itself,
//!   reporting the exact command to run.

pub mod android;
#[cfg(not(any(target_os = "android", target_os = "ios")))]
pub mod desktop;

use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use legion_link::Link;
use legion_update::{
    Arch, Artifact, CHECKSUMS_FILENAME, Channel, InstallKind, MANIFEST_FILENAME,
    MAX_MANIFEST_BYTES, MAX_SIG_BYTES, MAX_SUMS_BYTES, Os, Program, SIGNATURE_FILENAME,
    SequenceFloorState, SignerKind, TargetPlatform, TrustAnchors, VerifiedSetMeta,
    commit_sequence_floor, read_sequence_floor, select_verified_artifact, verify_artifact_file,
    verify_set_metadata,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tauri::ipc::Channel as IpcChannel;
use tempfile::NamedTempFile;
use tokio::sync::Mutex as AsyncMutex;

use crate::backend::{AppError, Backend, BoxFuture, ErrorCode};
#[cfg(not(any(target_os = "android", target_os = "ios")))]
use crate::updater::desktop::{DEFAULT_STARTUP_WAIT_SECS, DesktopInstall, detect_desktop_install};

/// Chunk size requested per `update.read` call (512 KiB).
const DOWNLOAD_CHUNK_BYTES: u64 = 512 * 1024;

/// Upper bound on a single downloaded app installer (512 MiB).
const MAX_DOWNLOAD_BYTES: u64 = 512 * 1024 * 1024;

/// Per-node timeout for `update.list` and metadata `update.read` calls.
const CHECK_CALL_TIMEOUT: Duration = Duration::from_secs(12);

/// Per-chunk timeout for artifact `update.read` calls.
const CHUNK_CALL_TIMEOUT: Duration = Duration::from_secs(30);

/// Build-time app semantic version from `Cargo.toml`.
pub const APP_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Build-time short revision embedded by `build.rs`.
pub const APP_BUILD_ID: &str = env!("LEGION_APP_BUILD_ID");

/// Parses the build-time update sequence emitted by `build.rs` (`0` for dev builds).
pub fn current_app_sequence() -> u64 {
    env!("LEGION_APP_SEQUENCE").parse::<u64>().unwrap_or(0)
}

/// Identifies the currently running app build and platform.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CurrentBuildInfo {
    /// Semantic version string (`Cargo.toml`).
    pub version: String,
    /// Short revision string embedded at build time.
    pub build: String,
    /// Strictly growing update sequence embedded at build time (`0` for local dev builds).
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
    /// Expected total artifact size in bytes (when known).
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
    /// Whether automatic updates (download when seen, install on next start or when idle) are on.
    pub auto_update: bool,
    /// Whether this installation can replace itself in place.
    pub can_self_install: bool,
    /// Plain explanation when in-place replacement is not supported on this install.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unsupported_reason: Option<String>,
    /// Exact shell command to run for package-managed installs (such as `.deb`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub manual_command: Option<String>,
    /// Newest verified update available, when one exists.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub available: Option<AvailableUpdateInfo>,
    /// Download progress while `phase == "downloading"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub progress: Option<DownloadProgress>,
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
    /// Android capability details when running on Android.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub android: Option<android::AndroidCapabilities>,
    /// Last user-facing or diagnostic error message.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
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
            cached_available: None,
        }
    }
}

#[derive(Clone, Debug)]
struct CandidateSource {
    channel: Channel,
    sequence: u64,
    nodes: Vec<String>,
    size_hint: Option<u64>,
}

struct VerifiedCandidate {
    meta: VerifiedSetMeta,
    artifact: Artifact,
    expected_sha256: String,
    sums_bytes: Vec<u8>,
    sig_text: String,
    manifest_bytes: Vec<u8>,
    source_node: String,
    size_hint: Option<u64>,
}

/// Shared managed state holding the app's [`UpdateManager`].
pub struct SharedUpdater(pub Arc<UpdateManager>);

/// Native app update coordinator persisted under `<app_data_dir>/updates/`.
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
    work_lock: AsyncMutex<()>,
}

impl UpdateManager {
    /// Creates the production update manager for this app using compiled-in trusted keys and
    /// detected platform/install kind.
    pub fn for_app(data_dir: impl AsRef<Path>) -> Self {
        Self::with_config(
            data_dir.as_ref().join("updates"),
            TrustAnchors::compiled_in(),
            current_os(),
            current_arch(),
            PlatformEnv::detect(),
            APP_VERSION.to_owned(),
            APP_BUILD_ID.to_owned(),
            current_app_sequence(),
        )
    }

    /// Creates an update manager with explicit trust anchors, platform, and build identity (used by
    /// unit tests).
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

        let can_self_install = platform.can_self_install();
        let unsupported_reason = platform.unsupported_reason();
        let mut initial_phase = if platform.target_platform(os, arch).is_none() {
            "unsupported".to_owned()
        } else {
            "up_to_date".to_owned()
        };

        // Rehydrate any already-downloaded verified update sitting on disk from a prior session.
        let mut available = None;
        let mut manual_command = None;
        if let Some(cached) = &prefs.cached_available
            && cached.sequence > current_sequence
            && cached.sequence > sequence_floor
            && let Some(target) = platform.target_platform(os, arch)
        {
            let set_dir = download_dir_for(&updates_dir, cached.channel, cached.sequence);
            let art_path = set_dir.join(&cached.artifact_name);
            if verify_downloaded_set(
                &set_dir,
                &cached.artifact_name,
                &anchors,
                target,
                current_sequence,
                &floor_path,
            )
            .is_ok()
            {
                let mut item = cached.clone();
                item.downloaded = true;
                manual_command = platform.manual_command_for(Some(&art_path));
                initial_phase = if manual_command.is_some() {
                    "manual".to_owned()
                } else if can_self_install {
                    "ready".to_owned()
                } else {
                    "available".to_owned()
                };
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
            available,
            progress: None,
            last_checked_at: prefs.last_checked_at.clone(),
            checked_nodes: Vec::new(),
            just_updated: prefs.just_updated.clone(),
            rolled_back: prefs.rolled_back.clone(),
            android: None,
            last_error: None,
        };

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
            work_lock: AsyncMutex::new(()),
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
        // Bound watcher list if a WebView reloads repeatedly.
        if guard.len() >= 8 {
            guard.remove(0);
        }
        guard.push(channel);
    }

    fn publish_state(&self, next: UpdateState) -> UpdateState {
        {
            let mut guard = self.state.lock().unwrap_or_else(|p| p.into_inner());
            *guard = next.clone();
        }
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
        let mut watchers = self.watchers.lock().unwrap_or_else(|p| p.into_inner());
        watchers.retain(|ch| ch.send(next.clone()).is_ok());
        next
    }

    /// Confirms startup after the UI paints its first frame.
    ///
    /// - Writes `started-<sequence>.ok` so any detached desktop swapper deletes `.prev`.
    /// - Writes `current-build.json` with the active version/sequence.
    /// - Advances the monotonic sequence floor to `current_sequence`.
    /// - Transitions a matching `pending_install` into a one-time `just_updated` notice, or
    ///   surfaces any rollback marker left by the desktop watchdog.
    pub fn on_app_ready(&self) {
        if fs::create_dir_all(&self.updates_dir).is_err() {
            return;
        }

        if self.current_sequence > 0 {
            let started_marker = self
                .updates_dir
                .join(format!("started-{}.ok", self.current_sequence));
            let _ = write_atomic(&started_marker, b"ok\n");
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
        let new_floor = if self.current_sequence > 0 {
            let _ = commit_sequence_floor(&floor_path, self.current_sequence);
            read_floor_value(&floor_path).max(self.current_sequence)
        } else {
            read_floor_value(&floor_path)
        };

        let mut prefs = self.prefs.lock().unwrap_or_else(|p| p.into_inner()).clone();
        let mut prefs_dirty = false;

        // Check for a desktop rollback marker left by the detached supervisor.
        if let Some((rb_path, rb)) = read_and_take_rollback(&self.updates_dir) {
            let _ = fs::remove_file(rb_path);
            if !prefs.failed_sequences.contains(&rb.sequence) {
                prefs.failed_sequences.push(rb.sequence);
            }
            prefs.pending_install = None;
            prefs.rolled_back = Some(rb);
            prefs_dirty = true;
        }

        // Check whether a pending update completed on this start.
        if let Some(pending) = prefs.pending_install.clone()
            && self.current_sequence > 0
            && pending.sequence == self.current_sequence
        {
            prefs.pending_install = None;
            prefs.rolled_back = None;
            prefs.cached_available = None;
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
        }

        if prefs_dirty {
            let _ = save_prefs(&self.updates_dir.join("prefs.json"), &prefs);
            let mut guard = self.prefs.lock().unwrap_or_else(|p| p.into_inner());
            *guard = prefs.clone();
        }

        let mut st = self.snapshot();
        st.sequence_floor = new_floor.max(self.current_sequence);
        st.just_updated = prefs.just_updated;
        st.rolled_back = prefs.rolled_back;
        if let Some(avail) = &st.available
            && avail.sequence <= st.sequence_floor
        {
            st.available = None;
            st.manual_command = None;
            if st.phase != "unsupported" {
                st.phase = "up_to_date".to_owned();
            }
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
    /// selected artifact immediately.
    pub async fn check_with_backend(
        &self,
        backend: &dyn Backend,
        force_download: bool,
    ) -> Result<UpdateState, AppError> {
        let Some((link, nodes)) = backend.update_sources()? else {
            return Ok(self.snapshot());
        };
        let client = LinkUpdateClient { link };
        self.check_with_client(&client, &nodes, force_download)
            .await
    }

    /// Core check + optional resumable download logic using `client` across `nodes`.
    pub async fn check_with_client(
        &self,
        client: &dyn UpdateSourceClient,
        nodes: &[String],
        force_download: bool,
    ) -> Result<UpdateState, AppError> {
        let _guard = self.work_lock.lock().await;

        let Some(target) = self.platform.target_platform(self.os, self.arch) else {
            return Ok(self.update_state(|st| {
                st.phase = "unsupported".to_owned();
                st.last_checked_at = Some(now_rfc3339_utc());
            }));
        };

        let floor_path = self.updates_dir.join("floor.json");
        let floor = read_floor_value(&floor_path).max(self.current_sequence);
        let prev_state = self.snapshot();
        self.update_state(|st| {
            st.phase = "checking".to_owned();
            st.sequence_floor = floor;
            st.last_error = None;
        });

        let checked_at = now_rfc3339_utc();
        let mut by_key: BTreeMap<(Channel, u64), CandidateSource> = BTreeMap::new();
        let mut answered_nodes = Vec::new();
        let mut last_query_err: Option<String> = None;

        for node in nodes {
            match client.list_sets(node).await {
                Ok(listed) => {
                    answered_nodes.push(node.clone());
                    for set in listed.sets {
                        if set.sequence <= self.current_sequence || set.sequence <= floor {
                            continue;
                        }
                        if set.channel == Channel::Preview && !self.anchors.has_preview() {
                            continue;
                        }
                        let matching_size = set
                            .artifacts
                            .iter()
                            .find(|a| {
                                a.program == target.program
                                    && a.os == target.os
                                    && a.kind == target.kind
                                    && (a.arch == target.arch || a.arch == Arch::Universal)
                            })
                            .and_then(|a| a.size);
                        let entry =
                            by_key
                                .entry((set.channel, set.sequence))
                                .or_insert_with(|| CandidateSource {
                                    channel: set.channel,
                                    sequence: set.sequence,
                                    nodes: Vec::new(),
                                    size_hint: matching_size,
                                });
                        if !entry.nodes.contains(node) {
                            entry.nodes.push(node.clone());
                        }
                        if entry.size_hint.is_none() {
                            entry.size_hint = matching_size;
                        }
                    }
                }
                Err(err) => {
                    if err.code != ErrorCode::UnknownTool {
                        last_query_err = Some(err.message);
                    }
                }
            }
        }

        // Order candidates newest sequence first (preferring stable on an exact tie).
        let mut candidates: Vec<CandidateSource> = by_key.into_values().collect();
        candidates.sort_by(|a, b| {
            b.sequence
                .cmp(&a.sequence)
                .then_with(|| a.channel.cmp(&b.channel))
        });

        let mut chosen: Option<VerifiedCandidate> = None;
        let mut refusal_note: Option<String> = None;

        'outer: for cand in candidates {
            for node in &cand.nodes {
                match self
                    .fetch_and_verify_candidate(client, node, &cand, target, &floor_path)
                    .await
                {
                    Ok(verified) => {
                        chosen = Some(verified);
                        break 'outer;
                    }
                    Err(err) => {
                        refusal_note = Some(err.message);
                    }
                }
            }
        }

        let auto_update = self
            .prefs
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .auto_update;

        let Some(chosen) = chosen else {
            self.record_last_checked(&checked_at, None);
            let next_phase = if answered_nodes.is_empty() && !nodes.is_empty() {
                // Keep any previously downloaded verified update ready if machines are merely offline.
                if let Some(existing) = prev_state.available.as_ref()
                    && existing.downloaded
                    && existing.sequence > floor
                {
                    return Ok(self.update_state(|st| {
                        st.phase = prev_state.phase.clone();
                        st.last_checked_at = Some(checked_at);
                        st.checked_nodes = answered_nodes;
                        st.last_error = last_query_err;
                    }));
                }
                "up_to_date".to_owned()
            } else {
                "up_to_date".to_owned()
            };
            let had_no_answers = answered_nodes.is_empty() && !nodes.is_empty();
            return Ok(self.update_state(|st| {
                st.phase = next_phase;
                st.available = None;
                st.manual_command = None;
                st.progress = None;
                st.last_checked_at = Some(checked_at);
                st.checked_nodes = answered_nodes;
                st.last_error = refusal_note.or(if had_no_answers { last_query_err } else { None });
            }));
        };

        let set_dir = download_dir_for(
            &self.updates_dir,
            chosen.meta.manifest.channel,
            chosen.meta.manifest.sequence,
        );
        let art_path = set_dir.join(&chosen.artifact.name);
        let already_downloaded = verify_downloaded_set(
            &set_dir,
            &chosen.artifact.name,
            &self.anchors,
            target,
            self.current_sequence,
            &floor_path,
        )
        .is_ok();

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
            artifact_size: chosen.size_hint,
            downloaded: already_downloaded,
        };

        if already_downloaded {
            self.record_last_checked(&checked_at, Some(avail.clone()));
            let manual_cmd = self.platform.manual_command_for(Some(&art_path));
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
                st.progress = None;
                st.last_checked_at = Some(checked_at);
                st.checked_nodes = answered_nodes;
                st.last_error = None;
            }));
        }

        let should_download = force_download || auto_update;
        if !should_download {
            self.record_last_checked(&checked_at, None);
            return Ok(self.update_state(|st| {
                st.phase = "available".to_owned();
                st.available = Some(avail);
                st.manual_command = None;
                st.progress = None;
                st.last_checked_at = Some(checked_at);
                st.checked_nodes = answered_nodes;
                st.last_error = None;
            }));
        }

        // Download and verify the artifact in bounded-memory resumable chunks.
        self.update_state(|st| {
            st.phase = "downloading".to_owned();
            st.available = Some(avail.clone());
            st.progress = Some(DownloadProgress {
                downloaded_bytes: 0,
                total_bytes: avail.artifact_size,
            });
            st.last_checked_at = Some(checked_at.clone());
            st.checked_nodes = answered_nodes.clone();
            st.last_error = None;
        });

        match self.download_verified_candidate(client, &chosen).await {
            Ok(final_size) => {
                avail.downloaded = true;
                avail.artifact_size = Some(final_size);
                self.record_last_checked(&checked_at, Some(avail.clone()));
                cleanup_other_downloads(
                    &self.updates_dir,
                    chosen.meta.manifest.channel,
                    chosen.meta.manifest.sequence,
                );
                let manual_cmd = self.platform.manual_command_for(Some(&art_path));
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
                    st.progress = None;
                    st.last_error = None;
                }))
            }
            Err(err) => {
                self.record_last_checked(&checked_at, None);
                Ok(self.update_state(|st| {
                    st.phase = "error".to_owned();
                    st.available = Some(avail);
                    st.progress = None;
                    st.last_error = Some(err.message);
                }))
            }
        }
    }

    async fn fetch_and_verify_candidate(
        &self,
        client: &dyn UpdateSourceClient,
        node: &str,
        cand: &CandidateSource,
        target: TargetPlatform,
        floor_path: &Path,
    ) -> Result<VerifiedCandidate, AppError> {
        let sums_bytes = read_small_file(
            client,
            node,
            cand.channel,
            cand.sequence,
            CHECKSUMS_FILENAME,
            MAX_SUMS_BYTES,
        )
        .await?;
        let sig_bytes = read_small_file(
            client,
            node,
            cand.channel,
            cand.sequence,
            SIGNATURE_FILENAME,
            MAX_SIG_BYTES,
        )
        .await?;
        let sig_text = String::from_utf8(sig_bytes).map_err(|err| {
            AppError::new(
                ErrorCode::Denied,
                format!("{SIGNATURE_FILENAME} from {node} is not valid UTF-8: {err}"),
            )
        })?;
        // Verify signature before reading update.json so untrusted sets are refused immediately.
        self.anchors
            .verify_for_channel(cand.channel, &sums_bytes, &sig_text)
            .map_err(|e| AppError::new(ErrorCode::Denied, e.message))?;

        let manifest_bytes = read_small_file(
            client,
            node,
            cand.channel,
            cand.sequence,
            MANIFEST_FILENAME,
            MAX_MANIFEST_BYTES,
        )
        .await?;

        let meta = verify_set_metadata(&sums_bytes, &sig_text, &manifest_bytes, &self.anchors)
            .map_err(|e| AppError::new(e.code.into(), e.message))?;
        let artifact = select_verified_artifact(&meta, self.current_sequence, floor_path, target)
            .map_err(|e| AppError::new(e.code.into(), e.message))?
            .clone();

        if meta.manifest.channel != cand.channel || meta.manifest.sequence != cand.sequence {
            return Err(AppError::new(
                ErrorCode::Denied,
                format!(
                    "Update set from {node} claims {}/{} instead of {}/{}.",
                    meta.manifest.channel, meta.manifest.sequence, cand.channel, cand.sequence
                ),
            ));
        }

        let expected_sha256 = meta
            .sums
            .get(&artifact.name)
            .ok_or_else(|| {
                AppError::new(
                    ErrorCode::Denied,
                    format!(
                        "Artifact {} is missing from {CHECKSUMS_FILENAME}.",
                        artifact.name
                    ),
                )
            })?
            .to_owned();

        Ok(VerifiedCandidate {
            meta,
            artifact,
            expected_sha256,
            sums_bytes,
            sig_text,
            manifest_bytes,
            source_node: node.to_owned(),
            size_hint: cand.size_hint,
        })
    }

    async fn download_verified_candidate(
        &self,
        client: &dyn UpdateSourceClient,
        cand: &VerifiedCandidate,
    ) -> Result<u64, AppError> {
        let set_dir = download_dir_for(
            &self.updates_dir,
            cand.meta.manifest.channel,
            cand.meta.manifest.sequence,
        );
        fs::create_dir_all(&set_dir).map_err(|err| {
            AppError::new(
                ErrorCode::Failed,
                format!(
                    "Could not create download folder {}: {err}",
                    set_dir.display()
                ),
            )
        })?;

        write_atomic(&set_dir.join(CHECKSUMS_FILENAME), &cand.sums_bytes)?;
        write_atomic(&set_dir.join(SIGNATURE_FILENAME), cand.sig_text.as_bytes())?;
        write_atomic(&set_dir.join(MANIFEST_FILENAME), &cand.manifest_bytes)?;

        let final_path = set_dir.join(&cand.artifact.name);
        let part_path = set_dir.join(format!("{}.part", cand.artifact.name));
        let part_hash_marker = set_dir.join(format!("{}.part.sha256", cand.artifact.name));

        // Reset any leftover `.part` file if it belongs to a different expected SHA-256.
        let mut offset = 0u64;
        if part_path.is_file() {
            let prev_hash = fs::read_to_string(&part_hash_marker).unwrap_or_default();
            if prev_hash.trim() == cand.expected_sha256 {
                offset = fs::metadata(&part_path).map_or(0, |m| m.len());
                if cand.size_hint.is_some_and(|total| offset > total) || offset > MAX_DOWNLOAD_BYTES
                {
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

        let mut total_size = cand.size_hint;
        if offset > 0 {
            self.update_state(|st| {
                st.progress = Some(DownloadProgress {
                    downloaded_bytes: offset,
                    total_bytes: total_size,
                });
            });
        }

        loop {
            let reply = client
                .read_chunk(
                    &cand.source_node,
                    cand.meta.manifest.channel,
                    cand.meta.manifest.sequence,
                    &cand.artifact.name,
                    offset,
                    DOWNLOAD_CHUNK_BYTES,
                )
                .await?;
            if reply.size > MAX_DOWNLOAD_BYTES {
                let _ = fs::remove_file(&part_path);
                return Err(AppError::new(
                    ErrorCode::Failed,
                    format!(
                        "Update artifact {} exceeds the {MAX_DOWNLOAD_BYTES}-byte limit.",
                        cand.artifact.name
                    ),
                ));
            }
            if offset > reply.size {
                // Source file shrank; restart cleanly from 0 on next attempt.
                let _ = fs::remove_file(&part_path);
                return Err(AppError::new(
                    ErrorCode::Failed,
                    "Partial download offset exceeded source file size; please retry.",
                ));
            }
            total_size = Some(reply.size);
            let chunk = BASE64.decode(reply.content.as_bytes()).map_err(|err| {
                AppError::new(
                    ErrorCode::Failed,
                    format!("Invalid base64 chunk for {}: {err}", cand.artifact.name),
                )
            })?;
            if chunk.len() as u64 != reply.length {
                return Err(AppError::new(
                    ErrorCode::Failed,
                    format!("Inconsistent chunk length for {}.", cand.artifact.name),
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
                        total_bytes: total_size,
                    });
                });
            }
            if reply.eof {
                break;
            }
            if chunk.is_empty() {
                return Err(AppError::new(
                    ErrorCode::Failed,
                    format!(
                        "Download from {} stopped making progress.",
                        cand.source_node
                    ),
                ));
            }
        }

        file.sync_all().map_err(|err| {
            AppError::new(
                ErrorCode::Failed,
                format!("Could not flush downloaded update: {err}"),
            )
        })?;
        drop(file);

        // Stream-verify the complete `.part` file against the signed SHA-256 before moving into place.
        if let Err(err) = verify_artifact_file(&cand.meta, &cand.artifact.name, &part_path) {
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

    /// Returns whether `sequence` already rolled back previously (used to prevent auto-install
    /// retry loops while still allowing explicit manual retry).
    pub fn sequence_failed_before(&self, sequence: u64) -> bool {
        self.prefs
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .failed_sequences
            .contains(&sequence)
    }

    /// Re-verifies the currently ready downloaded update on disk and returns its verified info and
    /// artifact path.
    pub fn prepare_verified_install(&self) -> Result<(AvailableUpdateInfo, PathBuf), AppError> {
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
        let verified_art = verify_downloaded_set(
            &set_dir,
            &avail.artifact_name,
            &self.anchors,
            target,
            self.current_sequence,
            &floor_path,
        )?;
        Ok((avail, verified_art))
    }

    /// Installs the verified update on desktop hosts and returns `true` when the caller should exit
    /// so the detached swapper can replace the running app and relaunch it.
    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    pub async fn install_desktop(
        &self,
        backend: &dyn Backend,
        auto_triggered: bool,
    ) -> Result<(UpdateState, bool), AppError> {
        // If an update is available but not yet downloaded (auto_update off), download first.
        if let Some(avail) = self.snapshot().available
            && !avail.downloaded
        {
            if auto_triggered {
                return Ok((self.snapshot(), false));
            }
            let after = self.check_with_backend(backend, true).await?;
            if after.phase != "ready" {
                return Ok((after, false));
            }
        }

        let _guard = self.work_lock.lock().await;
        let (avail, artifact_path) = self.prepare_verified_install()?;
        if auto_triggered && self.sequence_failed_before(avail.sequence) {
            return Ok((self.snapshot(), false));
        }

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

        match &self.platform {
            PlatformEnv::Desktop(DesktopInstall::LinuxDeb { .. })
            | PlatformEnv::Desktop(DesktopInstall::LinuxPacman { .. }) => {
                let cmd = self.platform.manual_command_for(Some(&artifact_path));
                let next = self.update_state(|st| {
                    st.phase = "manual".to_owned();
                    st.manual_command = cmd;
                });
                Ok((next, false))
            }
            PlatformEnv::Desktop(DesktopInstall::Standalone { reason, .. }) => {
                Err(AppError::new(ErrorCode::Unsupported, reason.clone()))
            }
            #[cfg(unix)]
            PlatformEnv::Desktop(DesktopInstall::LinuxAppImage { appimage_path }) => {
                self.record_pending_install(&avail)?;
                self.update_state(|st| {
                    st.phase = "installing".to_owned();
                    st.last_error = None;
                });
                if let Err(err) = desktop::stage_and_spawn_linux_appimage(
                    &artifact_path,
                    appimage_path,
                    &ready_marker,
                    &rollback_marker,
                    rollback_json,
                    DEFAULT_STARTUP_WAIT_SECS,
                ) {
                    self.clear_pending_install();
                    self.update_state(|st| {
                        st.phase = "error".to_owned();
                        st.last_error = Some(err.message.clone());
                    });
                    return Err(err);
                }
                Ok((self.snapshot(), true))
            }
            #[cfg(unix)]
            PlatformEnv::Desktop(DesktopInstall::MacosAppBundle {
                bundle_dir,
                executable_rel,
            }) => {
                self.record_pending_install(&avail)?;
                self.update_state(|st| {
                    st.phase = "installing".to_owned();
                    st.last_error = None;
                });
                if let Err(err) = desktop::stage_and_spawn_macos_bundle(
                    &artifact_path,
                    bundle_dir,
                    executable_rel,
                    &ready_marker,
                    &rollback_marker,
                    rollback_json,
                    DEFAULT_STARTUP_WAIT_SECS,
                ) {
                    self.clear_pending_install();
                    self.update_state(|st| {
                        st.phase = "error".to_owned();
                        st.last_error = Some(err.message.clone());
                    });
                    return Err(err);
                }
                Ok((self.snapshot(), true))
            }
            #[cfg(windows)]
            PlatformEnv::Desktop(DesktopInstall::WindowsNsis {
                install_dir,
                exe_path,
            }) => {
                self.record_pending_install(&avail)?;
                self.update_state(|st| {
                    st.phase = "installing".to_owned();
                    st.last_error = None;
                });
                let backup_dir = self
                    .updates_dir
                    .join(format!("backup-{}", self.current_sequence));
                if let Err(err) = desktop::stage_and_spawn_windows_nsis(
                    &artifact_path,
                    install_dir,
                    exe_path,
                    &backup_dir,
                    &ready_marker,
                    &rollback_marker,
                    rollback_json,
                    DEFAULT_STARTUP_WAIT_SECS,
                ) {
                    self.clear_pending_install();
                    self.update_state(|st| {
                        st.phase = "error".to_owned();
                        st.last_error = Some(err.message.clone());
                    });
                    return Err(err);
                }
                Ok((self.snapshot(), true))
            }
            _ => Err(AppError::new(
                ErrorCode::Unsupported,
                "In-place installation is not available for this build.",
            )),
        }
    }

    /// Installs the verified APK on Android via `AppUpdaterPlugin` (`PackageInstaller`).
    #[cfg(target_os = "android")]
    pub async fn install_android(
        &self,
        backend: &dyn Backend,
        plugin: &android::AndroidUpdaterHandle,
        auto_triggered: bool,
    ) -> Result<UpdateState, AppError> {
        if let Some(avail) = self.snapshot().available
            && !avail.downloaded
        {
            if auto_triggered {
                return Ok(self.snapshot());
            }
            let after = self.check_with_backend(backend, true).await?;
            if after.phase != "ready" {
                return Ok(after);
            }
        }

        let _guard = self.work_lock.lock().await;
        let (avail, apk_path) = self.prepare_verified_install()?;
        if auto_triggered && self.sequence_failed_before(avail.sequence) {
            return Ok(self.snapshot());
        }

        let mut caps = android::capabilities(plugin).await?;
        self.set_android_capabilities(caps.clone());

        // Never pop an unexpected OS permission or install dialog during background auto-install.
        if auto_triggered && !caps.supports_silent_self_update {
            return Ok(self.snapshot());
        }

        if !caps.can_request_package_installs {
            caps = android::request_install_permission(plugin).await?;
            self.set_android_capabilities(caps.clone());
            if !caps.can_request_package_installs {
                return Ok(self.update_state(|st| {
                    st.phase = "needs_permission".to_owned();
                    st.last_error = Some(
                        "Allow Legion to install updates in Android settings, then tap Update now again."
                            .to_owned(),
                    );
                }));
            }
        }

        self.record_pending_install(&avail)?;
        self.update_state(|st| {
            st.phase = "installing".to_owned();
            st.last_error = None;
        });

        let outcome = match android::install_apk(plugin, &apk_path).await {
            Ok(o) => o,
            Err(err) => {
                self.clear_pending_install();
                return Ok(self.update_state(|st| {
                    st.phase = "error".to_owned();
                    st.last_error = Some(err.message);
                }));
            }
        };

        match outcome.status.as_str() {
            "installed" => Ok(self.snapshot()),
            "cancelled" => {
                self.clear_pending_install();
                Ok(self.update_state(|st| {
                    st.phase = "ready".to_owned();
                    st.last_error = None;
                }))
            }
            "needs_permission" => {
                self.clear_pending_install();
                Ok(self.update_state(|st| {
                    st.phase = "needs_permission".to_owned();
                    st.last_error = Some(outcome.message);
                }))
            }
            _ => {
                self.clear_pending_install();
                Ok(self.update_state(|st| {
                    st.phase = "error".to_owned();
                    st.last_error = Some(if outcome.message.is_empty() {
                        "Android could not install the update.".to_owned()
                    } else {
                        outcome.message
                    });
                }))
            }
        }
    }
}

async fn read_small_file(
    client: &dyn UpdateSourceClient,
    node: &str,
    channel: Channel,
    sequence: u64,
    name: &str,
    max_bytes: usize,
) -> Result<Vec<u8>, AppError> {
    let mut out = Vec::new();
    let mut offset = 0u64;
    loop {
        let reply = client
            .read_chunk(
                node,
                channel,
                sequence,
                name,
                offset,
                DOWNLOAD_CHUNK_BYTES.min(max_bytes as u64),
            )
            .await?;
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
        if out.len().saturating_add(chunk.len()) > max_bytes {
            return Err(AppError::new(
                ErrorCode::Denied,
                format!("{name} from {node} exceeds the {max_bytes}-byte limit."),
            ));
        }
        out.extend_from_slice(&chunk);
        offset = offset.saturating_add(chunk.len() as u64);
        if reply.eof {
            break;
        }
        if chunk.is_empty() {
            return Err(AppError::new(
                ErrorCode::Failed,
                format!("Reading {name} from {node} stopped making progress."),
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

fn verify_downloaded_set(
    set_dir: &Path,
    expected_artifact_name: &str,
    anchors: &TrustAnchors,
    target: TargetPlatform,
    running_sequence: u64,
    floor_path: &Path,
) -> Result<PathBuf, AppError> {
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
    verify_artifact_file(&meta, &artifact.name, &artifact_path)
        .map_err(|e| AppError::new(e.code.into(), e.message))?;
    Ok(artifact_path)
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

fn download_dir_for(updates_dir: &Path, channel: Channel, sequence: u64) -> PathBuf {
    updates_dir
        .join("downloads")
        .join(format!("{}-{sequence}", channel.as_str()))
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
    fs::create_dir_all(parent).map_err(|err| {
        AppError::new(
            ErrorCode::Failed,
            format!("Could not create directory {}: {err}", parent.display()),
        )
    })?;
    let mut tmp = NamedTempFile::new_in(parent).map_err(|err| {
        AppError::new(
            ErrorCode::Failed,
            format!("Could not create temp file in {}: {err}", parent.display()),
        )
    })?;
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

    struct FakeSet {
        channel: Channel,
        sequence: u64,
        artifacts: Vec<NodeListedArtifact>,
        files: HashMap<String, Vec<u8>>,
    }

    struct FakeFleet {
        nodes: HashMap<String, Vec<FakeSet>>,
        chunk_cap: usize,
        fail_after_reads: Option<usize>,
        reads: AtomicUsize,
    }

    impl FakeFleet {
        fn new() -> Self {
            Self {
                nodes: HashMap::new(),
                chunk_cap: 16 * 1024,
                fail_after_reads: None,
                reads: AtomicUsize::new(0),
            }
        }

        fn add_set(&mut self, node: &str, set: FakeSet) {
            self.nodes.entry(node.to_owned()).or_default().push(set);
        }
    }

    impl UpdateSourceClient for FakeFleet {
        fn list_sets(&self, node: &str) -> BoxFuture<'_, Result<NodeUpdateList, AppError>> {
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
                    let take = (length as usize)
                        .min(self.chunk_cap)
                        .min(bytes.len().saturating_sub(start));
                    let slice = &bytes[start..start + take];
                    UpdateReadChunk {
                        size,
                        offset,
                        length: slice.len() as u64,
                        content: BASE64.encode(slice),
                        eof: start + take >= bytes.len(),
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

        let payload = vec![0x7Au8; 40 * 1024]; // 40 KiB -> multiple 16 KiB chunks
        let mut fleet = FakeFleet::new();
        // tower has older seq 202611250201; pi has newer seq 202611250202.
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
        // Disconnect on the 5th read (midway through downloading payload after sums/sig/manifest/chunk1).
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

        // First check picks pi's newer seq 202611250202, downloads chunk 1, then fails mid-download.
        let st1 = mgr.check_with_client(&fleet, &nodes, false).await.unwrap();
        assert_eq!(st1.phase, "error", "{st1:?}");
        let part = download_dir_for(&updates_dir, Channel::Preview, 202611250202)
            .join("Legion.AppImage.part");
        assert_eq!(fs::metadata(&part).unwrap().len(), 16 * 1024);

        // Second check resumes from 16 KiB, verifies SHA-256, and marks ready!
        let st2 = mgr.check_with_client(&fleet, &nodes, false).await.unwrap();
        assert_eq!(st2.phase, "ready", "{st2:?}");
        let avail = st2.available.clone().unwrap();
        assert_eq!(avail.sequence, 202611250202);
        assert_eq!(avail.source_node, "pi");
        assert!(avail.downloaded);
        assert!(!part.exists());

        // Simulate install record + restart at seq 202611250202 -> on_app_ready sets just_updated & advances floor.
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

        let dismissed = mgr_after.dismiss_notice().unwrap();
        assert!(dismissed.just_updated.is_none());
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

        // 1. Preview set on a release-only build -> refused (stays up_to_date).
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
            .check_with_client(&f1, &nodes, false)
            .await
            .unwrap();
        assert_eq!(st1.phase, "up_to_date");
        assert!(st1.available.is_none());

        // 2. Unknown signing key -> refused.
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
            .check_with_client(&f2, &nodes, false)
            .await
            .unwrap();
        assert_eq!(st2.phase, "up_to_date");
        assert!(st2.available.is_none());

        // 3. Tampered update.json -> refused.
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
            .check_with_client(&f3, &nodes, false)
            .await
            .unwrap();
        assert_eq!(st3.phase, "up_to_date");
        assert!(st3.available.is_none());

        // 4. Tampered artifact bytes -> SHA-256 mismatch refused, `.part` deleted, not ready!
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
            .check_with_client(&f4, &nodes, false)
            .await
            .unwrap();
        assert_eq!(st4.phase, "error", "{st4:?}");
        assert!(!st4.available.unwrap().downloaded);
        assert!(
            !download_dir_for(&dir.path().join("m4"), Channel::Stable, 202611250210)
                .join("Legion.apk")
                .exists()
        );

        // 5. Older/equal sequence & wrong platform -> refused.
        let mut f5 = FakeFleet::new();
        f5.add_set(
            "tower",
            build_fake_set(
                Channel::Stable,
                "0.1.0",
                202611250200,
                "Legion.apk",
                Os::Android,
                Arch::Universal,
                InstallKind::Apk,
                b"equal",
                &rel_sk,
            ),
        );
        f5.add_set(
            "tower",
            build_fake_set(
                Channel::Stable,
                "0.2.0",
                202611250210,
                "Legion.AppImage",
                Os::Linux,
                Arch::X86_64,
                InstallKind::AppImage,
                b"wrong-os",
                &rel_sk,
            ),
        );
        let st5 = make_mgr("m5")
            .check_with_client(&f5, &nodes, false)
            .await
            .unwrap();
        assert_eq!(st5.phase, "up_to_date");
        assert!(st5.available.is_none());
    }

    #[tokio::test]
    async fn linux_deb_downloads_verifies_and_reports_manual_apt_command() {
        let dir = tempfile::tempdir().unwrap();
        let updates_dir = dir.path().join("updates");
        let (rel_sk, rel_pk) = keypair(0x81);
        let anchors = TrustAnchors::with_keys(rel_pk, None);
        let mut fleet = FakeFleet::new();
        fleet.add_set(
            "tower",
            build_fake_set(
                Channel::Stable,
                "0.2.0",
                202611250220,
                "legion-app_0.2.0_amd64.deb",
                Os::Linux,
                Arch::X86_64,
                InstallKind::Deb,
                b"fake-deb-package",
                &rel_sk,
            ),
        );

        let mgr = UpdateManager::with_config(
            updates_dir,
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
            .check_with_client(&fleet, &["tower".to_owned()], false)
            .await
            .unwrap();
        assert_eq!(st.phase, "manual", "{st:?}");
        assert!(!st.can_self_install);
        let cmd = st.manual_command.unwrap();
        assert!(cmd.starts_with("sudo apt-get install "), "{cmd}");
        assert!(cmd.contains("legion-app_0.2.0_amd64.deb"), "{cmd}");
    }
}
