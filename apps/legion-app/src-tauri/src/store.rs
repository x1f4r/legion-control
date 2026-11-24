//! Private JSON persistence for pairings and device state.
//!
//! All data lives in one file inside the platform's private app data directory and is updated by
//! writing a sibling temporary file, syncing it to disk, and renaming it into place.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tempfile::NamedTempFile;

use crate::backend::{AppError, ErrorCode, NodeLink};

/// Current on-disk schema version.
pub const STORE_VERSION: u32 = 1;

/// Filename inside the platform's private app data directory.
pub const STORE_FILENAME: &str = "legion-app.json";

/// Persisted state owned by the native shell.
///
/// Stage two extends this structure with the device's Ed25519 key material and per-node connection
/// details while preserving the single-file atomic layout.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppStoreData {
    /// Schema version for forward migration.
    pub version: u32,
    /// Optional user-chosen label for this device.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_name: Option<String>,
    /// Nodes paired with this device.
    #[serde(default)]
    pub nodes: Vec<NodeLink>,
}

impl Default for AppStoreData {
    fn default() -> Self {
        Self {
            version: STORE_VERSION,
            device_name: None,
            nodes: Vec::new(),
        }
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

        Ok(())
    }
}

#[cfg(test)]
mod tests {
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
        let initial = AppStoreData {
            version: STORE_VERSION,
            device_name: Some("laptop".to_owned()),
            nodes: vec![NodeLink {
                node: "tower".to_owned(),
                principal: "phone".to_owned(),
                role: "operator".to_owned(),
                via: "ssh".to_owned(),
            }],
        };

        store.save(&initial)?;
        assert_eq!(store.load()?, initial);

        let updated = AppStoreData {
            device_name: Some("tablet".to_owned()),
            ..initial
        };
        store.save(&updated)?;
        assert_eq!(store.load()?, updated);

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(store.path())?.permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
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
