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
    let allows_destroy = ctx.grant().allows_effect(Effect::Destroy);
    let principal = ctx.principal().to_owned();
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
                if to_meta.is_dir() && !allows_destroy {
                    return Err(deny_replace_directory(&principal, &to));
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
                let root_dev = platform::device_id(&from, &from_meta);
                let same_dev = |p: &Path, m: &Metadata| platform::same_device(&root_dev, p, m);
                let mut totals = CopyTotals::default();
                let opts = CopyOptions {
                    overwrite,
                    recursive: true,
                    allows_destroy,
                    principal: &principal,
                    tool: "fs.move",
                    own_dirs: &own_dirs,
                    same_device: &same_dev,
                };
                copy_path(&from, &to, &from_meta, &opts, &cancel, &mut progress, &mut totals)?;
                if !totals.skipped.is_empty() {
                    return Err(ErrorBody::new(
                        ErrorCode::Conflict,
                        format!(
                            "Cross-device move of {} to {} skipped {} nested directory(s) on another file system, so source {} was not removed.",
                            from.display(),
                            to.display(),
                            totals.skipped.len(),
                            from.display()
                        ),
                    )
                    .with_hint("Move or unmount nested file systems separately before moving the parent directory.")
                    .with_details(json!({
                        "copy_completed": true,
                        "source_intact": true,
                        "files": totals.files,
                        "bytes": totals.bytes,
                        "skipped": totals.skipped,
                    })));
                }
                let mut removed = RemoveTotals::default();
                remove_path(
                    &from,
                    &from_meta,
                    true,
                    false,
                    "fs.move",
                    &same_dev,
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
    #[serde(skip_serializing_if = "Vec::is_empty")]
    skipped: Vec<String>,
}

async fn copy_entry(ctx: &Ctx, args: CopyArgs) -> Result<CopyResult, ToolError> {
    let from = ctx.grant().check_path(&args.from, Access::Read)?;
    let to = ctx.grant().check_path(&args.to, Access::Write)?;
    check_not_protected_tree(ctx.grant(), &to)?;
    let own_dirs = OwnDirFilter::for_ctx(ctx);
    let allows_destroy = ctx.grant().allows_effect(Effect::Destroy);
    let principal = ctx.principal().to_owned();
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
        let root_dev = platform::device_id(&from, &from_meta);
        let same_dev = |p: &Path, m: &Metadata| platform::same_device(&root_dev, p, m);
        let mut totals = CopyTotals::default();
        let opts = CopyOptions {
            overwrite,
            recursive,
            allows_destroy,
            principal: &principal,
            tool: "fs.copy",
            own_dirs: &own_dirs,
            same_device: &same_dev,
        };
        copy_path(
            &from,
            &to,
            &from_meta,
            &opts,
            &cancel,
            &mut progress,
            &mut totals,
        )
        .map_err(|err| {
            if err.code == ErrorCode::Cancelled {
                let mut details = json!({
                    "files": totals.files,
                    "bytes": totals.bytes,
                });
                if !totals.skipped.is_empty()
                    && let Some(obj) = details.as_object_mut()
                {
                    obj.insert("skipped".to_owned(), json!(totals.skipped));
                }
                err.with_details(details)
            } else {
                err
            }
        })?;
        Ok(CopyResult {
            from: path_to_string(&from),
            to: path_to_string(&to),
            files: totals.files,
            bytes: totals.bytes,
            skipped: totals.skipped,
        })
    })
    .await
}

struct CopyOptions<'a> {
    overwrite: bool,
    recursive: bool,
    allows_destroy: bool,
    principal: &'a str,
    tool: &'a str,
    own_dirs: &'a OwnDirFilter,
    same_device: &'a dyn Fn(&Path, &Metadata) -> bool,
}

fn deny_replace_directory(principal: &str, path: &Path) -> ErrorBody {
    ErrorBody::new(
        ErrorCode::Denied,
        format!(
            "{principal} may not replace existing directory {}: overwriting a directory requires effect:destroy.",
            path.display()
        ),
    )
    .with_hint(
        "Ask the owner to allow effect:destroy for this role, or choose a destination that does not already exist.",
    )
}

#[derive(Default)]
struct CopyTotals {
    files: u64,
    bytes: u64,
    skipped: Vec<String>,
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
            // Must not be `ErrorKind::Interrupted`, which `std::io::copy` silently retries.
            return Err(io::Error::other("cancelled"));
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

fn copy_path(
    from: &Path,
    to: &Path,
    from_meta: &Metadata,
    opts: &CopyOptions<'_>,
    cancel: &CancellationToken,
    progress: &mut Progress,
    totals: &mut CopyTotals,
) -> Result<(), ToolError> {
    if cancel.is_cancelled() {
        return Err(cancelled_error(opts.tool));
    }
    if opts.own_dirs.is_blocked(from) {
        return Err(ErrorBody::new(
            ErrorCode::Denied,
            format!(
                "Cannot copy {}: Legion's own configuration and state directories require effect:admin.",
                from.display()
            ),
        ));
    }

    if from_meta.file_type().is_symlink() {
        copy_symlink(from, to, from_meta, opts)?;
        totals.files += 1;
        let (files, bytes) = (totals.files, totals.bytes);
        progress.maybe_emit(|| json!({"files": files, "bytes": bytes}));
        return Ok(());
    }

    if from_meta.is_file() {
        let copied = copy_regular_file(from, to, from_meta, opts, cancel, progress, totals)?;
        totals.files += 1;
        totals.bytes += copied;
        let (files, bytes) = (totals.files, totals.bytes);
        progress.maybe_emit(|| json!({"files": files, "bytes": bytes}));
        return Ok(());
    }

    if from_meta.is_dir() {
        if !opts.recursive {
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
                if !opts.overwrite {
                    return Err(ErrorBody::new(
                        ErrorCode::Exists,
                        format!("{} already exists.", to.display()),
                    )
                    .with_hint("Pass overwrite: true to copy into an existing directory."));
                }
                if to_meta.is_dir() && !opts.allows_destroy {
                    return Err(deny_replace_directory(opts.principal, to));
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
                return Err(cancelled_error(opts.tool));
            }
            let child_meta = fs::symlink_metadata(&child_from)
                .map_err(|error| map_io_error("inspect", &child_from, error))?;
            if child_meta.is_dir() && !(opts.same_device)(&child_from, &child_meta) {
                totals.skipped.push(path_to_string(&child_from));
                continue;
            }
            let child_to = to.join(&name);
            copy_path(
                &child_from,
                &child_to,
                &child_meta,
                opts,
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
    opts: &CopyOptions<'_>,
) -> Result<(), ToolError> {
    let target = fs::read_link(from).map_err(|error| map_io_error("read symlink", from, error))?;
    ensure_parent_exists(to)?;
    match fs::symlink_metadata(to) {
        Ok(to_meta) => {
            if !opts.overwrite {
                return Err(ErrorBody::new(
                    ErrorCode::Exists,
                    format!("{} already exists.", to.display()),
                )
                .with_hint("Pass overwrite: true to replace it."));
            }
            if to_meta.is_dir() {
                if !opts.allows_destroy {
                    return Err(deny_replace_directory(opts.principal, to));
                }
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

fn copy_regular_file(
    from: &Path,
    to: &Path,
    from_meta: &Metadata,
    opts: &CopyOptions<'_>,
    cancel: &CancellationToken,
    progress: &mut Progress,
    totals: &CopyTotals,
) -> Result<u64, ToolError> {
    ensure_parent_exists(to)?;
    let mut existed_regular = false;
    match fs::symlink_metadata(to) {
        Ok(to_meta) => {
            if !opts.overwrite {
                return Err(ErrorBody::new(
                    ErrorCode::Exists,
                    format!("{} already exists.", to.display()),
                )
                .with_hint("Pass overwrite: true to replace it."));
            }
            if to_meta.is_dir() {
                if !opts.allows_destroy {
                    return Err(deny_replace_directory(opts.principal, to));
                }
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
    let write_opts = WriteOptions {
        mode,
        no_replace: !opts.overwrite,
    };
    let copied = match write_atomic_from_reader(to, &mut reader, write_opts) {
        Ok(copied) => copied,
        Err(_) if reader.cancelled => return Err(cancelled_error(opts.tool)),
        Err(OsError::AlreadyExists { path }) => {
            return Err(ErrorBody::new(
                ErrorCode::Exists,
                format!("{} already exists.", path.display()),
            )
            .with_hint("Pass overwrite: true to replace it."));
        }
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
    #[serde(skip_serializing_if = "Vec::is_empty")]
    skipped: Vec<String>,
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
        let root_dev = platform::device_id(&path, &meta);
        let same_dev = |p: &Path, m: &Metadata| platform::same_device(&root_dev, p, m);
        execute_remove(
            &path,
            &meta,
            recursive,
            dry_run,
            &same_dev,
            &cancel,
            &mut progress,
        )
    })
    .await
}

fn execute_remove(
    path: &Path,
    meta: &Metadata,
    recursive: bool,
    dry_run: bool,
    same_device: &dyn Fn(&Path, &Metadata) -> bool,
    cancel: &CancellationToken,
    progress: &mut Progress,
) -> Result<RemoveResult, ToolError> {
    if recursive && !dry_run && meta.is_dir() {
        let mut cross_device = Vec::new();
        scan_cross_device_dirs(path, same_device, cancel, "fs.remove", &mut cross_device)?;
        if !cross_device.is_empty() {
            return Err(cross_device_remove_conflict(path, 0, 0, cross_device));
        }
    }

    let mut totals = RemoveTotals::default();
    remove_path(
        path,
        meta,
        recursive,
        dry_run,
        "fs.remove",
        same_device,
        cancel,
        progress,
        &mut totals,
    )
    .map_err(|err| {
        if err.code == ErrorCode::Cancelled {
            err.with_details(json!({
                "removed": totals.removed,
                "bytes": totals.bytes,
                "dry_run": dry_run,
                "sample": totals.sample,
            }))
        } else {
            err
        }
    })?;

    if !dry_run && !totals.skipped.is_empty() {
        return Err(cross_device_remove_conflict(
            path,
            totals.removed,
            totals.bytes,
            totals.skipped,
        ));
    }

    Ok(RemoveResult {
        path: path_to_string(path),
        removed: totals.removed,
        bytes: totals.bytes,
        dry_run,
        sample: totals.sample,
        skipped: totals.skipped,
    })
}

fn cross_device_remove_conflict(
    path: &Path,
    removed: u64,
    bytes: u64,
    skipped: Vec<String>,
) -> ErrorBody {
    let first = skipped.first().map(String::as_str).unwrap_or("");
    let message = if skipped.len() == 1 {
        format!(
            "Refusing to remove {} recursively: {} is on a different file system.",
            path.display(),
            first
        )
    } else {
        format!(
            "Refusing to remove {} recursively: {} and {} other directory(s) are on different file systems.",
            path.display(),
            first,
            skipped.len() - 1
        )
    };
    ErrorBody::new(ErrorCode::Conflict, message)
        .with_hint("Unmount or remove the nested file-system directories separately so fs.remove never crosses mount boundaries.")
        .with_details(json!({
            "removed": removed,
            "bytes": bytes,
            "skipped": skipped,
        }))
}

fn scan_cross_device_dirs(
    dir: &Path,
    same_device: &dyn Fn(&Path, &Metadata) -> bool,
    cancel: &CancellationToken,
    tool: &str,
    found: &mut Vec<String>,
) -> Result<(), ToolError> {
    if cancel.is_cancelled() {
        return Err(cancelled_error(tool));
    }
    for (_, child_path) in sorted_children(dir)? {
        if cancel.is_cancelled() {
            return Err(cancelled_error(tool));
        }
        let child_meta = match fs::symlink_metadata(&child_path) {
            Ok(meta) => meta,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(map_io_error("inspect", &child_path, error)),
        };
        if child_meta.is_dir() {
            if !same_device(&child_path, &child_meta) {
                found.push(path_to_string(&child_path));
            } else {
                scan_cross_device_dirs(&child_path, same_device, cancel, tool, found)?;
            }
        }
    }
    Ok(())
}

#[derive(Default)]
struct RemoveTotals {
    removed: u64,
    bytes: u64,
    sample: Vec<String>,
    skipped: Vec<String>,
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
    same_device: &dyn Fn(&Path, &Metadata) -> bool,
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

    if dry_run {
        totals.record(path, 0);
    }
    let skipped_before = totals.skipped.len();
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
        if child_meta.is_dir() && !same_device(&child_path, &child_meta) {
            totals.skipped.push(path_to_string(&child_path));
            continue;
        }
        remove_path(
            &child_path,
            &child_meta,
            true,
            dry_run,
            tool,
            same_device,
            cancel,
            progress,
            totals,
        )?;
        let (removed, bytes) = (totals.removed, totals.bytes);
        progress.maybe_emit(|| json!({"removed": removed, "bytes": bytes, "dry_run": dry_run}));
    }
    if !dry_run && totals.skipped.len() == skipped_before {
        if cancel.is_cancelled() {
            return Err(cancelled_error(tool));
        }
        fs::remove_dir(path).map_err(|error| map_io_error("remove directory", path, error))?;
        totals.record(path, 0);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cancelled_remove_only_counts_entries_actually_deleted() {
        let dir = tempfile::tempdir().unwrap();
        let tree = dir.path().join("tree");
        fs::create_dir_all(&tree).unwrap();
        fs::write(tree.join("a.txt"), "1234").unwrap();
        fs::write(tree.join("b.txt"), "5678").unwrap();

        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let mut progress = Progress::for_test(tx);
        let cancel = CancellationToken::new();
        cancel.cancel();

        let meta = fs::symlink_metadata(&tree).unwrap();
        let mut totals = RemoveTotals::default();
        let all_same = |_p: &Path, _m: &Metadata| true;
        let err = remove_path(
            &tree,
            &meta,
            true,
            false,
            "fs.remove",
            &all_same,
            &cancel,
            &mut progress,
            &mut totals,
        )
        .unwrap_err();
        assert_eq!(err.code, ErrorCode::Cancelled);
        assert_eq!(totals.removed, 0);
        assert_eq!(totals.bytes, 0);
        assert!(tree.join("a.txt").exists());
    }

    #[test]
    fn copy_and_remove_refuse_to_cross_into_another_file_system() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src");
        let mnt = src.join("mnt");
        let sub = src.join("sub");
        fs::create_dir_all(&mnt).unwrap();
        fs::create_dir_all(&sub).unwrap();
        fs::write(src.join("a.txt"), "aaa").unwrap();
        fs::write(sub.join("b.txt"), "bb").unwrap();
        fs::write(mnt.join("mounted.txt"), "do-not-touch").unwrap();

        let same_dev = |p: &Path, _m: &Metadata| p != mnt.as_path();

        // `copy_path` copies same-device entries, skips `mnt`, and reports `mnt` in `skipped`.
        let dst = dir.path().join("dst");
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let mut progress = Progress::for_test(tx);
        let cancel = CancellationToken::new();
        let own_dirs = OwnDirFilter::allow_all_for_test();
        let opts = CopyOptions {
            overwrite: false,
            recursive: true,
            allows_destroy: true,
            principal: "owner",
            tool: "fs.copy",
            own_dirs: &own_dirs,
            same_device: &same_dev,
        };
        let src_meta = fs::symlink_metadata(&src).unwrap();
        let mut copy_totals = CopyTotals::default();
        copy_path(
            &src,
            &dst,
            &src_meta,
            &opts,
            &cancel,
            &mut progress,
            &mut copy_totals,
        )
        .unwrap();
        assert_eq!(copy_totals.files, 2);
        assert_eq!(copy_totals.bytes, 5);
        assert_eq!(copy_totals.skipped, vec![path_to_string(&mnt)]);
        assert!(!dst.join("mnt").exists());
        assert_eq!(
            fs::read_to_string(dst.join("sub").join("b.txt")).unwrap(),
            "bb"
        );

        // `execute_remove` dry_run reports `mnt` as skipped and does not count `mounted.txt`.
        let preview = execute_remove(
            &src,
            &src_meta,
            true,
            true,
            &same_dev,
            &cancel,
            &mut progress,
        )
        .unwrap();
        assert_eq!(preview.skipped, vec![path_to_string(&mnt)]);
        assert_eq!(preview.bytes, 5);

        // Live recursive remove fails with `conflict` before deleting anything, leaving `src` intact.
        let err = execute_remove(
            &src,
            &src_meta,
            true,
            false,
            &same_dev,
            &cancel,
            &mut progress,
        )
        .unwrap_err();
        assert_eq!(err.code, ErrorCode::Conflict);
        assert_eq!(
            err.details.as_ref().and_then(|d| d.get("skipped")),
            Some(&json!([path_to_string(&mnt)]))
        );
        assert!(src.join("a.txt").exists());
        assert!(mnt.join("mounted.txt").exists());
    }
}
