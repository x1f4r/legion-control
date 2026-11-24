//! Private JSON persistence for pairings, device key, and active node selection.
//!
//! All data lives in one file inside the platform's private app data directory and is updated by
//! writing a sibling temporary file, syncing it to disk, and renaming it into place.

use std::fmt;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use legion_link::LinkConfig;
use serde::{Deserialize, Serialize};
use tempfile::NamedTempFile;

use crate::backend::{AppError, ErrorCode};

/// Current on-disk schema version.
pub const STORE_VERSION: u32 = 1;

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
#[cfg(any(target_os = "ios", target_os = "macos"))]
fn exclude_from_backup(path: &Path) -> Result<(), AppError> {
    rustix::fs::setxattr(
        path,
        APPLE_BACKUP_EXCLUDE_XATTR,
        APPLE_BACKUP_EXCLUDE_VALUE,
        rustix::fs::XattrFlags::empty(),
    )
    .map_err(|err| {
        AppError::new(
            ErrorCode::Internal,
            format!("Could not exclude app data from Apple backups: {err}"),
        )
    })
}

/// No-op on non-Apple platforms (Android uses `data_extraction_rules.xml`).
#[cfg(not(any(target_os = "ios", target_os = "macos")))]
fn exclude_from_backup(_path: &Path) -> Result<(), AppError> {
    Ok(())
}

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

        exclude_from_backup(parent)?;
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

        serde_json::from_slice(&bytes).map_err(|err| {
            AppError::new(
                ErrorCode::Internal,
                format!("Stored app state is not valid JSON: {err}"),
            )
            .with_hint("Remove the damaged state file or reset app storage and pair again.")
        })
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

        exclude_from_backup(temp.path())?;

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

        exclude_from_backup(&self.path)?;

        Ok(())
    }
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
}
