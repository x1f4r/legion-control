//! Active paired-device session tracking and termination on revocation.
//!
//! Each `legion gate --as <principal> --device <name>` session registers its OS process identity
//! ([`legion_os::procid::ProcessId`] + [`legion_os::procid::boot_id`]) together with any verified
//! same-account parent `sshd` / `sshd-session` connection process in owner-only files under
//! `state/device-sessions/`. Gate records are removed when the gate exits; deduplicated parent
//! `sshd` connection records remain while that `sshd` connection lives so `device.remove` can
//! terminate both live gate processes and open multiplexed (`ControlMaster`) SSH connections
//! belonging to the same OS account (re-verifying the target's exact PID, start time, account,
//! and `sshd` identity before signalling, and never signalling anything else).

use std::collections::{BTreeMap, HashSet};
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use jiff::Timestamp;
use legion_core::Paths;
use legion_core::proto::{ErrorBody, ErrorCode};
use legion_os::atomic::{WriteOptions, write_atomic};
use legion_os::procid::{self, ProcessId};
use serde::{Deserialize, Serialize};
use tokio::task::JoinSet;

use super::store::{ensure_private_dir, format_rfc3339};

/// Subdirectory under `state/` storing active paired-device session records.
const SESSIONS_DIR_NAME: &str = "device-sessions";

/// Default grace period between graceful termination and forceful kill when revoking a device.
pub const DEFAULT_TERMINATE_GRACE: Duration = Duration::from_secs(2);

/// Maximum length accepted for a `--device` session label.
const MAX_DEVICE_LABEL_LEN: usize = 128;

/// Kind of process tracked by a [`DeviceSessionRecord`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionRecordKind {
    /// A live `legion gate --as <principal> --device <name>` process (deleted when the gate exits).
    #[default]
    Gate,
    /// A same-account parent `sshd` / `sshd-session` connection process that spawned a gate for
    /// this device (retained while `sshd` lives so idle `ControlMaster` connections are also ended
    /// on `device.remove`).
    SshConnection,
}

/// Identifies a paired device whose live sessions should be terminated.
///
/// When `pairing_id` is `Some`, session records that also carry a `pairing_id` are matched by
/// exact `pairing_id` so two devices sharing the same name after a merge never terminate each
/// other's sessions. Records written by older gates without `pairing_id` fall back to `device`.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct SessionTarget {
    /// Device name (`--device`).
    pub device: String,
    /// Pairing identifier (`--pairing`), when known.
    pub pairing_id: Option<String>,
}

impl SessionTarget {
    /// Creates a [`SessionTarget`] from a device name and optional pairing id.
    pub fn new(device: impl Into<String>, pairing_id: Option<String>) -> Self {
        Self {
            device: device.into(),
            pairing_id,
        }
    }

    fn matches(&self, record: &DeviceSessionRecord) -> bool {
        match (self.pairing_id.as_deref(), record.pairing_id.as_deref()) {
            (Some(want), Some(got)) => want == got,
            _ => record.device == self.device,
        }
    }
}

/// Persistent record for one live `legion gate --device <name>` session or its parent `sshd`
/// connection.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceSessionRecord {
    /// Device name (`legion-device:<name>`).
    pub device: String,
    /// Pairing identifier (`--pairing <id>`), when present; disambiguates devices sharing a name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pairing_id: Option<String>,
    /// Principal pinned by `legion gate --as`.
    pub principal: String,
    /// OS process ID of the recorded process (`legion gate` or parent `sshd`).
    pub pid: u32,
    /// Kernel process start timestamp matching `pid` (guards against PID reuse).
    pub start_time: u64,
    /// Boot identifier matching the current OS boot.
    pub boot_id: String,
    /// RFC 3339 UTC timestamp when the session registered.
    pub started: String,
    /// Whether this record tracks a `legion gate` process or a parent `sshd` connection.
    #[serde(default)]
    pub kind: SessionRecordKind,
    /// Verified same-account parent `sshd` / `sshd-session` process recorded when `legion gate`
    /// started, when one exists.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_sshd: Option<ProcessId>,
}

impl DeviceSessionRecord {
    fn process_id(&self) -> ProcessId {
        ProcessId::new(self.pid, self.start_time)
    }
}

/// Validates a `--device` label passed to `legion gate` or stored in a session file name.
pub fn validate_device_label(name: &str) -> Result<(), ErrorBody> {
    if name.is_empty()
        || name.len() > MAX_DEVICE_LABEL_LEN
        || name.contains("..")
        || name.chars().any(|c| {
            c.is_control()
                || c.is_whitespace()
                || matches!(c, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|')
        })
    {
        return Err(ErrorBody::new(
            ErrorCode::InvalidArgs,
            format!("{name:?} is not a valid device name."),
        )
        .with_hint(
            "Use a short device name without spaces, path separators, or control characters.",
        ));
    }
    Ok(())
}

/// RAII guard that registers this process as an active session for one paired device and removes
/// its gate session record file when dropped.
pub struct DeviceSessionGuard {
    path: PathBuf,
    conn_path: Option<PathBuf>,
    parent_sshd: Option<ProcessId>,
}

impl DeviceSessionGuard {
    /// Registers the current process (and any verified same-account parent `sshd` session process)
    /// under `state/device-sessions/` for `device`, optional `pairing_id`, and `principal`.
    pub fn register(
        paths: &Paths,
        device: &str,
        pairing_id: Option<&str>,
        principal: &str,
    ) -> Result<DeviceSessionGuard, ErrorBody> {
        validate_device_label(device)?;
        if let Some(pid) = pairing_id {
            validate_device_label(pid)?;
        }
        let dir = sessions_dir(paths);
        ensure_private_dir(&dir)?;

        let identity = procid::current().map_err(|error| {
            ErrorBody::new(
                ErrorCode::Internal,
                format!("Cannot determine current process identity for device session tracking: {error}."),
            )
        })?;
        let boot_id = procid::boot_id().map_err(|error| {
            ErrorBody::new(
                ErrorCode::Internal,
                format!("Cannot determine boot identity for device session tracking: {error}."),
            )
        })?;
        let parent_sshd = procid::sshd_session_ancestor();
        let now = Timestamp::now();
        let started = Timestamp::from_second(now.as_second())
            .map(format_rfc3339)
            .unwrap_or_else(|_| format_rfc3339(now));

        let record = DeviceSessionRecord {
            device: device.to_owned(),
            pairing_id: pairing_id.map(str::to_owned),
            principal: principal.to_owned(),
            pid: identity.pid,
            start_time: identity.start_time,
            boot_id: boot_id.clone(),
            started: started.clone(),
            kind: SessionRecordKind::Gate,
            parent_sshd,
        };
        let path = session_file_path(&dir, device, pairing_id, identity);
        write_session_record(&path, &record)?;

        // Also persist a deduplicated record for the parent `sshd` connection process so
        // `device.remove` can still tear down an idle `ControlMaster` connection after this
        // individual `legion gate` channel has exited cleanly.
        let conn_path = if let Some(sshd_id) = parent_sshd {
            let conn = DeviceSessionRecord {
                device: device.to_owned(),
                pairing_id: pairing_id.map(str::to_owned),
                principal: principal.to_owned(),
                pid: sshd_id.pid,
                start_time: sshd_id.start_time,
                boot_id,
                started,
                kind: SessionRecordKind::SshConnection,
                parent_sshd: None,
            };
            let conn_file = connection_file_path(&dir, device, pairing_id, sshd_id);
            write_session_record(&conn_file, &conn)?;
            Some(conn_file)
        } else {
            None
        };

        Ok(DeviceSessionGuard {
            path,
            conn_path,
            parent_sshd,
        })
    }

    /// Cleans up this gate's session and connection records and terminates its verified
    /// same-account parent `sshd` connection process (when one exists) so a revoked multiplexed
    /// `ControlMaster` connection cannot linger after the gate refuses to serve.
    pub async fn reject_and_terminate_parent(mut self) {
        if let Some(conn_path) = self.conn_path.take() {
            let _ = remove_ignoring_not_found(&conn_path);
        }
        let _ = remove_ignoring_not_found(&self.path);
        if let Some(sshd_id) = self.parent_sshd.take()
            && procid::verify_same_account_sshd(&sshd_id)
        {
            // Give sshd a brief beat to forward our flushed stderr refusal over the channel before
            // tearing down the connection.
            tokio::time::sleep(Duration::from_millis(50)).await;
            if procid::verify_same_account_sshd(&sshd_id) {
                let _ = procid::kill_process(&sshd_id, false, Duration::from_secs(1)).await;
            }
        }
    }

    /// Path of the session record file managed by this guard.
    #[cfg(test)]
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for DeviceSessionGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

fn write_session_record(path: &Path, record: &DeviceSessionRecord) -> Result<(), ErrorBody> {
    let mut bytes = serde_json::to_vec(record).map_err(|error| {
        ErrorBody::new(
            ErrorCode::Internal,
            format!("Device session record cannot be serialized: {error}."),
        )
    })?;
    bytes.push(b'\n');
    write_atomic(
        path,
        &bytes,
        WriteOptions::with_mode(0o600).force_mode(true),
    )
    .map_err(|error| {
        ErrorBody::new(
            ErrorCode::Failed,
            format!("{} cannot be written: {error}.", path.display()),
        )
    })
}

/// Removes stale session files (from exited processes, recycled PIDs, earlier boots, or corrupt
/// files) from `state/device-sessions/`, returning how many stale files were removed.
pub fn sweep_stale_sessions(paths: &Paths) -> usize {
    let expected_exe = std::env::current_exe().ok();
    sweep_stale_sessions_with_exe(paths, expected_exe.as_deref())
}

fn sweep_stale_sessions_with_exe(paths: &Paths, expected_exe: Option<&Path>) -> usize {
    let dir = sessions_dir(paths);
    let current_boot = procid::boot_id().ok();
    let entries = match std::fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(_) => return 0,
    };
    let mut removed = 0usize;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        if is_live_session_file(&path, current_boot.as_deref(), expected_exe).is_none()
            && remove_ignoring_not_found(&path)
        {
            removed += 1;
        }
    }
    removed
}

struct CollectedTargets {
    gates: Vec<(PathBuf, ProcessId, Option<ProcessId>)>,
    sshds: BTreeMap<(u32, u64), (ProcessId, Vec<PathBuf>)>,
}

enum TerminationResult {
    Gate {
        ended: bool,
        parent_sshd: Option<ProcessId>,
    },
    Sshd {
        ended: bool,
        id: ProcessId,
    },
}

/// Sweeps stale session records and immediately terminates every live recorded session and verified
/// same-account parent `sshd` connection for `device`, escalating from graceful termination to
/// forced kill after `grace`.
///
/// Protected processes (PID 0/1, current process, ancestors, and critical OS processes) are never
/// signaled, gate targets are re-verified as running this exact `legion` executable immediately
/// before signalling, and parent `sshd` targets are re-verified as live same-account non-listener
/// `sshd` session processes immediately before signalling. Returns the number of live
/// sessions/connections terminated (counting an active gate together with its parent `sshd` as one
/// session, and an idle `ControlMaster` connection with no active gate as one session).
pub async fn terminate_device_sessions(paths: &Paths, device: &str, grace: Duration) -> usize {
    let target = SessionTarget::new(device, None);
    terminate_device_sessions_for_target(paths, &target, grace).await
}

/// Sweeps stale session records and terminates every live recorded session and verified
/// same-account parent `sshd` connection matching `target` (disambiguated by `pairing_id` when
/// both target and record carry one).
pub async fn terminate_device_sessions_for_target(
    paths: &Paths,
    target: &SessionTarget,
    grace: Duration,
) -> usize {
    let expected_exe = std::env::current_exe().ok();
    terminate_device_sessions_with_exe(paths, target, grace, expected_exe.as_deref()).await
}

async fn terminate_device_sessions_with_exe(
    paths: &Paths,
    target: &SessionTarget,
    grace: Duration,
    expected_exe: Option<&Path>,
) -> usize {
    let dir = sessions_dir(paths);
    let target_owned = target.clone();
    let exe_for_collect = expected_exe.map(Path::to_path_buf);
    let collected = tokio::task::spawn_blocking(move || {
        collect_live_and_sweep(&dir, &target_owned, exe_for_collect.as_deref())
    })
    .await
    .unwrap_or(CollectedTargets {
        gates: Vec::new(),
        sshds: BTreeMap::new(),
    });

    let mut set: JoinSet<TerminationResult> = JoinSet::new();
    for (path, identity, parent_sshd) in collected.gates {
        let exe_for_gate = expected_exe.map(Path::to_path_buf);
        set.spawn(async move {
            let Some(ref expected) = exe_for_gate else {
                return TerminationResult::Gate {
                    ended: false,
                    parent_sshd,
                };
            };
            // Re-verify immediately before signalling: never touch a protected process or a
            // forged/unrelated process that is not running this exact `legion` executable.
            if procid::is_protected(&identity).is_some()
                || !procid::verify_same_executable(&identity, expected)
            {
                return TerminationResult::Gate {
                    ended: false,
                    parent_sshd,
                };
            }
            let outcome = procid::kill_process(&identity, false, grace).await;
            let gone = matches!(outcome, Ok(true)) || !procid::is_alive(&identity);
            if gone {
                let _ = remove_ignoring_not_found(&path);
            }
            TerminationResult::Gate {
                ended: gone,
                parent_sshd,
            }
        });
    }

    for (_, (sshd_id, conn_paths)) in collected.sshds {
        set.spawn(async move {
            // Re-verify immediately before signalling: never touch a protected process, another OS
            // account's sshd, a listener daemon, a recycled PID, or any non-sshd process.
            if procid::is_protected(&sshd_id).is_some()
                || !procid::verify_same_account_sshd(&sshd_id)
            {
                return TerminationResult::Sshd {
                    ended: false,
                    id: sshd_id,
                };
            }
            let outcome = procid::kill_process(&sshd_id, false, grace).await;
            let gone = matches!(outcome, Ok(true)) || !procid::is_alive(&sshd_id);
            if gone {
                for conn_path in conn_paths {
                    let _ = remove_ignoring_not_found(&conn_path);
                }
            }
            TerminationResult::Sshd {
                ended: gone,
                id: sshd_id,
            }
        });
    }

    let mut ended_gates = 0usize;
    let mut sshds_covered_by_ended_gates: HashSet<ProcessId> = HashSet::new();
    let mut ended_sshds: Vec<ProcessId> = Vec::new();
    while let Some(joined) = set.join_next().await {
        match joined {
            Ok(TerminationResult::Gate { ended, parent_sshd }) => {
                if ended {
                    ended_gates += 1;
                    if let Some(sshd_id) = parent_sshd {
                        sshds_covered_by_ended_gates.insert(sshd_id);
                    }
                }
            }
            Ok(TerminationResult::Sshd { ended, id }) => {
                if ended {
                    ended_sshds.push(id);
                }
            }
            Err(_) => {}
        }
    }

    let ended_idle_connections = ended_sshds
        .into_iter()
        .filter(|id| !sshds_covered_by_ended_gates.contains(id))
        .count();
    ended_gates + ended_idle_connections
}

fn collect_live_and_sweep(
    dir: &Path,
    target: &SessionTarget,
    expected_exe: Option<&Path>,
) -> CollectedTargets {
    let current_boot = procid::boot_id().ok();
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(_) => {
            return CollectedTargets {
                gates: Vec::new(),
                sshds: BTreeMap::new(),
            };
        }
    };
    let mut gates = Vec::new();
    let mut sshds: BTreeMap<(u32, u64), (ProcessId, Vec<PathBuf>)> = BTreeMap::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        match is_live_session_file(&path, current_boot.as_deref(), expected_exe) {
            Some(record) => {
                if !target.matches(&record) {
                    continue;
                }
                match record.kind {
                    SessionRecordKind::Gate => {
                        let gate_id = record.process_id();
                        if let Some(sshd_id) = record.parent_sshd
                            && procid::verify_same_account_sshd(&sshd_id)
                        {
                            sshds
                                .entry((sshd_id.pid, sshd_id.start_time))
                                .or_insert_with(|| (sshd_id, Vec::new()));
                        }
                        gates.push((path, gate_id, record.parent_sshd));
                    }
                    SessionRecordKind::SshConnection => {
                        let sshd_id = record.process_id();
                        sshds
                            .entry((sshd_id.pid, sshd_id.start_time))
                            .or_insert_with(|| (sshd_id, Vec::new()))
                            .1
                            .push(path);
                    }
                }
            }
            None => {
                let _ = remove_ignoring_not_found(&path);
            }
        }
    }
    CollectedTargets { gates, sshds }
}

fn is_live_session_file(
    path: &Path,
    current_boot: Option<&str>,
    expected_exe: Option<&Path>,
) -> Option<DeviceSessionRecord> {
    let bytes = std::fs::read(path).ok()?;
    let record: DeviceSessionRecord = serde_json::from_slice(&bytes).ok()?;
    if let Some(cur) = current_boot
        && !procid::same_boot(&record.boot_id, cur)
    {
        return None;
    }
    let identity = record.process_id();
    match record.kind {
        SessionRecordKind::Gate => {
            let expected = expected_exe?;
            if !procid::verify_same_executable(&identity, expected) {
                return None;
            }
        }
        SessionRecordKind::SshConnection => {
            if !procid::verify_same_account_sshd(&identity) {
                return None;
            }
        }
    }
    Some(record)
}

fn remove_ignoring_not_found(path: &Path) -> bool {
    match std::fs::remove_file(path) {
        Ok(()) => true,
        Err(error) if error.kind() == io::ErrorKind::NotFound => false,
        Err(_) => false,
    }
}

fn sessions_dir(paths: &Paths) -> PathBuf {
    paths.state_dir().join(SESSIONS_DIR_NAME)
}

fn session_file_path(
    dir: &Path,
    device: &str,
    pairing_id: Option<&str>,
    identity: ProcessId,
) -> PathBuf {
    match pairing_id {
        Some(pid) => dir.join(format!(
            "{device}-{pid}-{}-{}.json",
            identity.pid, identity.start_time
        )),
        None => dir.join(format!(
            "{device}-{}-{}.json",
            identity.pid, identity.start_time
        )),
    }
}

fn connection_file_path(
    dir: &Path,
    device: &str,
    pairing_id: Option<&str>,
    sshd_id: ProcessId,
) -> PathBuf {
    match pairing_id {
        Some(pid) => dir.join(format!(
            "conn-{device}-{pid}-{}-{}.json",
            sshd_id.pid, sshd_id.start_time
        )),
        None => dir.join(format!(
            "conn-{device}-{}-{}.json",
            sshd_id.pid, sshd_id.start_time
        )),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use std::process::{Child, Command, Stdio};

    use super::*;

    fn sleeper_executable() -> PathBuf {
        #[cfg(unix)]
        {
            for candidate in ["/usr/bin/sleep", "/bin/sleep"] {
                let p = Path::new(candidate);
                if p.exists() {
                    return p.to_path_buf();
                }
            }
            PathBuf::from("sleep")
        }
        #[cfg(windows)]
        {
            std::env::var_os("SystemRoot")
                .map(|root| PathBuf::from(root).join("System32").join("PING.EXE"))
                .unwrap_or_else(|| PathBuf::from(r"C:\Windows\System32\PING.EXE"))
        }
    }

    fn spawn_sleeper() -> Child {
        let exe = sleeper_executable();
        #[cfg(unix)]
        {
            Command::new(exe)
                .arg("60")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap()
        }
        #[cfg(windows)]
        {
            Command::new(exe)
                .args(["-n", "61", "127.0.0.1"])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap()
        }
    }

    fn write_record(dir: &Path, file: &str, record: &DeviceSessionRecord) -> PathBuf {
        std::fs::create_dir_all(dir).unwrap();
        let path = dir.join(file);
        std::fs::write(&path, serde_json::to_vec(record).unwrap()).unwrap();
        path
    }

    #[tokio::test]
    async fn terminate_device_sessions_kills_live_child_and_sweeps_stale_records() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::under(dir.path().join("legion"));
        let sess_dir = sessions_dir(&paths);
        let boot = procid::boot_id().unwrap();

        let mut victim = spawn_sleeper();
        let victim_id = procid::of_pid(victim.id()).unwrap().unwrap();

        let mut bystander = spawn_sleeper();
        let bystander_id = procid::of_pid(bystander.id()).unwrap().unwrap();

        // 1. Live session belonging to `pixel` (must be terminated), with a forged `parent_sshd`
        //    pointing at the non-sshd `bystander` (which MUST NOT be signalled!).
        let victim_file = write_record(
            &sess_dir,
            &format!("pixel-{}-{}.json", victim_id.pid, victim_id.start_time),
            &DeviceSessionRecord {
                device: "pixel".to_owned(),
                pairing_id: None,
                principal: "phone".to_owned(),
                pid: victim_id.pid,
                start_time: victim_id.start_time,
                boot_id: boot.clone(),
                started: "2026-11-24T12:00:00Z".to_owned(),
                kind: SessionRecordKind::Gate,
                parent_sshd: Some(bystander_id),
            },
        );
        // 1b. Forged `SshConnection` record for `pixel` pointing at the non-sshd `bystander`
        //     (must be swept without signalling bystander).
        let forged_conn_file = write_record(
            &sess_dir,
            &format!(
                "conn-pixel-{}-{}.json",
                bystander_id.pid, bystander_id.start_time
            ),
            &DeviceSessionRecord {
                device: "pixel".to_owned(),
                pairing_id: None,
                principal: "phone".to_owned(),
                pid: bystander_id.pid,
                start_time: bystander_id.start_time,
                boot_id: boot.clone(),
                started: "2026-11-24T12:00:00Z".to_owned(),
                kind: SessionRecordKind::SshConnection,
                parent_sshd: None,
            },
        );
        // 2. Recycled-PID record for `pixel` pointing at bystander's PID with a wrong start_time
        //    (must be swept without touching bystander).
        let recycled_file = write_record(
            &sess_dir,
            "pixel-recycled.json",
            &DeviceSessionRecord {
                device: "pixel".to_owned(),
                pairing_id: None,
                principal: "phone".to_owned(),
                pid: bystander_id.pid,
                start_time: bystander_id.start_time.wrapping_add(999),
                boot_id: boot.clone(),
                started: "2026-11-24T12:00:00Z".to_owned(),
                kind: SessionRecordKind::Gate,
                parent_sshd: None,
            },
        );
        // 3. Prior-boot record for `pixel` pointing at bystander's exact ProcessId with another
        //    boot id (must be swept without touching bystander).
        let old_boot_file = write_record(
            &sess_dir,
            "pixel-oldboot.json",
            &DeviceSessionRecord {
                device: "pixel".to_owned(),
                pairing_id: None,
                principal: "phone".to_owned(),
                pid: bystander_id.pid,
                start_time: bystander_id.start_time,
                boot_id: "00000000-0000-0000-0000-000000000000".to_owned(),
                started: "2026-11-24T12:00:00Z".to_owned(),
                kind: SessionRecordKind::Gate,
                parent_sshd: None,
            },
        );
        // 4. Live session belonging to another device `tablet` (must remain untouched).
        let other_device_file = write_record(
            &sess_dir,
            &format!(
                "tablet-{}-{}.json",
                bystander_id.pid, bystander_id.start_time
            ),
            &DeviceSessionRecord {
                device: "tablet".to_owned(),
                pairing_id: None,
                principal: "tablet".to_owned(),
                pid: bystander_id.pid,
                start_time: bystander_id.start_time,
                boot_id: boot,
                started: "2026-11-24T12:00:00Z".to_owned(),
                kind: SessionRecordKind::Gate,
                parent_sshd: None,
            },
        );
        // 5. Corrupt JSON file (must be swept).
        let corrupt_file = sess_dir.join("corrupt.json");
        std::fs::write(&corrupt_file, b"{not json").unwrap();

        let sleeper_exe = sleeper_executable();
        let ended = terminate_device_sessions_with_exe(
            &paths,
            &SessionTarget::new("pixel", None),
            Duration::from_secs(2),
            Some(&sleeper_exe),
        )
        .await;
        assert_eq!(ended, 1);

        // Reap victim and confirm it exited.
        let status = victim.wait().unwrap();
        assert!(!status.success() || !procid::is_alive(&victim_id));
        assert!(!victim_file.exists());
        assert!(!forged_conn_file.exists());
        assert!(!recycled_file.exists());
        assert!(!old_boot_file.exists());
        assert!(!corrupt_file.exists());

        // Bystander and its unrelated `tablet` session must remain completely intact.
        assert!(procid::is_alive(&bystander_id));
        assert!(other_device_file.exists());

        // 6. Forged `Gate` record pointing at `bystander` (running `sleep`/`ping`, NOT the
        //    current `legion` executable) MUST NOT be signalled by `terminate_device_sessions`
        //    and MUST be swept.
        let forged_gate_file = write_record(
            &sess_dir,
            &format!(
                "forged-{}-{}.json",
                bystander_id.pid, bystander_id.start_time
            ),
            &DeviceSessionRecord {
                device: "forged".to_owned(),
                pairing_id: None,
                principal: "phone".to_owned(),
                pid: bystander_id.pid,
                start_time: bystander_id.start_time,
                boot_id: procid::boot_id().unwrap(),
                started: "2026-11-24T12:00:00Z".to_owned(),
                kind: SessionRecordKind::Gate,
                parent_sshd: None,
            },
        );
        let ended_forged =
            terminate_device_sessions(&paths, "forged", Duration::from_secs(2)).await;
        assert_eq!(ended_forged, 0);
        assert!(
            procid::is_alive(&bystander_id),
            "forged Gate record targeting a different executable must not signal bystander"
        );
        assert!(!forged_gate_file.exists());

        let _ = bystander.kill();
        let _ = bystander.wait();
    }

    #[tokio::test]
    async fn duplicate_device_names_are_disambiguated_by_pairing_id() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::under(dir.path().join("legion"));
        let sess_dir = sessions_dir(&paths);
        let boot = procid::boot_id().unwrap();

        let id1 = "11".repeat(16);
        let id2 = "22".repeat(16);

        let mut victim = spawn_sleeper();
        let victim_id = procid::of_pid(victim.id()).unwrap().unwrap();
        let mut survivor = spawn_sleeper();
        let survivor_id = procid::of_pid(survivor.id()).unwrap().unwrap();

        let victim_file = write_record(
            &sess_dir,
            &format!(
                "pixel-{id1}-{}-{}.json",
                victim_id.pid, victim_id.start_time
            ),
            &DeviceSessionRecord {
                device: "pixel".to_owned(),
                pairing_id: Some(id1.clone()),
                principal: "device:pixel".to_owned(),
                pid: victim_id.pid,
                start_time: victim_id.start_time,
                boot_id: boot.clone(),
                started: "2026-11-24T12:00:00Z".to_owned(),
                kind: SessionRecordKind::Gate,
                parent_sshd: None,
            },
        );
        let survivor_file = write_record(
            &sess_dir,
            &format!(
                "pixel-{id2}-{}-{}.json",
                survivor_id.pid, survivor_id.start_time
            ),
            &DeviceSessionRecord {
                device: "pixel".to_owned(),
                pairing_id: Some(id2.clone()),
                principal: "device:pixel-2".to_owned(),
                pid: survivor_id.pid,
                start_time: survivor_id.start_time,
                boot_id: boot,
                started: "2026-11-24T12:00:00Z".to_owned(),
                kind: SessionRecordKind::Gate,
                parent_sshd: None,
            },
        );

        let sleeper_exe = sleeper_executable();
        let ended = terminate_device_sessions_with_exe(
            &paths,
            &SessionTarget::new("pixel", Some(id1)),
            Duration::from_secs(2),
            Some(&sleeper_exe),
        )
        .await;
        assert_eq!(ended, 1);

        let status = victim.wait().unwrap();
        assert!(!status.success() || !procid::is_alive(&victim_id));
        assert!(!victim_file.exists());
        assert!(
            procid::is_alive(&survivor_id),
            "second device sharing the same name with another pairing_id must stay alive"
        );
        assert!(survivor_file.exists());

        let _ = survivor.kill();
        let _ = survivor.wait();
    }

    #[cfg(unix)]
    #[test]
    fn guard_creates_owner_only_dir_and_file_and_cleans_up_on_drop() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::under(dir.path().join("legion"));
        let sess_dir = sessions_dir(&paths);

        let recorded_path = {
            let guard = DeviceSessionGuard::register(
                &paths,
                "pixel",
                Some(&"aa".repeat(16)),
                "phone",
            )
            .unwrap();
            let dir_mode = std::fs::metadata(&sess_dir).unwrap().permissions().mode() & 0o777;
            let file_mode = std::fs::metadata(guard.path())
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(dir_mode, 0o700);
            assert_eq!(file_mode, 0o600);
            guard.path().to_path_buf()
        };
        assert!(!recorded_path.exists());
    }
}
