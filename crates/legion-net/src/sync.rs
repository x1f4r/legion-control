//! Shared configuration synchronisation across the fleet (`fleet.sync`, `config.list`,
//! `config.read`, `config.write`).
//!
//! Only the shared configuration documents (`devices.toml`, `fleet.toml`, `policy.toml`,
//! `projects.toml`, and validated files under `tools.d/<name>.toml`) may be read, written, or
//! synchronised here; machine-local files such as `node.toml` and `tokens.toml` are always
//! refused.
//!
//! A revision counter cannot distinguish "newer" from "diverged", so each node keeps a small JSON
//! sidecar under `<state_dir>/sync/sidecars/` recording the BLAKE3 content hash last synchronised
//! with each peer. `devices.toml` is a commutative mergeable registry (tombstones always win) that
//! merges cleanly in both directions without conflict and reconciles `device:*` principals and
//! `authorized_keys` on the receiving node. Other writes are validated by the receiving node's own
//! strict loader before replacing anything, backed up to `<state_dir>/sync/backups/`, written
//! atomically, and recorded in the audit log with hashes before and after.

use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::marker::PhantomData;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use jiff::Timestamp;
use legion_core::config::{NodeFile, parse, write_atomically};
use legion_core::policy::{OWNER, Policy};
use legion_core::{
    BoxFuture, Ctx, EventSink, NoArgs, Registry, Tool, ToolError, input_schema, parse_args,
};
use legion_os::OsError;
use legion_os::lock::{FileLock, LockMode};
use legion_proto::{Effect, ErrorBody, ErrorCode, ToolSpec};
use legion_tools::device::registry::{
    DevicesFile, MergeChange, apply_devices_merge_blocking, merge_devices,
    read_devices_for_sync_blocking,
};
use legion_tools::device::sessions::{DEFAULT_TERMINATE_GRACE, terminate_device_sessions};
use legion_tools::ext::def::parse_toml as parse_extension_toml;
use legion_tools::proj::ProjectsFile;
use legion_xfer::hash::{Hash, hash_bytes};
use schemars::JsonSchema;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::config::FleetFile;
use crate::pool::Fleet;

/// Fixed shared configuration documents synced across machines.
pub const FIXED_SHARED_DOCUMENTS: [&str; 4] =
    ["devices.toml", "fleet.toml", "policy.toml", "projects.toml"];

/// Maximum per-machine timeout accepted by `fleet.sync` (5 minutes).
const MAX_SYNC_TIMEOUT_S: f64 = 300.0;

/// Maximum size of a single configuration document accepted by `config.write` (1 MiB).
const MAX_DOCUMENT_BYTES: usize = 1024 * 1024;

/// Longest time to wait for the configuration or sidecar file lock.
const LOCK_TIMEOUT: Duration = Duration::from_secs(10);

/// Directory prefix for shared extension tool files.
const TOOLS_D_PREFIX: &str = "tools.d/";

/// Maximum length of a `tools.d` file name (including `.toml`).
const MAX_TOOLS_D_NAME: usize = 128;

/// Registers `config.list`, `config.read`, `config.write`, and `fleet.sync`.
pub(crate) fn register(registry: &mut Registry, fleet: Fleet) {
    registry.add_fn(
        "config.list",
        Effect::Admin,
        "List the shared configuration documents present on this node (devices.toml, fleet.toml, policy.toml, projects.toml, and files in tools.d/). Machine-local files (node.toml, tokens.toml) are never included.",
        config_list,
    );
    registry.add_fn(
        "config.read",
        Effect::Admin,
        "Read one shared configuration document (devices.toml, fleet.toml, policy.toml, projects.toml, or tools.d/<name>.toml) and its BLAKE3 content hash. Refuses machine-local files such as node.toml and tokens.toml.",
        config_read,
    );
    registry.add_fn(
        "config.write",
        Effect::Admin,
        "Validate and atomically update one shared configuration document (devices.toml, fleet.toml, policy.toml, projects.toml, or tools.d/<name>.toml), keeping the previous version as a backup in the state directory and recording the hash before and after in the audit log. Writing devices.toml merges incoming entries with local paired devices (tombstones always win), reconciles registry-owned device:* principals (marked with `device = \"<pairing id>\"` in policy.toml) without touching shared or unmarked principals, updates authorized_keys, and ends active sessions for newly removed devices. Writing policy.toml changes who may do what on this machine. Refuses invalid documents before replacing anything, refuses mismatched expect_hash or expect_absent (when a file already exists) with conflict, and refuses machine-local files such as node.toml and tokens.toml.",
        config_write,
    );
    registry.add(SyncTool::<SyncArgs, SyncResult, _>::new(
        "fleet.sync",
        Effect::Admin,
        "Compare, push, or pull shared configuration documents (devices.toml, fleet.toml, policy.toml, projects.toml, and tools.d/*.toml) across machines in fleet.toml using per-peer BLAKE3 sync sidecars to detect conflicts. In status mode, reports same, changed_here, changed_there, both, only_here, or only_there. Paired devices (devices.toml) merge cleanly across machines in both directions with tombstones always winning, reconciling device:* principals and authorized_keys and ending active sessions for newly removed devices. Removal of a tools.d file is never propagated: push never deletes remote files, pull never deletes local files, and status reports files present on only one side as only_here or only_there. Syncing policy.toml is powerful because it changes who may do what on the receiving machine; every replacement is strictly validated by the receiver before installing, backed up, and recorded in the audit log with its hash before and after. Never syncs node.toml or tokens.toml.",
        fleet,
        |fleet, ctx, args| Box::pin(fleet_sync(fleet, ctx, args)),
    ));
}

struct SyncTool<A, O, F> {
    spec: ToolSpec,
    fleet: Fleet,
    handler: F,
    types: PhantomData<fn(A) -> O>,
}

impl<A, O, F> SyncTool<A, O, F>
where
    A: DeserializeOwned + JsonSchema + Send + 'static,
    O: Serialize + 'static,
    F: for<'a> Fn(&'a Fleet, &'a Ctx, A) -> BoxFuture<'a, Result<O, ToolError>>
        + Send
        + Sync
        + 'static,
{
    fn new(name: &str, effect: Effect, description: &str, fleet: Fleet, handler: F) -> Self {
        Self {
            spec: ToolSpec {
                name: name.to_owned(),
                description: description.to_owned(),
                effect,
                input_schema: input_schema::<A>(),
            },
            fleet,
            handler,
            types: PhantomData,
        }
    }
}

impl<A, O, F> Tool for SyncTool<A, O, F>
where
    A: DeserializeOwned + JsonSchema + Send + 'static,
    O: Serialize + 'static,
    F: for<'a> Fn(&'a Fleet, &'a Ctx, A) -> BoxFuture<'a, Result<O, ToolError>>
        + Send
        + Sync
        + 'static,
{
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn call<'a>(&'a self, ctx: &'a Ctx, args: Value) -> BoxFuture<'a, Result<Value, ToolError>> {
        Box::pin(async move {
            let parsed = parse_args::<A>(&self.spec.name, args)?;
            let out = (self.handler)(&self.fleet, ctx, parsed).await?;
            serde_json::to_value(&out).map_err(|error| {
                ErrorBody::new(
                    ErrorCode::Internal,
                    format!(
                        "{} returned a result that cannot be written as JSON: {error}.",
                        self.spec.name
                    ),
                )
            })
        })
    }
}

/// Validated shared configuration document identifier.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum SharedDocument {
    /// `devices.toml`.
    Devices,
    /// `fleet.toml`.
    Fleet,
    /// `policy.toml`.
    Policy,
    /// `projects.toml`.
    Projects,
    /// `tools.d/<name>.toml`, storing only the validated bare file name `<name>.toml`.
    ToolExt(String),
}

impl SharedDocument {
    /// Parses and validates a shared document name, refusing machine-local files and path traversal.
    pub fn parse(name: &str) -> Result<Self, ErrorBody> {
        match name {
            "devices.toml" => Ok(Self::Devices),
            "fleet.toml" => Ok(Self::Fleet),
            "policy.toml" => Ok(Self::Policy),
            "projects.toml" => Ok(Self::Projects),
            "node.toml" | "tokens.toml" => Err(ErrorBody::new(
                ErrorCode::InvalidArgs,
                format!(
                    "{name} belongs to one machine and is not a shared configuration document."
                ),
            )
            .with_hint(
                "Only devices.toml, fleet.toml, policy.toml, projects.toml, and tools.d/<name>.toml can be synced.",
            )),
            other => {
                if let Some(bare) = other.strip_prefix(TOOLS_D_PREFIX) {
                    if is_valid_tools_d_filename(bare) {
                        return Ok(Self::ToolExt(bare.to_owned()));
                    }
                    return Err(ErrorBody::new(
                        ErrorCode::InvalidArgs,
                        format!(
                            "{other:?} is not a valid tools.d document name: use tools.d/<name>.toml with a bare file name made of ASCII letters, digits, -, _ and internal dots (no leading dot, trailing dot, or `..`)."
                        ),
                    )
                    .with_hint("Name a single .toml file directly under tools.d/, such as tools.d/gpu.free.toml."));
                }
                Err(ErrorBody::new(
                    ErrorCode::InvalidArgs,
                    format!("{other:?} is not a shared configuration document."),
                )
                .with_hint(
                    "Allowed shared documents are devices.toml, fleet.toml, policy.toml, projects.toml, and tools.d/<name>.toml.",
                ))
            }
        }
    }

    /// Canonical wire name (always using forward slash for `tools.d/<name>.toml`).
    pub fn canonical_name(&self) -> String {
        match self {
            Self::Devices => "devices.toml".to_owned(),
            Self::Fleet => "fleet.toml".to_owned(),
            Self::Policy => "policy.toml".to_owned(),
            Self::Projects => "projects.toml".to_owned(),
            Self::ToolExt(bare) => format!("{TOOLS_D_PREFIX}{bare}"),
        }
    }

    /// Location of this document under `config_dir`.
    pub fn config_path(&self, config_dir: &Path) -> PathBuf {
        match self {
            Self::Devices => config_dir.join("devices.toml"),
            Self::Fleet => config_dir.join("fleet.toml"),
            Self::Policy => config_dir.join("policy.toml"),
            Self::Projects => config_dir.join("projects.toml"),
            Self::ToolExt(bare) => config_dir.join("tools.d").join(bare),
        }
    }

    /// Location of this document's backup file under `state_dir`.
    pub fn backup_path(&self, state_dir: &Path) -> PathBuf {
        let base = state_dir.join("sync").join("backups");
        match self {
            Self::Devices => base.join("devices.toml.bak"),
            Self::Fleet => base.join("fleet.toml.bak"),
            Self::Policy => base.join("policy.toml.bak"),
            Self::Projects => base.join("projects.toml.bak"),
            Self::ToolExt(bare) => base.join("tools.d").join(format!("{bare}.bak")),
        }
    }

    /// Location of this document's sync sidecar under `state_dir`.
    pub fn sidecar_path(&self, state_dir: &Path) -> PathBuf {
        let base = state_dir.join("sync").join("sidecars");
        match self {
            Self::Devices => base.join("devices.toml.json"),
            Self::Fleet => base.join("fleet.toml.json"),
            Self::Policy => base.join("policy.toml.json"),
            Self::Projects => base.join("projects.toml.json"),
            Self::ToolExt(bare) => base.join("tools.d").join(format!("{bare}.json")),
        }
    }

    /// Validates `content` using this node's own strict loader before replacing anything.
    pub fn validate(
        &self,
        path: &Path,
        content: &str,
        is_builtin: &dyn Fn(&str) -> bool,
    ) -> Result<(), ErrorBody> {
        if content.len() > MAX_DOCUMENT_BYTES {
            return Err(ErrorBody::new(
                ErrorCode::InvalidArgs,
                format!(
                    "{} exceeds the {} byte configuration limit ({} bytes given).",
                    self.canonical_name(),
                    MAX_DOCUMENT_BYTES,
                    content.len()
                ),
            ));
        }
        match self {
            Self::Devices => {
                parse::<DevicesFile>(path, content.as_bytes())?;
                Ok(())
            }
            Self::Fleet => {
                parse::<FleetFile>(path, content.as_bytes())?;
                Ok(())
            }
            Self::Policy => {
                parse::<Policy>(path, content.as_bytes())?;
                Ok(())
            }
            Self::Projects => {
                parse::<ProjectsFile>(path, content.as_bytes())?;
                Ok(())
            }
            Self::ToolExt(bare) => {
                validate_tools_d_toml(path, bare, &self.canonical_name(), content, is_builtin)
            }
        }
    }
}

fn is_valid_tools_d_filename(name: &str) -> bool {
    let Some(stem) = name.strip_suffix(".toml") else {
        return false;
    };
    if stem.is_empty() || name.len() > MAX_TOOLS_D_NAME {
        return false;
    }
    if stem.ends_with('.') || stem.contains("..") {
        return false;
    }
    let starts_ok = stem
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_alphanumeric());
    starts_ok
        && stem
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

fn validate_tools_d_toml(
    path: &Path,
    bare: &str,
    canonical_name: &str,
    content: &str,
    is_builtin: &dyn Fn(&str) -> bool,
) -> Result<(), ErrorBody> {
    let def = parse_extension_toml(content.as_bytes(), is_builtin).map_err(|failure| {
        let mut details = json!({"file": path});
        if let Some(line) = failure.line
            && let Some(obj) = details.as_object_mut()
        {
            obj.insert("line".to_owned(), json!(line));
        }
        ErrorBody::new(
            ErrorCode::Failed,
            format!(
                "{} is not a valid extension definition: {}.",
                path.display(),
                failure.reason
            ),
        )
        .with_hint(
            "Fix the extension definition to match `ext.define` rules before writing or syncing.",
        )
        .with_details(details)
    })?;

    let stem = bare.strip_suffix(".toml").unwrap_or(bare);
    if def.name != stem {
        return Err(ErrorBody::new(
            ErrorCode::Failed,
            format!(
                "{canonical_name} defines tool {:?}, which does not match file name stem {:?}.",
                def.name, stem
            ),
        )
        .with_hint(format!(
            "Store this tool at tools.d/{}.toml, or set name = {:?}.",
            def.name, stem
        ))
        .with_details(json!({"file": path, "name": def.name, "stem": stem})));
    }

    if let Some(tools_dir) = path.parent()
        && let Ok(entries) = std::fs::read_dir(tools_dir)
    {
        for entry in entries.flatten() {
            let other_name = entry.file_name();
            let Some(other_str) = other_name.to_str() else {
                continue;
            };
            if !other_str.ends_with(".toml") {
                continue;
            }
            let same_file = other_str == bare
                || (cfg!(any(windows, target_os = "macos"))
                    && other_str.eq_ignore_ascii_case(bare));
            if same_file {
                continue;
            }
            let other_path = entry.path();
            if !other_path.is_file() {
                continue;
            }
            let Ok(other_bytes) = std::fs::read(&other_path) else {
                continue;
            };
            if let Ok(other_def) = parse_extension_toml(&other_bytes, is_builtin)
                && other_def.name == def.name
            {
                return Err(ErrorBody::new(
                    ErrorCode::Failed,
                    format!(
                        "{canonical_name} defines tool {:?}, which is already defined by {}.",
                        def.name,
                        other_path.display()
                    ),
                )
                .with_hint(
                    "Remove or rename the conflicting file in tools.d/ before installing this tool.",
                )
                .with_details(json!({
                    "file": path,
                    "name": def.name,
                    "existing_file": other_path,
                })));
            }
        }
    }

    Ok(())
}

/// Sync sidecar stored under `<state_dir>/sync/sidecars/<document>.json`.
///
/// Tracks both the most recent sync overall (`last_hash`, `last_peer`, `synced_at`) and the last
/// hash synchronised with each individual peer so multi-machine fleets can detect divergence per
/// peer without relying on revision numbers or clocks.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncSidecar {
    /// Canonical shared document name.
    pub document: String,
    /// BLAKE3 hash of the bytes last synchronised with any peer.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_hash: Option<String>,
    /// Peer involved in the most recent synchronisation.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_peer: Option<String>,
    /// RFC 3339 UTC timestamp of the most recent synchronisation.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub synced_at: Option<String>,
    /// Per-peer last-synchronised content hash and timestamp.
    #[serde(default)]
    pub peers: BTreeMap<String, PeerSyncRecord>,
}

/// Last synchronised state recorded for one peer machine.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeerSyncRecord {
    /// BLAKE3 hash (`blake3:<hex>`) of the document bytes last agreed with this peer.
    pub hash: String,
    /// RFC 3339 UTC timestamp when the sync completed.
    pub synced_at: String,
}

fn read_sidecar(state_dir: &Path, doc: &SharedDocument) -> Result<SyncSidecar, ErrorBody> {
    let path = doc.sidecar_path(state_dir);
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(SyncSidecar {
                document: doc.canonical_name(),
                ..Default::default()
            });
        }
        Err(error) => {
            return Err(ErrorBody::new(
                ErrorCode::Failed,
                format!("Sync sidecar {} cannot be read: {error}.", path.display()),
            ));
        }
    };
    serde_json::from_slice(&bytes).map_err(|error| {
        ErrorBody::new(
            ErrorCode::Failed,
            format!("Sync sidecar {} is corrupt: {error}.", path.display()),
        )
        .with_hint("Inspect or remove the corrupt sidecar file in the state directory, then sync with force=true if needed.")
    })
}

fn record_sidecar_sync(
    state_dir: &Path,
    doc: &SharedDocument,
    peer: &str,
    hash: &str,
) -> Result<(), ErrorBody> {
    let lock_path = state_dir.join("sync").join("sidecars.lock");
    let _held = acquire_lock(&lock_path, "sync sidecar")?;
    let mut sidecar = read_sidecar(state_dir, doc)?;
    let now = rfc3339_now();
    sidecar.document = doc.canonical_name();
    sidecar.last_hash = Some(hash.to_owned());
    sidecar.last_peer = Some(peer.to_owned());
    sidecar.synced_at = Some(now.clone());
    sidecar.peers.insert(
        peer.to_owned(),
        PeerSyncRecord {
            hash: hash.to_owned(),
            synced_at: now,
        },
    );
    let path = doc.sidecar_path(state_dir);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| {
            ErrorBody::new(
                ErrorCode::Failed,
                format!("{} cannot be created: {error}.", parent.display()),
            )
        })?;
    }
    let encoded = serde_json::to_vec_pretty(&sidecar).map_err(|error| {
        ErrorBody::new(
            ErrorCode::Internal,
            format!("Sync sidecar could not be serialised: {error}."),
        )
    })?;
    write_atomically(&path, &encoded).map_err(|error| {
        ErrorBody::new(
            ErrorCode::Failed,
            format!(
                "Sync sidecar {} cannot be written: {error}.",
                path.display()
            ),
        )
    })
}

fn acquire_lock(lock_path: &Path, what: &str) -> Result<FileLock, ErrorBody> {
    if let Some(parent) = lock_path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| {
            ErrorBody::new(
                ErrorCode::Failed,
                format!("{} cannot be created: {error}.", parent.display()),
            )
        })?;
    }
    FileLock::acquire_timeout(lock_path, LockMode::Exclusive, LOCK_TIMEOUT).map_err(|error| {
        match error {
            OsError::TimedOut { .. } => ErrorBody::new(
                ErrorCode::Busy,
                format!("Another call has been updating {what} for too long."),
            )
            .with_hint("Try again in a moment."),
            other => ErrorBody::new(
                ErrorCode::Failed,
                format!("{} cannot be locked: {other}.", lock_path.display()),
            ),
        }
    })
}

fn rfc3339_now() -> String {
    Timestamp::now().strftime("%Y-%m-%dT%H:%M:%SZ").to_string()
}

/// Lists existing shared configuration documents under `config_dir`.
fn list_local_shared_documents(config_dir: &Path) -> Result<Vec<String>, ErrorBody> {
    let mut found = BTreeSet::new();
    for fixed in FIXED_SHARED_DOCUMENTS {
        if config_dir.join(fixed).is_file() {
            found.insert(fixed.to_owned());
        }
    }
    let tools_d = config_dir.join("tools.d");
    match std::fs::read_dir(&tools_d) {
        Ok(entries) => {
            for entry in entries {
                let entry = entry.map_err(|error| {
                    ErrorBody::new(
                        ErrorCode::Failed,
                        format!("{} cannot be listed: {error}.", tools_d.display()),
                    )
                })?;
                let file_name = entry.file_name();
                let Some(name) = file_name.to_str() else {
                    continue;
                };
                if is_valid_tools_d_filename(name) && entry.path().is_file() {
                    found.insert(format!("{TOOLS_D_PREFIX}{name}"));
                }
            }
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(ErrorBody::new(
                ErrorCode::Failed,
                format!("{} cannot be listed: {error}.", tools_d.display()),
            ));
        }
    }
    Ok(found.into_iter().collect())
}

/// Reads one shared document from `config_dir`.
fn read_local_document(
    config_dir: &Path,
    doc: &SharedDocument,
) -> Result<ConfigReadResult, ErrorBody> {
    let path = doc.config_path(config_dir);
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(ConfigReadResult {
                document: doc.canonical_name(),
                exists: false,
                hash: None,
                content: None,
            });
        }
        Err(error) => {
            return Err(ErrorBody::new(
                ErrorCode::Failed,
                format!("{} cannot be read: {error}.", path.display()),
            ));
        }
    };
    let content = String::from_utf8(bytes).map_err(|error| {
        ErrorBody::new(
            ErrorCode::Failed,
            format!("{} is not valid UTF-8 text: {error}.", path.display()),
        )
    })?;
    let hash = hash_bytes(content.as_bytes()).to_string();
    Ok(ConfigReadResult {
        document: doc.canonical_name(),
        exists: true,
        hash: Some(hash),
        content: Some(content),
    })
}

struct WriteOutcome {
    document: String,
    hash_before: Option<String>,
    hash_after: String,
    backup: Option<String>,
    canonical_content: Option<String>,
    warnings: Vec<String>,
}

/// Precondition on the existing target document before `config.write` replaces it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WritePrecondition<'a> {
    /// Overwrite whether the document exists or not.
    Any,
    /// Target must exist with this exact BLAKE3 hash.
    Hash(&'a str),
    /// Target must not currently exist.
    Absent,
}

/// Validates, backs up, and atomically writes one shared configuration document.
fn write_local_document_blocking(
    config_dir: &Path,
    state_dir: &Path,
    doc: &SharedDocument,
    content: &str,
    precondition: WritePrecondition<'_>,
    is_builtin: &dyn Fn(&str) -> bool,
) -> Result<WriteOutcome, ErrorBody> {
    if let WritePrecondition::Hash(expected) = precondition {
        expected.parse::<Hash>().map_err(|_| {
            ErrorBody::new(
                ErrorCode::InvalidArgs,
                format!("expect_hash {expected:?} is not a valid BLAKE3 hash."),
            )
            .with_hint("Hashes are written blake3:<64 hex digits>.")
        })?;
    }

    let lock_path = state_dir.join("sync").join("config.lock");
    let _held = acquire_lock(&lock_path, "shared configuration")?;

    let target = doc.config_path(config_dir);
    let existing = match std::fs::read(&target) {
        Ok(bytes) => Some(bytes),
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(error) => {
            return Err(ErrorBody::new(
                ErrorCode::Failed,
                format!("{} cannot be read: {error}.", target.display()),
            ));
        }
    };
    let hash_before = existing.as_deref().map(|b| hash_bytes(b).to_string());

    match precondition {
        WritePrecondition::Any => {}
        WritePrecondition::Hash(expected) => {
            if hash_before.as_deref() != Some(expected) {
                return Err(ErrorBody::new(
                    ErrorCode::Conflict,
                    format!(
                        "{} changed on this node before it could be replaced (expected {expected}, found {}).",
                        doc.canonical_name(),
                        hash_before.as_deref().unwrap_or("missing")
                    ),
                )
                .with_hint("Read the current document again, or pass force=true to overwrite.")
                .with_details(json!({
                    "document": doc.canonical_name(),
                    "expected": expected,
                    "actual": hash_before,
                })));
            }
        }
        WritePrecondition::Absent => {
            if let Some(ref actual) = hash_before {
                return Err(ErrorBody::new(
                    ErrorCode::Conflict,
                    format!(
                        "{} already exists on this node before it could be created (expected absent, found {actual}).",
                        doc.canonical_name(),
                    ),
                )
                .with_hint("Read the current document again, or pass force=true to overwrite.")
                .with_details(json!({
                    "document": doc.canonical_name(),
                    "expected": "absent",
                    "actual": actual,
                })));
            }
        }
    }

    // Validate strictly with the receiving node's own loader BEFORE touching backup or target.
    doc.validate(&target, content, is_builtin)?;

    let backup = if let Some(old_bytes) = existing.as_deref() {
        let backup_path = doc.backup_path(state_dir);
        if let Some(parent) = backup_path.parent() {
            std::fs::create_dir_all(parent).map_err(|error| {
                ErrorBody::new(
                    ErrorCode::Failed,
                    format!("{} cannot be created: {error}.", parent.display()),
                )
            })?;
        }
        write_atomically(&backup_path, old_bytes).map_err(|error| {
            ErrorBody::new(
                ErrorCode::Failed,
                format!(
                    "Backup {} cannot be written: {error}.",
                    backup_path.display()
                ),
            )
        })?;
        Some(backup_path.display().to_string())
    } else {
        None
    };

    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent).map_err(|error| {
            ErrorBody::new(
                ErrorCode::Failed,
                format!("{} cannot be created: {error}.", parent.display()),
            )
        })?;
    }
    write_atomically(&target, content.as_bytes()).map_err(|error| {
        ErrorBody::new(
            ErrorCode::Failed,
            format!("{} cannot be written: {error}.", target.display()),
        )
    })?;

    let hash_after = hash_bytes(content.as_bytes()).to_string();
    Ok(WriteOutcome {
        document: doc.canonical_name(),
        hash_before,
        hash_after,
        backup,
        canonical_content: None,
        warnings: Vec::new(),
    })
}

async fn read_local_document_for_ctx(
    ctx: &Ctx,
    doc: &SharedDocument,
) -> Result<ConfigReadResult, ErrorBody> {
    if *doc == SharedDocument::Devices {
        let paths = ctx.paths().clone();
        let node_name = ctx.node_name().to_owned();
        let node_file = ctx.config().get::<NodeFile>().ok();
        return tokio::task::spawn_blocking(move || {
            let content = read_devices_for_sync_blocking(&paths, &node_name, node_file.as_deref())?;
            match content {
                Some(text) => {
                    let hash = hash_bytes(text.as_bytes()).to_string();
                    Ok(ConfigReadResult {
                        document: SharedDocument::Devices.canonical_name(),
                        exists: true,
                        hash: Some(hash),
                        content: Some(text),
                    })
                }
                None => Ok(ConfigReadResult {
                    document: SharedDocument::Devices.canonical_name(),
                    exists: false,
                    hash: None,
                    content: None,
                }),
            }
        })
        .await
        .map_err(|error| {
            ErrorBody::new(
                ErrorCode::Internal,
                format!("Reading devices.toml stopped unexpectedly: {error}."),
            )
        })?;
    }
    let config_dir = ctx.paths().config_dir().to_path_buf();
    let doc = doc.clone();
    tokio::task::spawn_blocking(move || read_local_document(&config_dir, &doc))
        .await
        .map_err(|error| {
            ErrorBody::new(
                ErrorCode::Internal,
                format!("Reading local document stopped unexpectedly: {error}."),
            )
        })?
}

async fn apply_local_write(
    ctx: &Ctx,
    doc: SharedDocument,
    content: String,
    expect_hash: Option<String>,
    expect_absent: bool,
) -> Result<WriteOutcome, ErrorBody> {
    if expect_hash.is_some() && expect_absent {
        return Err(ErrorBody::new(
            ErrorCode::InvalidArgs,
            "Cannot set both `expect_hash` and `expect_absent` on config.write.",
        )
        .with_hint("Pass `expect_hash` when replacing an existing file, or `expect_absent=true` when creating a new one."));
    }
    let started = Instant::now();

    if doc == SharedDocument::Devices {
        if content.len() > MAX_DOCUMENT_BYTES {
            return Err(ErrorBody::new(
                ErrorCode::InvalidArgs,
                format!(
                    "devices.toml exceeds the {MAX_DOCUMENT_BYTES} byte configuration limit ({} bytes given).",
                    content.len()
                ),
            ));
        }
        if let Some(ref expected) = expect_hash {
            expected.parse::<Hash>().map_err(|_| {
                ErrorBody::new(
                    ErrorCode::InvalidArgs,
                    format!("expect_hash {expected:?} is not a valid BLAKE3 hash."),
                )
                .with_hint("Hashes are written blake3:<64 hex digits>.")
            })?;
        }
        let target = doc.config_path(ctx.paths().config_dir());
        let incoming = parse::<DevicesFile>(&target, content.as_bytes())?;
        let node_file = ctx.config().get::<NodeFile>()?;
        let paths = ctx.paths().clone();
        let paths_for_sessions = paths.clone();
        let node_name = ctx.node_name().to_owned();
        let caller_is_owner = ctx.grant().role() == OWNER;
        let caller_principal = ctx.principal().to_owned();
        let caller_role = ctx.grant().role().to_owned();
        let credential_expires = ctx.credential_expires();

        let merge_outcome = tokio::task::spawn_blocking(move || {
            let check_precondition = |bytes_before: Option<&[u8]>| -> Result<(), ErrorBody> {
                let hash_before = bytes_before.map(|b| hash_bytes(b).to_string());
                match (expect_hash.as_deref(), expect_absent) {
                    (Some(expected), false) => {
                        if hash_before.as_deref() != Some(expected) {
                            return Err(ErrorBody::new(
                                ErrorCode::Conflict,
                                format!(
                                    "devices.toml changed on this node before it could be replaced (expected {expected}, found {}).",
                                    hash_before.as_deref().unwrap_or("missing")
                                ),
                            )
                            .with_hint("Read devices.toml again, or omit expect_hash to merge cleanly.")
                            .with_details(json!({
                                "document": "devices.toml",
                                "expected": expected,
                                "actual": hash_before,
                            })));
                        }
                    }
                    (None, true) => {
                        if let Some(ref actual) = hash_before {
                            return Err(ErrorBody::new(
                                ErrorCode::Conflict,
                                format!(
                                    "devices.toml already exists on this node before it could be created (expected absent, found {actual}).",
                                ),
                            )
                            .with_hint("Read devices.toml again, or omit expect_absent to merge cleanly.")
                            .with_details(json!({
                                "document": "devices.toml",
                                "expected": "absent",
                                "actual": actual,
                            })));
                        }
                    }
                    _ => {}
                }
                Ok(())
            };
            let authorize_change = |change: MergeChange<'_>| -> Result<(), ErrorBody> {
                if let Some(limit) = credential_expires
                    && matches!(change, MergeChange::Live { .. })
                {
                    return Err(ErrorBody::new(
                        ErrorCode::Denied,
                        format!(
                            "The credential this call came with expires at {}, so it cannot add or change live paired devices in devices.toml.",
                            limit.strftime("%Y-%m-%dT%H:%M:%SZ")
                        ),
                    )
                    .with_hint(
                        "Use a local shell or a non-expiring credential to add or change paired devices. Removing (tombstoning) devices is still allowed.",
                    ));
                }
                if caller_is_owner {
                    return Ok(());
                }
                match change {
                    MergeChange::Live { entry, previous } => {
                        if let Some(prev) = previous
                            && prev.principal != caller_principal
                        {
                            return Err(ErrorBody::new(
                                ErrorCode::Denied,
                                format!(
                                    "As {caller_principal}, you may not modify a paired device owned by {}.",
                                    prev.principal
                                ),
                            )
                            .with_hint("Modify other principals' devices as the owner of this machine."));
                        }
                        if entry.principal != caller_principal {
                            return Err(ErrorBody::new(
                                ErrorCode::Denied,
                                format!(
                                    "As {caller_principal}, you may only sync paired devices pinned to {caller_principal}, not {}.",
                                    entry.principal
                                ),
                            )
                            .with_hint("Sync devices for other principals as the owner of this machine."));
                        }
                        if let Some(ref role) = entry.role
                            && role != &caller_role
                        {
                            return Err(ErrorBody::new(
                                ErrorCode::Denied,
                                format!(
                                    "As {caller_principal} ({caller_role}), you may not assign role {role:?} to a paired device."
                                ),
                            )
                            .with_hint("Only a caller with the owner role may assign or change device roles."));
                        }
                    }
                    MergeChange::Removed { previous_live, .. } => {
                        let Some(prev) = previous_live else {
                            return Err(ErrorBody::new(
                                ErrorCode::Denied,
                                format!(
                                    "As {caller_principal}, you may only revoke paired devices that are live on this machine and pinned to {caller_principal}."
                                ),
                            )
                            .with_hint("Only the owner may record tombstones for devices that are not live on this node."));
                        };
                        if prev.principal != caller_principal {
                            return Err(ErrorBody::new(
                                ErrorCode::Denied,
                                format!(
                                    "As {caller_principal}, you may only revoke paired devices pinned to {caller_principal}, not {}.",
                                    prev.principal
                                ),
                            )
                            .with_hint("Revoke other principals' devices as the owner of this machine."));
                        }
                    }
                }
                Ok(())
            };
            apply_devices_merge_blocking(
                &paths,
                &node_name,
                &node_file,
                &incoming,
                check_precondition,
                authorize_change,
            )
        })
        .await
        .map_err(|error| {
            ErrorBody::new(
                ErrorCode::Internal,
                format!("Updating devices.toml stopped unexpectedly: {error}."),
            )
        })??;

        // End live sessions and open SSH connections for any devices removed by the merge.
        for device_name in &merge_outcome.removed_device_names {
            let _ = terminate_device_sessions(
                &paths_for_sessions,
                device_name,
                DEFAULT_TERMINATE_GRACE,
            )
            .await;
        }

        let hash_before = merge_outcome
            .bytes_before
            .as_deref()
            .map(|b| hash_bytes(b).to_string());
        let hash_after = hash_bytes(merge_outcome.canonical_toml.as_bytes()).to_string();
        let mut audit_args = json!({
            "document": "devices.toml",
            "hash_before": hash_before,
            "hash_after": hash_after,
            "backup": merge_outcome.backup,
        });
        if !merge_outcome.warnings.is_empty()
            && let Some(obj) = audit_args.as_object_mut()
        {
            obj.insert("warnings".to_owned(), json!(merge_outcome.warnings));
        }
        ctx.node()
            .record_audit_with_device(
                ctx.node_name().to_owned(),
                Some(ctx.principal().to_owned()),
                ctx.door(),
                ctx.device().map(str::to_owned),
                "config.write.applied".to_owned(),
                &audit_args,
                started,
                &Ok(json!({})),
            )
            .await;

        return Ok(WriteOutcome {
            document: "devices.toml".to_owned(),
            hash_before,
            hash_after,
            backup: merge_outcome.backup,
            canonical_content: Some(merge_outcome.canonical_toml),
            warnings: merge_outcome.warnings,
        });
    }

    let config_dir = ctx.paths().config_dir().to_path_buf();
    let state_dir = ctx.paths().state_dir().to_path_buf();
    let builtins: BTreeSet<String> = ctx
        .node()
        .registry()
        .specs()
        .into_iter()
        .map(|spec| spec.name)
        .collect();
    let outcome = tokio::task::spawn_blocking(move || {
        let is_builtin = |name: &str| builtins.contains(name);
        let precondition = match (expect_hash.as_deref(), expect_absent) {
            (Some(hash), false) => WritePrecondition::Hash(hash),
            (None, true) => WritePrecondition::Absent,
            _ => WritePrecondition::Any,
        };
        write_local_document_blocking(
            &config_dir,
            &state_dir,
            &doc,
            &content,
            precondition,
            &is_builtin,
        )
    })
    .await
    .map_err(|error| {
        ErrorBody::new(
            ErrorCode::Internal,
            format!("Writing configuration stopped unexpectedly: {error}."),
        )
    })??;

    // Record hashes before and after in the audit log (Node::call redacts `content` and does not
    // record tool results, so policy and config replacements record their before/after hashes here).
    let audit_args = json!({
        "document": outcome.document,
        "hash_before": outcome.hash_before,
        "hash_after": outcome.hash_after,
        "backup": outcome.backup,
    });
    ctx.node()
        .record_audit_with_device(
            ctx.node_name().to_owned(),
            Some(ctx.principal().to_owned()),
            ctx.door(),
            ctx.device().map(str::to_owned),
            "config.write.applied".to_owned(),
            &audit_args,
            started,
            &Ok(json!({})),
        )
        .await;

    Ok(outcome)
}

#[derive(Debug, Serialize, Deserialize)]
struct ConfigListResult {
    documents: Vec<String>,
}

async fn config_list(ctx: &Ctx, _args: NoArgs) -> Result<ConfigListResult, ToolError> {
    let config_dir = ctx.paths().config_dir().to_path_buf();
    let documents = tokio::task::spawn_blocking(move || list_local_shared_documents(&config_dir))
        .await
        .map_err(|error| {
            ErrorBody::new(
                ErrorCode::Internal,
                format!("config.list stopped unexpectedly: {error}."),
            )
        })??;
    Ok(ConfigListResult { documents })
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct ConfigReadArgs {
    /// Shared document name: `devices.toml`, `fleet.toml`, `policy.toml`, `projects.toml`, or `tools.d/<name>.toml`.
    document: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct ConfigReadResult {
    document: String,
    exists: bool,
    hash: Option<String>,
    content: Option<String>,
}

async fn config_read(ctx: &Ctx, args: ConfigReadArgs) -> Result<ConfigReadResult, ToolError> {
    let doc = SharedDocument::parse(&args.document)?;
    read_local_document_for_ctx(ctx, &doc).await
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct ConfigWriteArgs {
    /// Shared document name: `devices.toml`, `fleet.toml`, `policy.toml`, `projects.toml`, or `tools.d/<name>.toml`.
    document: String,
    /// Full UTF-8 TOML text to validate and write atomically.
    content: String,
    /// Expected BLAKE3 hash (`blake3:<hex>`) of the existing file; fails with `conflict` if different.
    #[serde(default)]
    expect_hash: Option<String>,
    /// Require that the document does not currently exist; fails with `conflict` if already present.
    #[serde(default)]
    expect_absent: bool,
}

#[derive(Debug, Serialize, Deserialize)]
struct ConfigWriteResult {
    document: String,
    hash: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    backup: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    warnings: Vec<String>,
}

async fn config_write(ctx: &Ctx, args: ConfigWriteArgs) -> Result<ConfigWriteResult, ToolError> {
    let doc = SharedDocument::parse(&args.document)?;
    let outcome =
        apply_local_write(ctx, doc, args.content, args.expect_hash, args.expect_absent).await?;
    Ok(ConfigWriteResult {
        document: outcome.document,
        hash: outcome.hash_after,
        backup: outcome.backup,
        warnings: outcome.warnings,
    })
}

/// Direction of `fleet.sync`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum SyncDirection {
    /// Compare BLAKE3 hashes without writing anything.
    #[default]
    Status,
    /// Send local shared documents to remote machines when they have not diverged.
    Push,
    /// Fetch shared documents from one remote machine when local copies have not diverged.
    Pull,
}

/// Per-document sync status reported by `fleet.sync`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SyncStatus {
    /// Local and remote bytes have the same hash (or both are absent).
    Same,
    /// Local document was pushed to the remote (or would be under `dry_run`).
    Pushed,
    /// Remote document was pulled locally (or would be under `dry_run`).
    Pulled,
    /// Only the local node changed since the last recorded sync with this peer.
    ChangedHere,
    /// Only the remote node changed since the last recorded sync with this peer.
    ChangedThere,
    /// Both nodes differ from the last recorded sync (or both exist with different bytes and no prior sync).
    Both,
    /// The document exists only on this node (not on the remote).
    OnlyHere,
    /// The document exists only on the remote node (not on this node).
    OnlyThere,
    /// The destination changed since the last recorded sync with this peer (or existed without one).
    Conflict,
    /// Reading, validating, or writing failed; see `error`.
    Error,
}

/// Arguments of `fleet.sync`.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct SyncArgs {
    /// Machines to sync with; defaults to all machines in fleet.toml other than this node.
    #[serde(default)]
    machines: Option<Vec<String>>,
    /// Shared documents to sync; defaults to devices.toml, fleet.toml, policy.toml, projects.toml, and any tools.d/*.toml files.
    #[serde(default)]
    documents: Option<Vec<String>>,
    /// `status` (default, compares hashes only), `push` (local -> remote), or `pull` (remote -> local).
    #[serde(default)]
    direction: SyncDirection,
    /// Source machine to pull from when `direction` is `pull`.
    #[serde(default)]
    from: Option<String>,
    /// Overwrite conflicts without failing (the receiver still validates every document strictly).
    #[serde(default)]
    force: bool,
    /// Report what would happen without modifying any configuration, backup, or sidecar files.
    #[serde(default)]
    dry_run: bool,
    /// Optional per-machine timeout in seconds (up to 300).
    #[serde(default)]
    timeout_s: Option<f64>,
}

#[derive(Debug, Serialize)]
struct SyncResult {
    node: String,
    direction: SyncDirection,
    dry_run: bool,
    results: BTreeMap<String, BTreeMap<String, DocSyncOutcome>>,
}

#[derive(Debug, Serialize)]
struct DocSyncOutcome {
    status: SyncStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    local_hash: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    remote_hash: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    last_synced_hash: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    backup: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    detail: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    warnings: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<ErrorBody>,
}

async fn fleet_sync(fleet: &Fleet, ctx: &Ctx, args: SyncArgs) -> Result<SyncResult, ToolError> {
    let per_machine_timeout = match args.timeout_s {
        Some(secs) => {
            if !secs.is_finite() || secs <= 0.0 || secs > MAX_SYNC_TIMEOUT_S {
                return Err(ErrorBody::new(
                    ErrorCode::InvalidArgs,
                    format!(
                        "fleet.sync timeout_s must be greater than 0 and at most {MAX_SYNC_TIMEOUT_S}, and {secs} was given."
                    ),
                )
                .with_hint("Give timeout_s as a positive number of seconds up to 300."));
            }
            Some(Duration::from_secs_f64(secs))
        }
        None => None,
    };

    let here = ctx.node_name().to_owned();
    let fleet_file = ctx.config().get::<FleetFile>()?;

    // Validate requested document names up front so disallowed files fail immediately.
    let explicit_docs: Option<Vec<SharedDocument>> = match &args.documents {
        Some(names) => {
            let mut parsed = Vec::with_capacity(names.len());
            for name in names {
                parsed.push(SharedDocument::parse(name)?);
            }
            Some(parsed)
        }
        None => None,
    };

    let targets = resolve_sync_targets(&fleet_file, &here, &args)?;

    let local_docs = {
        let config_dir = ctx.paths().config_dir().to_path_buf();
        tokio::task::spawn_blocking(move || list_local_shared_documents(&config_dir))
            .await
            .map_err(|error| {
                ErrorBody::new(
                    ErrorCode::Internal,
                    format!("Listing shared documents stopped unexpectedly: {error}."),
                )
            })??
    };

    let mut results = BTreeMap::new();
    match args.direction {
        SyncDirection::Pull => {
            // Pull targets exactly one machine so local files are never overwritten by racing peers.
            if let Some(machine) = targets.into_iter().next() {
                let per_doc = sync_one_machine_with_timeout(
                    fleet,
                    ctx,
                    &machine,
                    args.direction,
                    explicit_docs.as_deref(),
                    &local_docs,
                    args.force,
                    args.dry_run,
                    per_machine_timeout,
                )
                .await;
                results.insert(machine, per_doc);
            }
        }
        SyncDirection::Status | SyncDirection::Push => {
            let futs: Vec<BoxFuture<'_, (String, BTreeMap<String, DocSyncOutcome>)>> = targets
                .into_iter()
                .map(|machine| {
                    let explicit_docs = explicit_docs.as_deref();
                    let local_docs = local_docs.as_slice();
                    Box::pin(async move {
                        let per_doc = sync_one_machine_with_timeout(
                            fleet,
                            ctx,
                            &machine,
                            args.direction,
                            explicit_docs,
                            local_docs,
                            args.force,
                            args.dry_run,
                            per_machine_timeout,
                        )
                        .await;
                        (machine, per_doc)
                    }) as BoxFuture<'_, _>
                })
                .collect();
            for (machine, per_doc) in join_all_concurrent(futs).await {
                results.insert(machine, per_doc);
            }
            if ctx.is_cancelled() {
                return Err(ErrorBody::new(
                    ErrorCode::Cancelled,
                    "The call to fleet.sync was cancelled.",
                ));
            }
        }
    }

    Ok(SyncResult {
        node: here,
        direction: args.direction,
        dry_run: args.dry_run,
        results,
    })
}

#[allow(clippy::too_many_arguments)]
async fn sync_one_machine_with_timeout(
    fleet: &Fleet,
    ctx: &Ctx,
    machine: &str,
    direction: SyncDirection,
    explicit_docs: Option<&[SharedDocument]>,
    local_docs: &[String],
    force: bool,
    dry_run: bool,
    timeout: Option<Duration>,
) -> BTreeMap<String, DocSyncOutcome> {
    let fut = sync_one_machine(
        fleet,
        ctx,
        machine,
        direction,
        explicit_docs,
        local_docs,
        force,
        dry_run,
    );
    let Some(limit) = timeout else {
        return fut.await;
    };
    match tokio::time::timeout(limit, fut).await {
        Ok(outcomes) => outcomes,
        Err(_) => {
            let err = ErrorBody::new(
                ErrorCode::Timeout,
                format!(
                    "Timed out syncing with {machine} after {:.1}s.",
                    limit.as_secs_f64()
                ),
            )
            .with_machine(machine);
            let mut map = BTreeMap::new();
            for doc in default_doc_list(explicit_docs, local_docs) {
                map.insert(doc.canonical_name(), error_outcome(err.clone()));
            }
            map
        }
    }
}

fn resolve_sync_targets(
    fleet_file: &FleetFile,
    here: &str,
    args: &SyncArgs,
) -> Result<Vec<String>, ErrorBody> {
    match args.direction {
        SyncDirection::Pull => {
            let source = match (&args.from, &args.machines) {
                (Some(from), Some(machines)) => {
                    if machines.len() != 1 || machines[0] != *from {
                        return Err(ErrorBody::new(
                            ErrorCode::InvalidArgs,
                            "For pull, `from` and `machines` must not name different machines.",
                        )
                        .with_hint("Pass `from` naming the single machine to pull from."));
                    }
                    from.clone()
                }
                (Some(from), None) => from.clone(),
                (None, Some(machines)) if machines.len() == 1 => machines[0].clone(),
                (None, _) => {
                    return Err(ErrorBody::new(
                        ErrorCode::InvalidArgs,
                        "fleet.sync with direction=\"pull\" requires `from` naming one machine.",
                    )
                    .with_hint("Pass `from=<machine>` to choose which machine to pull from."));
                }
            };
            if source == here {
                return Err(ErrorBody::new(
                    ErrorCode::InvalidArgs,
                    format!("Cannot pull configuration from {here}, which is this node."),
                ));
            }
            Ok(vec![source])
        }
        SyncDirection::Status | SyncDirection::Push => {
            if args.from.is_some() {
                return Err(ErrorBody::new(
                    ErrorCode::InvalidArgs,
                    "`from` is only used when direction is \"pull\".",
                )
                .with_hint("Use `machines` to select targets for status or push."));
            }
            match &args.machines {
                Some(machines) => {
                    let mut out = Vec::new();
                    let mut seen = BTreeSet::new();
                    for machine in machines {
                        if machine == here {
                            return Err(ErrorBody::new(
                                ErrorCode::InvalidArgs,
                                format!("Cannot sync with {here}, which is this node."),
                            ));
                        }
                        if seen.insert(machine.clone()) {
                            out.push(machine.clone());
                        }
                    }
                    Ok(out)
                }
                None => Ok(fleet_file
                    .machines
                    .keys()
                    .filter(|name| name.as_str() != here)
                    .cloned()
                    .collect()),
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn sync_one_machine(
    fleet: &Fleet,
    ctx: &Ctx,
    machine: &str,
    direction: SyncDirection,
    explicit_docs: Option<&[SharedDocument]>,
    local_docs: &[String],
    force: bool,
    dry_run: bool,
) -> BTreeMap<String, DocSyncOutcome> {
    let docs = match determine_documents_for_machine(fleet, ctx, machine, explicit_docs, local_docs)
        .await
    {
        Ok(docs) => docs,
        Err(error) => {
            let mut map = BTreeMap::new();
            let fallback_docs = default_doc_list(explicit_docs, local_docs);
            for doc in fallback_docs {
                map.insert(doc.canonical_name(), error_outcome(error.clone()));
            }
            return map;
        }
    };

    let mut outcomes = BTreeMap::new();
    for doc in docs {
        let name = doc.canonical_name();
        let outcome = sync_one_document(fleet, ctx, machine, &doc, direction, force, dry_run).await;
        outcomes.insert(name, outcome);
    }
    outcomes
}

/// Drives borrowed machine-sync futures concurrently on the current task so every remote call
/// goes straight through [`Fleet::call_remote`] with the caller's live [`Ctx`].
async fn join_all_concurrent<T>(futures: Vec<BoxFuture<'_, T>>) -> Vec<T> {
    let mut pending: Vec<Option<BoxFuture<'_, T>>> = futures.into_iter().map(Some).collect();
    let mut done: Vec<Option<T>> = (0..pending.len()).map(|_| None).collect();
    std::future::poll_fn(|cx| {
        let mut remaining = 0;
        for (slot, out) in pending.iter_mut().zip(done.iter_mut()) {
            if let Some(fut) = slot.as_mut() {
                match fut.as_mut().poll(cx) {
                    std::task::Poll::Ready(val) => {
                        *out = Some(val);
                        *slot = None;
                    }
                    std::task::Poll::Pending => remaining += 1,
                }
            }
        }
        if remaining == 0 {
            std::task::Poll::Ready(())
        } else {
            std::task::Poll::Pending
        }
    })
    .await;
    done.into_iter().flatten().collect()
}

async fn determine_documents_for_machine(
    fleet: &Fleet,
    ctx: &Ctx,
    machine: &str,
    explicit_docs: Option<&[SharedDocument]>,
    local_docs: &[String],
) -> Result<Vec<SharedDocument>, ErrorBody> {
    if let Some(explicit) = explicit_docs {
        return Ok(explicit.to_vec());
    }
    let mut names: BTreeSet<String> = FIXED_SHARED_DOCUMENTS
        .iter()
        .map(|s| (*s).to_owned())
        .collect();
    for name in local_docs {
        names.insert(name.clone());
    }
    match fleet
        .call_remote(ctx, machine, "config.list", json!({}), EventSink::discard())
        .await
    {
        Ok(value) => {
            if let Ok(listed) = serde_json::from_value::<ConfigListResult>(value) {
                for name in listed.documents {
                    if SharedDocument::parse(&name).is_ok() {
                        names.insert(name);
                    }
                }
            }
        }
        Err(error) if error.code == ErrorCode::UnknownTool => {}
        Err(error) => return Err(error),
    }
    Ok(names
        .into_iter()
        .filter_map(|name| SharedDocument::parse(&name).ok())
        .collect())
}

fn default_doc_list(
    explicit_docs: Option<&[SharedDocument]>,
    local_docs: &[String],
) -> Vec<SharedDocument> {
    if let Some(explicit) = explicit_docs {
        return explicit.to_vec();
    }
    let mut names: BTreeSet<String> = FIXED_SHARED_DOCUMENTS
        .iter()
        .map(|s| (*s).to_owned())
        .collect();
    for name in local_docs {
        names.insert(name.clone());
    }
    names
        .into_iter()
        .filter_map(|name| SharedDocument::parse(&name).ok())
        .collect()
}

async fn sync_one_document(
    fleet: &Fleet,
    ctx: &Ctx,
    machine: &str,
    doc: &SharedDocument,
    direction: SyncDirection,
    force: bool,
    dry_run: bool,
) -> DocSyncOutcome {
    if *doc == SharedDocument::Devices {
        return sync_devices_document(fleet, ctx, machine, direction, dry_run).await;
    }

    let config_dir = ctx.paths().config_dir().to_path_buf();
    let state_dir = ctx.paths().state_dir().to_path_buf();
    let doc_clone = doc.clone();

    let local =
        match tokio::task::spawn_blocking(move || read_local_document(&config_dir, &doc_clone))
            .await
        {
            Ok(Ok(read)) => read,
            Ok(Err(error)) => return error_outcome(error),
            Err(error) => {
                return error_outcome(ErrorBody::new(
                    ErrorCode::Internal,
                    format!("Reading local document stopped unexpectedly: {error}."),
                ));
            }
        };

    let sidecar_state = state_dir.clone();
    let doc_for_sidecar = doc.clone();
    let sidecar =
        match tokio::task::spawn_blocking(move || read_sidecar(&sidecar_state, &doc_for_sidecar))
            .await
        {
            Ok(Ok(sidecar)) => sidecar,
            Ok(Err(error)) => return error_outcome(error),
            Err(error) => {
                return error_outcome(ErrorBody::new(
                    ErrorCode::Internal,
                    format!("Reading sync sidecar stopped unexpectedly: {error}."),
                ));
            }
        };
    let last_synced_hash = sidecar.peers.get(machine).map(|rec| rec.hash.clone());

    let remote_val = match fleet
        .call_remote(
            ctx,
            machine,
            "config.read",
            json!({"document": doc.canonical_name()}),
            EventSink::discard(),
        )
        .await
    {
        Ok(val) => val,
        Err(error) => return error_outcome(error),
    };
    let remote: ConfigReadResult = match serde_json::from_value(remote_val) {
        Ok(parsed) => parsed,
        Err(error) => {
            return error_outcome(ErrorBody::new(
                ErrorCode::Failed,
                format!("Remote config.read returned an unexpected shape: {error}."),
            ));
        }
    };

    let decision = decide_sync(
        direction,
        local.hash.as_deref(),
        remote.hash.as_deref(),
        last_synced_hash.as_deref(),
        force,
    );

    match decision {
        SyncDecision::Same => {
            if !dry_run
                && direction != SyncDirection::Status
                && let Some(hash) = local.hash.as_deref()
                && last_synced_hash.as_deref() != Some(hash)
            {
                let _ = record_sidecar_async(&state_dir, doc, machine, hash).await;
            }
            DocSyncOutcome {
                status: SyncStatus::Same,
                local_hash: local.hash,
                remote_hash: remote.hash,
                last_synced_hash,
                backup: None,
                detail: None,
                warnings: Vec::new(),
                error: None,
            }
        }
        SyncDecision::Report(status, reason) => DocSyncOutcome {
            status,
            local_hash: local.hash,
            remote_hash: remote.hash,
            last_synced_hash,
            backup: None,
            detail: reason,
            warnings: Vec::new(),
            error: None,
        },
        SyncDecision::Conflict(reason) => DocSyncOutcome {
            status: SyncStatus::Conflict,
            local_hash: local.hash,
            remote_hash: remote.hash,
            last_synced_hash,
            backup: None,
            detail: Some(reason),
            warnings: Vec::new(),
            error: None,
        },
        SyncDecision::Push => {
            let Some(local_content) = local.content else {
                return DocSyncOutcome {
                    status: SyncStatus::OnlyThere,
                    local_hash: local.hash,
                    remote_hash: remote.hash,
                    last_synced_hash,
                    backup: None,
                    detail: Some("Document does not exist locally.".to_owned()),
                    warnings: Vec::new(),
                    error: None,
                };
            };
            let Some(local_hash) = local.hash.clone() else {
                return error_outcome(ErrorBody::new(
                    ErrorCode::Internal,
                    "Local document had content without a hash.",
                ));
            };
            if dry_run {
                return DocSyncOutcome {
                    status: SyncStatus::Pushed,
                    local_hash: Some(local_hash),
                    remote_hash: remote.hash,
                    last_synced_hash,
                    backup: None,
                    detail: Some("Would push to remote (dry_run).".to_owned()),
                    warnings: Vec::new(),
                    error: None,
                };
            }
            let mut write_args = json!({
                "document": doc.canonical_name(),
                "content": local_content,
            });
            if !force && let Some(obj) = write_args.as_object_mut() {
                match remote.hash {
                    Some(ref rh) => {
                        obj.insert("expect_hash".to_owned(), json!(rh));
                    }
                    None => {
                        obj.insert("expect_absent".to_owned(), json!(true));
                    }
                }
            }
            match fleet
                .call_remote(
                    ctx,
                    machine,
                    "config.write",
                    write_args,
                    EventSink::discard(),
                )
                .await
            {
                Ok(val) => {
                    let written: ConfigWriteResult = match serde_json::from_value(val) {
                        Ok(parsed) => parsed,
                        Err(error) => {
                            return error_outcome(ErrorBody::new(
                                ErrorCode::Failed,
                                format!(
                                    "Remote config.write returned an unexpected shape: {error}."
                                ),
                            ));
                        }
                    };
                    if let Err(error) =
                        record_sidecar_async(&state_dir, doc, machine, &written.hash).await
                    {
                        return error_outcome(error);
                    }
                    DocSyncOutcome {
                        status: SyncStatus::Pushed,
                        local_hash: Some(local_hash),
                        remote_hash: Some(written.hash.clone()),
                        last_synced_hash: Some(written.hash),
                        backup: written.backup,
                        detail: None,
                        warnings: written.warnings,
                        error: None,
                    }
                }
                Err(error) if error.code == ErrorCode::Conflict => DocSyncOutcome {
                    status: SyncStatus::Conflict,
                    local_hash: Some(local_hash),
                    remote_hash: remote.hash,
                    last_synced_hash,
                    backup: None,
                    detail: Some(error.message.clone()),
                    warnings: Vec::new(),
                    error: Some(error),
                },
                Err(error) => DocSyncOutcome {
                    status: SyncStatus::Error,
                    local_hash: Some(local_hash),
                    remote_hash: remote.hash,
                    last_synced_hash,
                    backup: None,
                    detail: None,
                    warnings: Vec::new(),
                    error: Some(error),
                },
            }
        }
        SyncDecision::Pull => {
            let Some(remote_content) = remote.content else {
                return error_outcome(ErrorBody::new(
                    ErrorCode::Failed,
                    "Remote config.read reported exists=true without content.",
                ));
            };
            let Some(remote_hash) = remote.hash.clone() else {
                return error_outcome(ErrorBody::new(
                    ErrorCode::Failed,
                    "Remote config.read reported exists=true without hash.",
                ));
            };
            if dry_run {
                let target = doc.config_path(ctx.paths().config_dir());
                let is_builtin = |name: &str| ctx.has_builtin(name);
                if let Err(error) = doc.validate(&target, &remote_content, &is_builtin) {
                    return DocSyncOutcome {
                        status: SyncStatus::Error,
                        local_hash: local.hash,
                        remote_hash: Some(remote_hash),
                        last_synced_hash,
                        backup: None,
                        detail: None,
                        warnings: Vec::new(),
                        error: Some(error),
                    };
                }
                return DocSyncOutcome {
                    status: SyncStatus::Pulled,
                    local_hash: local.hash,
                    remote_hash: Some(remote_hash),
                    last_synced_hash,
                    backup: None,
                    detail: Some("Would pull from remote (dry_run).".to_owned()),
                    warnings: Vec::new(),
                    error: None,
                };
            }
            let (expect_hash, expect_absent) = if force {
                (None, false)
            } else {
                match local.hash.clone() {
                    Some(lh) => (Some(lh), false),
                    None => (None, true),
                }
            };
            match apply_local_write(ctx, doc.clone(), remote_content, expect_hash, expect_absent)
                .await
            {
                Ok(written) => {
                    if let Err(error) =
                        record_sidecar_async(&state_dir, doc, machine, &written.hash_after).await
                    {
                        return error_outcome(error);
                    }
                    DocSyncOutcome {
                        status: SyncStatus::Pulled,
                        local_hash: Some(written.hash_after.clone()),
                        remote_hash: Some(remote_hash),
                        last_synced_hash: Some(written.hash_after),
                        backup: written.backup,
                        detail: None,
                        warnings: written.warnings,
                        error: None,
                    }
                }
                Err(error) if error.code == ErrorCode::Conflict => DocSyncOutcome {
                    status: SyncStatus::Conflict,
                    local_hash: local.hash,
                    remote_hash: Some(remote_hash),
                    last_synced_hash,
                    backup: None,
                    detail: Some(error.message.clone()),
                    warnings: Vec::new(),
                    error: Some(error),
                },
                Err(error) => DocSyncOutcome {
                    status: SyncStatus::Error,
                    local_hash: local.hash,
                    remote_hash: Some(remote_hash),
                    last_synced_hash,
                    backup: None,
                    detail: None,
                    warnings: Vec::new(),
                    error: Some(error),
                },
            }
        }
    }
}

async fn sync_devices_document(
    fleet: &Fleet,
    ctx: &Ctx,
    machine: &str,
    direction: SyncDirection,
    dry_run: bool,
) -> DocSyncOutcome {
    let doc = SharedDocument::Devices;
    let state_dir = ctx.paths().state_dir().to_path_buf();
    let local_file_exists = ctx.paths().config_file("devices.toml").is_file();

    let local = match read_local_document_for_ctx(ctx, &doc).await {
        Ok(read) => read,
        Err(error) => return error_outcome(error),
    };
    let sidecar_state = state_dir.clone();
    let sidecar = match tokio::task::spawn_blocking(move || {
        read_sidecar(&sidecar_state, &SharedDocument::Devices)
    })
    .await
    {
        Ok(Ok(sidecar)) => sidecar,
        Ok(Err(error)) => return error_outcome(error),
        Err(error) => {
            return error_outcome(ErrorBody::new(
                ErrorCode::Internal,
                format!("Reading sync sidecar stopped unexpectedly: {error}."),
            ));
        }
    };
    let last_synced_hash = sidecar.peers.get(machine).map(|rec| rec.hash.clone());

    let remote_val = match fleet
        .call_remote(
            ctx,
            machine,
            "config.read",
            json!({"document": "devices.toml"}),
            EventSink::discard(),
        )
        .await
    {
        Ok(val) => val,
        Err(error) => return error_outcome(error),
    };
    let remote: ConfigReadResult = match serde_json::from_value(remote_val) {
        Ok(parsed) => parsed,
        Err(error) => {
            return error_outcome(ErrorBody::new(
                ErrorCode::Failed,
                format!("Remote config.read returned an unexpected shape: {error}."),
            ));
        }
    };

    let local_path = doc.config_path(ctx.paths().config_dir());
    let local_devices = match local.content.as_deref() {
        Some(text) => match parse::<DevicesFile>(&local_path, text.as_bytes()) {
            Ok(parsed) => parsed,
            Err(error) => return error_outcome(error),
        },
        None => DevicesFile::default(),
    };
    let remote_devices = match remote.content.as_deref() {
        Some(text) => match parse::<DevicesFile>(Path::new("devices.toml"), text.as_bytes()) {
            Ok(parsed) => parsed,
            Err(error) => return error_outcome(error),
        },
        None => DevicesFile::default(),
    };

    let merged = merge_devices(&local_devices, &remote_devices);
    let merged_is_empty = merged.devices.is_empty() && merged.removed.is_empty();
    let merged_toml = merged.to_toml_string();
    let merged_hash = hash_bytes(merged_toml.as_bytes()).to_string();

    if direction == SyncDirection::Status {
        let (status, detail) = classify_devices_status(
            &local_devices,
            &remote_devices,
            &merged,
            local.exists,
            remote.exists,
        );
        return DocSyncOutcome {
            status,
            local_hash: local.hash,
            remote_hash: remote.hash,
            last_synced_hash,
            backup: None,
            detail,
            warnings: Vec::new(),
            error: None,
        };
    }

    let local_up_to_date = local_devices == merged && (merged_is_empty || local_file_exists);
    let remote_up_to_date = remote_devices == merged && (merged_is_empty || remote.exists);

    if local_up_to_date && remote_up_to_date {
        if !dry_run && !merged_is_empty && last_synced_hash.as_deref() != Some(merged_hash.as_str())
        {
            let _ = record_sidecar_async(&state_dir, &doc, machine, &merged_hash).await;
        }
        return DocSyncOutcome {
            status: SyncStatus::Same,
            local_hash: local.hash,
            remote_hash: remote.hash,
            last_synced_hash,
            backup: None,
            detail: None,
            warnings: Vec::new(),
            error: None,
        };
    }

    let applied_status = match direction {
        SyncDirection::Push => SyncStatus::Pushed,
        SyncDirection::Pull => SyncStatus::Pulled,
        SyncDirection::Status => SyncStatus::Same,
    };

    if dry_run {
        return DocSyncOutcome {
            status: applied_status,
            local_hash: local.hash,
            remote_hash: remote.hash,
            last_synced_hash,
            backup: None,
            detail: Some("Would merge devices.toml on both machines (dry_run).".to_owned()),
            warnings: Vec::new(),
            error: None,
        };
    }

    let mut current_toml = merged_toml;
    let mut current_hash = merged_hash.clone();
    let mut local_backup = None;
    let mut warnings = Vec::new();

    if !local_up_to_date {
        match apply_local_write(
            ctx,
            SharedDocument::Devices,
            current_toml.clone(),
            None,
            false,
        )
        .await
        {
            Ok(written) => {
                local_backup = written.backup;
                current_hash = written.hash_after;
                warnings.extend(written.warnings);
                if let Some(canonical) = written.canonical_content {
                    current_toml = canonical;
                }
            }
            Err(error) => {
                return DocSyncOutcome {
                    status: SyncStatus::Error,
                    local_hash: local.hash,
                    remote_hash: remote.hash,
                    last_synced_hash,
                    backup: None,
                    detail: None,
                    warnings: Vec::new(),
                    error: Some(error),
                };
            }
        }
    }

    let need_remote_write = !remote_up_to_date || current_hash != merged_hash;
    let mut final_remote_hash = remote.hash.clone();
    if need_remote_write {
        match fleet
            .call_remote(
                ctx,
                machine,
                "config.write",
                json!({
                    "document": "devices.toml",
                    "content": current_toml,
                }),
                EventSink::discard(),
            )
            .await
        {
            Ok(val) => {
                let written: ConfigWriteResult = match serde_json::from_value(val) {
                    Ok(parsed) => parsed,
                    Err(error) => {
                        return error_outcome(ErrorBody::new(
                            ErrorCode::Failed,
                            format!("Remote config.write returned an unexpected shape: {error}."),
                        ));
                    }
                };
                final_remote_hash = Some(written.hash.clone());
                warnings.extend(written.warnings);
                // If the remote concurrently merged another change while accepting our write,
                // pull the remote's merged state once more so both machines converge identically.
                if written.hash != current_hash
                    && let Ok(again_val) = fleet
                        .call_remote(
                            ctx,
                            machine,
                            "config.read",
                            json!({"document": "devices.toml"}),
                            EventSink::discard(),
                        )
                        .await
                    && let Ok(again) = serde_json::from_value::<ConfigReadResult>(again_val)
                    && let Some(again_content) = again.content
                    && let Ok(final_local) =
                        apply_local_write(ctx, SharedDocument::Devices, again_content, None, false)
                            .await
                {
                    current_hash = final_local.hash_after;
                    warnings.extend(final_local.warnings);
                }
            }
            Err(error) => {
                return DocSyncOutcome {
                    status: SyncStatus::Error,
                    local_hash: Some(current_hash),
                    remote_hash: remote.hash,
                    last_synced_hash,
                    backup: local_backup,
                    detail: None,
                    warnings,
                    error: Some(error),
                };
            }
        }
    }

    if let Err(error) = record_sidecar_async(&state_dir, &doc, machine, &current_hash).await {
        return error_outcome(error);
    }

    DocSyncOutcome {
        status: applied_status,
        local_hash: Some(current_hash.clone()),
        remote_hash: final_remote_hash.or_else(|| Some(current_hash.clone())),
        last_synced_hash: Some(current_hash),
        backup: local_backup,
        detail: None,
        warnings,
        error: None,
    }
}

fn classify_devices_status(
    local: &DevicesFile,
    remote: &DevicesFile,
    merged: &DevicesFile,
    local_exists: bool,
    remote_exists: bool,
) -> (SyncStatus, Option<String>) {
    if local == remote {
        return (SyncStatus::Same, None);
    }
    if !local_exists && remote_exists {
        return (SyncStatus::OnlyThere, None);
    }
    if local_exists && !remote_exists {
        return (SyncStatus::OnlyHere, None);
    }
    if merged == local && merged != remote {
        return (SyncStatus::ChangedHere, None);
    }
    if merged == remote && merged != local {
        return (SyncStatus::ChangedThere, None);
    }
    (
        SyncStatus::Both,
        Some("Both machines have device changes; push or pull will merge them cleanly.".to_owned()),
    )
}

async fn record_sidecar_async(
    state_dir: &Path,
    doc: &SharedDocument,
    machine: &str,
    hash: &str,
) -> Result<(), ErrorBody> {
    let state_dir = state_dir.to_path_buf();
    let doc = doc.clone();
    let machine = machine.to_owned();
    let hash = hash.to_owned();
    tokio::task::spawn_blocking(move || record_sidecar_sync(&state_dir, &doc, &machine, &hash))
        .await
        .map_err(|error| {
            ErrorBody::new(
                ErrorCode::Internal,
                format!("Updating sync sidecar stopped unexpectedly: {error}."),
            )
        })?
}

fn error_outcome(error: ErrorBody) -> DocSyncOutcome {
    DocSyncOutcome {
        status: SyncStatus::Error,
        local_hash: None,
        remote_hash: None,
        last_synced_hash: None,
        backup: None,
        detail: None,
        warnings: Vec::new(),
        error: Some(error),
    }
}

/// Pure sync decision for one document between local and remote hashes and the last synced hash.
#[derive(Debug, PartialEq, Eq)]
pub enum SyncDecision {
    /// Local and remote match (or both are absent).
    Same,
    /// Non-mutating report (`status` mode, or one-sided presence where push/pull does not delete).
    Report(SyncStatus, Option<String>),
    /// Destination diverged since last sync (or exists with no prior sync) and `force` is false.
    Conflict(String),
    /// Safe (or forced) to push local content to remote.
    Push,
    /// Safe (or forced) to pull remote content to local.
    Pull,
}

/// Decides what `fleet.sync` should do for one `(machine, document)` pair.
pub fn decide_sync(
    direction: SyncDirection,
    local_hash: Option<&str>,
    remote_hash: Option<&str>,
    last_synced_hash: Option<&str>,
    force: bool,
) -> SyncDecision {
    if local_hash == remote_hash {
        return SyncDecision::Same;
    }
    match direction {
        SyncDirection::Status => match (local_hash, remote_hash) {
            (Some(_), None) => SyncDecision::Report(SyncStatus::OnlyHere, None),
            (None, Some(_)) => SyncDecision::Report(SyncStatus::OnlyThere, None),
            (Some(local), Some(remote)) => match last_synced_hash {
                Some(synced) if synced == remote => {
                    SyncDecision::Report(SyncStatus::ChangedHere, None)
                }
                Some(synced) if synced == local => {
                    SyncDecision::Report(SyncStatus::ChangedThere, None)
                }
                Some(_) => SyncDecision::Report(
                    SyncStatus::Both,
                    Some("Both machines changed since the last sync.".to_owned()),
                ),
                None => SyncDecision::Report(
                    SyncStatus::Both,
                    Some(
                        "Both machines have different copies and no prior sync is recorded."
                            .to_owned(),
                    ),
                ),
            },
            (None, None) => SyncDecision::Same,
        },
        SyncDirection::Push => {
            if local_hash.is_none() {
                return SyncDecision::Report(
                    SyncStatus::OnlyThere,
                    Some(
                        "Document does not exist locally; push does not delete remote files."
                            .to_owned(),
                    ),
                );
            }
            if force || remote_hash.is_none() || remote_hash == last_synced_hash {
                SyncDecision::Push
            } else {
                let reason = match last_synced_hash {
                    Some(_) => "Remote document changed since the last sync with this machine; pull first or pass force=true to overwrite.".to_owned(),
                    None => "Remote already has a different copy and no prior sync is recorded; pull first or pass force=true to overwrite.".to_owned(),
                };
                SyncDecision::Conflict(reason)
            }
        }
        SyncDirection::Pull => {
            if remote_hash.is_none() {
                return SyncDecision::Report(
                    SyncStatus::OnlyHere,
                    Some(
                        "Document does not exist on the remote; pull does not delete local files."
                            .to_owned(),
                    ),
                );
            }
            if force || local_hash.is_none() || local_hash == last_synced_hash {
                SyncDecision::Pull
            } else {
                let reason = match last_synced_hash {
                    Some(_) => "Local document changed since the last sync with this machine; push first or pass force=true to overwrite.".to_owned(),
                    None => "Local already has a different copy and no prior sync is recorded; push first or pass force=true to overwrite.".to_owned(),
                };
                SyncDecision::Conflict(reason)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shared_document_whitelist_refuses_local_files_and_traversal() {
        assert_eq!(
            SharedDocument::parse("devices.toml").unwrap(),
            SharedDocument::Devices
        );
        assert_eq!(
            SharedDocument::parse("fleet.toml").unwrap(),
            SharedDocument::Fleet
        );
        assert_eq!(
            SharedDocument::parse("policy.toml").unwrap(),
            SharedDocument::Policy
        );
        assert_eq!(
            SharedDocument::parse("projects.toml").unwrap(),
            SharedDocument::Projects
        );
        assert_eq!(
            SharedDocument::parse("tools.d/deploy.toml").unwrap(),
            SharedDocument::ToolExt("deploy.toml".to_owned())
        );
        assert_eq!(
            SharedDocument::parse("tools.d/gpu.free.toml").unwrap(),
            SharedDocument::ToolExt("gpu.free.toml".to_owned())
        );
        assert_eq!(
            SharedDocument::parse("tools.d/a.b.c.toml").unwrap(),
            SharedDocument::ToolExt("a.b.c.toml".to_owned())
        );

        let too_long = format!("tools.d/{}.toml", "a".repeat(MAX_TOOLS_D_NAME));
        for rejected in [
            "node.toml",
            "tokens.toml",
            "other.toml",
            "../policy.toml",
            "tools.d/",
            "tools.d/.toml",
            "tools.d/.hidden.toml",
            "tools.d/sub/a.toml",
            "tools.d/sub/a.b.toml",
            "tools.d/a.json",
            "tools.d/a\\b.toml",
            "tools.d/..toml",
            "tools.d/a..toml",
            "tools.d/a..b.toml",
            too_long.as_str(),
        ] {
            let err = SharedDocument::parse(rejected).unwrap_err();
            assert_eq!(err.code, ErrorCode::InvalidArgs, "{rejected}");
        }
    }

    #[test]
    fn decide_sync_enforces_sidecar_ancestry_and_force() {
        assert_eq!(
            decide_sync(SyncDirection::Status, Some("h1"), Some("h1"), None, false),
            SyncDecision::Same
        );
        assert_eq!(
            decide_sync(SyncDirection::Status, Some("h1"), None, None, false),
            SyncDecision::Report(SyncStatus::OnlyHere, None)
        );
        assert_eq!(
            decide_sync(SyncDirection::Status, None, Some("h1"), Some("h1"), false),
            SyncDecision::Report(SyncStatus::OnlyThere, None)
        );
        assert_eq!(
            decide_sync(
                SyncDirection::Status,
                Some("h2"),
                Some("h1"),
                Some("h1"),
                false
            ),
            SyncDecision::Report(SyncStatus::ChangedHere, None)
        );
        assert_eq!(
            decide_sync(
                SyncDirection::Status,
                Some("h1"),
                Some("h2"),
                Some("h1"),
                false
            ),
            SyncDecision::Report(SyncStatus::ChangedThere, None)
        );
        assert!(matches!(
            decide_sync(
                SyncDirection::Status,
                Some("h2"),
                Some("h3"),
                Some("h1"),
                false
            ),
            SyncDecision::Report(SyncStatus::Both, Some(_))
        ));
        assert!(matches!(
            decide_sync(SyncDirection::Status, Some("h1"), Some("h2"), None, false),
            SyncDecision::Report(SyncStatus::Both, Some(_))
        ));
        // Push to empty remote succeeds without prior sync.
        assert_eq!(
            decide_sync(SyncDirection::Push, Some("h1"), None, None, false),
            SyncDecision::Push
        );
        // Push when remote still matches last synced hash succeeds.
        assert_eq!(
            decide_sync(
                SyncDirection::Push,
                Some("h2"),
                Some("h1"),
                Some("h1"),
                false
            ),
            SyncDecision::Push
        );
        // Push when remote changed since last sync is a conflict unless forced.
        assert!(matches!(
            decide_sync(
                SyncDirection::Push,
                Some("h2"),
                Some("h3"),
                Some("h1"),
                false
            ),
            SyncDecision::Conflict(_)
        ));
        assert_eq!(
            decide_sync(
                SyncDirection::Push,
                Some("h2"),
                Some("h3"),
                Some("h1"),
                true
            ),
            SyncDecision::Push
        );
        // Pull mirrors push.
        assert_eq!(
            decide_sync(
                SyncDirection::Pull,
                Some("h1"),
                Some("h2"),
                Some("h1"),
                false
            ),
            SyncDecision::Pull
        );
        assert!(matches!(
            decide_sync(
                SyncDirection::Pull,
                Some("h3"),
                Some("h2"),
                Some("h1"),
                false
            ),
            SyncDecision::Conflict(_)
        ));
        assert_eq!(
            decide_sync(
                SyncDirection::Pull,
                Some("h3"),
                Some("h2"),
                Some("h1"),
                true
            ),
            SyncDecision::Pull
        );
    }

    #[test]
    fn local_write_validates_strictly_and_writes_single_backup() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("config");
        let state = dir.path().join("state");

        // Invalid policy is refused before creating anything.
        let no_builtins = |_: &str| false;
        let bad = write_local_document_blocking(
            &cfg,
            &state,
            &SharedDocument::Policy,
            "default = 123\n",
            WritePrecondition::Any,
            &no_builtins,
        );
        assert!(bad.is_err());
        assert!(!cfg.join("policy.toml").exists());

        // First valid write with Absent precondition creates the file with no backup.
        let v1 = "default = \"owner\"\n";
        let first = write_local_document_blocking(
            &cfg,
            &state,
            &SharedDocument::Policy,
            v1,
            WritePrecondition::Absent,
            &no_builtins,
        )
        .unwrap();
        assert!(first.backup.is_none());
        assert!(first.hash_before.is_none());

        // Second write with Absent precondition fails with Conflict and leaves v1 intact.
        let already_exists = write_local_document_blocking(
            &cfg,
            &state,
            &SharedDocument::Policy,
            "default = \"owner\"\n# racing\n",
            WritePrecondition::Absent,
            &no_builtins,
        );
        assert_eq!(
            already_exists.err().map(|e| e.code),
            Some(ErrorCode::Conflict)
        );
        assert_eq!(
            std::fs::read_to_string(cfg.join("policy.toml")).unwrap(),
            v1
        );

        // Second valid write with matching expect_hash backs up v1 and replaces atomically.
        let v2 = "default = \"owner\"\n\n[roles.viewer]\nallow = [\"effect:read\"]\n";
        let second = write_local_document_blocking(
            &cfg,
            &state,
            &SharedDocument::Policy,
            v2,
            WritePrecondition::Hash(&first.hash_after),
            &no_builtins,
        )
        .unwrap();
        let backup_path = second.backup.as_deref().unwrap();
        assert_eq!(std::fs::read_to_string(backup_path).unwrap(), v1);
        assert_eq!(
            std::fs::read_to_string(cfg.join("policy.toml")).unwrap(),
            v2
        );

        // Stale expect_hash is refused with Conflict and keeps v2 intact.
        let stale = write_local_document_blocking(
            &cfg,
            &state,
            &SharedDocument::Policy,
            v1,
            WritePrecondition::Hash(&first.hash_after),
            &no_builtins,
        );
        assert_eq!(stale.err().map(|e| e.code), Some(ErrorCode::Conflict));
        assert_eq!(
            std::fs::read_to_string(cfg.join("policy.toml")).unwrap(),
            v2
        );
    }

    #[test]
    fn tools_d_validation_enforces_ext_parser_stem_match_and_no_duplicates() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("config");
        let state = dir.path().join("state");
        let is_builtin = |name: &str| matches!(name, "agent.list" | "config.read");

        let valid = "name = \"demo.hello\"\ndescription = \"Say hello.\"\neffect = \"read\"\ncmd = \"echo hi\"\n";

        // Unknown field is refused by ext.define's strict parser.
        let unknown = format!("{valid}bogus = 1\n");
        let doc = SharedDocument::parse("tools.d/demo.hello.toml").unwrap();
        assert!(
            write_local_document_blocking(
                &cfg,
                &state,
                &doc,
                &unknown,
                WritePrecondition::Any,
                &is_builtin,
            )
            .is_err()
        );

        // Reserved family is refused.
        let reserved_doc = SharedDocument::parse("tools.d/meta.evil.toml").unwrap();
        let reserved =
            "name = \"meta.evil\"\ndescription = \"Evil.\"\neffect = \"read\"\ncmd = \"echo x\"\n";
        let err = write_local_document_blocking(
            &cfg,
            &state,
            &reserved_doc,
            reserved,
            WritePrecondition::Any,
            &is_builtin,
        )
        .err()
        .unwrap();
        assert!(err.message.contains("reserved"), "{err:?}");

        // Built-in shadowing is refused.
        let shadow_doc = SharedDocument::parse("tools.d/agent.list.toml").unwrap();
        let shadow = "name = \"agent.list\"\ndescription = \"Shadow.\"\neffect = \"read\"\ncmd = \"echo x\"\n";
        let err = write_local_document_blocking(
            &cfg,
            &state,
            &shadow_doc,
            shadow,
            WritePrecondition::Any,
            &is_builtin,
        )
        .err()
        .unwrap();
        assert!(err.message.contains("built-in"), "{err:?}");

        // File stem mismatch is refused.
        let wrong_stem = SharedDocument::parse("tools.d/other.toml").unwrap();
        let err = write_local_document_blocking(
            &cfg,
            &state,
            &wrong_stem,
            valid,
            WritePrecondition::Any,
            &is_builtin,
        )
        .err()
        .unwrap();
        assert!(
            err.message.contains("does not match file name stem"),
            "{err:?}"
        );

        // Duplicate tool name already defined in another tools.d file is refused.
        std::fs::create_dir_all(cfg.join("tools.d")).unwrap();
        std::fs::write(cfg.join("tools.d").join("legacy.toml"), valid).unwrap();
        let err = write_local_document_blocking(
            &cfg,
            &state,
            &doc,
            valid,
            WritePrecondition::Any,
            &is_builtin,
        )
        .err()
        .unwrap();
        assert!(err.message.contains("already defined"), "{err:?}");

        // Removing the conflicting file allows writing and updating tools.d/demo.hello.toml.
        std::fs::remove_file(cfg.join("tools.d").join("legacy.toml")).unwrap();
        let first = write_local_document_blocking(
            &cfg,
            &state,
            &doc,
            valid,
            WritePrecondition::Absent,
            &is_builtin,
        )
        .unwrap();
        let updated = "name = \"demo.hello\"\ndescription = \"Say hi.\"\neffect = \"read\"\ncmd = \"echo hi2\"\n";
        write_local_document_blocking(
            &cfg,
            &state,
            &doc,
            updated,
            WritePrecondition::Hash(&first.hash_after),
            &is_builtin,
        )
        .unwrap();
    }

    #[test]
    fn classify_devices_status_distinguishes_all_merge_states() {
        use legion_tools::device::keys::ed25519_public_key_from_seed;
        use legion_tools::device::registry::FleetDevice;

        let empty = DevicesFile::default();
        assert_eq!(
            classify_devices_status(&empty, &empty, &empty, false, false).0,
            SyncStatus::Same
        );

        let id_a = "a1".repeat(16);
        let id_b = "b2".repeat(16);
        let dev_a = FleetDevice {
            name: "phone-a".to_owned(),
            public_key: ed25519_public_key_from_seed(&[1u8; 32]),
            principal: "device:phone-a".to_owned(),
            role: Some("observer".to_owned()),
            paired_at: "2026-11-24T10:00:00Z".to_owned(),
            paired_on: "node-a".to_owned(),
            updated_at: "2026-11-24T10:00:00Z".to_owned(),
            updated_on: "node-a".to_owned(),
        };
        let dev_b = FleetDevice {
            name: "phone-b".to_owned(),
            public_key: ed25519_public_key_from_seed(&[2u8; 32]),
            principal: "device:phone-b".to_owned(),
            role: Some("observer".to_owned()),
            paired_at: "2026-11-24T10:01:00Z".to_owned(),
            paired_on: "node-b".to_owned(),
            updated_at: "2026-11-24T10:01:00Z".to_owned(),
            updated_on: "node-b".to_owned(),
        };

        let mut only_a = DevicesFile::default();
        only_a.devices.insert(id_a.clone(), dev_a.clone());
        let mut only_b = DevicesFile::default();
        only_b.devices.insert(id_b.clone(), dev_b.clone());
        let both = merge_devices(&only_a, &only_b);

        assert_eq!(
            classify_devices_status(&only_a, &empty, &only_a, true, false).0,
            SyncStatus::OnlyHere
        );
        assert_eq!(
            classify_devices_status(&empty, &only_b, &only_b, false, true).0,
            SyncStatus::OnlyThere
        );
        assert_eq!(
            classify_devices_status(&both, &only_a, &both, true, true).0,
            SyncStatus::ChangedHere
        );
        assert_eq!(
            classify_devices_status(&only_a, &both, &both, true, true).0,
            SyncStatus::ChangedThere
        );
        let (st, detail) = classify_devices_status(&only_a, &only_b, &both, true, true);
        assert_eq!(st, SyncStatus::Both);
        assert!(detail.is_some());
    }

    #[tokio::test]
    async fn config_write_devices_merges_keeps_tombstones_and_blocks_non_owner_escalation() {
        use legion_core::{Call, Door, Node, Paths};
        use legion_tools::device::keys::ed25519_public_key_from_seed;
        use legion_tools::device::registry::{DeviceTombstone, FleetDevice};

        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        std::fs::create_dir_all(home.join(".ssh")).unwrap();
        let auth_keys = dir.path().join("authorized_keys");
        let paths = Paths::under(dir.path().join("legion")).with_home(&home);
        std::fs::create_dir_all(paths.config_dir()).unwrap();
        let auth_escaped = auth_keys.display().to_string().replace('\\', "\\\\");
        std::fs::write(
            paths.config_file("node.toml"),
            format!("name = \"box-a\"\n[devices]\nauthorized_keys = \"{auth_escaped}\"\n"),
        )
        .unwrap();
        std::fs::write(
            paths.config_file("policy.toml"),
            "\
default = \"owner\"

[roles.observer]
allow = [\"effect:read\"]

[roles.phone_admin]
allow = [\"effect:read\", \"effect:admin\"]

[principals.tablet]
role = \"phone_admin\"
",
        )
        .unwrap();

        let mut reg = legion_tools::registry();
        let fleet = Fleet::new(&paths);
        register(&mut reg, fleet);
        let node = Node::new(paths.clone(), reg);

        let id_live = "11".repeat(16);
        let id_tomb = "22".repeat(16);
        let id_incoming = "33".repeat(16);

        let mut local = DevicesFile::default();
        local.devices.insert(
            id_live.clone(),
            FleetDevice {
                name: "local-phone".to_owned(),
                public_key: ed25519_public_key_from_seed(&[11u8; 32]),
                principal: "device:local-phone".to_owned(),
                role: Some("observer".to_owned()),
                paired_at: "2026-11-24T10:00:00Z".to_owned(),
                paired_on: "box-a".to_owned(),
                updated_at: "2026-11-24T10:00:00Z".to_owned(),
                updated_on: "box-a".to_owned(),
            },
        );
        local.removed.insert(
            id_tomb.clone(),
            DeviceTombstone {
                name: "revoked-phone".to_owned(),
                principal: "device:revoked-phone".to_owned(),
                paired_at: Some("2026-11-24T09:00:00Z".to_owned()),
                paired_on: Some("box-a".to_owned()),
                removed_at: "2026-11-24T10:05:00Z".to_owned(),
                removed_on: "box-a".to_owned(),
            },
        );
        std::fs::write(paths.config_file("devices.toml"), local.to_toml_string()).unwrap();

        // Owner writes an incoming file trying to resurrect id_tomb while adding id_incoming:
        // tombstone wins (never dropped), id_incoming is added, and local-phone stays intact.
        let mut incoming = DevicesFile::default();
        incoming.devices.insert(
            id_tomb.clone(),
            FleetDevice {
                name: "revoked-phone".to_owned(),
                public_key: ed25519_public_key_from_seed(&[22u8; 32]),
                principal: "device:revoked-phone".to_owned(),
                role: Some("observer".to_owned()),
                paired_at: "2026-11-24T09:00:00Z".to_owned(),
                paired_on: "box-a".to_owned(),
                updated_at: "2026-11-24T11:00:00Z".to_owned(),
                updated_on: "box-b".to_owned(),
            },
        );
        incoming.devices.insert(
            id_incoming.clone(),
            FleetDevice {
                name: "peer-phone".to_owned(),
                public_key: ed25519_public_key_from_seed(&[33u8; 32]),
                principal: "device:peer-phone".to_owned(),
                role: Some("observer".to_owned()),
                paired_at: "2026-11-24T10:10:00Z".to_owned(),
                paired_on: "box-b".to_owned(),
                updated_at: "2026-11-24T10:10:00Z".to_owned(),
                updated_on: "box-b".to_owned(),
            },
        );

        let write_ok = node
            .call(Call::new(
                Door::Cli,
                "config.write",
                json!({
                    "document": "devices.toml",
                    "content": incoming.to_toml_string(),
                }),
            ))
            .await
            .unwrap();
        assert_eq!(write_ok["document"], json!("devices.toml"));

        let saved: DevicesFile = parse(
            &paths.config_file("devices.toml"),
            &std::fs::read(paths.config_file("devices.toml")).unwrap(),
        )
        .unwrap();
        assert!(saved.devices.contains_key(&id_live));
        assert!(saved.devices.contains_key(&id_incoming));
        assert!(!saved.devices.contains_key(&id_tomb));
        assert!(saved.removed.contains_key(&id_tomb));

        // policy.toml gained device:peer-phone as observer, and authorized_keys has both live keys.
        let pol: Policy = parse(
            &paths.config_file("policy.toml"),
            &std::fs::read(paths.config_file("policy.toml")).unwrap(),
        )
        .unwrap();
        assert_eq!(pol.role_of("device:peer-phone").0, "observer");
        let auth_text = std::fs::read_to_string(&auth_keys).unwrap();
        assert!(auth_text.contains("legion-device:local-phone"));
        assert!(auth_text.contains("legion-device:peer-phone"));
        assert!(!auth_text.contains("legion-device:revoked-phone"));

        // Non-owner admin (`tablet`) cannot inject an owner device or revoke another principal's device.
        let mut rogue = saved.clone();
        rogue.devices.insert(
            "44".repeat(16),
            FleetDevice {
                name: "rogue-owner".to_owned(),
                public_key: ed25519_public_key_from_seed(&[44u8; 32]),
                principal: "owner".to_owned(),
                role: Some("owner".to_owned()),
                paired_at: "2026-11-24T10:20:00Z".to_owned(),
                paired_on: "box-b".to_owned(),
                updated_at: "2026-11-24T10:20:00Z".to_owned(),
                updated_on: "box-b".to_owned(),
            },
        );
        let mut rogue_call = Call::new(
            Door::Cli,
            "config.write",
            json!({
                "document": "devices.toml",
                "content": rogue.to_toml_string(),
            }),
        );
        rogue_call.pinned = Some("tablet".to_owned());
        let err = node.call(rogue_call).await.unwrap_err();
        assert_eq!(err.code, ErrorCode::Denied);

        let mut rogue_revoke = saved.clone();
        rogue_revoke.devices.remove(&id_incoming);
        rogue_revoke.removed.insert(
            id_incoming.clone(),
            DeviceTombstone {
                name: "peer-phone".to_owned(),
                principal: "device:peer-phone".to_owned(),
                paired_at: None,
                paired_on: None,
                removed_at: "2026-11-24T10:25:00Z".to_owned(),
                removed_on: "box-b".to_owned(),
            },
        );
        let mut revoke_call = Call::new(
            Door::Cli,
            "config.write",
            json!({
                "document": "devices.toml",
                "content": rogue_revoke.to_toml_string(),
            }),
        );
        revoke_call.pinned = Some("tablet".to_owned());
        let err2 = node.call(revoke_call).await.unwrap_err();
        assert_eq!(err2.code, ErrorCode::Denied);
    }

    #[tokio::test]
    async fn expiring_credential_cannot_add_or_change_live_devices_but_may_tombstone() {
        use jiff::SignedDuration;
        use legion_core::{Call, Door, Node, Paths};
        use legion_tools::device::keys::ed25519_public_key_from_seed;
        use legion_tools::device::registry::{DeviceTombstone, FleetDevice};

        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        std::fs::create_dir_all(home.join(".ssh")).unwrap();
        let auth_keys = dir.path().join("authorized_keys");
        let paths = Paths::under(dir.path().join("legion")).with_home(&home);
        std::fs::create_dir_all(paths.config_dir()).unwrap();
        let auth_escaped = auth_keys.display().to_string().replace('\\', "\\\\");
        std::fs::write(
            paths.config_file("node.toml"),
            format!("name = \"box-a\"\n[devices]\nauthorized_keys = \"{auth_escaped}\"\n"),
        )
        .unwrap();
        std::fs::write(
            paths.config_file("policy.toml"),
            "\
default = \"owner\"

[roles.observer]
allow = [\"effect:read\"]

[principals.\"device:pixel\"]
role = \"observer\"
device = \"11111111111111111111111111111111\"
",
        )
        .unwrap();

        let mut reg = legion_tools::registry();
        let fleet = Fleet::new(&paths);
        register(&mut reg, fleet);
        let node = Node::new(paths.clone(), reg);

        let id_existing = "11".repeat(16);
        let mut initial = DevicesFile::default();
        initial.devices.insert(
            id_existing.clone(),
            FleetDevice {
                name: "pixel".to_owned(),
                public_key: ed25519_public_key_from_seed(&[11u8; 32]),
                principal: "device:pixel".to_owned(),
                role: Some("observer".to_owned()),
                paired_at: "2026-11-24T10:00:00Z".to_owned(),
                paired_on: "box-a".to_owned(),
                updated_at: "2026-11-24T10:00:00Z".to_owned(),
                updated_on: "box-a".to_owned(),
            },
        );
        std::fs::write(paths.config_file("devices.toml"), initial.to_toml_string()).unwrap();

        let expires = Some(Timestamp::now() + SignedDuration::from_hours(1));

        // 1. Adding a new live device through an expiring credential is refused.
        let mut add_live = initial.clone();
        add_live.devices.insert(
            "22".repeat(16),
            FleetDevice {
                name: "backdoor".to_owned(),
                public_key: ed25519_public_key_from_seed(&[22u8; 32]),
                principal: "owner".to_owned(),
                role: Some("owner".to_owned()),
                paired_at: "2026-11-24T10:05:00Z".to_owned(),
                paired_on: "box-a".to_owned(),
                updated_at: "2026-11-24T10:05:00Z".to_owned(),
                updated_on: "box-a".to_owned(),
            },
        );
        let mut add_call = Call::new(
            Door::Http,
            "config.write",
            json!({"document": "devices.toml", "content": add_live.to_toml_string()}),
        );
        add_call.credential_expires = expires;
        let err = node.call(add_call).await.unwrap_err();
        assert_eq!(err.code, ErrorCode::Denied, "{err:?}");

        // 2. Changing an existing live device's role/name through an expiring credential is refused.
        let mut raise_role = initial.clone();
        let entry = raise_role.devices.get_mut(&id_existing).unwrap();
        entry.role = Some("owner".to_owned());
        entry.updated_at = "2026-11-24T10:06:00Z".to_owned();
        let mut change_call = Call::new(
            Door::Http,
            "config.write",
            json!({"document": "devices.toml", "content": raise_role.to_toml_string()}),
        );
        change_call.credential_expires = expires;
        let err2 = node.call(change_call).await.unwrap_err();
        assert_eq!(err2.code, ErrorCode::Denied, "{err2:?}");

        // 3. Tombstoning an existing device through an expiring credential is allowed.
        let mut revoke = DevicesFile::default();
        revoke.removed.insert(
            id_existing.clone(),
            DeviceTombstone {
                name: "pixel".to_owned(),
                principal: "device:pixel".to_owned(),
                paired_at: Some("2026-11-24T10:00:00Z".to_owned()),
                paired_on: Some("box-a".to_owned()),
                removed_at: "2026-11-24T10:07:00Z".to_owned(),
                removed_on: "box-a".to_owned(),
            },
        );
        let mut tomb_call = Call::new(
            Door::Http,
            "config.write",
            json!({"document": "devices.toml", "content": revoke.to_toml_string()}),
        );
        tomb_call.credential_expires = expires;
        node.call(tomb_call).await.expect("tombstone should be allowed with expiring credential");

        let saved: DevicesFile = parse(
            &paths.config_file("devices.toml"),
            &std::fs::read(paths.config_file("devices.toml")).unwrap(),
        )
        .unwrap();
        assert!(saved.devices.is_empty());
        assert!(saved.removed.contains_key(&id_existing));
    }

    #[tokio::test]
    async fn non_owner_merge_cannot_steal_owner_id_tombstone_absent_id_or_collide_keys() {
        use legion_core::{Call, Door, Node, Paths};
        use legion_tools::device::keys::ed25519_public_key_from_seed;
        use legion_tools::device::registry::{DeviceTombstone, FleetDevice};

        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        std::fs::create_dir_all(home.join(".ssh")).unwrap();
        let auth_keys = dir.path().join("authorized_keys");
        let paths = Paths::under(dir.path().join("legion")).with_home(&home);
        std::fs::create_dir_all(paths.config_dir()).unwrap();
        let auth_escaped = auth_keys.display().to_string().replace('\\', "\\\\");
        std::fs::write(
            paths.config_file("node.toml"),
            format!("name = \"box-a\"\n[devices]\nauthorized_keys = \"{auth_escaped}\"\n"),
        )
        .unwrap();
        std::fs::write(
            paths.config_file("policy.toml"),
            "\
default = \"owner\"

[roles.phone_admin]
allow = [\"effect:read\", \"effect:admin\"]

[principals.tablet]
role = \"phone_admin\"
",
        )
        .unwrap();

        let mut reg = legion_tools::registry();
        let fleet = Fleet::new(&paths);
        register(&mut reg, fleet);
        let node = Node::new(paths.clone(), reg);

        let id_owner = "aa".repeat(16);
        let owner_key = ed25519_public_key_from_seed(&[51u8; 32]);
        let mut local = DevicesFile::default();
        local.devices.insert(
            id_owner.clone(),
            FleetDevice {
                name: "owners-laptop".to_owned(),
                public_key: owner_key.clone(),
                principal: "owner".to_owned(),
                role: Some("owner".to_owned()),
                paired_at: "2026-11-24T10:00:00Z".to_owned(),
                paired_on: "box-a".to_owned(),
                updated_at: "2026-11-24T10:00:00Z".to_owned(),
                updated_on: "box-a".to_owned(),
            },
        );
        std::fs::write(paths.config_file("devices.toml"), local.to_toml_string()).unwrap();

        // 1. Non-owner admin (`tablet`) cannot overwrite an `owner` pairing id by changing its
        //    principal to `tablet` with a newer timestamp.
        let mut hijack = DevicesFile::default();
        hijack.devices.insert(
            id_owner.clone(),
            FleetDevice {
                name: "hijacked".to_owned(),
                public_key: ed25519_public_key_from_seed(&[52u8; 32]),
                principal: "tablet".to_owned(),
                role: None,
                paired_at: "2026-11-24T10:00:00Z".to_owned(),
                paired_on: "box-a".to_owned(),
                updated_at: "2026-11-24T10:05:00Z".to_owned(),
                updated_on: "box-b".to_owned(),
            },
        );
        let mut hijack_call = Call::new(
            Door::Cli,
            "config.write",
            json!({"document": "devices.toml", "content": hijack.to_toml_string()}),
        );
        hijack_call.pinned = Some("tablet".to_owned());
        let err = node.call(hijack_call).await.unwrap_err();
        assert_eq!(err.code, ErrorCode::Denied, "hijack must be denied: {err:?}");

        // 2. Non-owner admin (`tablet`) cannot plant a tombstone for an id that is not live locally.
        let mut phantom_tomb = DevicesFile::default();
        phantom_tomb.removed.insert(
            "bb".repeat(16),
            DeviceTombstone {
                name: "phantom".to_owned(),
                principal: "tablet".to_owned(),
                paired_at: None,
                paired_on: None,
                removed_at: "2026-11-24T10:06:00Z".to_owned(),
                removed_on: "box-b".to_owned(),
            },
        );
        let mut tomb_call = Call::new(
            Door::Cli,
            "config.write",
            json!({"document": "devices.toml", "content": phantom_tomb.to_toml_string()}),
        );
        tomb_call.pinned = Some("tablet".to_owned());
        let err2 = node.call(tomb_call).await.unwrap_err();
        assert_eq!(
            err2.code,
            ErrorCode::Denied,
            "tombstone for absent id must be denied for non-owner: {err2:?}"
        );

        // 3. Merging a live entry whose public key is already used by another live entry is refused
        //    with Conflict (even by owner).
        let mut dup_key = DevicesFile::default();
        dup_key.devices.insert(
            "cc".repeat(16),
            FleetDevice {
                name: "duplicate-key-device".to_owned(),
                public_key: owner_key,
                principal: "tablet".to_owned(),
                role: None,
                paired_at: "2026-11-24T10:07:00Z".to_owned(),
                paired_on: "box-b".to_owned(),
                updated_at: "2026-11-24T10:07:00Z".to_owned(),
                updated_on: "box-b".to_owned(),
            },
        );
        let err3 = node
            .call(Call::new(
                Door::Cli,
                "config.write",
                json!({"document": "devices.toml", "content": dup_key.to_toml_string()}),
            ))
            .await
            .unwrap_err();
        assert_eq!(
            err3.code,
            ErrorCode::Conflict,
            "duplicate live public key in merge must fail with Conflict: {err3:?}"
        );
    }
}
