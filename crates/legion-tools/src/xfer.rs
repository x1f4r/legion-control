//! `fs.manifest`, and what both ends of a transfer share with it: how a tree is listed for a
//! grant, how a root is resolved, and how the transfer engine's errors read to a caller.
//!
//! `fs.transfer` and `legion stream` live in `legion-net`, which reaches other machines; the
//! listing they compare is the one `fs.manifest` shows, so a caller can look at a tree exactly the
//! way a transfer sees it. Keeping that here keeps the two from drifting apart.

use std::fs;
use std::path::{Path, PathBuf};

use legion_core::proto::{Effect, ErrorBody, ErrorCode};
use legion_core::{Access, Ctx, Grant, Paths, Registry, ToolError};
use legion_xfer::{Entry, Manifest, ManifestOptions, SkippedEntry};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::json;

/// How many entries `fs.manifest` returns when the caller names no limit.
const DEFAULT_LIMIT: u32 = 1000;
/// The most entries one `fs.manifest` reply carries; a bigger tree is for `fs.transfer` itself.
const MAX_LIMIT: u32 = 10_000;

/// Adds `fs.manifest`.
pub fn register(registry: &mut Registry) {
    registry.add_fn(
        "fs.manifest",
        Effect::Read,
        "List a directory tree (or one file) the way fs.transfer compares it: each entry's relative path, kind, size, modification time, mode, link target and, with checksum, its BLAKE3 hash. Bounded by limit.",
        manifest_tool,
    );
}

/// Arguments of `fs.manifest`.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct ManifestArgs {
    /// Directory or file to list (absolute or starting with `~`).
    path: String,
    /// Hash every regular file with BLAKE3, which reads all of them.
    #[serde(default)]
    checksum: bool,
    /// Globs of paths to leave out, relative to `path`; a trailing `/` matches directories only.
    #[serde(default)]
    exclude: Vec<String>,
    /// Most entries to return, from 1 to 10000.
    #[serde(default = "default_limit")]
    #[schemars(range(min = 1, max = 10000))]
    limit: u32,
}

fn default_limit() -> u32 {
    DEFAULT_LIMIT
}

/// What `fs.manifest` returns.
#[derive(Debug, Serialize)]
struct ManifestResult {
    path: String,
    entries: Vec<Entry>,
    total: usize,
    truncated: bool,
    skipped: Vec<SkippedEntry>,
    skipped_total: usize,
}

async fn manifest_tool(ctx: &Ctx, args: ManifestArgs) -> Result<ManifestResult, ToolError> {
    if !(1..=MAX_LIMIT).contains(&args.limit) {
        return Err(ErrorBody::new(
            ErrorCode::InvalidArgs,
            format!(
                "fs.manifest limit must be between 1 and {MAX_LIMIT}, and {} was given.",
                args.limit
            ),
        )
        .with_hint(
            "Use fs.transfer to copy a bigger tree; it compares the whole listing itself.",
        ));
    }
    let root = resolve_root(ctx.grant(), &args.path, Access::Read)?;
    let grant = ctx.grant().clone();
    let paths = ctx.paths().clone();
    let options = ManifestOptions {
        checksum: args.checksum,
        exclude: args.exclude,
    };
    let limit = usize::try_from(args.limit).unwrap_or(usize::MAX);
    let listed = root.clone();
    let work = tokio::task::spawn_blocking(move || manifest(&grant, &paths, &listed, &options));
    // Listing only reads, so a cancelled call can leave a walk to finish on its own.
    let manifest = tokio::select! {
        joined = work => joined.map_err(|error| {
            ErrorBody::new(ErrorCode::Internal, format!("fs.manifest stopped unexpectedly: {error}."))
        })??,
        () = ctx.cancelled() => {
            return Err(ErrorBody::new(
                ErrorCode::Cancelled,
                "The call to fs.manifest was cancelled before it finished.",
            ));
        }
    };
    let total = manifest.entries.len();
    let skipped_total = manifest.skipped.len();
    let mut entries = manifest.entries;
    entries.truncate(limit);
    let mut skipped = manifest.skipped;
    skipped.truncate(limit);
    Ok(ManifestResult {
        path: root.to_string_lossy().into_owned(),
        truncated: total > entries.len() || skipped_total > skipped.len(),
        entries,
        total,
        skipped,
        skipped_total,
    })
}

/// Checks a root a transfer lists, reads or writes, and follows it when it is a symbolic link.
///
/// [`Grant::check_path`] keeps a last part that is a link as the link, which is right for a tool
/// that removes or renames it; a transfer means the tree the link leads to, as `cp -r link/` does,
/// so the target is resolved and checked again in its own right. A link that leads nowhere is left
/// as it is, for the operation to report.
pub fn resolve_root(grant: &Grant, path: &str, access: Access) -> Result<PathBuf, ErrorBody> {
    let checked = grant.check_path(path, access)?;
    match fs::symlink_metadata(&checked) {
        Ok(meta) if meta.file_type().is_symlink() => match dunce::canonicalize(&checked) {
            Ok(target) => grant.check_path(&target, access),
            Err(_) => Ok(checked),
        },
        _ => Ok(checked),
    }
}

/// Lists `root`, already checked with [`resolve_root`], the way a transfer compares it.
///
/// A role without `effect:admin` may list a tree that holds Legion's own configuration or state
/// (a home directory does), but not what is in them: the walk does not go into them at all, so
/// nothing there is read, hashed or named, and each is named in `skipped` instead, just as every
/// file tool refuses them one by one.
pub fn manifest(
    grant: &Grant,
    _paths: &Paths,
    root: &Path,
    options: &ManifestOptions,
) -> Result<Manifest, ErrorBody> {
    if grant.allows_effect(Effect::Admin) {
        return legion_xfer::manifest::build(root, options).map_err(error);
    }
    let filter = grant.protected_filter();
    if filter.is_empty() {
        return legion_xfer::manifest::build(root, options).map_err(error);
    }
    let prune = |entry_path: &Path| {
        filter
            .blocked(entry_path)
            .map(|(_, reason)| reason.to_owned())
    };
    let mut manifest = legion_xfer::manifest::build_pruned(root, options, prune).map_err(error)?;
    // Belt-and-suspenders: strip any entry under a protected path if a walk still produced one.
    if let Ok(canonical_root) = dunce::canonicalize(root) {
        manifest
            .entries
            .retain(|entry| !filter.is_blocked(&canonical_root.join(&entry.path)));
    }
    Ok(manifest)
}

/// The transfer engine's error as a caller reads it: the closed code an agent branches on and a
/// sentence a person can act on.
pub fn error(error: legion_xfer::Error) -> ErrorBody {
    use legion_xfer::Error;
    match error {
        Error::NotFound { path, .. } => ErrorBody::new(
            ErrorCode::NotFound,
            format!("{} does not exist.", path.display()),
        ),
        Error::PermissionDenied { path, .. } => ErrorBody::new(
            ErrorCode::Denied,
            format!("The operating system refused access to {}.", path.display()),
        )
        .with_hint("Check the permissions of the file for the account legion runs as."),
        Error::Exists { path, .. } => ErrorBody::new(
            ErrorCode::Exists,
            format!("{} already exists.", path.display()),
        ),
        Error::Conflict { path, reason } => ErrorBody::new(
            ErrorCode::Conflict,
            format!("{} cannot be used: {reason}.", path.display()),
        ),
        Error::HashMismatch {
            path,
            expected,
            actual,
        } => ErrorBody::new(
            ErrorCode::Conflict,
            format!(
                "The bytes received for {} do not match the source's hash.",
                path.display()
            ),
        )
        .with_hint("The source probably changed while it was sent; send it again.")
        .with_details(json!({"expected": expected, "actual": actual})),
        Error::BadOffset {
            path,
            requested,
            actual,
        } => ErrorBody::new(
            ErrorCode::Conflict,
            format!(
                "{} cannot be continued at byte {requested}: there are {actual} bytes.",
                path.display()
            ),
        )
        .with_details(json!({"requested": requested, "actual": actual})),
        Error::UnsafeArchiveEntry { entry, reason } => ErrorBody::new(
            ErrorCode::InvalidArgs,
            format!("{entry} cannot be transferred: {reason}."),
        )
        .with_hint("Rename it, or leave it out with exclude."),
        Error::InvalidHash { input, reason } => ErrorBody::new(
            ErrorCode::InvalidArgs,
            format!("{input} is not a hash: {reason}."),
        )
        .with_hint("Hashes are written blake3:<64 hex digits>."),
        Error::InvalidPattern { pattern, source } => ErrorBody::new(
            ErrorCode::InvalidArgs,
            format!("The exclude pattern {pattern} is not a valid glob: {source}."),
        ),
        Error::Io { path, source } => ErrorBody::new(
            ErrorCode::Failed,
            match path {
                Some(path) => format!("Reading or writing {} failed: {source}.", path.display()),
                None => format!("The transfer stream failed: {source}."),
            },
        ),
    }
}
