//! Unix implementation of atomic file writes and replacement.

use std::fs::{self, File, Metadata, OpenOptions, Permissions};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::Path;

use crate::{OsError, Result};

use super::WriteOptions;

pub(super) fn configure_temp_open(open: &mut OpenOptions, restrictive: bool) {
    if restrictive {
        open.mode(0o600);
    }
}

pub(super) fn apply_final_permissions(
    file: &File,
    existing_meta: Option<&Metadata>,
    options: WriteOptions,
    target: &Path,
) -> Result<()> {
    let desired = if let Some(meta) = existing_meta {
        Some(meta.permissions())
    } else {
        options.mode.map(Permissions::from_mode)
    };

    if let Some(perms) = desired {
        file.set_permissions(perms).map_err(|err| {
            OsError::from_io(
                format!(
                    "cannot set permissions on temporary file for {}",
                    target.display()
                ),
                err,
            )
        })?;
    }
    Ok(())
}

pub(super) fn replace_file(from: &Path, to: &Path) -> Result<()> {
    fs::rename(from, to).map_err(|err| {
        OsError::from_io(
            format!("cannot rename {} to {}", from.display(), to.display()),
            err,
        )
    })
}

pub(super) async fn replace_file_async(from: &Path, to: &Path) -> Result<()> {
    replace_file(from, to)
}

pub(super) fn sync_parent_dir(parent: &Path) {
    // Syncing the parent directory ensures the rename entry survives power loss; ignore errors
    // on virtual or network filesystems that reject fsync on directories.
    if let Ok(dir) = File::open(parent) {
        let _ = dir.sync_all();
    }
}
