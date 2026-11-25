//! `fav`: fleet-wide starred and pinned files and folders stored in `favorites.toml`.
//!
//! `favorites.toml` sits beside `policy.toml` as a shared, mergeable document. Every mutable field
//! carries its own `(updated_at, updated_on)` stamp so two nodes editing different fields of the
//! same entry while apart keep both edits when they exchange the file, and removals are stamped
//! tombstones kept for 90 days. Merging is a join-semilattice: commutative, associative, and
//! idempotent.
//!
//! Unlike `devices.toml`, favourites have no bearing on access control, so any node that changes the
//! list pushes it to reachable peers with node authority and `fleet.sync` carries it without
//! owner-only restrictions. Every incoming file is strictly validated and size-bounded on receipt,
//! and `fav.list` omits entries outside the caller's `paths`.

use std::collections::BTreeMap;
use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hasher};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use jiff::Timestamp;
use legion_core::config::{Document, parse, write_atomically};
use legion_core::policy::{OWNER, Principal};
use legion_core::proto::{Effect, ErrorBody, ErrorCode};
use legion_core::{Access, Call, Ctx, Door, Node, Paths, Registry, ToolError};
use legion_os::OsError;
use legion_os::lock::{FileLock, LockMode};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use toml_edit::{DocumentMut, Item, Table, value};

use crate::device::registry::{FleetPushOutcome, fleet_peer_names};

/// Schema version of `favorites.toml`.
pub const FAVORITES_VERSION: u32 = 1;

/// Maximum serialized size of `favorites.toml` accepted from any peer or written locally (256 KiB).
pub const MAX_FAVORITES_BYTES: usize = 256 * 1024;

/// Maximum number of live favourite entries allowed in `favorites.toml`.
pub const MAX_LIVE_FAVORITES: usize = 1000;

/// Maximum number of tombstone records allowed in `favorites.toml`.
pub const MAX_REMOVED_FAVORITES: usize = 2000;

/// Days a removal tombstone is retained before automatic pruning.
pub const TOMBSTONE_RETENTION_DAYS: i64 = 90;

/// Maximum byte length of a stored favourite path.
pub const MAX_PATH_BYTES: usize = 4096;

/// Maximum Unicode scalar values in a favourite's optional display label.
pub const MAX_LABEL_CHARS: usize = 120;

/// Per-peer timeout used when pushing `favorites.toml` after a local mutation.
const FAV_PUSH_TIMEOUT_S: f64 = 5.0;

/// Maximum time to wait for `<state_dir>/sync/favorites.lock`.
const LOCK_TIMEOUT: Duration = Duration::from_secs(10);

/// Monotonic counter mixed into generated favourite IDs.
static ID_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Registers `fav.list`, `fav.add`, `fav.set`, `fav.remove` and `favorites.toml`.
pub fn register(registry: &mut Registry) {
    registry.add_document::<FavoritesFile>();
    registry.add_fn(
        "fav.list",
        Effect::Read,
        "List fleet-wide starred and pinned files and folders from `favorites.toml`. Filter by `machine` or `pinned`. Returns `{id, machine, path, kind, category?, label?, pinned, added_at, added_by}` and, cheaply, `exists` for entries on this node. Omits entries outside the caller's `paths`.",
        list,
    );
    registry.add_fn(
        "fav.add",
        Effect::Write,
        "Star (and optionally pin) a file or folder in the fleet-wide `favorites.toml` list and push the update to reachable peers. `machine` defaults to this node; an entry may name any fleet machine. Adding a path that is already starred updates its label or pin flag instead of creating a duplicate.",
        add,
    );
    registry.add_fn(
        "fav.set",
        Effect::Write,
        "Update a favourite's optional `label` (pass an empty string to clear it) or `pinned` flag in `favorites.toml` and push the update to reachable peers.",
        set,
    );
    registry.add_fn(
        "fav.remove",
        Effect::Write,
        "Remove a favourite by `id`, recording a stamped 90-day tombstone in `favorites.toml` and pushing the update to reachable peers.",
        remove,
    );
}

/// Whether a favourite points to a file or a directory.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum FavoriteKind {
    /// A regular file.
    File,
    /// A directory / folder.
    Dir,
}

/// On-disk `favorites.toml` shared document.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FavoritesFile {
    /// Schema version (`1`).
    #[serde(default = "default_version")]
    pub version: u32,
    /// Live starred/pinned entries keyed by stable favourite `id`.
    #[serde(default)]
    pub entries: BTreeMap<String, FavoriteRecord>,
    /// Stamped removal tombstones keyed by `id`, kept for 90 days.
    #[serde(default)]
    pub removed: BTreeMap<String, FavoriteTombstone>,
}

fn default_version() -> u32 {
    FAVORITES_VERSION
}

impl Default for FavoritesFile {
    fn default() -> Self {
        Self {
            version: FAVORITES_VERSION,
            entries: BTreeMap::new(),
            removed: BTreeMap::new(),
        }
    }
}

/// One live entry stored in `favorites.toml`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FavoriteRecord {
    /// Fleet machine that holds `path`.
    pub machine: String,
    /// Absolute or `~`-prefixed path on `machine`.
    pub path: String,
    /// Whether this entry is a file or directory.
    pub kind: FavoriteKind,
    /// Optional human-readable label.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// Whether this favourite is pinned to the top of Files.
    #[serde(default)]
    pub pinned: bool,
    /// RFC 3339 UTC timestamp when first added.
    pub added_at: String,
    /// Principal that originally added this entry.
    pub added_by: String,
    /// Latest RFC 3339 UTC timestamp of any change to this entry.
    pub updated_at: String,
    /// Node that recorded `updated_at`.
    pub updated_on: String,
    /// RFC 3339 UTC timestamp of the latest `label` change.
    pub label_updated_at: String,
    /// Node that recorded `label_updated_at`.
    pub label_updated_on: String,
    /// RFC 3339 UTC timestamp of the latest `pinned` change.
    pub pinned_updated_at: String,
    /// Node that recorded `pinned_updated_on`.
    pub pinned_updated_on: String,
}

/// Stamped tombstone stored under `[removed.<id>]` in `favorites.toml`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FavoriteTombstone {
    /// RFC 3339 UTC timestamp when removed.
    pub removed_at: String,
    /// Node that recorded the removal.
    pub removed_on: String,
}

impl Document for FavoritesFile {
    const FILE: &'static str = "favorites.toml";

    fn validate(&self) -> Result<(), String> {
        if self.version != FAVORITES_VERSION {
            return Err(format!(
                "unsupported favorites.toml version {} (expected {FAVORITES_VERSION})",
                self.version
            ));
        }
        if self.entries.len() > MAX_LIVE_FAVORITES {
            return Err(format!(
                "favorites.toml holds {} entries, exceeding the maximum of {MAX_LIVE_FAVORITES}",
                self.entries.len()
            ));
        }
        if self.removed.len() > MAX_REMOVED_FAVORITES {
            return Err(format!(
                "favorites.toml holds {} tombstones, exceeding the maximum of {MAX_REMOVED_FAVORITES}",
                self.removed.len()
            ));
        }
        for (id, entry) in &self.entries {
            validate_id(id)?;
            if self.removed.contains_key(id) {
                return Err(format!(
                    "favourite id {id:?} appears in both [entries] and [removed]"
                ));
            }
            validate_machine_name(&entry.machine)
                .map_err(|reason| format!("entry {id:?}: machine {:?}: {reason}", entry.machine))?;
            validate_path(&entry.path).map_err(|reason| format!("entry {id:?}: path: {reason}"))?;
            if let Some(ref label) = entry.label {
                validate_label(label).map_err(|reason| format!("entry {id:?}: label: {reason}"))?;
            }
            Principal::try_from(entry.added_by.clone())
                .map_err(|reason| format!("entry {id:?}: added_by: {reason}"))?;
            validate_utc_timestamp(&entry.added_at)
                .map_err(|reason| format!("entry {id:?}: added_at: {reason}"))?;
            validate_utc_timestamp(&entry.updated_at)
                .map_err(|reason| format!("entry {id:?}: updated_at: {reason}"))?;
            validate_node_stamp(&entry.updated_on)
                .map_err(|reason| format!("entry {id:?}: updated_on: {reason}"))?;
            validate_utc_timestamp(&entry.label_updated_at)
                .map_err(|reason| format!("entry {id:?}: label_updated_at: {reason}"))?;
            validate_node_stamp(&entry.label_updated_on)
                .map_err(|reason| format!("entry {id:?}: label_updated_on: {reason}"))?;
            validate_utc_timestamp(&entry.pinned_updated_at)
                .map_err(|reason| format!("entry {id:?}: pinned_updated_at: {reason}"))?;
            validate_node_stamp(&entry.pinned_updated_on)
                .map_err(|reason| format!("entry {id:?}: pinned_updated_on: {reason}"))?;
        }
        for (id, tomb) in &self.removed {
            validate_id(id)?;
            validate_utc_timestamp(&tomb.removed_at)
                .map_err(|reason| format!("removed {id:?}: removed_at: {reason}"))?;
            validate_node_stamp(&tomb.removed_on)
                .map_err(|reason| format!("removed {id:?}: removed_on: {reason}"))?;
        }
        Ok(())
    }
}

impl FavoritesFile {
    /// Serializes this document deterministically as canonical TOML.
    pub fn to_toml_string(&self) -> String {
        let mut doc = DocumentMut::new();
        doc["version"] = value(i64::from(self.version));
        if !self.entries.is_empty() {
            let mut entries_table = Table::new();
            entries_table.set_implicit(true);
            for (id, rec) in &self.entries {
                let mut t = Table::new();
                t["machine"] = value(rec.machine.as_str());
                t["path"] = value(rec.path.as_str());
                t["kind"] = value(match rec.kind {
                    FavoriteKind::File => "file",
                    FavoriteKind::Dir => "dir",
                });
                if let Some(ref label) = rec.label {
                    t["label"] = value(label.as_str());
                }
                t["pinned"] = value(rec.pinned);
                t["added_at"] = value(rec.added_at.as_str());
                t["added_by"] = value(rec.added_by.as_str());
                t["updated_at"] = value(rec.updated_at.as_str());
                t["updated_on"] = value(rec.updated_on.as_str());
                t["label_updated_at"] = value(rec.label_updated_at.as_str());
                t["label_updated_on"] = value(rec.label_updated_on.as_str());
                t["pinned_updated_at"] = value(rec.pinned_updated_at.as_str());
                t["pinned_updated_on"] = value(rec.pinned_updated_on.as_str());
                entries_table.insert(id, Item::Table(t));
            }
            doc.insert("entries", Item::Table(entries_table));
        }
        if !self.removed.is_empty() {
            let mut removed_table = Table::new();
            removed_table.set_implicit(true);
            for (id, tomb) in &self.removed {
                let mut t = Table::new();
                t["removed_at"] = value(tomb.removed_at.as_str());
                t["removed_on"] = value(tomb.removed_on.as_str());
                removed_table.insert(id, Item::Table(t));
            }
            doc.insert("removed", Item::Table(removed_table));
        }
        doc.to_string()
    }
}

/// Returns the error for a `favorites.toml` document exceeding [`MAX_FAVORITES_BYTES`].
pub fn favorites_size_limit_error(actual_bytes: usize) -> ErrorBody {
    ErrorBody::new(
        ErrorCode::InvalidArgs,
        format!(
            "favorites.toml exceeds the {MAX_FAVORITES_BYTES} byte limit ({actual_bytes} bytes)."
        ),
    )
    .with_hint("Remove unused favourites so favorites.toml stays within its size limit.")
}

/// Parses and validates raw `favorites.toml` bytes, enforcing [`MAX_FAVORITES_BYTES`].
pub fn parse_favorites_bytes(path: &Path, bytes: &[u8]) -> Result<FavoritesFile, ErrorBody> {
    if bytes.len() > MAX_FAVORITES_BYTES {
        return Err(favorites_size_limit_error(bytes.len()));
    }
    parse::<FavoritesFile>(path, bytes)
}

/// Merges two `favorites.toml` documents using a join-semilattice:
/// - Removals (tombstones) always win over live entries for the same `id`; two tombstones keep the
///   newer `(removed_at, removed_on)`.
/// - Live entries with the same `id` merge field by field (`label` and `pinned` independently) by
///   comparing `(timestamp, updated_on, value)` so ties break deterministically.
///
/// This function is strictly commutative (`merge(a, b) == merge(b, a)`), associative
/// (`merge(merge(a, b), c) == merge(a, merge(b, c))`), and idempotent (`merge(a, a) == a`).
pub fn merge_favorites(a: &FavoritesFile, b: &FavoritesFile) -> FavoritesFile {
    let mut out = FavoritesFile {
        version: FAVORITES_VERSION,
        entries: BTreeMap::new(),
        removed: BTreeMap::new(),
    };

    // First, collect every tombstoned id from either side; tombstones dominate live entries.
    for (id, tomb_a) in &a.removed {
        let chosen = match b.removed.get(id) {
            Some(tomb_b) => merge_tombstone(tomb_a, tomb_b),
            None => tomb_a.clone(),
        };
        out.removed.insert(id.clone(), chosen);
    }
    for (id, tomb_b) in &b.removed {
        if !out.removed.contains_key(id) {
            out.removed.insert(id.clone(), tomb_b.clone());
        }
    }

    // Next, merge live entries that are not tombstoned on either side.
    for (id, entry_a) in &a.entries {
        if out.removed.contains_key(id) {
            continue;
        }
        let chosen = match b.entries.get(id) {
            Some(entry_b) => merge_record(entry_a, entry_b),
            None => entry_a.clone(),
        };
        out.entries.insert(id.clone(), chosen);
    }
    for (id, entry_b) in &b.entries {
        if out.removed.contains_key(id) || out.entries.contains_key(id) {
            continue;
        }
        out.entries.insert(id.clone(), entry_b.clone());
    }

    out
}

fn stamp_key<'a>(at: &'a str, on: &'a str) -> (i128, &'a str, &'a str) {
    let nanos = at
        .parse::<Timestamp>()
        .map(Timestamp::as_nanosecond)
        .unwrap_or(i128::MIN);
    (nanos, at, on)
}

fn merge_tombstone(a: &FavoriteTombstone, b: &FavoriteTombstone) -> FavoriteTombstone {
    if stamp_key(&a.removed_at, &a.removed_on) >= stamp_key(&b.removed_at, &b.removed_on) {
        a.clone()
    } else {
        b.clone()
    }
}

fn merge_record(a: &FavoriteRecord, b: &FavoriteRecord) -> FavoriteRecord {
    // Base immutable-ish fields tie-break deterministically so the join is a total order.
    fn base_key(r: &FavoriteRecord) -> ((i128, &str, &str), &str, &str, FavoriteKind) {
        (
            stamp_key(&r.added_at, &r.added_by),
            r.machine.as_str(),
            r.path.as_str(),
            r.kind,
        )
    }
    fn label_key(r: &FavoriteRecord) -> ((i128, &str, &str), Option<&str>) {
        (
            stamp_key(&r.label_updated_at, &r.label_updated_on),
            r.label.as_deref(),
        )
    }
    fn pinned_key(r: &FavoriteRecord) -> ((i128, &str, &str), bool) {
        (
            stamp_key(&r.pinned_updated_at, &r.pinned_updated_on),
            r.pinned,
        )
    }
    let base = if base_key(a) >= base_key(b) { a } else { b };
    let label_src = if label_key(a) >= label_key(b) { a } else { b };
    let pinned_src = if pinned_key(a) >= pinned_key(b) { a } else { b };

    let (updated_at, updated_on) =
        if stamp_key(&a.updated_at, &a.updated_on) >= stamp_key(&b.updated_at, &b.updated_on) {
            (a.updated_at.as_str(), a.updated_on.as_str())
        } else {
            (b.updated_at.as_str(), b.updated_on.as_str())
        };

    FavoriteRecord {
        machine: base.machine.clone(),
        path: base.path.clone(),
        kind: base.kind,
        label: label_src.label.clone(),
        pinned: pinned_src.pinned,
        added_at: base.added_at.clone(),
        added_by: base.added_by.clone(),
        updated_at: updated_at.to_owned(),
        updated_on: updated_on.to_owned(),
        label_updated_at: label_src.label_updated_at.clone(),
        label_updated_on: label_src.label_updated_on.clone(),
        pinned_updated_at: pinned_src.pinned_updated_at.clone(),
        pinned_updated_on: pinned_src.pinned_updated_on.clone(),
    }
}

/// Removes tombstones whose `removed_at` is strictly older than `TOMBSTONE_RETENTION_DAYS` (90
/// days) relative to `now`. Returns how many tombstones were pruned.
pub fn prune_expired_tombstones(file: &mut FavoritesFile, now: Timestamp) -> usize {
    let retention_secs = TOMBSTONE_RETENTION_DAYS * 24 * 60 * 60;
    let cutoff_nanos = now
        .as_nanosecond()
        .saturating_sub(i128::from(retention_secs) * 1_000_000_000);
    let before = file.removed.len();
    file.removed.retain(|_, tomb| {
        tomb.removed_at
            .parse::<Timestamp>()
            .is_ok_and(|ts| ts.as_nanosecond() >= cutoff_nanos)
    });
    before - file.removed.len()
}

/// Outcome of atomically merging and saving `favorites.toml`.
#[derive(Clone, Debug)]
pub struct FavoritesMergeOutcome {
    /// Bytes on disk before the merge, or `None` if `favorites.toml` did not exist.
    pub bytes_before: Option<Vec<u8>>,
    /// Canonical TOML written to `favorites.toml`.
    pub canonical_toml: String,
    /// Merged document after pruning expired tombstones.
    pub favorites: FavoritesFile,
    /// Backup path under `<state_dir>/sync/backups/favorites.toml.bak`, if a previous file existed.
    pub backup: Option<String>,
}

/// Acquires `<state_dir>/sync/favorites.lock`, checks `check_precondition`, merges `incoming` into
/// local `favorites.toml`, prunes expired tombstones, validates bounds, backs up any previous file,
/// and writes the merged file atomically.
pub fn apply_favorites_merge_blocking<F>(
    paths: &Paths,
    incoming: &FavoritesFile,
    check_precondition: F,
) -> Result<FavoritesMergeOutcome, ErrorBody>
where
    F: FnOnce(Option<&[u8]>) -> Result<(), ErrorBody>,
{
    let lock_path = favorites_lock_path(paths);
    let _held = acquire_favorites_lock(&lock_path)?;

    let target = paths.config_file(FavoritesFile::FILE);
    let bytes_before = read_optional_bytes(&target)?;
    check_precondition(bytes_before.as_deref())?;

    let local = match bytes_before.as_deref() {
        Some(bytes) => parse_favorites_bytes(&target, bytes)?,
        None => FavoritesFile::default(),
    };
    let mut merged = merge_favorites(&local, incoming);
    prune_expired_tombstones(&mut merged, Timestamp::now());
    merged.validate().map_err(|reason| {
        ErrorBody::new(
            ErrorCode::InvalidArgs,
            format!("Merged favorites.toml is invalid: {reason}."),
        )
    })?;
    let canonical_toml = merged.to_toml_string();
    if canonical_toml.len() > MAX_FAVORITES_BYTES {
        return Err(favorites_size_limit_error(canonical_toml.len()));
    }

    let backup = write_backup_if_present(paths, bytes_before.as_deref())?;
    write_favorites_atomically(&target, &canonical_toml)?;

    Ok(FavoritesMergeOutcome {
        bytes_before,
        canonical_toml,
        favorites: merged,
        backup,
    })
}

fn favorites_lock_path(paths: &Paths) -> PathBuf {
    paths.state_dir().join("sync").join("favorites.lock")
}

fn acquire_favorites_lock(lock_path: &Path) -> Result<FileLock, ErrorBody> {
    if let Some(parent) = lock_path.parent() {
        std::fs::create_dir_all(parent).map_err(|err| {
            ErrorBody::new(
                ErrorCode::Failed,
                format!("{} cannot be created: {err}.", parent.display()),
            )
        })?;
    }
    FileLock::acquire_timeout(lock_path, LockMode::Exclusive, LOCK_TIMEOUT).map_err(|err| match err
    {
        OsError::TimedOut { .. } => ErrorBody::new(
            ErrorCode::Busy,
            "Another call has been updating favorites.toml for too long.",
        )
        .with_hint("Try again in a moment."),
        other => ErrorBody::new(
            ErrorCode::Failed,
            format!("{} cannot be locked: {other}.", lock_path.display()),
        ),
    })
}

fn read_optional_bytes(path: &Path) -> Result<Option<Vec<u8>>, ErrorBody> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(ErrorBody::new(
            ErrorCode::Failed,
            format!("{} cannot be read: {err}.", path.display()),
        )),
    }
}

fn write_backup_if_present(
    paths: &Paths,
    bytes_before: Option<&[u8]>,
) -> Result<Option<String>, ErrorBody> {
    let Some(old_bytes) = bytes_before else {
        return Ok(None);
    };
    let backup_path = paths
        .state_dir()
        .join("sync")
        .join("backups")
        .join("favorites.toml.bak");
    if let Some(parent) = backup_path.parent() {
        std::fs::create_dir_all(parent).map_err(|err| {
            ErrorBody::new(
                ErrorCode::Failed,
                format!("{} cannot be created: {err}.", parent.display()),
            )
        })?;
    }
    write_atomically(&backup_path, old_bytes).map_err(|err| {
        ErrorBody::new(
            ErrorCode::Failed,
            format!("Backup {} cannot be written: {err}.", backup_path.display()),
        )
    })?;
    Ok(Some(backup_path.display().to_string()))
}

fn write_favorites_atomically(target: &Path, canonical_toml: &str) -> Result<(), ErrorBody> {
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent).map_err(|err| {
            ErrorBody::new(
                ErrorCode::Failed,
                format!("{} cannot be created: {err}.", parent.display()),
            )
        })?;
    }
    write_atomically(target, canonical_toml.as_bytes()).map_err(|err| {
        ErrorBody::new(
            ErrorCode::Failed,
            format!("{} cannot be written: {err}.", target.display()),
        )
    })
}

fn load_favorites(paths: &Paths) -> Result<FavoritesFile, ErrorBody> {
    let target = paths.config_file(FavoritesFile::FILE);
    match read_optional_bytes(&target)? {
        Some(bytes) => parse_favorites_bytes(&target, &bytes),
        None => Ok(FavoritesFile::default()),
    }
}

/// Entry view returned by `fav.list`, `fav.add`, and `fav.set`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FavoriteView {
    /// Stable favourite identifier.
    pub id: String,
    /// Machine holding this path.
    pub machine: String,
    /// Path on `machine`.
    pub path: String,
    /// `file` or `dir`.
    pub kind: FavoriteKind,
    /// Extension-based category for files (`image`, `video`, `audio`, `document`, `text`,
    /// `archive`, `other`); omitted for directories.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub category: Option<&'static str>,
    /// Optional human-readable label.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// Whether pinned to the top of Files.
    pub pinned: bool,
    /// RFC 3339 UTC timestamp when added.
    pub added_at: String,
    /// Principal that added the favourite.
    pub added_by: String,
    /// Whether the path currently exists on disk, reported cheaply when `machine` is this node.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exists: Option<bool>,
}

/// Maps a file path's extension (case-insensitive) to its category per `docs/legion/files.md`.
pub fn category_for_path(path: &str) -> &'static str {
    let file_name = path
        .trim_end_matches(['/', '\\'])
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or("");
    let ext = file_name
        .rsplit_once('.')
        .filter(|(stem, _)| !stem.is_empty())
        .map(|(_, ext)| ext.to_ascii_lowercase());
    let Some(ext) = ext else {
        return "other";
    };
    match ext.as_str() {
        "jpg" | "jpeg" | "png" | "gif" | "webp" | "avif" | "heic" | "heif" | "bmp" | "svg"
        | "tif" | "tiff" | "ico" | "jxl" => "image",
        "mp4" | "m4v" | "mov" | "webm" | "mkv" | "avi" | "wmv" | "flv" | "m2ts" | "3gp" | "ogv"
        | "mpg" | "mpeg" => "video",
        "mp3" | "m4a" | "aac" | "flac" | "ogg" | "opus" | "wav" | "wma" | "aiff" | "aif"
        | "mid" | "midi" => "audio",
        "pdf" | "epub" | "doc" | "docx" | "xls" | "xlsx" | "ppt" | "pptx" | "odt" | "ods"
        | "odp" | "rtf" | "pages" | "numbers" | "key" => "document",
        "txt" | "md" | "markdown" | "log" | "csv" | "tsv" | "json" | "jsonl" | "toml" | "yaml"
        | "yml" | "xml" | "html" | "htm" | "css" | "scss" | "less" | "js" | "mjs" | "cjs"
        | "ts" | "tsx" | "jsx" | "rs" | "py" | "go" | "c" | "h" | "cpp" | "hpp" | "cc" | "hh"
        | "java" | "kt" | "swift" | "rb" | "php" | "sh" | "bash" | "zsh" | "fish" | "ps1"
        | "sql" | "ini" | "cfg" | "conf" | "env" | "diff" | "patch" | "tex" | "rst" | "org"
        | "nix" | "lua" | "zig" | "r" | "dart" | "ex" | "exs" | "erl" | "hs" | "ml" | "scala"
        | "groovy" | "gradle" | "cmake" => "text",
        "zip" | "tar" | "gz" | "tgz" | "bz2" | "xz" | "zst" | "zstd" | "7z" | "rar" | "lz4"
        | "cab" | "iso" | "dmg" | "deb" | "rpm" | "apk" => "archive",
        _ => "other",
    }
}

fn view_of(
    id: &str,
    record: &FavoriteRecord,
    here: &str,
    local_checked: Option<&Path>,
) -> FavoriteView {
    let exists = if record.machine == here {
        local_checked.map(|p| std::fs::metadata(p).is_ok())
    } else {
        None
    };
    FavoriteView {
        id: id.to_owned(),
        machine: record.machine.clone(),
        path: record.path.clone(),
        kind: record.kind,
        category: (record.kind == FavoriteKind::File).then(|| category_for_path(&record.path)),
        label: record.label.clone(),
        pinned: record.pinned,
        added_at: record.added_at.clone(),
        added_by: record.added_by.clone(),
        exists,
    }
}

/// Checks whether a favourite entry is visible to `ctx.grant()`, returning the checked local path
/// when `record.machine == ctx.node_name()`.
fn check_visibility(ctx: &Ctx, record: &FavoriteRecord) -> Option<Option<PathBuf>> {
    if record.machine == ctx.node_name() {
        let checked = ctx.grant().check_path(&record.path, Access::Read).ok()?;
        Some(Some(checked))
    } else if ctx.grant().roots().is_some() {
        // A caller confined by `paths` only sees entries whose path falls inside its allowed roots.
        ctx.grant().check_path(&record.path, Access::Read).ok()?;
        Some(None)
    } else {
        Some(None)
    }
}

/// Arguments of `fav.list`.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct ListArgs {
    /// Keep only entries on this fleet machine.
    #[serde(default)]
    machine: Option<String>,
    /// When set, keep only pinned (`true`) or unpinned (`false`) entries.
    #[serde(default)]
    pinned: Option<bool>,
}

/// Result of `fav.list`.
#[derive(Debug, Serialize)]
struct ListResult {
    entries: Vec<FavoriteView>,
}

async fn list(ctx: &Ctx, args: ListArgs) -> Result<ListResult, ToolError> {
    if let Some(ref machine) = args.machine {
        validate_machine_name(machine).map_err(|reason| {
            ErrorBody::new(
                ErrorCode::InvalidArgs,
                format!("fav.list: `machine` {machine:?} is not valid: {reason}."),
            )
        })?;
    }
    let paths = ctx.paths().clone();
    let favorites = blocking("fav.list", move || load_favorites(&paths)).await?;
    let here = ctx.node_name();
    let mut entries = Vec::new();
    for (id, record) in &favorites.entries {
        if let Some(ref want_machine) = args.machine
            && record.machine != *want_machine
        {
            continue;
        }
        if let Some(want_pinned) = args.pinned
            && record.pinned != want_pinned
        {
            continue;
        }
        let Some(local_checked) = check_visibility(ctx, record) else {
            continue;
        };
        entries.push(view_of(id, record, here, local_checked.as_deref()));
    }
    // Pinned first, then newest added first, then id for deterministic ordering.
    entries.sort_by(|a, b| {
        b.pinned
            .cmp(&a.pinned)
            .then_with(|| b.added_at.cmp(&a.added_at))
            .then_with(|| a.id.cmp(&b.id))
    });
    Ok(ListResult { entries })
}

/// Arguments of `fav.add`.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct AddArgs {
    /// Fleet machine holding `path`; defaults to this node.
    #[serde(default)]
    machine: Option<String>,
    /// Absolute or `~`-prefixed path to star.
    path: String,
    /// Whether `path` is a `file` or `dir`. Detected automatically for existing paths on this node.
    #[serde(default)]
    kind: Option<FavoriteKind>,
    /// Optional display label.
    #[serde(default)]
    label: Option<String>,
    /// Pin this favourite to the top of Files (default `false`).
    #[serde(default)]
    pinned: bool,
}

/// Result of `fav.add`.
#[derive(Debug, Serialize)]
struct AddResult {
    entry: FavoriteView,
    existing: bool,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    fleet: BTreeMap<String, FleetPushOutcome>,
}

async fn add(ctx: &Ctx, args: AddArgs) -> Result<AddResult, ToolError> {
    let here = ctx.node_name().to_owned();
    let machine = match args.machine {
        Some(m) => {
            validate_machine_name(&m).map_err(|reason| {
                ErrorBody::new(
                    ErrorCode::InvalidArgs,
                    format!("fav.add: `machine` {m:?} is not valid: {reason}."),
                )
            })?;
            m
        }
        None => here.clone(),
    };
    let had_trailing_sep = args.path.ends_with('/') || args.path.ends_with('\\');
    let normalized_path = normalize_path(&args.path).map_err(|reason| {
        ErrorBody::new(
            ErrorCode::InvalidArgs,
            format!("fav.add: `path`: {reason}."),
        )
    })?;
    let label = match args.label {
        Some(raw) => {
            let trimmed = raw.trim().to_owned();
            if trimmed.is_empty() {
                None
            } else {
                validate_label(&trimmed).map_err(|reason| {
                    ErrorBody::new(
                        ErrorCode::InvalidArgs,
                        format!("fav.add: `label`: {reason}."),
                    )
                })?;
                Some(trimmed)
            }
        }
        None => None,
    };

    let local_checked = if machine == here {
        Some(ctx.grant().check_path(&normalized_path, Access::Read)?)
    } else if ctx.grant().roots().is_some() {
        ctx.grant().check_path(&normalized_path, Access::Read)?;
        None
    } else {
        None
    };

    let detected_kind = match args.kind {
        Some(kind) => kind,
        None => match local_checked.as_deref() {
            Some(local_path) => match std::fs::metadata(local_path) {
                Ok(meta) if meta.is_dir() => FavoriteKind::Dir,
                Ok(_) => FavoriteKind::File,
                Err(_) if had_trailing_sep => FavoriteKind::Dir,
                Err(_) => FavoriteKind::File,
            },
            None => {
                if had_trailing_sep {
                    FavoriteKind::Dir
                } else {
                    FavoriteKind::File
                }
            }
        },
    };

    let paths = ctx.paths().clone();
    let principal = ctx.principal().to_owned();
    let node_name = here.clone();
    let pinned_arg = args.pinned;
    let (id, record, existing) = blocking("fav.add", move || {
        let lock_path = favorites_lock_path(&paths);
        let _held = acquire_favorites_lock(&lock_path)?;

        let target = paths.config_file(FavoritesFile::FILE);
        let bytes_before = read_optional_bytes(&target)?;
        let mut file = match bytes_before.as_deref() {
            Some(bytes) => parse_favorites_bytes(&target, bytes)?,
            None => FavoritesFile::default(),
        };
        let now = Timestamp::now();
        prune_expired_tombstones(&mut file, now);

        // Idempotent add: if `(machine, path)` is already live, update label/pinned stamps in place.
        let existing_id = file
            .entries
            .iter()
            .find(|(_, rec)| rec.machine == machine && rec.path == normalized_path)
            .map(|(id, _)| id.clone());

        let (id, record, existing) = if let Some(existing_id) = existing_id {
            let rec = file.entries.get_mut(&existing_id).ok_or_else(|| {
                ErrorBody::new(ErrorCode::Internal, "Favourite entry disappeared under lock.")
            })?;
            let stamp = next_stamp(now, &rec.updated_at);
            if label.is_some() && label != rec.label {
                rec.label = label;
                rec.label_updated_at = stamp.clone();
                rec.label_updated_on = node_name.clone();
            }
            if pinned_arg && !rec.pinned {
                rec.pinned = true;
                rec.pinned_updated_at = stamp.clone();
                rec.pinned_updated_on = node_name.clone();
            }
            rec.updated_at = stamp;
            rec.updated_on = node_name;
            (existing_id, rec.clone(), true)
        } else {
            if file.entries.len() >= MAX_LIVE_FAVORITES {
                return Err(ErrorBody::new(
                    ErrorCode::Failed,
                    format!(
                        "Cannot add favourite: favorites.toml already holds the maximum of {MAX_LIVE_FAVORITES} entries."
                    ),
                ));
            }
            let id = generate_fav_id(&file);
            let stamp = format_timestamp(now);
            let record = FavoriteRecord {
                machine,
                path: normalized_path,
                kind: detected_kind,
                label,
                pinned: pinned_arg,
                added_at: stamp.clone(),
                added_by: principal,
                updated_at: stamp.clone(),
                updated_on: node_name.clone(),
                label_updated_at: stamp.clone(),
                label_updated_on: node_name.clone(),
                pinned_updated_at: stamp,
                pinned_updated_on: node_name,
            };
            file.entries.insert(id.clone(), record.clone());
            (id, record, false)
        };

        file.validate().map_err(|reason| {
            ErrorBody::new(
                ErrorCode::InvalidArgs,
                format!("favorites.toml validation failed: {reason}."),
            )
        })?;
        let canonical = file.to_toml_string();
        if canonical.len() > MAX_FAVORITES_BYTES {
            return Err(favorites_size_limit_error(canonical.len()));
        }
        write_backup_if_present(&paths, bytes_before.as_deref())?;
        write_favorites_atomically(&target, &canonical)?;
        Ok((id, record, existing))
    })
    .await?;

    let fleet = push_favorites_to_fleet(
        ctx.node(),
        ctx.door(),
        ctx.device().map(str::to_owned),
        ctx.credential_expires(),
    )
    .await;

    Ok(AddResult {
        entry: view_of(&id, &record, &here, local_checked.as_deref()),
        existing,
        fleet,
    })
}

/// Arguments of `fav.set`.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct SetArgs {
    /// Identifier of the favourite returned by `fav.list` or `fav.add`.
    id: String,
    /// New display label; pass `""` to clear the label. Omit to leave unchanged.
    #[serde(default)]
    label: Option<String>,
    /// Pin (`true`) or unpin (`false`). Omit to leave unchanged.
    #[serde(default)]
    pinned: Option<bool>,
}

/// Result of `fav.set`.
#[derive(Debug, Serialize)]
struct SetResult {
    entry: FavoriteView,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    fleet: BTreeMap<String, FleetPushOutcome>,
}

async fn set(ctx: &Ctx, args: SetArgs) -> Result<SetResult, ToolError> {
    validate_id(&args.id).map_err(|reason| {
        ErrorBody::new(ErrorCode::InvalidArgs, format!("fav.set: `id`: {reason}."))
    })?;
    if args.label.is_none() && args.pinned.is_none() {
        return Err(ErrorBody::new(
            ErrorCode::InvalidArgs,
            "fav.set requires at least one of `label` or `pinned`.",
        )
        .with_hint("Pass `label` and/or `pinned` to update the favourite."));
    }
    let next_label = match args.label {
        None => None,
        Some(raw) => {
            let trimmed = raw.trim().to_owned();
            if trimmed.is_empty() {
                Some(None)
            } else {
                validate_label(&trimmed).map_err(|reason| {
                    ErrorBody::new(
                        ErrorCode::InvalidArgs,
                        format!("fav.set: `label`: {reason}."),
                    )
                })?;
                Some(Some(trimmed))
            }
        }
    };

    // Check visibility against the caller's grant before mutating.
    let paths_for_check = ctx.paths().clone();
    let existing_file = blocking("fav.set", move || load_favorites(&paths_for_check)).await?;
    let Some(preview) = existing_file.entries.get(&args.id) else {
        return Err(not_found_fav(&args.id));
    };
    let Some(local_checked) = check_visibility(ctx, preview) else {
        return Err(not_found_fav(&args.id));
    };

    let here = ctx.node_name().to_owned();
    let node_name = here.clone();
    let id = args.id.clone();
    let next_pinned = args.pinned;
    let paths = ctx.paths().clone();

    let updated = blocking("fav.set", move || {
        let lock_path = favorites_lock_path(&paths);
        let _held = acquire_favorites_lock(&lock_path)?;
        let target = paths.config_file(FavoritesFile::FILE);
        let bytes_before = read_optional_bytes(&target)?;
        let mut file = match bytes_before.as_deref() {
            Some(bytes) => parse_favorites_bytes(&target, bytes)?,
            None => FavoritesFile::default(),
        };
        let now = Timestamp::now();
        prune_expired_tombstones(&mut file, now);

        let rec = file
            .entries
            .get_mut(&id)
            .ok_or_else(|| not_found_fav(&id))?;
        let stamp = next_stamp(now, &rec.updated_at);
        if let Some(label_update) = next_label {
            rec.label = label_update;
            rec.label_updated_at = stamp.clone();
            rec.label_updated_on = node_name.clone();
        }
        if let Some(pinned_update) = next_pinned {
            rec.pinned = pinned_update;
            rec.pinned_updated_at = stamp.clone();
            rec.pinned_updated_on = node_name.clone();
        }
        rec.updated_at = stamp;
        rec.updated_on = node_name;
        let out = rec.clone();

        file.validate().map_err(|reason| {
            ErrorBody::new(
                ErrorCode::InvalidArgs,
                format!("favorites.toml validation failed: {reason}."),
            )
        })?;
        let canonical = file.to_toml_string();
        if canonical.len() > MAX_FAVORITES_BYTES {
            return Err(favorites_size_limit_error(canonical.len()));
        }
        write_backup_if_present(&paths, bytes_before.as_deref())?;
        write_favorites_atomically(&target, &canonical)?;
        Ok(out)
    })
    .await?;

    let fleet = push_favorites_to_fleet(
        ctx.node(),
        ctx.door(),
        ctx.device().map(str::to_owned),
        ctx.credential_expires(),
    )
    .await;

    Ok(SetResult {
        entry: view_of(&args.id, &updated, &here, local_checked.as_deref()),
        fleet,
    })
}

/// Arguments of `fav.remove`.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct RemoveArgs {
    /// Identifier of the favourite to remove.
    id: String,
}

/// Result of `fav.remove`.
#[derive(Debug, Serialize)]
struct RemoveResult {
    id: String,
    removed: bool,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    fleet: BTreeMap<String, FleetPushOutcome>,
}

async fn remove(ctx: &Ctx, args: RemoveArgs) -> Result<RemoveResult, ToolError> {
    validate_id(&args.id).map_err(|reason| {
        ErrorBody::new(
            ErrorCode::InvalidArgs,
            format!("fav.remove: `id`: {reason}."),
        )
    })?;

    // If the entry is currently live, ensure the caller is allowed to see it before removing.
    let paths_for_check = ctx.paths().clone();
    let existing_file = blocking("fav.remove", move || load_favorites(&paths_for_check)).await?;
    if let Some(preview) = existing_file.entries.get(&args.id)
        && check_visibility(ctx, preview).is_none()
    {
        return Err(not_found_fav(&args.id));
    }

    let id = args.id.clone();
    let node_name = ctx.node_name().to_owned();
    let paths = ctx.paths().clone();

    let changed = blocking("fav.remove", move || {
        let lock_path = favorites_lock_path(&paths);
        let _held = acquire_favorites_lock(&lock_path)?;
        let target = paths.config_file(FavoritesFile::FILE);
        let bytes_before = read_optional_bytes(&target)?;
        let mut file = match bytes_before.as_deref() {
            Some(bytes) => parse_favorites_bytes(&target, bytes)?,
            None => FavoritesFile::default(),
        };
        let now = Timestamp::now();
        prune_expired_tombstones(&mut file, now);

        if let Some(prev) = file.entries.remove(&id) {
            if file.removed.len() >= MAX_REMOVED_FAVORITES {
                // Drop the oldest tombstone to stay within bounds.
                if let Some(oldest_id) = file
                    .removed
                    .iter()
                    .min_by(|(_, a), (_, b)| {
                        stamp_key(&a.removed_at, &a.removed_on)
                            .cmp(&stamp_key(&b.removed_at, &b.removed_on))
                    })
                    .map(|(k, _)| k.clone())
                {
                    file.removed.remove(&oldest_id);
                }
            }
            let stamp = next_stamp(now, &prev.updated_at);
            file.removed.insert(
                id,
                FavoriteTombstone {
                    removed_at: stamp,
                    removed_on: node_name,
                },
            );
            let canonical = file.to_toml_string();
            if canonical.len() > MAX_FAVORITES_BYTES {
                return Err(favorites_size_limit_error(canonical.len()));
            }
            write_backup_if_present(&paths, bytes_before.as_deref())?;
            write_favorites_atomically(&target, &canonical)?;
            Ok(true)
        } else if file.removed.contains_key(&id) {
            Ok(false)
        } else {
            Err(not_found_fav(&id))
        }
    })
    .await?;

    let fleet = push_favorites_to_fleet(
        ctx.node(),
        ctx.door(),
        ctx.device().map(str::to_owned),
        ctx.credential_expires(),
    )
    .await;

    Ok(RemoveResult {
        id: args.id,
        removed: changed,
        fleet,
    })
}

fn not_found_fav(id: &str) -> ErrorBody {
    ErrorBody::new(
        ErrorCode::NotFound,
        format!("There is no favourite with id {id:?}."),
    )
    .with_hint("Run `fav.list` to see the current favourites.")
    .with_details(json!({"id": id}))
}

/// Pushes `favorites.toml` to every reachable peer in `fleet.toml` with node authority.
pub async fn push_favorites_to_fleet(
    node: &Node,
    door: Door,
    device: Option<String>,
    credential_expires: Option<Timestamp>,
) -> BTreeMap<String, FleetPushOutcome> {
    let peers = fleet_peer_names(node.paths(), &node.name());
    if peers.is_empty() {
        return BTreeMap::new();
    }
    let all_not_reached = |reason: &str| -> BTreeMap<String, FleetPushOutcome> {
        peers
            .iter()
            .map(|peer| (peer.clone(), FleetPushOutcome::not_reached(reason)))
            .collect()
    };
    if node.registry().get("fleet.sync").is_none() {
        return all_not_reached(
            "fleet.sync is not available on this node; run `legion fleet.sync direction=push documents='[\"favorites.toml\"]'` manually",
        );
    }
    let args = json!({
        "direction": "push",
        "documents": [FavoritesFile::FILE],
        "timeout_s": FAV_PUSH_TIMEOUT_S,
    });
    let mut call = Call::new(door, "fleet.sync", args);
    call.pinned = Some(OWNER.to_owned());
    call.device = device;
    call.credential_expires = credential_expires;
    let reply = node.call(call).await;
    match reply {
        Ok(value) => {
            let mut outcomes = parse_favorites_push_outcomes(&value);
            for peer in &peers {
                outcomes.entry(peer.clone()).or_insert_with(|| {
                    FleetPushOutcome::not_reached("machine did not finish syncing favorites.toml")
                });
            }
            outcomes
        }
        Err(error) => all_not_reached(&error.message),
    }
}

fn parse_favorites_push_outcomes(sync_result: &Value) -> BTreeMap<String, FleetPushOutcome> {
    let mut out = BTreeMap::new();
    let Some(results) = sync_result.get("results").and_then(Value::as_object) else {
        return out;
    };
    for (machine, docs) in results {
        let doc_outcome = docs.get(FavoritesFile::FILE);
        let status = doc_outcome
            .and_then(|d| d.get("status"))
            .and_then(Value::as_str)
            .unwrap_or("error");
        let outcome = if matches!(status, "same" | "pushed" | "pulled") {
            FleetPushOutcome::synced()
        } else {
            let reason = doc_outcome
                .and_then(|d| {
                    d.get("error")
                        .and_then(|e| e.get("message"))
                        .and_then(Value::as_str)
                        .or_else(|| d.get("detail").and_then(Value::as_str))
                })
                .unwrap_or("machine did not finish syncing favorites.toml");
            FleetPushOutcome::not_reached(reason)
        };
        out.insert(machine.clone(), outcome);
    }
    out
}

fn format_timestamp(ts: Timestamp) -> String {
    ts.strftime("%Y-%m-%dT%H:%M:%S%.3fZ").to_string()
}

fn next_stamp(now: Timestamp, prev: &str) -> String {
    let min_next = prev
        .parse::<Timestamp>()
        .ok()
        .and_then(|p| p.checked_add(jiff::SignedDuration::from_millis(1)).ok());
    let chosen = match min_next {
        Some(next) if next > now => next,
        _ => now,
    };
    format_timestamp(chosen)
}

fn generate_fav_id(file: &FavoritesFile) -> String {
    for _ in 0..64 {
        let since_epoch = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default();
        let nanos = u64::try_from(since_epoch.as_nanos()).unwrap_or(u64::MAX);
        let seq = ID_COUNTER.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        let mut hasher = RandomState::new().build_hasher();
        hasher.write_u64(nanos);
        hasher.write_u64(seq);
        hasher.write_u32(pid);
        let a = hasher.finish();
        let b = RandomState::new().build_hasher().finish();
        let mut material = [0u8; 32];
        material[0..8].copy_from_slice(&nanos.to_be_bytes());
        material[8..16].copy_from_slice(&seq.to_be_bytes());
        material[16..24].copy_from_slice(&a.to_be_bytes());
        material[24..32].copy_from_slice(&b.to_be_bytes());
        let digest = blake3::hash(&material);
        let mut id = String::with_capacity(24);
        for byte in &digest.as_bytes()[..12] {
            use std::fmt::Write as _;
            let _ = write!(id, "{byte:02x}");
        }
        if !file.entries.contains_key(&id) && !file.removed.contains_key(&id) {
            return id;
        }
    }
    format!("fav-{}", ID_COUNTER.fetch_add(1, Ordering::Relaxed))
}

fn validate_id(id: &str) -> Result<(), String> {
    let ok = !id.is_empty()
        && id.len() <= 64
        && id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'-' | b'_'));
    if !ok {
        return Err(format!(
            "{id:?} is not a valid favourite id (use 1..=64 lowercase letters, digits, - or _)"
        ));
    }
    Ok(())
}

fn validate_machine_name(name: &str) -> Result<(), String> {
    let ok = !name.is_empty()
        && name.len() <= 64
        && name
            .bytes()
            .next()
            .is_some_and(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'-' | b'_'));
    if !ok {
        return Err(
            "use 1..=64 lowercase letters, digits, - or _, starting with a letter or digit"
                .to_owned(),
        );
    }
    Ok(())
}

fn validate_node_stamp(name: &str) -> Result<(), String> {
    let ok = !name.is_empty()
        && name.len() <= 64
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'));
    if !ok {
        return Err(format!("{name:?} is not a valid node stamp name"));
    }
    Ok(())
}

fn is_absolute_or_tilde(path: &str) -> bool {
    if path == "~" || path.starts_with("~/") || path.starts_with("~\\") || path.starts_with('/') {
        return true;
    }
    let bytes = path.as_bytes();
    bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && (bytes[2] == b'\\' || bytes[2] == b'/')
}

fn normalize_path(raw: &str) -> Result<String, String> {
    validate_path(raw)?;
    let is_drive_root = raw.len() == 3
        && raw.as_bytes()[0].is_ascii_alphabetic()
        && raw.as_bytes()[1] == b':'
        && (raw.as_bytes()[2] == b'\\' || raw.as_bytes()[2] == b'/');
    let trimmed = if raw == "/" || raw == "~" || is_drive_root {
        raw.to_owned()
    } else {
        let stripped = raw.trim_end_matches(['/', '\\']);
        if stripped.is_empty() {
            "/".to_owned()
        } else if stripped.len() == 2
            && stripped.as_bytes()[0].is_ascii_alphabetic()
            && stripped.as_bytes()[1] == b':'
        {
            format!("{stripped}\\")
        } else {
            stripped.to_owned()
        }
    };
    Ok(trimmed)
}

fn validate_path(path: &str) -> Result<(), String> {
    if path.is_empty() {
        return Err("path must not be empty".to_owned());
    }
    if path.len() > MAX_PATH_BYTES {
        return Err(format!("path exceeds the {MAX_PATH_BYTES} byte limit"));
    }
    if path.chars().any(char::is_control) {
        return Err("path must not contain control characters".to_owned());
    }
    if !is_absolute_or_tilde(path) {
        return Err("path must be absolute or start with ~".to_owned());
    }
    if path.split(['/', '\\']).any(|seg| seg == "..") {
        return Err("path must not contain `..` segments".to_owned());
    }
    Ok(())
}

fn validate_label(label: &str) -> Result<(), String> {
    if label.trim().is_empty() {
        return Err("label must not be blank".to_owned());
    }
    if label.chars().count() > MAX_LABEL_CHARS {
        return Err(format!(
            "label exceeds the {MAX_LABEL_CHARS} character limit"
        ));
    }
    if label.chars().any(char::is_control) {
        return Err("label must not contain control characters".to_owned());
    }
    Ok(())
}

fn validate_utc_timestamp(ts: &str) -> Result<(), String> {
    if !ts.ends_with('Z') || ts.contains('[') || ts.contains('+') || ts.contains(' ') {
        return Err(format!(
            "{ts:?} is not a canonical UTC RFC 3339 timestamp ending in Z"
        ));
    }
    ts.parse::<Timestamp>()
        .map_err(|err| format!("{ts:?} is not a valid RFC 3339 timestamp: {err}"))?;
    Ok(())
}

async fn blocking<T: Send + 'static>(
    tool: &'static str,
    work: impl FnOnce() -> Result<T, ErrorBody> + Send + 'static,
) -> Result<T, ErrorBody> {
    tokio::task::spawn_blocking(work).await.map_err(|err| {
        ErrorBody::new(
            ErrorCode::Internal,
            format!("{tool} stopped unexpectedly: {err}."),
        )
    })?
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use legion_core::Node;

    use super::*;

    struct Rng(u64);

    impl Rng {
        fn next_u64(&mut self) -> u64 {
            // SplitMix64 for deterministic property tests with zero extra dependencies.
            self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
            let mut z = self.0;
            z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
            z ^ (z >> 31)
        }

        fn gen_range(&mut self, upper: usize) -> usize {
            usize::try_from(self.next_u64() % (upper as u64)).unwrap()
        }

        fn gen_bool(&mut self) -> bool {
            self.next_u64() & 1 == 1
        }
    }

    fn random_ts(rng: &mut Rng) -> String {
        let base: Timestamp = "2026-11-01T00:00:00.000Z".parse().unwrap();
        let secs = i64::try_from(rng.gen_range(20)).unwrap();
        let millis = i64::try_from(rng.gen_range(4)).unwrap() * 250;
        let ts = base
            .checked_add(jiff::SignedDuration::from_millis(secs * 1000 + millis))
            .unwrap();
        format_timestamp(ts)
    }

    fn random_favorites(rng: &mut Rng) -> FavoritesFile {
        const IDS: [&str; 6] = ["id-1", "id-2", "id-3", "id-4", "id-5", "id-6"];
        const NODES: [&str; 3] = ["tower", "pi", "laptop"];
        const LABELS: [Option<&str>; 4] = [None, Some("Work"), Some("Holiday"), Some("Music")];

        let mut file = FavoritesFile::default();
        for id in IDS {
            match rng.gen_range(4) {
                0 => {} // absent
                1 => {
                    file.removed.insert(
                        id.to_owned(),
                        FavoriteTombstone {
                            removed_at: random_ts(rng),
                            removed_on: NODES[rng.gen_range(NODES.len())].to_owned(),
                        },
                    );
                }
                _ => {
                    let added_at = random_ts(rng);
                    let label_at = random_ts(rng);
                    let pinned_at = random_ts(rng);
                    let updated_at = random_ts(rng);
                    file.entries.insert(
                        id.to_owned(),
                        FavoriteRecord {
                            machine: NODES[rng.gen_range(NODES.len())].to_owned(),
                            path: format!("/srv/media/{id}.mp4"),
                            kind: if rng.gen_bool() {
                                FavoriteKind::File
                            } else {
                                FavoriteKind::Dir
                            },
                            label: LABELS[rng.gen_range(LABELS.len())].map(str::to_owned),
                            pinned: rng.gen_bool(),
                            added_at,
                            added_by: "owner".to_owned(),
                            updated_at,
                            updated_on: NODES[rng.gen_range(NODES.len())].to_owned(),
                            label_updated_at: label_at,
                            label_updated_on: NODES[rng.gen_range(NODES.len())].to_owned(),
                            pinned_updated_at: pinned_at,
                            pinned_updated_on: NODES[rng.gen_range(NODES.len())].to_owned(),
                        },
                    );
                }
            }
        }
        file.validate().unwrap();
        file
    }

    #[test]
    fn merge_is_commutative_associative_and_idempotent_property() {
        let mut rng = Rng(0xfeed_face_cafe_beef);
        for case in 0..300 {
            let a = random_favorites(&mut rng);
            let b = random_favorites(&mut rng);
            let c = random_favorites(&mut rng);

            // Commutative: merge(a, b) == merge(b, a)
            let ab = merge_favorites(&a, &b);
            let ba = merge_favorites(&b, &a);
            assert_eq!(ab, ba, "commutativity failed on case {case}");

            // Associative: merge(merge(a, b), c) == merge(a, merge(b, c))
            let ab_c = merge_favorites(&ab, &c);
            let bc = merge_favorites(&b, &c);
            let a_bc = merge_favorites(&a, &bc);
            assert_eq!(ab_c, a_bc, "associativity failed on case {case}");

            // Idempotent: merge(a, a) == a (and on merged results)
            assert_eq!(
                merge_favorites(&a, &a),
                a,
                "idempotence failed on case {case}"
            );
            assert_eq!(
                merge_favorites(&ab_c, &ab_c),
                ab_c,
                "idempotence on merged failed on case {case}"
            );

            // Canonical TOML round-trip preserves equality and validity.
            let encoded = ab_c.to_toml_string();
            let decoded = parse_favorites_bytes(Path::new("favorites.toml"), encoded.as_bytes())
                .expect("merged TOML must parse and validate");
            assert_eq!(decoded, ab_c, "TOML round-trip mismatch on case {case}");
        }
    }

    #[test]
    fn per_field_stamps_preserve_concurrent_label_and_pin_edits() {
        let base = FavoriteRecord {
            machine: "tower".to_owned(),
            path: "/srv/photos".to_owned(),
            kind: FavoriteKind::Dir,
            label: Some("Photos".to_owned()),
            pinned: false,
            added_at: "2026-11-01T10:00:00.000Z".to_owned(),
            added_by: "owner".to_owned(),
            updated_at: "2026-11-01T10:00:00.000Z".to_owned(),
            updated_on: "tower".to_owned(),
            label_updated_at: "2026-11-01T10:00:00.000Z".to_owned(),
            label_updated_on: "tower".to_owned(),
            pinned_updated_at: "2026-11-01T10:00:00.000Z".to_owned(),
            pinned_updated_on: "tower".to_owned(),
        };

        let mut node_a = FavoritesFile::default();
        let mut rec_a = base.clone();
        rec_a.label = Some("Family Photos".to_owned());
        rec_a.label_updated_at = "2026-11-02T12:00:00.000Z".to_owned();
        rec_a.label_updated_on = "tower".to_owned();
        rec_a.updated_at = "2026-11-02T12:00:00.000Z".to_owned();
        rec_a.updated_on = "tower".to_owned();
        node_a.entries.insert("fav1".to_owned(), rec_a);

        let mut node_b = FavoritesFile::default();
        let mut rec_b = base;
        rec_b.pinned = true;
        rec_b.pinned_updated_at = "2026-11-02T12:05:00.000Z".to_owned();
        rec_b.pinned_updated_on = "pi".to_owned();
        rec_b.updated_at = "2026-11-02T12:05:00.000Z".to_owned();
        rec_b.updated_on = "pi".to_owned();
        node_b.entries.insert("fav1".to_owned(), rec_b);

        let merged = merge_favorites(&node_a, &node_b);
        assert_eq!(merged, merge_favorites(&node_b, &node_a));
        let entry = &merged.entries["fav1"];
        assert_eq!(entry.label.as_deref(), Some("Family Photos"));
        assert!(entry.pinned);
    }

    #[test]
    fn tombstones_expire_after_90_days() {
        let now: Timestamp = "2026-11-25T00:00:00.000Z".parse().unwrap();
        let mut file = FavoritesFile::default();
        // 89 days old: kept
        let kept_ts = now
            .checked_sub(jiff::SignedDuration::from_hours(89 * 24))
            .unwrap();
        // 90 days + 1 second old: expired
        let expired_ts = now
            .checked_sub(jiff::SignedDuration::from_secs(90 * 24 * 3600 + 1))
            .unwrap();
        file.removed.insert(
            "recent".to_owned(),
            FavoriteTombstone {
                removed_at: format_timestamp(kept_ts),
                removed_on: "tower".to_owned(),
            },
        );
        file.removed.insert(
            "old".to_owned(),
            FavoriteTombstone {
                removed_at: format_timestamp(expired_ts),
                removed_on: "tower".to_owned(),
            },
        );

        let pruned = prune_expired_tombstones(&mut file, now);
        assert_eq!(pruned, 1);
        assert!(file.removed.contains_key("recent"));
        assert!(!file.removed.contains_key("old"));
    }

    #[test]
    fn hostile_peer_inputs_are_rejected() {
        // 1. Oversized file (> 256 KiB).
        let huge = vec![b' '; MAX_FAVORITES_BYTES + 1];
        assert_eq!(
            parse_favorites_bytes(Path::new("favorites.toml"), &huge)
                .unwrap_err()
                .code,
            ErrorCode::InvalidArgs
        );

        // 2. Path traversal, control characters, relative paths.
        for bad_path in [
            "/srv/../etc/passwd",
            "relative/path.txt",
            "/srv/good\n[entries.injected]",
            "/srv/nul\0byte",
        ] {
            let mut file = FavoritesFile::default();
            file.entries.insert(
                "bad1".to_owned(),
                FavoriteRecord {
                    machine: "tower".to_owned(),
                    path: bad_path.to_owned(),
                    kind: FavoriteKind::File,
                    label: None,
                    pinned: false,
                    added_at: "2026-11-01T00:00:00.000Z".to_owned(),
                    added_by: "owner".to_owned(),
                    updated_at: "2026-11-01T00:00:00.000Z".to_owned(),
                    updated_on: "tower".to_owned(),
                    label_updated_at: "2026-11-01T00:00:00.000Z".to_owned(),
                    label_updated_on: "tower".to_owned(),
                    pinned_updated_at: "2026-11-01T00:00:00.000Z".to_owned(),
                    pinned_updated_on: "tower".to_owned(),
                },
            );
            assert!(
                file.validate().is_err(),
                "expected rejection for {bad_path:?}"
            );
        }

        // 3. Unknown field in TOML.
        let unknown_field = r#"
version = 1
evil = true
"#;
        assert!(
            parse_favorites_bytes(Path::new("favorites.toml"), unknown_field.as_bytes()).is_err()
        );

        // 4. Non-canonical offset timestamp.
        let bad_ts = r#"
version = 1
[removed.a]
removed_at = "2026-11-01T00:00:00+02:00"
removed_on = "tower"
"#;
        assert!(parse_favorites_bytes(Path::new("favorites.toml"), bad_ts.as_bytes()).is_err());

        // 5. Same id in both entries and removed.
        let both = r#"
version = 1
[entries.dup]
machine = "tower"
path = "/srv/a.txt"
kind = "file"
pinned = false
added_at = "2026-11-01T00:00:00.000Z"
added_by = "owner"
updated_at = "2026-11-01T00:00:00.000Z"
updated_on = "tower"
label_updated_at = "2026-11-01T00:00:00.000Z"
label_updated_on = "tower"
pinned_updated_at = "2026-11-01T00:00:00.000Z"
pinned_updated_on = "tower"

[removed.dup]
removed_at = "2026-11-02T00:00:00.000Z"
removed_on = "tower"
"#;
        assert!(parse_favorites_bytes(Path::new("favorites.toml"), both.as_bytes()).is_err());
    }

    #[tokio::test]
    async fn fav_list_filters_entries_outside_callers_paths() {
        let tmp = tempfile::tempdir().unwrap();
        let allowed = tmp.path().join("allowed");
        let secret = tmp.path().join("secret");
        std::fs::create_dir_all(&allowed).unwrap();
        std::fs::create_dir_all(&secret).unwrap();
        let allowed_file = allowed.join("clip.mp4");
        let secret_file = secret.join("private.txt");
        std::fs::write(&allowed_file, b"video").unwrap();
        std::fs::write(&secret_file, b"secret").unwrap();

        let paths = Paths::under(tmp.path().join("home"));
        std::fs::create_dir_all(paths.config_dir()).unwrap();
        std::fs::write(paths.config_file("node.toml"), "name = \"tower\"\n").unwrap();
        std::fs::write(
            paths.config_file("policy.toml"),
            format!(
                r#"
[principals.confined]
role = "confined"

[roles.confined]
allow = ["effect:read", "effect:write"]
paths = [{:?}]
"#,
                allowed.to_string_lossy()
            ),
        )
        .unwrap();

        let mut registry = Registry::new();
        register(&mut registry);
        let node = Node::new(paths, registry);

        // Owner adds one entry inside `allowed` and one inside `secret`.
        let added_allowed = node
            .call(Call::new(
                Door::Cli,
                "fav.add",
                json!({
                    "path": allowed_file.to_string_lossy(),
                    "pinned": true,
                    "label": "Holiday clip",
                }),
            ))
            .await
            .unwrap();
        assert_eq!(added_allowed["entry"]["category"], json!("video"));
        assert_eq!(added_allowed["entry"]["exists"], json!(true));

        let added_secret = node
            .call(Call::new(
                Door::Cli,
                "fav.add",
                json!({
                    "path": secret_file.to_string_lossy(),
                }),
            ))
            .await
            .unwrap();
        let secret_id = added_secret["entry"]["id"].as_str().unwrap().to_owned();

        // Owner sees both entries.
        let owner_list = node
            .call(Call::new(Door::Cli, "fav.list", json!({})))
            .await
            .unwrap();
        assert_eq!(owner_list["entries"].as_array().unwrap().len(), 2);

        // Confined principal sees ONLY the entry inside `allowed`.
        let mut list_call = Call::new(Door::Cli, "fav.list", json!({}));
        list_call.as_principal = Some("confined".to_owned());
        let confined_list = node.call(list_call).await.unwrap();
        let entries = confined_list["entries"].as_array().unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(
            entries[0]["path"],
            json!(allowed_file.to_string_lossy().as_ref())
        );

        // Confined principal cannot add or modify or remove entries outside its `paths`.
        let mut bad_add = Call::new(
            Door::Cli,
            "fav.add",
            json!({ "path": secret_file.to_string_lossy() }),
        );
        bad_add.as_principal = Some("confined".to_owned());
        assert_eq!(
            node.call(bad_add).await.unwrap_err().code,
            ErrorCode::Denied
        );

        let mut bad_set = Call::new(
            Door::Cli,
            "fav.set",
            json!({ "id": secret_id, "pinned": true }),
        );
        bad_set.as_principal = Some("confined".to_owned());
        assert_eq!(
            node.call(bad_set).await.unwrap_err().code,
            ErrorCode::NotFound
        );
    }
}
