//! Owner-only local control IPC and atomic per-folder status storage for `legion sync run`.
//!
//! Layout under `<state_dir>/sync/`:
//! - `runner.lock` — exclusive kernel lock held by the single running `legion sync run` instance.
//! - `runner.pid` — atomic JSON record of `{ pid, started_at, node }`.
//! - `control.token` — 32-byte random hex token (`0600` on Unix) required on every control frame.
//! - `control.sock` — Unix domain socket (`0600` on Unix; when `state_dir` is deeper than
//!   `SUN_LEN`, `control.sock.ref` points to an owner-only `0700` socket directory). On Windows,
//!   a named pipe `\\.\pipe\legion-sync-<hash16>` authenticated by `control.token`.
//! - `indexes/<folder_id>.redb` — `RedbIndexStore` database per folder.
//! - `status/<folder_id>.json` — atomically written [`PersistedFolderStatus`] read directly by
//!   `sync.status` without contacting the runner.

use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use legion_core::Paths;
use legion_core::config::write_atomically;
use legion_core::proto::{ErrorBody, ErrorCode};
use legion_os::lock::{FileLock, LockMode};
use legion_sync::{
    BinEntry, ConflictChoice, ConflictInfo, DeletionBrakeDecision, FolderStatus, HandshakeHello,
    InboundDeletionBrakeDecision, MemberState, PeerHandshake, ProblemKind, RecordsPage,
    RestoreStrategy,
};
use legion_xfer::Hash;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader as TokioBufReader};

/// Maximum length of a single JSON control message line (32 MiB — accommodates a 4,096-record page).
pub const MAX_CONTROL_LINE_BYTES: usize = 32 * 1024 * 1024;

/// Default timeout for a local control IPC round-trip.
pub const CONTROL_IPC_TIMEOUT: Duration = Duration::from_secs(30);

/// Longer timeout when `sync.now wait=true` waits for an active pull pass.
pub const CONTROL_SYNC_NOW_TIMEOUT: Duration = Duration::from_secs(120);

/// Maximum Unix socket path length we bind directly inside `<state_dir>/sync/` before falling back
/// to an owner-only short socket directory referenced by `control.sock.ref`.
#[cfg(unix)]
pub const MAX_DIRECT_UNIX_SOCK_LEN: usize = 96;

/// Directory holding all sync runner runtime state (`<state_dir>/sync`).
pub fn sync_state_dir(paths: &Paths) -> PathBuf {
    paths.state_dir().join("sync")
}

/// Directory holding per-folder `redb` indexes (`<state_dir>/sync/indexes`).
pub fn sync_indexes_dir(paths: &Paths) -> PathBuf {
    sync_state_dir(paths).join("indexes")
}

/// Path to one folder's `redb` index file.
pub fn folder_index_path(paths: &Paths, folder_id: &str) -> PathBuf {
    let hash = blake3::hash(folder_id.as_bytes()).to_hex();
    sync_indexes_dir(paths).join(format!("{}.redb", &hash[..24]))
}

/// Directory holding atomic per-folder status JSON files (`<state_dir>/sync/status`).
pub fn sync_status_dir(paths: &Paths) -> PathBuf {
    sync_state_dir(paths).join("status")
}

/// Path to one folder's atomic status JSON file.
pub fn folder_status_path(paths: &Paths, folder_id: &str) -> PathBuf {
    let hash = blake3::hash(folder_id.as_bytes()).to_hex();
    sync_status_dir(paths).join(format!("{}.json", &hash[..24]))
}

/// Path to the runner's exclusive single-instance lock file.
pub fn runner_lock_path(paths: &Paths) -> PathBuf {
    sync_state_dir(paths).join("runner.lock")
}

/// Path to the runner's atomic PID metadata file.
pub fn runner_pid_path(paths: &Paths) -> PathBuf {
    sync_state_dir(paths).join("runner.pid")
}

/// Path to the owner-only control auth token file.
pub fn control_token_path(paths: &Paths) -> PathBuf {
    sync_state_dir(paths).join("control.token")
}

/// Direct Unix socket path under `<state_dir>/sync/control.sock`.
pub fn control_sock_path(paths: &Paths) -> PathBuf {
    sync_state_dir(paths).join("control.sock")
}

/// Reference file used on Unix when `control_sock_path` exceeds `SUN_LEN`.
pub fn control_sock_ref_path(paths: &Paths) -> PathBuf {
    sync_state_dir(paths).join("control.sock.ref")
}

/// Deterministic Windows named pipe path derived from `paths.state_dir()`.
pub fn windows_pipe_name(paths: &Paths) -> String {
    let canonical = std::fs::canonicalize(paths.state_dir())
        .unwrap_or_else(|_| paths.state_dir().to_path_buf());
    let hash = blake3::hash(canonical.to_string_lossy().as_bytes()).to_hex();
    format!(r"\\.\pipe\legion-sync-{}", &hash[..24])
}

/// Ensures `dir` exists with `0700` owner-only permissions on Unix.
pub fn ensure_owner_dir(dir: &Path) -> io::Result<()> {
    std::fs::create_dir_all(dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

/// Writes an owner-only file (`0600` on Unix) atomically.
pub fn write_owner_file(path: &Path, bytes: &[u8]) -> Result<(), ErrorBody> {
    if let Some(parent) = path.parent() {
        ensure_owner_dir(parent).map_err(|error| {
            ErrorBody::new(
                ErrorCode::Failed,
                format!("{} cannot be created: {error}.", parent.display()),
            )
        })?;
    }
    write_atomically(path, bytes).map_err(|error| {
        ErrorBody::new(
            ErrorCode::Failed,
            format!("{} cannot be written: {error}.", path.display()),
        )
    })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

/// Generates a 64-hex-character (256-bit) cryptographically random control auth token.
pub fn generate_control_token() -> Result<String, ErrorBody> {
    let mut buf = [0u8; 32];
    getrandom::fill(&mut buf).map_err(|error| {
        ErrorBody::new(
            ErrorCode::Internal,
            format!("Could not generate sync control token: {error}."),
        )
    })?;
    let mut out = String::with_capacity(64);
    for b in buf {
        out.push_str(&format!("{b:02x}"));
    }
    Ok(out)
}

/// Reads the active runner's control token from `<state_dir>/sync/control.token`.
pub fn read_control_token(paths: &Paths) -> Result<String, ErrorBody> {
    let path = control_token_path(paths);
    let text = std::fs::read_to_string(&path).map_err(|error| {
        ErrorBody::new(
            ErrorCode::Failed,
            format!("Sync runner is not reachable ({error})."),
        )
        .with_hint("Start the sync runner with `legion sync run`.")
    })?;
    Ok(text.trim().to_owned())
}

/// Resolves the live Unix domain socket path for `paths`.
#[cfg(unix)]
pub fn resolve_unix_socket_path(paths: &Paths) -> PathBuf {
    let direct = control_sock_path(paths);
    if direct.as_os_str().len() <= MAX_DIRECT_UNIX_SOCK_LEN {
        return direct;
    }
    if let Ok(referenced) = std::fs::read_to_string(control_sock_ref_path(paths)) {
        let trimmed = referenced.trim();
        if !trimmed.is_empty() {
            return PathBuf::from(trimmed);
        }
    }
    let hash = blake3::hash(paths.state_dir().to_string_lossy().as_bytes()).to_hex();
    let base = std::env::var_os("XDG_RUNTIME_DIR")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    base.join(format!("lgs-{}", &hash[..16])).join("c.sock")
}

/// Checks whether a sync runner currently holds `<state_dir>/sync/runner.lock`.
pub fn is_runner_running(paths: &Paths) -> bool {
    let lock_path = runner_lock_path(paths);
    if !lock_path.exists() {
        return false;
    }
    match FileLock::try_acquire(&lock_path, LockMode::Exclusive) {
        Ok(_lock) => false,
        Err(legion_os::OsError::WouldBlock { .. }) => true,
        Err(_) => false,
    }
}

/// Metadata written atomically to `<state_dir>/sync/runner.pid`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunnerPidInfo {
    /// OS process id of the runner.
    pub pid: u32,
    /// RFC 3339 UTC start timestamp.
    pub started_at: String,
    /// Node name the runner serves.
    pub node: String,
}

/// Summary of one unapplied or held-back sync item (remote peer record or local scan hold).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HeldBackSummary {
    /// Relative path inside the sync folder.
    pub path: String,
    /// Peer that sent the unapplied record, or `None` for local items held back during scanning.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub peer: Option<String>,
    /// Machine-readable problem classification.
    pub kind: ProblemKind,
    /// Plain-language sentence explaining why the item is held back.
    pub reason: String,
}

/// Atomically persisted per-folder status file written by `legion sync run` to
/// `<state_dir>/sync/status/<hash>.json` so `sync.status` can read status without talking to the
/// runner.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PersistedFolderStatus {
    /// Folder identifier.
    pub folder_id: String,
    /// Human-readable folder name from `sync.toml`.
    pub folder_name: String,
    /// Resolved local directory path on this machine.
    pub local_path: String,
    /// Whether syncing is paused in `sync.toml`.
    pub paused_by_config: bool,
    /// `true` when this folder's sync index is damaged/missing and stopped awaiting `sync.rebuild`.
    #[serde(default)]
    pub index_damaged: bool,
    /// Unapplied and held-back items (both remote peer records and local scan holds such as
    /// escaping symbolic links) with their plain-language reasons.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub held_back: Vec<HeldBackSummary>,
    /// RFC 3339 UTC timestamp when this status snapshot was written.
    pub updated_at: String,
    /// Full engine status snapshot (five-state machine, per-member status, conflicts, bin, brakes).
    pub status: FolderStatus,
}

/// Writes `persisted` atomically to `<state_dir>/sync/status/<hash>.json`.
pub fn write_folder_status(
    paths: &Paths,
    persisted: &PersistedFolderStatus,
) -> Result<(), ErrorBody> {
    let dir = sync_status_dir(paths);
    ensure_owner_dir(&dir).map_err(|error| {
        ErrorBody::new(
            ErrorCode::Failed,
            format!("{} cannot be created: {error}.", dir.display()),
        )
    })?;
    let path = folder_status_path(paths, &persisted.folder_id);
    let encoded = serde_json::to_vec_pretty(persisted).map_err(|error| {
        ErrorBody::new(
            ErrorCode::Internal,
            format!("Folder status could not be serialised: {error}."),
        )
    })?;
    write_atomically(&path, &encoded).map_err(|error| {
        ErrorBody::new(
            ErrorCode::Failed,
            format!("{} cannot be written: {error}.", path.display()),
        )
    })
}

/// Reads `<state_dir>/sync/status/<hash>.json` if present.
pub fn read_folder_status(
    paths: &Paths,
    folder_id: &str,
) -> Result<Option<PersistedFolderStatus>, ErrorBody> {
    let path = folder_status_path(paths, folder_id);
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(ErrorBody::new(
                ErrorCode::Failed,
                format!("{} cannot be read: {error}.", path.display()),
            ));
        }
    };
    serde_json::from_slice(&bytes).map(Some).map_err(|error| {
        ErrorBody::new(
            ErrorCode::Failed,
            format!("Status file {} is corrupt: {error}.", path.display()),
        )
    })
}

/// Removes a folder's persisted status file when the folder is removed from this machine.
pub fn remove_folder_status(paths: &Paths, folder_id: &str) {
    let _ = std::fs::remove_file(folder_status_path(paths, folder_id));
}

/// Authenticated JSON envelope sent over the local runner control endpoint.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ControlEnvelope {
    /// Owner-only per-runner auth token read from `<state_dir>/sync/control.token`.
    pub token: String,
    /// Command to execute inside the runner.
    pub request: ControlRequest,
}

/// Local control command sent from tools or `legion stream` to `legion sync run`.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum ControlRequest {
    /// Ping the runner to verify liveness.
    Ping,
    /// Reload `sync.toml` immediately (open added folders, close removed folders, apply config edits).
    Reload,
    /// Trigger an immediate scan and pull pass (optionally for one folder; optionally wait).
    SyncNow {
        /// Folder identifier, or `None` for all folders on this machine.
        folder_id: Option<String>,
        /// When `true`, wait until the triggered scan+pull pass completes before replying.
        wait: bool,
    },
    /// Pause syncing on one folder immediately.
    Pause {
        /// Folder identifier.
        folder_id: String,
    },
    /// Resume syncing on one folder immediately (and trigger a pass).
    Resume {
        /// Folder identifier.
        folder_id: String,
    },
    /// Resolve a tripped outbound mass-deletion brake on one folder.
    ResolveBrake {
        /// Folder identifier.
        folder_id: String,
        /// `confirm_deleted` or `rescan`.
        decision: DeletionBrakeDecision,
    },
    /// Resolve a tripped inbound mass-deletion brake for one peer on one folder.
    ResolveInboundBrake {
        /// Folder identifier.
        folder_id: String,
        /// Peer member whose deletions tripped the inbound brake.
        peer: String,
        /// `apply_deletions` or `keep_local_files`.
        decision: InboundDeletionBrakeDecision,
    },
    /// Explicitly rebuild a damaged (or forced) folder index from live files, bin sidecars, and
    /// `.legion-sync/deletions.log`.
    RebuildIndex {
        /// Folder identifier.
        folder_id: String,
        /// Allow rebuilding even when the index is currently healthy.
        force: bool,
    },
    /// Dismiss informational durable problems matching `kind` (and optional `peer`).
    DismissProblem {
        /// Folder identifier.
        folder_id: String,
        /// Informational problem kind to dismiss.
        kind: ProblemKind,
        /// Optional peer filter.
        peer: Option<String>,
    },
    /// List unresolved two-version conflicts on one folder.
    ListConflicts {
        /// Folder identifier.
        folder_id: String,
    },
    /// Resolve one conflict copy on one folder.
    ResolveConflict {
        /// Folder identifier.
        folder_id: String,
        /// Relative conflict copy path.
        conflict_path: String,
        /// Resolution choice.
        choice: ConflictChoice,
    },
    /// List `.legion-sync/bin` entries on one folder.
    ListBin {
        /// Folder identifier.
        folder_id: String,
    },
    /// Restore one `.legion-sync/bin` entry back to the live folder.
    RestoreBin {
        /// Folder identifier.
        folder_id: String,
        /// Bin entry ID.
        entry_id: String,
        /// Optional relative destination path.
        dest_path: Option<String>,
        /// Conflict handling strategy (`fail`, `replace`, `rename`).
        strategy: RestoreStrategyWire,
    },
    /// Concurrent MVCC read: serve bidirectional [`PeerHandshake`] to `legion stream`.
    ServeHandshake {
        /// Folder identifier.
        folder_id: String,
        /// Calling fleet member name.
        caller: String,
        /// Caller's [`HandshakeHello`] carrying incarnation, seq, fingerprint, and pull/ack views.
        hello: HandshakeHello,
    },
    /// Concurrent MVCC read: serve [`RecordsPage`] to `legion stream`.
    ServeRecordsSince {
        /// Folder identifier.
        folder_id: String,
        /// Calling fleet member name.
        caller: String,
        /// Caller's last seen incarnation (`0` for first sync).
        since_incarnation: u64,
        /// Caller's last seen sequence number.
        since_seq: u64,
        /// Maximum records per page.
        limit: usize,
    },
    /// Concurrent MVCC read: verify live indexed file & hash before `legion stream` streams bytes
    /// directly from disk.
    ServeFetchCheck {
        /// Folder identifier.
        folder_id: String,
        /// Calling fleet member name.
        caller: String,
        /// Normalized relative path inside the folder.
        path: String,
        /// Expected BLAKE3 digest.
        expected_hash: Hash,
    },
}

/// Wire-serializable mirror of [`RestoreStrategy`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RestoreStrategyWire {
    /// Fail if a live entry already exists at the destination.
    FailIfExists,
    /// Move existing live entry to bin before restoring.
    Replace,
    /// Restore alongside the existing file under a renamed path.
    Rename,
}

impl From<RestoreStrategyWire> for RestoreStrategy {
    fn from(value: RestoreStrategyWire) -> Self {
        match value {
            RestoreStrategyWire::FailIfExists => RestoreStrategy::FailIfExists,
            RestoreStrategyWire::Replace => RestoreStrategy::Replace,
            RestoreStrategyWire::Rename => RestoreStrategy::Rename,
        }
    }
}

/// Reply returned by `legion sync run` over the local control endpoint.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ControlResponse {
    /// Operation succeeded with no extra payload.
    Ok,
    /// Status snapshots after `sync.now` / `pause` / `resume` / `resolve_brake`.
    Statuses {
        /// Updated persisted folder statuses.
        folders: Vec<PersistedFolderStatus>,
    },
    /// Unresolved two-version files.
    Conflicts {
        /// Conflicts list.
        conflicts: Vec<ConflictInfo>,
    },
    /// Bin contents.
    Bin {
        /// Bin entries list.
        entries: Vec<BinEntry>,
    },
    /// Restored file path from `.legion-sync/bin`.
    Restored {
        /// Relative path where the entry was restored.
        path: String,
    },
    /// Served [`PeerHandshake`].
    Handshake {
        /// Handshake payload.
        handshake: PeerHandshake,
    },
    /// Served [`RecordsPage`].
    Records {
        /// Records page payload.
        page: RecordsPage,
    },
    /// Served fetch pre-check confirmation.
    FetchAllowed {
        /// Indexed file byte length.
        size: u64,
    },
    /// Explicit index rebuild completed.
    Rebuilt {
        /// Number of live entries indexed from disk.
        files_indexed: usize,
        /// Number of bin entries restored from `.legion-sync/bin`.
        bin_entries: usize,
        /// Updated persisted folder status.
        status: Box<PersistedFolderStatus>,
    },
    /// Informational durable problems dismissed.
    Dismissed {
        /// Number of problem entries cleared.
        count: usize,
        /// Updated persisted folder status.
        status: Box<PersistedFolderStatus>,
    },
    /// Structured error returned by the runner.
    Error {
        /// Error body.
        error: ErrorBody,
    },
}

/// Summarizes a [`MemberState`] into its single canonical tag string.
pub fn member_state_tag(state: &MemberState) -> &'static str {
    match state {
        MemberState::UpToDate => "up_to_date",
        MemberState::Syncing { .. } => "syncing",
        MemberState::WaitingForMember { .. } => "waiting_for_member",
        MemberState::Paused => "paused",
        MemberState::Problem { .. } => "problem",
    }
}

/// Sends one authenticated [`ControlRequest`] asynchronously to the local runner.
///
/// Returns `Ok(None)` if no runner appears to be running (socket/pipe missing or refused),
/// `Ok(Some(resp))` when the runner answered, or `Err(ErrorBody)` on protocol/timeout failures.
pub async fn send_control_request(
    paths: &Paths,
    request: ControlRequest,
    timeout: Duration,
) -> Result<Option<ControlResponse>, ErrorBody> {
    let token = match read_control_token(paths) {
        Ok(tok) => tok,
        Err(_) => return Ok(None),
    };
    let envelope = ControlEnvelope { token, request };
    let mut payload = serde_json::to_vec(&envelope).map_err(|error| {
        ErrorBody::new(
            ErrorCode::Internal,
            format!("Sync control request could not be serialised: {error}."),
        )
    })?;
    payload.push(b'\n');

    tokio::time::timeout(timeout, send_control_bytes_async(paths, payload))
        .await
        .map_err(|_| {
            ErrorBody::new(
                ErrorCode::Timeout,
                format!(
                    "The local sync runner did not answer within {}s.",
                    timeout.as_secs()
                ),
            )
            .with_hint("Check whether `legion sync run` is busy or stuck and retry.")
        })?
}

#[cfg(unix)]
async fn send_control_bytes_async(
    paths: &Paths,
    payload: Vec<u8>,
) -> Result<Option<ControlResponse>, ErrorBody> {
    let sock_path = resolve_unix_socket_path(paths);
    let stream = match tokio::net::UnixStream::connect(&sock_path).await {
        Ok(stream) => stream,
        Err(err)
            if matches!(
                err.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
            ) =>
        {
            return Ok(None);
        }
        Err(err) => {
            return Err(ErrorBody::new(
                ErrorCode::Failed,
                format!(
                    "Could not connect to sync runner control socket {}: {err}.",
                    sock_path.display()
                ),
            ));
        }
    };
    let (read_half, mut write_half) = stream.into_split();
    write_half.write_all(&payload).await.map_err(|err| {
        ErrorBody::new(
            ErrorCode::Failed,
            format!("Could not send command to sync runner: {err}."),
        )
    })?;
    write_half.shutdown().await.map_err(|err| {
        ErrorBody::new(
            ErrorCode::Failed,
            format!("Could not finish command to sync runner: {err}."),
        )
    })?;
    let mut reader = TokioBufReader::new(read_half);
    let mut line = String::new();
    let n = reader.read_line(&mut line).await.map_err(|err| {
        ErrorBody::new(
            ErrorCode::Failed,
            format!("Could not read reply from sync runner: {err}."),
        )
    })?;
    if n == 0 {
        return Err(ErrorBody::new(
            ErrorCode::Failed,
            "Sync runner closed the control connection without replying.",
        ));
    }
    let resp: ControlResponse = serde_json::from_str(&line).map_err(|err| {
        ErrorBody::new(
            ErrorCode::Failed,
            format!("Sync runner returned an unreadable control reply: {err}."),
        )
    })?;
    Ok(Some(resp))
}

#[cfg(windows)]
async fn send_control_bytes_async(
    paths: &Paths,
    payload: Vec<u8>,
) -> Result<Option<ControlResponse>, ErrorBody> {
    use tokio::net::windows::named_pipe::ClientOptions;

    let pipe_name = windows_pipe_name(paths);
    let mut client = {
        let mut attempts = 0u32;
        loop {
            match ClientOptions::new().open(&pipe_name) {
                Ok(c) => break c,
                Err(err) if err.raw_os_error() == Some(231) && attempts < 20 => {
                    // ERROR_PIPE_BUSY: wait briefly for the next server instance.
                    attempts += 1;
                    tokio::time::sleep(Duration::from_millis(25)).await;
                }
                Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
                Err(err) => {
                    return Err(ErrorBody::new(
                        ErrorCode::Failed,
                        format!("Could not connect to sync runner pipe {pipe_name}: {err}."),
                    ));
                }
            }
        }
    };
    client.write_all(&payload).await.map_err(|err| {
        ErrorBody::new(
            ErrorCode::Failed,
            format!("Could not send command to sync runner: {err}."),
        )
    })?;
    client.flush().await.map_err(|err| {
        ErrorBody::new(
            ErrorCode::Failed,
            format!("Could not flush command to sync runner: {err}."),
        )
    })?;
    let mut reader = TokioBufReader::new(client);
    let mut line = String::new();
    let n = reader.read_line(&mut line).await.map_err(|err| {
        ErrorBody::new(
            ErrorCode::Failed,
            format!("Could not read reply from sync runner: {err}."),
        )
    })?;
    if n == 0 {
        return Err(ErrorBody::new(
            ErrorCode::Failed,
            "Sync runner closed the control pipe without replying.",
        ));
    }
    let resp: ControlResponse = serde_json::from_str(&line).map_err(|err| {
        ErrorBody::new(
            ErrorCode::Failed,
            format!("Sync runner returned an unreadable control reply: {err}."),
        )
    })?;
    Ok(Some(resp))
}

/// Persistent blocking client connection to the local runner control endpoint (used by synchronous
/// `legion stream` sessions so 10,000 small-file fetch checks reuse one local connection).
pub struct BlockingControlClient {
    token: String,
    #[cfg(unix)]
    reader: BufReader<std::os::unix::net::UnixStream>,
    #[cfg(unix)]
    writer: std::os::unix::net::UnixStream,
    #[cfg(windows)]
    paths: Paths,
}

impl BlockingControlClient {
    /// Connects to the local sync runner if it is running, returning `Ok(None)` when absent.
    pub fn connect(paths: &Paths, timeout: Duration) -> Result<Option<Self>, ErrorBody> {
        let token = match read_control_token(paths) {
            Ok(tok) => tok,
            Err(_) => return Ok(None),
        };
        #[cfg(unix)]
        {
            let sock_path = resolve_unix_socket_path(paths);
            let stream = match std::os::unix::net::UnixStream::connect(&sock_path) {
                Ok(s) => s,
                Err(err)
                    if matches!(
                        err.kind(),
                        io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
                    ) =>
                {
                    return Ok(None);
                }
                Err(err) => {
                    return Err(ErrorBody::new(
                        ErrorCode::Failed,
                        format!(
                            "Could not connect to sync runner socket {}: {err}.",
                            sock_path.display()
                        ),
                    ));
                }
            };
            let _ = stream.set_read_timeout(Some(timeout));
            let _ = stream.set_write_timeout(Some(timeout));
            let writer = stream.try_clone().map_err(|err| {
                ErrorBody::new(
                    ErrorCode::Failed,
                    format!("Could not clone sync control socket: {err}."),
                )
            })?;
            Ok(Some(Self {
                token,
                reader: BufReader::new(stream),
                writer,
            }))
        }
        #[cfg(windows)]
        {
            let _ = timeout;
            if !is_runner_running(paths) {
                return Ok(None);
            }
            Ok(Some(Self {
                token,
                paths: paths.clone(),
            }))
        }
    }

    /// Sends one [`ControlRequest`] over this connection and reads its [`ControlResponse`].
    pub fn call(&mut self, request: ControlRequest) -> Result<ControlResponse, ErrorBody> {
        let envelope = ControlEnvelope {
            token: self.token.clone(),
            request,
        };
        let mut payload = serde_json::to_vec(&envelope).map_err(|error| {
            ErrorBody::new(
                ErrorCode::Internal,
                format!("Sync control request could not be serialised: {error}."),
            )
        })?;
        payload.push(b'\n');

        #[cfg(unix)]
        {
            self.writer.write_all(&payload).map_err(|err| {
                ErrorBody::new(
                    ErrorCode::Failed,
                    format!("Could not write sync control request: {err}."),
                )
            })?;
            self.writer.flush().map_err(|err| {
                ErrorBody::new(
                    ErrorCode::Failed,
                    format!("Could not flush sync control request: {err}."),
                )
            })?;
            let mut line = String::new();
            let n = self.reader.read_line(&mut line).map_err(|err| {
                ErrorBody::new(
                    ErrorCode::Failed,
                    format!("Could not read sync control reply: {err}."),
                )
            })?;
            if n == 0 {
                return Err(ErrorBody::new(
                    ErrorCode::Failed,
                    "Sync runner closed the control socket.",
                ));
            }
            serde_json::from_str(&line).map_err(|err| {
                ErrorBody::new(
                    ErrorCode::Failed,
                    format!("Unreadable sync control reply: {err}."),
                )
            })
        }
        #[cfg(windows)]
        {
            use std::fs::OpenOptions;

            let pipe_name = windows_pipe_name(&self.paths);
            let mut file = OpenOptions::new()
                .read(true)
                .write(true)
                .open(&pipe_name)
                .map_err(|err| {
                    ErrorBody::new(
                        ErrorCode::Failed,
                        format!("Could not open sync runner pipe {pipe_name}: {err}."),
                    )
                })?;
            file.write_all(&payload).map_err(|err| {
                ErrorBody::new(
                    ErrorCode::Failed,
                    format!("Could not write sync control request: {err}."),
                )
            })?;
            file.flush().map_err(|err| {
                ErrorBody::new(
                    ErrorCode::Failed,
                    format!("Could not flush sync control request: {err}."),
                )
            })?;
            let mut reader = BufReader::new(file);
            let mut line = String::new();
            let n = reader.read_line(&mut line).map_err(|err| {
                ErrorBody::new(
                    ErrorCode::Failed,
                    format!("Could not read sync control reply: {err}."),
                )
            })?;
            if n == 0 {
                return Err(ErrorBody::new(
                    ErrorCode::Failed,
                    "Sync runner closed the control pipe.",
                ));
            }
            serde_json::from_str(&line).map_err(|err| {
                ErrorBody::new(
                    ErrorCode::Failed,
                    format!("Unreadable sync control reply: {err}."),
                )
            })
        }
    }
}
