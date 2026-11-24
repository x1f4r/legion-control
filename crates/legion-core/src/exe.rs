//! Process executable path captured once at startup.
//!
//! On Linux, `/proc/self/exe` (what [`std::env::current_exe`] reads) turns into
//! `.../legion (deleted)` as soon as `meta.update` atomically renames a new binary over the
//! running one. A long-lived node (`legion serve`, `legion rpc`, `legion mcp`) that re-queried
//! `std::env::current_exe()` would then launch supervisors or apply a second update to a bogus
//! `legion (deleted)` path. Recording the path once at startup avoids that and lets every tool
//! validate that the recorded path still names a regular file before using it.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use legion_proto::{ErrorBody, ErrorCode};

static RECORDED_EXE: OnceLock<PathBuf> = OnceLock::new();

/// Captures [`std::env::current_exe`] once at process startup before anything can replace the
/// binary on disk. Safe to call multiple times; only the first successful capture is kept.
pub fn record_current_exe() {
    if RECORDED_EXE.get().is_none()
        && let Ok(path) = std::env::current_exe()
    {
        let _ = RECORDED_EXE.set(path);
    }
}

/// Returns the executable path recorded at startup (recording it on first call if
/// [`record_current_exe`] has not run yet) after verifying that it still names an existing regular
/// file on disk.
pub fn current_exe() -> Result<PathBuf, ErrorBody> {
    let path = if let Some(recorded) = RECORDED_EXE.get() {
        recorded.clone()
    } else {
        let discovered = std::env::current_exe().map_err(|err| {
            ErrorBody::new(
                ErrorCode::Failed,
                format!("Cannot determine the path of the running legion executable: {err}."),
            )
        })?;
        let _ = RECORDED_EXE.set(discovered.clone());
        RECORDED_EXE.get().cloned().unwrap_or(discovered)
    };
    validate_recorded_exe(&path)
}

/// Validates that a recorded executable path does not carry Linux's `" (deleted)"` suffix and
/// currently names an existing regular file on disk.
pub fn validate_recorded_exe(path: &Path) -> Result<PathBuf, ErrorBody> {
    if path
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.ends_with(" (deleted)"))
    {
        return Err(ErrorBody::new(
            ErrorCode::Failed,
            format!(
                "Recorded legion executable path {} refers to an unlinked binary.",
                path.display()
            ),
        )
        .with_hint("Restart legion from its installed path before updating or spawning helpers."));
    }
    let meta = fs::metadata(path).map_err(|err| {
        ErrorBody::new(
            ErrorCode::Failed,
            format!(
                "Recorded legion executable path {} no longer exists or cannot be read: {err}.",
                path.display()
            ),
        )
        .with_hint("Reinstall legion or restart it from its current location on disk.")
    })?;
    if !meta.is_file() {
        return Err(ErrorBody::new(
            ErrorCode::Failed,
            format!(
                "Recorded legion executable path {} does not name a regular file.",
                path.display()
            ),
        )
        .with_hint(
            "Ensure legion's installed path points to the executable file, not a directory.",
        ));
    }
    Ok(path.to_path_buf())
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use super::*;

    #[test]
    fn validate_recorded_exe_requires_existing_regular_file_and_rejects_deleted_suffix() {
        let dir = tempdir().unwrap();
        let bin = dir.path().join("legion");
        fs::write(&bin, b"binary").unwrap();
        assert_eq!(validate_recorded_exe(&bin).unwrap(), bin);

        // Directory is refused.
        let sub = dir.path().join("subdir");
        fs::create_dir(&sub).unwrap();
        assert_eq!(
            validate_recorded_exe(&sub).unwrap_err().code,
            ErrorCode::Failed
        );

        // Missing file is refused.
        let missing = dir.path().join("missing-legion");
        assert_eq!(
            validate_recorded_exe(&missing).unwrap_err().code,
            ErrorCode::Failed
        );

        // Even if a file literally ends with " (deleted)", it is refused.
        let deleted = dir.path().join("legion (deleted)");
        fs::write(&deleted, b"ghost").unwrap();
        assert_eq!(
            validate_recorded_exe(&deleted).unwrap_err().code,
            ErrorCode::Failed
        );
    }
}
