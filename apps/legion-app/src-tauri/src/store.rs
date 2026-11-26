//! Private JSON persistence for pairings, device key, and active node selection.
//!
//! All data lives in one file inside the platform's private app data directory and is updated by
//! writing a sibling temporary file, syncing it to disk, and renaming it into place.

use std::fmt;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use legion_link::LinkConfig;
use serde::{Deserialize, Serialize};
use tempfile::NamedTempFile;

use crate::backend::{AppError, ErrorCode};

/// Current on-disk schema version.
///
/// Version 2 records node provenance (`origin`, `learned_from`), conflict-aware pending identity
/// changes, and explicitly forgotten machine names inside `link`. Version 1 files load cleanly
/// and upgrade to version 2 on the next save; version 1 builds refuse version 2 files cleanly.
pub const STORE_VERSION: u32 = 2;

/// Filename inside the platform's private app data directory.
pub const STORE_FILENAME: &str = "legion-app.json";

/// Extended attribute Apple's Foundation uses to mark a file or directory excluded from iCloud,
/// iTunes and Time Machine backups (`NSURLIsExcludedFromBackupKey`).
#[cfg(any(target_os = "ios", target_os = "macos"))]
const APPLE_BACKUP_EXCLUDE_XATTR: &str = "com.apple.metadata:com_apple_backup_excludeItem";

/// Binary property list encoding the string `"com.apple.backupd"`, matching the payload written
/// by `NSURL.setResourceValue(true, forKey: .isExcludedFromBackupKey)`.
#[cfg(any(target_os = "ios", target_os = "macos"))]
const APPLE_BACKUP_EXCLUDE_VALUE: &[u8] = &[
    0x62, 0x70, 0x6c, 0x69, 0x73, 0x74, 0x30, 0x30, 0x5f, 0x10, 0x11, 0x63, 0x6f, 0x6d, 0x2e, 0x61,
    0x70, 0x70, 0x6c, 0x65, 0x2e, 0x62, 0x61, 0x63, 0x6b, 0x75, 0x70, 0x64, 0x08, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x01, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x1c,
];

/// Marks `path` excluded from iCloud, iTunes and Time Machine backups on Apple platforms.
///
/// Best-effort: if the underlying filesystem does not support extended attributes, logs a warning
/// and continues so startup and saves never fail on non-APFS/HFS+ volumes.
#[cfg(any(target_os = "ios", target_os = "macos"))]
fn exclude_from_backup(path: &Path) {
    if let Err(err) = rustix::fs::setxattr(
        path,
        APPLE_BACKUP_EXCLUDE_XATTR,
        APPLE_BACKUP_EXCLUDE_VALUE,
        rustix::fs::XattrFlags::empty(),
    ) {
        #[cfg(debug_assertions)]
        eprintln!(
            "legion-app: could not exclude {} from Apple backups: {err}",
            path.display()
        );
        #[cfg(not(debug_assertions))]
        {
            let _ = err;
            eprintln!("legion-app: could not set Apple backup exclusion attribute");
        }
    }
}

/// No-op on non-Apple platforms (Android uses `data_extraction_rules.xml`).
#[cfg(not(any(target_os = "ios", target_os = "macos")))]
fn exclude_from_backup(_path: &Path) {}

/// Non-secret display metadata recorded alongside each paired node in [`LinkConfig`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredNodeMeta {
    /// Paired node name matching [`legion_link::NodeLink::node`].
    pub node: String,
    /// Role reported by the node when enrolled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    /// Device name enrolled on the node (needed for `legion device.remove` reminders).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device: Option<String>,
    /// RFC 3339 UTC timestamp of when `role` was last confirmed on this node.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role_checked_at: Option<String>,
}

/// Persisted state owned by the native shell.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppStoreData {
    /// Schema version for forward migration.
    #[serde(default = "default_version")]
    pub version: u32,
    /// Serialized `legion-link` configuration (device key and paired SSH nodes).
    #[serde(default)]
    pub link: LinkConfig,
    /// Name of the active node preferred for calls when more than one node exists.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_node: Option<String>,
    /// Non-secret metadata per paired node (role and enrolled device name).
    #[serde(default)]
    pub node_meta: Vec<StoredNodeMeta>,
}

fn default_version() -> u32 {
    STORE_VERSION
}

impl Default for AppStoreData {
    fn default() -> Self {
        Self {
            version: STORE_VERSION,
            link: LinkConfig::default(),
            current_node: None,
            node_meta: Vec::new(),
        }
    }
}

impl fmt::Debug for AppStoreData {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AppStoreData")
            .field("version", &self.version)
            .field("link", &self.link)
            .field("current_node", &self.current_node)
            .field("node_meta", &self.node_meta)
            .finish()
    }
}

impl AppStoreData {
    /// Inserts or updates metadata for a paired node.
    pub fn upsert_meta(&mut self, meta: StoredNodeMeta) {
        if let Some(existing) = self.node_meta.iter_mut().find(|m| m.node == meta.node) {
            *existing = meta;
        } else {
            self.node_meta.push(meta);
        }
    }

    /// Removes metadata for a node, returning the removed record if present.
    pub fn remove_meta(&mut self, node: &str) -> Option<StoredNodeMeta> {
        let pos = self.node_meta.iter().position(|m| m.node == node)?;
        Some(self.node_meta.remove(pos))
    }

    /// Looks up metadata for a node by name.
    pub fn meta(&self, node: &str) -> Option<&StoredNodeMeta> {
        self.node_meta.iter().find(|m| m.node == node)
    }
}

/// Handle to the single JSON state file in the app's private data directory.
#[derive(Clone, Debug)]
pub struct AppStore {
    path: PathBuf,
}

impl AppStore {
    /// Creates a store handle for the given private app data directory.
    pub fn in_dir(data_dir: impl AsRef<Path>) -> Self {
        Self {
            path: data_dir.as_ref().join(STORE_FILENAME),
        }
    }

    /// Returns the full path of the backing JSON file.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Ensures the backing app data directory exists, is owner-only on Unix, and is excluded from
    /// Apple iCloud/iTunes/Time Machine backups.
    pub fn prepare_dir(&self) -> Result<(), AppError> {
        let parent = self.path.parent().ok_or_else(|| {
            AppError::new(
                ErrorCode::Internal,
                "App state path has no parent directory.",
            )
        })?;
        fs::create_dir_all(parent).map_err(|err| {
            AppError::new(
                ErrorCode::Internal,
                format!("Could not create app data directory: {err}"),
            )
        })?;

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(parent, fs::Permissions::from_mode(0o700));
        }

        exclude_from_backup(parent);
        Ok(())
    }

    /// Loads the stored state, returning [`AppStoreData::default`] when the file does not exist.
    pub fn load(&self) -> Result<AppStoreData, AppError> {
        let bytes = match fs::read(&self.path) {
            Ok(bytes) => bytes,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                return Ok(AppStoreData::default());
            }
            Err(err) => {
                return Err(AppError::new(
                    ErrorCode::Internal,
                    format!("Could not read app state: {err}"),
                )
                .with_hint(
                    "Check that the app has permission to access its private data folder.",
                ));
            }
        };

        let mut data: AppStoreData = serde_json::from_slice(&bytes).map_err(|err| {
            AppError::new(
                ErrorCode::Internal,
                format!("Stored app state is not valid JSON: {err}"),
            )
            .with_hint("Move the damaged state file aside to start fresh, or restore a backup.")
        })?;

        if data.version > STORE_VERSION {
            return Err(AppError::new(
                ErrorCode::Unsupported,
                format!(
                    "Saved app state uses schema version {}, which is newer than this build supports (version {STORE_VERSION}).",
                    data.version
                ),
            )
            .with_hint(
                "Update Legion to open this file, or move the file aside to start fresh.",
            ));
        }

        // Upgrade earlier schemas (v1) in memory so the next atomic save writes STORE_VERSION.
        data.version = STORE_VERSION;
        Ok(data)
    }

    /// Moves a damaged or unsupported state file aside to `legion-app.json.damaged-<timestamp>`
    /// without ever deleting it, returning the archived file path.
    pub fn archive_damaged(&self) -> Result<PathBuf, AppError> {
        let parent = self.path.parent().ok_or_else(|| {
            AppError::new(
                ErrorCode::Internal,
                "App state path has no parent directory.",
            )
        })?;

        let stamp = format_utc_timestamp(
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0),
        );

        let mut dest = parent.join(format!("{STORE_FILENAME}.damaged-{stamp}"));
        let mut suffix: u32 = 1;
        while dest.exists() {
            dest = parent.join(format!("{STORE_FILENAME}.damaged-{stamp}-{suffix}"));
            suffix = suffix.saturating_add(1);
        }

        fs::rename(&self.path, &dest).map_err(|err| {
            AppError::new(
                ErrorCode::Internal,
                format!("Could not move damaged state file aside: {err}"),
            )
        })?;

        #[cfg(unix)]
        if let Ok(dir) = fs::File::open(parent) {
            let _ = dir.sync_all();
        }

        Ok(dest)
    }

    /// Atomically writes `data` to the backing JSON file with owner-only permissions on Unix.
    pub fn save(&self, data: &AppStoreData) -> Result<(), AppError> {
        self.prepare_dir()?;
        let parent = self.path.parent().ok_or_else(|| {
            AppError::new(
                ErrorCode::Internal,
                "App state path has no parent directory.",
            )
        })?;

        let mut payload = serde_json::to_vec_pretty(data).map_err(|err| {
            AppError::new(
                ErrorCode::Internal,
                format!("Could not encode app state as JSON: {err}"),
            )
        })?;
        payload.push(b'\n');

        let mut temp = NamedTempFile::new_in(parent).map_err(|err| {
            AppError::new(
                ErrorCode::Internal,
                format!("Could not create temporary state file: {err}"),
            )
        })?;

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            temp.as_file()
                .set_permissions(fs::Permissions::from_mode(0o600))
                .map_err(|err| {
                    AppError::new(
                        ErrorCode::Internal,
                        format!("Could not restrict state file permissions: {err}"),
                    )
                })?;
        }

        exclude_from_backup(temp.path());

        temp.write_all(&payload).map_err(|err| {
            AppError::new(
                ErrorCode::Internal,
                format!("Could not write temporary state file: {err}"),
            )
        })?;
        temp.as_file().sync_all().map_err(|err| {
            AppError::new(
                ErrorCode::Internal,
                format!("Could not flush temporary state file: {err}"),
            )
        })?;

        temp.persist(&self.path).map_err(|err| {
            AppError::new(
                ErrorCode::Internal,
                format!("Could not replace app state file: {}", err.error),
            )
        })?;

        #[cfg(unix)]
        if let Ok(dir) = fs::File::open(parent) {
            let _ = dir.sync_all();
        }

        exclude_from_backup(&self.path);

        Ok(())
    }
}

/// Formats a Unix timestamp in seconds as `YYYYMMDD-HHMMSSZ` in UTC without external crates.
fn format_utc_timestamp(unix_secs: u64) -> String {
    let secs_of_day = unix_secs % 86_400;
    let hour = secs_of_day / 3_600;
    let minute = (secs_of_day % 3_600) / 60;
    let second = secs_of_day % 60;

    // Howard Hinnant's civil-from-days algorithm (proleptic Gregorian calendar).
    let z = (unix_secs / 86_400) as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097) as u64;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if month <= 2 { y + 1 } else { y };

    format!("{year:04}{month:02}{day:02}-{hour:02}{minute:02}{second:02}Z")
}

#[cfg(test)]
mod tests {
    use legion_link::{NodeLink as LinkNode, SshTarget};

    use super::*;

    #[test]
    fn missing_file_loads_default() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let store = AppStore::in_dir(dir.path());
        let data = store.load()?;
        assert_eq!(data, AppStoreData::default());
        Ok(())
    }

    #[test]
    fn atomic_save_and_reload_round_trips() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let store = AppStore::in_dir(dir.path());
        let mut initial = AppStoreData::default();
        initial.link.device_key = Some("PLACEHOLDER-PRIVATE-KEY".to_owned());
        initial.link.upsert_node(LinkNode::ssh(
            "tower",
            SshTarget {
                hosts: vec!["tower.example".to_owned()],
                port: 22,
                user: "me".to_owned(),
                host_keys: vec!["ssh-ed25519 AAAA".to_owned()],
            },
            Some("phone".to_owned()),
        ));
        initial.current_node = Some("tower".to_owned());
        initial.upsert_meta(StoredNodeMeta {
            node: "tower".to_owned(),
            role: Some("operator".to_owned()),
            device: Some("my-phone".to_owned()),
            role_checked_at: Some("2026-11-24T23:00:00Z".to_owned()),
        });

        store.save(&initial)?;
        assert_eq!(store.load()?, initial);

        let debug_text = format!("{initial:?}");
        assert!(!debug_text.contains("PLACEHOLDER-PRIVATE-KEY"));

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(store.path())?.permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }

        #[cfg(any(target_os = "ios", target_os = "macos"))]
        {
            let mut dir_buf = [0u8; 128];
            let dir_len =
                rustix::fs::getxattr(dir.path(), APPLE_BACKUP_EXCLUDE_XATTR, &mut dir_buf)?;
            assert_eq!(&dir_buf[..dir_len], APPLE_BACKUP_EXCLUDE_VALUE);

            let mut file_buf = [0u8; 128];
            let file_len =
                rustix::fs::getxattr(store.path(), APPLE_BACKUP_EXCLUDE_XATTR, &mut file_buf)?;
            assert_eq!(&file_buf[..file_len], APPLE_BACKUP_EXCLUDE_VALUE);
        }

        Ok(())
    }

    #[test]
    fn invalid_json_returns_structured_error() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let store = AppStore::in_dir(dir.path());
        fs::write(store.path(), b"{not valid json")?;
        let err = store.load().err().ok_or("expected parse error")?;
        assert_eq!(err.code, ErrorCode::Internal);
        assert!(err.hint.is_some());
        Ok(())
    }

    #[test]
    fn schema_v1_loads_cleanly_and_upgrades_to_v2_on_save() -> Result<(), Box<dyn std::error::Error>>
    {
        assert_eq!(STORE_VERSION, 2);
        let dir = tempfile::tempdir()?;
        let store = AppStore::in_dir(dir.path());
        let v1_json = serde_json::json!({
            "version": 1,
            "link": {
                "nodes": [
                    {
                        "node": "tower",
                        "target": {
                            "hosts": ["tower.example"],
                            "port": 22,
                            "user": "me",
                            "host_keys": ["ssh-ed25519 AAAA"]
                        },
                        "principal": "phone"
                    }
                ]
            },
            "current_node": "tower",
            "node_meta": [{ "node": "tower", "role": "operator", "device": "phone" }]
        })
        .to_string();
        fs::write(store.path(), v1_json)?;

        let loaded = store.load()?;
        assert_eq!(loaded.version, STORE_VERSION);
        assert_eq!(loaded.link.nodes.len(), 1);
        assert_eq!(loaded.link.nodes[0].node, "tower");
        assert!(loaded.link.forgotten_nodes.is_empty());

        store.save(&loaded)?;
        let raw: serde_json::Value = serde_json::from_str(&fs::read_to_string(store.path())?)?;
        assert_eq!(raw["version"], 2);
        Ok(())
    }

    #[test]
    fn newer_schema_version_is_refused() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let store = AppStore::in_dir(dir.path());
        fs::write(
            store.path(),
            serde_json::json!({ "version": STORE_VERSION + 1, "link": {} }).to_string(),
        )?;
        let err = store.load().err().ok_or("expected newer-version error")?;
        assert_eq!(err.code, ErrorCode::Unsupported);
        assert!(err.message.contains("newer"));
        Ok(())
    }

    #[test]
    fn archive_damaged_moves_file_aside_without_deleting_bytes()
    -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let store = AppStore::in_dir(dir.path());
        let original = b"{\"version\":1,\"truncated\":";
        fs::write(store.path(), original)?;

        let archived = store.archive_damaged()?;
        assert!(!store.path().exists());
        assert!(archived.exists());
        let name = archived
            .file_name()
            .and_then(|n| n.to_str())
            .ok_or("missing filename")?;
        assert!(name.starts_with("legion-app.json.damaged-"));
        assert_eq!(fs::read(&archived)?, original);
        Ok(())
    }

    #[test]
    fn sealed_ratchet_round_trips_and_legacy_json_defaults_false()
    -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let store = AppStore::in_dir(dir.path());
        let legacy = serde_json::json!({
            "version": 2,
            "link": {
                "nodes": [
                    {
                        "node": "studio",
                        "target": {
                            "hosts": ["studio.example"],
                            "port": 22,
                            "user": "me",
                            "host_keys": ["ssh-ed25519 AAAA"]
                        },
                        "principal": "phone"
                    }
                ]
            },
            "current_node": "studio"
        })
        .to_string();
        fs::write(store.path(), legacy)?;

        let mut loaded = store.load()?;
        assert!(!loaded.link.node("studio").ok_or("missing studio")?.sealed);

        // Saving with sealed=false omits the field from JSON.
        store.save(&loaded)?;
        let raw_false: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(store.path())?)?;
        assert!(raw_false["link"]["nodes"][0].get("sealed").is_none());

        // Setting sealed=true persists cleanly across save/load.
        loaded.link.nodes[0].sealed = true;
        store.save(&loaded)?;
        let raw_true: serde_json::Value = serde_json::from_str(&fs::read_to_string(store.path())?)?;
        assert_eq!(raw_true["link"]["nodes"][0]["sealed"], true);

        let reloaded = store.load()?;
        assert!(reloaded.link.node("studio").ok_or("missing studio")?.sealed);
        Ok(())
    }
}
