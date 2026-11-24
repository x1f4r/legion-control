//! Tree and entry management tools: `fs.move`, `fs.copy` and `fs.remove`.

use std::fs::{self, File, Metadata};
use std::io::{self, Read};
use std::path::{Path, PathBuf};

use legion_core::proto::{Effect, ErrorBody, ErrorCode};
use legion_core::{Access, CancellationToken, Ctx, Registry, ToolError};
use legion_os::OsError;
use legion_os::atomic::{WriteOptions, replace_file, write_atomic_from_reader};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::json;

use super::{
    OwnDirFilter, Progress, cancelled_error, check_not_protected_tree, is_same_or_descendant,
    is_strict_ancestor, map_io_error, map_os_error, path_to_string, paths_equal, platform,
    run_blocking,
};

const REMOVE_SAMPLE_LIMIT: usize = 50;

pub(super) fn register(registry: &mut Registry) {
    registry.add_fn(
        "fs.move",
        Effect::Write,
        "Move or rename a file, symbolic link or directory, falling back to copy-then-remove across file systems.",
        move_entry,
    );
    registry.add_fn(
        "fs.copy",
        Effect::Write,
        "Copy a file, symbolic link or directory tree without following symbolic links out of the tree.",
        copy_entry,
    );
    registry.add_fn(
        "fs.remove",
        Effect::Destroy,
        "Remove a file, symbolic link or directory (with optional recursive and dry_run modes), refusing roots, the home directory and Legion's own directories.",
        remove_entry,
    );
}

/// Arguments of `fs.move`.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct MoveArgs {
    /// Existing path to move (absolute or starting with `~`).
    from: String,
    /// Destination path (absolute or starting with `~`).
    to: String,
    /// Replace an existing file, symbolic link or empty directory at `to`.
    #[serde(default)]
    overwrite: bool,
}

/// Result of `fs.move`.
#[derive(Debug, Serialize)]
struct MoveResult {
    from: String,
    to: String,
}

async fn move_entry(ctx: &Ctx, args: MoveArgs) -> Result<MoveResult, ToolError> {
    let from = ctx.grant().check_path(&args.from, Access::Write)?;
    let to = ctx.grant().check_path(&args.to, Access::Write)?;
    check_not_protected_tree(ctx.grant(), &from)?;
    check_not_protected_tree(ctx.grant(), &to)?;
    let own_dirs = OwnDirFilter::for_ctx(ctx);
    let overwrite = args.overwrite;

    run_blocking(ctx, "fs.move", move |cancel, mut progress| {
        if cancel.is_cancelled() {
            return Err(cancelled_error("fs.move"));
        }
        let from_meta = fs::symlink_metadata(&from)
            .map_err(|error| map_io_error("move", &from, error))?;
        if from.as_os_str() == to.as_os_str() {
            return Ok(MoveResult {
                from: path_to_string(&from),
                to: path_to_string(&to),
            });
        }
        if from_meta.is_dir() && is_strict_ancestor(&from, &to) {
            return Err(ErrorBody::new(
                ErrorCode::InvalidArgs,
                format!(
                    "Cannot move {} inside itself ({}).",
                    from.display(),
                    to.display()
                ),
            )
            .with_hint("Choose a destination outside the source directory."));
        }
        ensure_parent_exists(&to)?;

        let mut to_was_empty_dir = false;
        match fs::symlink_metadata(&to) {
            Ok(to_meta) => {
                if !overwrite {
                    return Err(ErrorBody::new(
                        ErrorCode::Exists,
                        format!("{} already exists.", to.display()),
                    )
                    .with_hint("Pass overwrite: true to replace it."));
                }
                if from_meta.is_dir() != to_meta.is_dir() {
                    return Err(ErrorBody::new(
                        ErrorCode::Conflict,
                        format!(
                            "Cannot overwrite {} with {} because one is a directory and the other is not.",
                            to.display(),
                            from.display()
                        ),
                    )
                    .with_hint("Remove the destination first with fs.remove, or choose another path."));
                }
                if to_meta.is_dir() {
                    if dir_is_non_empty(&to)? {
                        return Err(ErrorBody::new(
                            ErrorCode::Conflict,
                            format!("Destination directory {} is not empty.", to.display()),
                        )
                        .with_hint(
                            "Remove the destination directory first with fs.remove if you intend to replace it.",
                        ));
                    }
                    to_was_empty_dir = true;
                }
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(map_io_error("inspect", &to, error)),
        }

        let rename_result = match replace_file(&from, &to) {
            Ok(()) => Ok(()),
            // Windows cannot rename a directory onto an existing empty directory without removing
            // the empty target first.
            Err(OsError::Io { ref source, .. } | OsError::PermissionDenied { source: Some(ref source), .. })
                if to_was_empty_dir
                    && matches!(
                        source.kind(),
                        io::ErrorKind::AlreadyExists | io::ErrorKind::PermissionDenied
                    ) =>
            {
                fs::remove_dir(&to)
                    .map_err(|error| map_io_error("replace empty directory", &to, error))?;
                replace_file(&from, &to)
            }
            Err(other) => Err(other),
        };

        match rename_result {
            Ok(()) => {}
            Err(OsError::Io { source, .. }) if source.kind() == io::ErrorKind::CrossesDevices => {
                let mut totals = CopyTotals::default();
                copy_path(
                    &from,
                    &to,
                    &from_meta,
                    overwrite,
                    true,
                    "fs.move",
                    &own_dirs,
                    &cancel,
                    &mut progress,
                    &mut totals,
                )?;
                let mut removed = RemoveTotals::default();
                remove_path(
                    &from,
                    &from_meta,
                    true,
                    false,
                    "fs.move",
                    &cancel,
                    &mut progress,
                    &mut removed,
                )?;
            }
            Err(error) => return Err(map_os_error("move", &from, error)),
        }

        Ok(MoveResult {
            from: path_to_string(&from),
            to: path_to_string(&to),
        })
    })
    .await
}

/// Arguments of `fs.copy`.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct CopyArgs {
    /// Source file, symbolic link or directory (absolute or starting with `~`).
    from: String,
    /// Destination path (absolute or starting with `~`).
    to: String,
    /// Overwrite existing files or merge into an existing directory at `to`.
    #[serde(default)]
    overwrite: bool,
    /// Copy directories and their contents recursively.
    #[serde(default)]
    recursive: bool,
}

/// Result of `fs.copy`.
#[derive(Debug, Serialize)]
struct CopyResult {
    from: String,
    to: String,
    files: u64,
    bytes: u64,
}

async fn copy_entry(ctx: &Ctx, args: CopyArgs) -> Result<CopyResult, ToolError> {
    let from = ctx.grant().check_path(&args.from, Access::Read)?;
    let to = ctx.grant().check_path(&args.to, Access::Write)?;
    check_not_protected_tree(ctx.grant(), &to)?;
    let own_dirs = OwnDirFilter::for_ctx(ctx);
    let overwrite = args.overwrite;
    let recursive = args.recursive;

    run_blocking(ctx, "fs.copy", move |cancel, mut progress| {
        if cancel.is_cancelled() {
            return Err(cancelled_error("fs.copy"));
        }
        let from_meta =
            fs::symlink_metadata(&from).map_err(|error| map_io_error("copy", &from, error))?;
        if paths_equal(&from, &to) {
            return Err(ErrorBody::new(
                ErrorCode::InvalidArgs,
                format!(
                    "Source and destination are the same path ({}).",
                    from.display()
                ),
            ));
        }
        let mut totals = CopyTotals::default();
        copy_path(
            &from,
            &to,
            &from_meta,
            overwrite,
            recursive,
            "fs.copy",
            &own_dirs,
            &cancel,
            &mut progress,
            &mut totals,
        )?;
        Ok(CopyResult {
            from: path_to_string(&from),
            to: path_to_string(&to),
            files: totals.files,
            bytes: totals.bytes,
        })
    })
    .await
}

#[derive(Default)]
struct CopyTotals {
    files: u64,
    bytes: u64,
}

/// Reader wrapper that aborts a streaming atomic copy on cancellation and emits progress events.
struct CancelProgressReader<'a> {
    inner: File,
    cancel: &'a CancellationToken,
    progress: &'a mut Progress,
    files: u64,
    bytes_so_far: u64,
    cancelled: bool,
}

impl Read for CancelProgressReader<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.cancel.is_cancelled() {
            self.cancelled = true;
            return Err(io::Error::new(io::ErrorKind::Interrupted, "cancelled"));
        }
        let n = self.inner.read(buf)?;
        self.bytes_so_far += n as u64;
        let files = self.files;
        let bytes = self.bytes_so_far;
        self.progress
            .maybe_emit(|| json!({"files": files, "bytes": bytes}));
        Ok(n)
    }
}

#[allow(clippy::too_many_arguments)]
fn copy_path(
    from: &Path,
    to: &Path,
    from_meta: &Metadata,
    overwrite: bool,
    recursive: bool,
    tool: &str,
    own_dirs: &OwnDirFilter,
    cancel: &CancellationToken,
    progress: &mut Progress,
    totals: &mut CopyTotals,
) -> Result<(), ToolError> {
    if cancel.is_cancelled() {
        return Err(cancelled_error(tool));
    }
    if own_dirs.is_blocked(from) {
        return Err(ErrorBody::new(
            ErrorCode::Denied,
            format!(
                "Cannot copy {}: Legion's own configuration and state directories require effect:admin.",
                from.display()
            ),
        ));
    }

    if from_meta.file_type().is_symlink() {
        copy_symlink(from, to, from_meta, overwrite)?;
        totals.files += 1;
        let (files, bytes) = (totals.files, totals.bytes);
        progress.maybe_emit(|| json!({"files": files, "bytes": bytes}));
        return Ok(());
    }

    if from_meta.is_file() {
        let copied = copy_regular_file(
            from, to, from_meta, overwrite, tool, cancel, progress, totals,
        )?;
        totals.files += 1;
        totals.bytes += copied;
        let (files, bytes) = (totals.files, totals.bytes);
        progress.maybe_emit(|| json!({"files": files, "bytes": bytes}));
        return Ok(());
    }

    if from_meta.is_dir() {
        if !recursive {
            return Err(ErrorBody::new(
                ErrorCode::InvalidArgs,
                format!(
                    "{} is a directory, and `recursive` is false.",
                    from.display()
                ),
            )
            .with_hint("Pass recursive: true to copy a directory and its contents."));
        }
        if is_same_or_descendant(to, from) {
            return Err(ErrorBody::new(
                ErrorCode::InvalidArgs,
                format!(
                    "Cannot copy {} inside itself ({}).",
                    from.display(),
                    to.display()
                ),
            )
            .with_hint("Choose a destination outside the source directory."));
        }
        // Inspect destination with `symlink_metadata` so a directory symlink inside an existing
        // destination tree is never followed outside the allowed root.
        match fs::symlink_metadata(to) {
            Ok(to_meta) => {
                if !overwrite {
                    return Err(ErrorBody::new(
                        ErrorCode::Exists,
                        format!("{} already exists.", to.display()),
                    )
                    .with_hint("Pass overwrite: true to copy into an existing directory."));
                }
                if !to_meta.is_dir() {
                    return Err(ErrorBody::new(
                        ErrorCode::Conflict,
                        format!(
                            "Cannot overwrite non-directory {} with directory {}.",
                            to.display(),
                            from.display()
                        ),
                    ));
                }
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                ensure_parent_exists(to)?;
                fs::create_dir(to).map_err(|error| map_io_error("create directory", to, error))?;
            }
            Err(error) => return Err(map_io_error("inspect", to, error)),
        }

        let mut children = sorted_children(from)?;
        for (name, child_from) in children.drain(..) {
            if cancel.is_cancelled() {
                return Err(cancelled_error(tool));
            }
            let child_meta = fs::symlink_metadata(&child_from)
                .map_err(|error| map_io_error("inspect", &child_from, error))?;
            let child_to = to.join(&name);
            copy_path(
                &child_from,
                &child_to,
                &child_meta,
                overwrite,
                true,
                tool,
                own_dirs,
                cancel,
                progress,
                totals,
            )?;
        }
        if let Some(bits) = platform::unix_mode_bits(from_meta) {
            let _ = platform::set_unix_mode(to, bits);
        }
        return Ok(());
    }

    Err(ErrorBody::new(
        ErrorCode::Unsupported,
        format!("{} is a special file and cannot be copied.", from.display()),
    ))
}

fn copy_symlink(
    from: &Path,
    to: &Path,
    from_meta: &Metadata,
    overwrite: bool,
) -> Result<(), ToolError> {
    let target = fs::read_link(from).map_err(|error| map_io_error("read symlink", from, error))?;
    ensure_parent_exists(to)?;
    match fs::symlink_metadata(to) {
        Ok(to_meta) => {
            if !overwrite {
                return Err(ErrorBody::new(
                    ErrorCode::Exists,
                    format!("{} already exists.", to.display()),
                )
                .with_hint("Pass overwrite: true to replace it."));
            }
            if to_meta.is_dir() {
                return Err(ErrorBody::new(
                    ErrorCode::Conflict,
                    format!(
                        "Cannot overwrite directory {} with symbolic link {}.",
                        to.display(),
                        from.display()
                    ),
                ));
            }
            if to_meta.file_type().is_symlink() {
                platform::remove_symlink(to, &to_meta)
                    .map_err(|error| map_io_error("replace symlink", to, error))?;
            } else {
                fs::remove_file(to).map_err(|error| map_io_error("replace", to, error))?;
            }
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(map_io_error("inspect", to, error)),
    }
    platform::create_symlink(&target, to, platform::symlink_is_dir(from_meta))
        .map_err(|error| map_io_error("create symlink", to, error))
}

#[allow(clippy::too_many_arguments)]
fn copy_regular_file(
    from: &Path,
    to: &Path,
    from_meta: &Metadata,
    overwrite: bool,
    tool: &str,
    cancel: &CancellationToken,
    progress: &mut Progress,
    totals: &CopyTotals,
) -> Result<u64, ToolError> {
    ensure_parent_exists(to)?;
    let mut existed_regular = false;
    match fs::symlink_metadata(to) {
        Ok(to_meta) => {
            if !overwrite {
                return Err(ErrorBody::new(
                    ErrorCode::Exists,
                    format!("{} already exists.", to.display()),
                )
                .with_hint("Pass overwrite: true to replace it."));
            }
            if to_meta.is_dir() {
                return Err(ErrorBody::new(
                    ErrorCode::Conflict,
                    format!(
                        "Cannot overwrite directory {} with file {}.",
                        to.display(),
                        from.display()
                    ),
                ));
            }
            if to_meta.file_type().is_symlink() {
                // Remove an existing symlink before calling `write_atomic_from_reader` so it
                // cannot inspect or adopt the symlink target's metadata (or fail if the link
                // points at a directory).
                platform::remove_symlink(to, &to_meta)
                    .map_err(|error| map_io_error("replace symlink", to, error))?;
            } else {
                existed_regular = true;
            }
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(map_io_error("inspect", to, error)),
    }

    let source_file = File::open(from).map_err(|error| map_io_error("read", from, error))?;
    let mut reader = CancelProgressReader {
        inner: source_file,
        cancel,
        progress,
        files: totals.files,
        bytes_so_far: totals.bytes,
        cancelled: false,
    };
    let mode = platform::unix_mode_bits(from_meta);
    let copied = match write_atomic_from_reader(to, &mut reader, WriteOptions { mode }) {
        Ok(copied) => copied,
        Err(_) if reader.cancelled => return Err(cancelled_error(tool)),
        Err(error) => return Err(map_os_error("copy to", to, error)),
    };
    if existed_regular && let Some(bits) = mode {
        platform::set_unix_mode(to, bits)
            .map_err(|error| map_io_error("set permissions on", to, error))?;
    }
    Ok(copied)
}

/// Arguments of `fs.remove`.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct RemoveArgs {
    /// Path to remove (absolute or starting with `~`).
    path: String,
    /// Remove a directory and everything inside it.
    #[serde(default)]
    recursive: bool,
    /// Report what would be removed without changing the file system.
    #[serde(default)]
    dry_run: bool,
}

/// Result of `fs.remove`.
#[derive(Debug, Serialize)]
struct RemoveResult {
    path: String,
    removed: u64,
    bytes: u64,
    dry_run: bool,
    sample: Vec<String>,
}

async fn remove_entry(ctx: &Ctx, args: RemoveArgs) -> Result<RemoveResult, ToolError> {
    let path = ctx.grant().check_path(&args.path, Access::Write)?;
    check_not_protected_tree(ctx.grant(), &path)?;
    let recursive = args.recursive;
    let dry_run = args.dry_run;

    run_blocking(ctx, "fs.remove", move |cancel, mut progress| {
        if cancel.is_cancelled() {
            return Err(cancelled_error("fs.remove"));
        }

        let meta =
            fs::symlink_metadata(&path).map_err(|error| map_io_error("remove", &path, error))?;
        let mut totals = RemoveTotals::default();
        remove_path(
            &path,
            &meta,
            recursive,
            dry_run,
            "fs.remove",
            &cancel,
            &mut progress,
            &mut totals,
        )?;

        Ok(RemoveResult {
            path: path_to_string(&path),
            removed: totals.removed,
            bytes: totals.bytes,
            dry_run,
            sample: totals.sample,
        })
    })
    .await
}

#[derive(Default)]
struct RemoveTotals {
    removed: u64,
    bytes: u64,
    sample: Vec<String>,
}

impl RemoveTotals {
    fn record(&mut self, path: &Path, bytes: u64) {
        self.removed += 1;
        self.bytes += bytes;
        if self.sample.len() < REMOVE_SAMPLE_LIMIT {
            self.sample.push(path_to_string(path));
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn remove_path(
    path: &Path,
    meta: &Metadata,
    recursive: bool,
    dry_run: bool,
    tool: &str,
    cancel: &CancellationToken,
    progress: &mut Progress,
    totals: &mut RemoveTotals,
) -> Result<(), ToolError> {
    if cancel.is_cancelled() {
        return Err(cancelled_error(tool));
    }

    if meta.file_type().is_symlink() {
        if !dry_run {
            platform::remove_symlink(path, meta)
                .map_err(|error| map_io_error("remove symlink", path, error))?;
        }
        totals.record(path, 0);
        return Ok(());
    }

    if !meta.is_dir() {
        let size = meta.len();
        if !dry_run {
            fs::remove_file(path).map_err(|error| map_io_error("remove", path, error))?;
        }
        totals.record(path, size);
        return Ok(());
    }

    if !recursive {
        if dir_is_non_empty(path)? {
            return Err(ErrorBody::new(
                ErrorCode::Conflict,
                format!(
                    "{} is not empty, and `recursive` is false.",
                    path.display()
                ),
            )
            .with_hint(
                "Pass recursive: true (optionally with dry_run: true first) to remove a directory and its contents.",
            ));
        }
        if !dry_run {
            fs::remove_dir(path).map_err(|error| map_io_error("remove directory", path, error))?;
        }
        totals.record(path, 0);
        return Ok(());
    }

    // Record the directory first in `sample` for readability (pre-order), then remove children
    // before removing the directory itself (post-order).
    totals.record(path, 0);
    let children = sorted_children(path)?;
    for (_, child_path) in children {
        if cancel.is_cancelled() {
            return Err(cancelled_error(tool));
        }
        let child_meta = match fs::symlink_metadata(&child_path) {
            Ok(meta) => meta,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(map_io_error("inspect", &child_path, error)),
        };
        remove_path(
            &child_path,
            &child_meta,
            true,
            dry_run,
            tool,
            cancel,
            progress,
            totals,
        )?;
        let (removed, bytes) = (totals.removed, totals.bytes);
        progress.maybe_emit(|| json!({"removed": removed, "bytes": bytes, "dry_run": dry_run}));
    }
    if !dry_run {
        fs::remove_dir(path).map_err(|error| map_io_error("remove directory", path, error))?;
    }
    Ok(())
}

fn ensure_parent_exists(path: &Path) -> Result<(), ToolError> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
        && !parent.exists()
    {
        return Err(ErrorBody::new(
            ErrorCode::NotFound,
            format!("Parent directory {} does not exist.", parent.display()),
        )
        .with_hint("Create the destination parent directory first with fs.mkdir."));
    }
    Ok(())
}

fn dir_is_non_empty(path: &Path) -> Result<bool, ToolError> {
    let mut entries = fs::read_dir(path).map_err(|error| map_io_error("read", path, error))?;
    match entries.next() {
        Some(Ok(_)) => Ok(true),
        Some(Err(error)) => Err(map_io_error("read", path, error)),
        None => Ok(false),
    }
}

fn sorted_children(dir: &Path) -> Result<Vec<(std::ffi::OsString, PathBuf)>, ToolError> {
    let mut children = Vec::new();
    for entry in fs::read_dir(dir).map_err(|error| map_io_error("read", dir, error))? {
        let entry = entry.map_err(|error| map_io_error("read", dir, error))?;
        children.push((entry.file_name(), entry.path()));
    }
    children.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(children)
}
