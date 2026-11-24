//! Active paired-device session tracking and termination on revocation.
//!
//! Each `legion gate --as <principal> --device <name>` session registers its OS process identity
//! ([`legion_os::procid::ProcessId`] + [`legion_os::procid::boot_id`]) in an owner-only file under
//! `state/device-sessions/` for the duration of the session and removes it on exit. When
//! `device.remove` revokes a device, it terminates every live recorded session for that device
//! (SIGTERM then SIGKILL after grace on Unix; handle-pinned TerminateProcess on Windows) so a
//! held-open SSH session cannot keep running after its key is removed from `authorized_keys`.

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

/// Persistent record for one live `legion gate --device <name>` session.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceSessionRecord {
    /// Device name (`legion-device:<name>`).
    pub device: String,
    /// Principal pinned by `legion gate --as`.
    pub principal: String,
    /// OS process ID of the `legion gate` process.
    pub pid: u32,
    /// Kernel process start timestamp matching `pid` (guards against PID reuse).
    pub start_time: u64,
    /// Boot identifier matching the current OS boot.
    pub boot_id: String,
    /// RFC 3339 UTC timestamp when the session registered.
    pub started: String,
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
/// its session record file when dropped.
pub struct DeviceSessionGuard {
    path: PathBuf,
}

impl DeviceSessionGuard {
    /// Registers the current process under `state/device-sessions/` for `device` and `principal`.
    pub fn register(
        paths: &Paths,
        device: &str,
        principal: &str,
    ) -> Result<DeviceSessionGuard, ErrorBody> {
        validate_device_label(device)?;
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
        let now = Timestamp::now();
        let started = Timestamp::from_second(now.as_second())
            .map(format_rfc3339)
            .unwrap_or_else(|_| format_rfc3339(now));

        let record = DeviceSessionRecord {
            device: device.to_owned(),
            principal: principal.to_owned(),
            pid: identity.pid,
            start_time: identity.start_time,
            boot_id,
            started,
        };
        let path = session_file_path(&dir, device, identity);
        let mut bytes = serde_json::to_vec(&record).map_err(|error| {
            ErrorBody::new(
                ErrorCode::Internal,
                format!("Device session record cannot be serialized: {error}."),
            )
        })?;
        bytes.push(b'\n');
        write_atomic(
            &path,
            &bytes,
            WriteOptions::with_mode(0o600).force_mode(true),
        )
        .map_err(|error| {
            ErrorBody::new(
                ErrorCode::Failed,
                format!("{} cannot be written: {error}.", path.display()),
            )
        })?;

        Ok(DeviceSessionGuard { path })
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

/// Removes stale session files (from exited processes, recycled PIDs, earlier boots, or corrupt
/// files) from `state/device-sessions/`, returning how many stale files were removed.
pub fn sweep_stale_sessions(paths: &Paths) -> usize {
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
        if is_live_session_file(&path, current_boot.as_deref()).is_none()
            && remove_ignoring_not_found(&path)
        {
            removed += 1;
        }
    }
    removed
}

/// Sweeps stale session records and immediately terminates every live recorded session for
/// `device`, escalating from graceful termination to forced kill after `grace`.
///
/// Protected processes (PID 0/1, current process, ancestors, and critical OS processes) are never
/// signaled. Returns the number of live sessions terminated.
pub async fn terminate_device_sessions(paths: &Paths, device: &str, grace: Duration) -> usize {
    let dir = sessions_dir(paths);
    let device_owned = device.to_owned();
    let targets = tokio::task::spawn_blocking(move || collect_live_and_sweep(&dir, &device_owned))
        .await
        .unwrap_or_default();

    let mut set = JoinSet::new();
    for (path, identity) in targets {
        set.spawn(async move {
            if procid::is_protected(&identity).is_some() {
                return false;
            }
            let outcome = procid::kill_process(&identity, false, grace).await;
            let gone = matches!(outcome, Ok(true)) || !procid::is_alive(&identity);
            if gone {
                let _ = remove_ignoring_not_found(&path);
            }
            gone
        });
    }

    let mut ended = 0usize;
    while let Some(joined) = set.join_next().await {
        if matches!(joined, Ok(true)) {
            ended += 1;
        }
    }
    ended
}

fn collect_live_and_sweep(dir: &Path, device: &str) -> Vec<(PathBuf, ProcessId)> {
    let current_boot = procid::boot_id().ok();
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(_) => return Vec::new(),
    };
    let mut targets = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        match is_live_session_file(&path, current_boot.as_deref()) {
            Some(record) => {
                if record.device == device {
                    targets.push((path, record.process_id()));
                }
            }
            None => {
                let _ = remove_ignoring_not_found(&path);
            }
        }
    }
    targets
}

fn is_live_session_file(path: &Path, current_boot: Option<&str>) -> Option<DeviceSessionRecord> {
    let bytes = std::fs::read(path).ok()?;
    let record: DeviceSessionRecord = serde_json::from_slice(&bytes).ok()?;
    if let Some(cur) = current_boot
        && !procid::same_boot(&record.boot_id, cur)
    {
        return None;
    }
    if !procid::is_alive(&record.process_id()) {
        return None;
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

fn session_file_path(dir: &Path, device: &str, identity: ProcessId) -> PathBuf {
    dir.join(format!(
        "{device}-{}-{}.json",
        identity.pid, identity.start_time
    ))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use std::process::{Child, Command, Stdio};

    use super::*;

    fn spawn_sleeper() -> Child {
        #[cfg(unix)]
        {
            Command::new("sleep")
                .arg("60")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap()
        }
        #[cfg(windows)]
        {
            Command::new("ping")
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

        // 1. Live session belonging to `pixel` (must be terminated).
        let victim_file = write_record(
            &sess_dir,
            &format!("pixel-{}-{}.json", victim_id.pid, victim_id.start_time),
            &DeviceSessionRecord {
                device: "pixel".to_owned(),
                principal: "phone".to_owned(),
                pid: victim_id.pid,
                start_time: victim_id.start_time,
                boot_id: boot.clone(),
                started: "2026-11-24T12:00:00Z".to_owned(),
            },
        );
        // 2. Recycled-PID record for `pixel` pointing at bystander's PID with a wrong start_time
        //    (must be swept without touching bystander).
        let recycled_file = write_record(
            &sess_dir,
            "pixel-recycled.json",
            &DeviceSessionRecord {
                device: "pixel".to_owned(),
                principal: "phone".to_owned(),
                pid: bystander_id.pid,
                start_time: bystander_id.start_time.wrapping_add(999),
                boot_id: boot.clone(),
                started: "2026-11-24T12:00:00Z".to_owned(),
            },
        );
        // 3. Prior-boot record for `pixel` pointing at bystander's exact ProcessId with another
        //    boot id (must be swept without touching bystander).
        let old_boot_file = write_record(
            &sess_dir,
            "pixel-oldboot.json",
            &DeviceSessionRecord {
                device: "pixel".to_owned(),
                principal: "phone".to_owned(),
                pid: bystander_id.pid,
                start_time: bystander_id.start_time,
                boot_id: "00000000-0000-0000-0000-000000000000".to_owned(),
                started: "2026-11-24T12:00:00Z".to_owned(),
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
                principal: "tablet".to_owned(),
                pid: bystander_id.pid,
                start_time: bystander_id.start_time,
                boot_id: boot,
                started: "2026-11-24T12:00:00Z".to_owned(),
            },
        );
        // 5. Corrupt JSON file (must be swept).
        let corrupt_file = sess_dir.join("corrupt.json");
        std::fs::write(&corrupt_file, b"{not json").unwrap();

        let ended = terminate_device_sessions(&paths, "pixel", Duration::from_secs(2)).await;
        assert_eq!(ended, 1);

        // Reap victim and confirm it exited.
        let status = victim.wait().unwrap();
        assert!(!status.success() || !procid::is_alive(&victim_id));
        assert!(!victim_file.exists());
        assert!(!recycled_file.exists());
        assert!(!old_boot_file.exists());
        assert!(!corrupt_file.exists());

        // Bystander and its unrelated `tablet` session must remain completely intact.
        assert!(procid::is_alive(&bystander_id));
        assert!(other_device_file.exists());

        let _ = bystander.kill();
        let _ = bystander.wait();
    }

    #[cfg(unix)]
    #[test]
    fn guard_creates_owner_only_dir_and_file_and_cleans_up_on_drop() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::under(dir.path().join("legion"));
        let sess_dir = sessions_dir(&paths);

        let recorded_path = {
            let guard = DeviceSessionGuard::register(&paths, "pixel", "phone").unwrap();
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
