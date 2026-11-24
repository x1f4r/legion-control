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
    DEVICES_VERSION, DeviceRegistryPushAudit, DevicesFile, MergeApplyError, MergeChange,
    apply_devices_merge_blocking, merge_devices, preserve_local_confirmations,
    rewrite_tombstones_from_local,
};
use legion_tools::device::sessions::{
    DEFAULT_TERMINATE_GRACE, terminate_device_sessions_for_target,
};
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
        "Validate and atomically update one shared configuration document (devices.toml, fleet.toml, policy.toml, projects.toml, or tools.d/<name>.toml), keeping the previous version as a backup in the state directory and recording the hash before and after in the audit log. Writing devices.toml directly requires the owner role on this node: it merges incoming entries with local paired devices (tombstones always win and rewrite identity fields from matching local entries), reconciles registry-owned device:* principals (marked with `device = \"<pairing id>\"` in policy.toml) without touching shared or unmarked principals, updates authorized_keys, and ends active sessions for newly removed devices. Writing policy.toml changes who may do what on this machine, so any role allowed to call config.write (including effect:admin) is as strong as owner. Refuses invalid documents before replacing anything, refuses mismatched expect_hash or expect_absent (when a file already exists) with conflict, and refuses machine-local files such as node.toml and tokens.toml.",
        config_write,
    );
    registry.add(SyncTool::<SyncArgs, SyncResult, _>::new(
        "fleet.sync",
        Effect::Admin,
        "Compare, push, or pull shared configuration documents (devices.toml, fleet.toml, policy.toml, projects.toml, and tools.d/*.toml) across machines in fleet.toml using per-peer BLAKE3 sync sidecars to detect conflicts. In status mode, reports same, changed_here, changed_there, both, only_here, or only_there without importing anything. Pushing or pulling devices.toml requires the owner role on the node doing the writing (for non-owner callers, fleet.sync syncs the other documents and reports devices.toml as skipped with the reason; propagation after local device tools runs node to node with the node's own authority). Paired devices (devices.toml) obey a strict one-way trust rule for live entries: push sends local entries to peers and imports ONLY tombstones back — never a peer's live devices. New or changed live devices from a peer are accepted only via an explicit `fleet.sync direction=pull from=<peer>` run by an owner, or when that peer pushes to this node with owner authority. Tombstones always win and may flow in every direction. Removal of a tools.d file is never propagated: push never deletes remote files, pull never deletes local files, and status reports files present on only one side as only_here or only_there. Syncing policy.toml is powerful because it changes who may do what on the receiving machine; every replacement is strictly validated by the receiver before installing, backed up, and recorded in the audit log with its hash before and after. Never syncs node.toml or tokens.toml.",
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
            if *self == Self::Devices {
                return Err(legion_tools::device::registry::devices_size_limit_error(
                    content.len(),
                ));
            }
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
        warnings: Vec::new(),
    })
}

async fn read_local_document_for_ctx(
    ctx: &Ctx,
    doc: &SharedDocument,
) -> Result<ConfigReadResult, ErrorBody> {
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
    device_registry_push: Option<DeviceRegistryPushAudit>,
) -> Result<WriteOutcome, ErrorBody> {
    if expect_hash.is_some() && expect_absent {
        return Err(ErrorBody::new(
            ErrorCode::InvalidArgs,
            "Cannot set both `expect_hash` and `expect_absent` on config.write.",
        )
        .with_hint("Pass `expect_hash` when replacing an existing file, or `expect_absent=true` when creating a new one."));
    }
    if device_registry_push.is_some() && doc != SharedDocument::Devices {
        return Err(ErrorBody::new(
            ErrorCode::InvalidArgs,
            "`device_registry_push` may only be attached when writing devices.toml.",
        ));
    }
    let started = Instant::now();

    if doc == SharedDocument::Devices {
        if ctx.grant().role() != OWNER {
            return Err(ErrorBody::new(
                ErrorCode::Denied,
                "Writing devices.toml directly requires the owner role on this node.",
            )
            .with_hint(
                "Run `config.write devices.toml` as the owner, or manage paired devices through `device.invite`, `device.set`, and `device.remove`.",
            ));
        }
        // Bound raw incoming parse size to protect memory, while allowing valid tombstone-only
        // `devices.toml` payloads slightly above 1 MiB to reach the merge (which never refuses
        // tombstone-only changes for size, and still enforces the 1 MiB tombstone-reserve limit
        // whenever any live entry is added or modified).
        const MAX_DEVICES_PARSE_BYTES: usize = MAX_DOCUMENT_BYTES * 4;
        if content.len() > MAX_DEVICES_PARSE_BYTES {
            return Err(legion_tools::device::registry::devices_size_limit_error(
                content.len(),
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
        let incoming = match parse::<DevicesFile>(&target, content.as_bytes()) {
            Ok(parsed) => parsed,
            Err(_) if content.len() > MAX_DOCUMENT_BYTES => {
                return Err(legion_tools::device::registry::devices_size_limit_error(
                    content.len(),
                ));
            }
            Err(error) => return Err(error),
        };
        let legacy_single_stamp_ids =
            legion_tools::device::registry::legacy_single_stamp_device_ids(&content);
        let node_file = ctx.config().get::<NodeFile>()?;
        let paths = ctx.paths().clone();
        let paths_for_sessions = paths.clone();
        let node_name = ctx.node_name().to_owned();
        let credential_expires = ctx.credential_expires();

        let merge_result = tokio::task::spawn_blocking(move || {
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
            let prepare_incoming =
                |local: &DevicesFile,
                 _live_policy: &Policy,
                 incoming_mut: &mut DevicesFile|
                 -> Result<(), ErrorBody> {
                    if !legacy_single_stamp_ids.is_empty() {
                        return Err(
                            legion_tools::device::registry::legacy_single_stamp_incoming_error(
                                &legacy_single_stamp_ids,
                            ),
                        );
                    }
                    for (id, inc) in &mut incoming_mut.devices {
                        if local.removed.contains_key(id) {
                            continue;
                        }
                        if let Some(prev) = local.devices.get(id) {
                            // Confirmation is ONLY allowed through `device.set confirm=true`,
                            // never through `config.write` (even for owner callers).
                            if (inc.confirmed_by.is_some() || inc.confirmed_at.is_some())
                                && (inc.confirmed_by != prev.confirmed_by
                                    || inc.confirmed_at != prev.confirmed_at)
                            {
                                return Err(ErrorBody::new(
                                    ErrorCode::Denied,
                                    "Setting confirmed_by or confirmed_at through `config.write devices.toml` is not allowed.",
                                )
                                .with_hint("Confirm a quarantined device with `legion device.set id=<pairing_id> confirm=true`."));
                            }
                            // Dropping creator metadata on an existing entry is refused even for
                            // owner callers so quarantine provenance cannot be stripped through
                            // `config.write`.
                            let drops_paired_by =
                                prev.paired_by.is_some() && inc.paired_by.is_none();
                            let drops_creator_device = prev
                                .paired_by
                                .as_ref()
                                .and_then(|p| p.device.as_ref())
                                .is_some()
                                && inc
                                    .paired_by
                                    .as_ref()
                                    .and_then(|p| p.device.as_ref())
                                    .is_none();
                            if drops_paired_by || drops_creator_device {
                                return Err(ErrorBody::new(
                                    ErrorCode::Denied,
                                    "Removing paired_by provenance from an existing paired device via `config.write` is not allowed.",
                                )
                                .with_hint("Keep the existing paired_by metadata intact, or confirm a quarantined device with `device.set confirm=true`."));
                            }
                        } else if inc.confirmed_by.is_some() || inc.confirmed_at.is_some() {
                            return Err(ErrorBody::new(
                                ErrorCode::Denied,
                                "Setting confirmed_by or confirmed_at through `config.write devices.toml` is not allowed.",
                            )
                            .with_hint("Confirm a quarantined device with `legion device.set id=<pairing_id> confirm=true`."));
                        }
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
                Ok(())
            };
            apply_devices_merge_blocking(
                &paths,
                &node_name,
                &node_file,
                &incoming,
                check_precondition,
                prepare_incoming,
                authorize_change,
            )
        })
        .await
        .map_err(|error| {
            ErrorBody::new(
                ErrorCode::Internal,
                format!("Updating devices.toml stopped unexpectedly: {error}."),
            )
        })?;

        let merge_outcome = match merge_result {
            Ok(outcome) => outcome,
            Err(MergeApplyError::BeforeSave(err)) => return Err(err),
            Err(MergeApplyError::PartialTombstonesApplied {
                outcome,
                tombstones_changed,
                mut live_error,
            }) => {
                // Always end live sessions for any devices removed or newly quarantined by the
                // tombstones that landed.
                for target in &outcome.removed_targets {
                    let _ = terminate_device_sessions_for_target(
                        &paths_for_sessions,
                        target,
                        DEFAULT_TERMINATE_GRACE,
                    )
                    .await;
                }
                let hash_before = outcome
                    .bytes_before
                    .as_deref()
                    .map(|b| hash_bytes(b).to_string());
                let hash_after = hash_bytes(outcome.canonical_toml.as_bytes()).to_string();
                if tombstones_changed {
                    let mut audit_args = json!({
                        "document": "devices.toml",
                        "hash_before": hash_before,
                        "hash_after": hash_after,
                        "backup": outcome.backup,
                        "partial": true,
                        "tombstones_delivered": true,
                        "live_refused": {
                            "code": live_error.code,
                            "message": live_error.message,
                        },
                    });
                    if let Some(ref push_audit) = device_registry_push
                        && let Some(obj) = audit_args.as_object_mut()
                    {
                        obj.insert("device_registry_push".to_owned(), json!(push_audit));
                    }
                    if !outcome.warnings.is_empty()
                        && let Some(obj) = audit_args.as_object_mut()
                    {
                        obj.insert("warnings".to_owned(), json!(outcome.warnings));
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
                }
                let mut details = match live_error.details.take() {
                    Some(Value::Object(map)) => map,
                    _ => serde_json::Map::new(),
                };
                details.insert("partial".to_owned(), json!(true));
                details.insert("tombstones_delivered".to_owned(), json!(true));
                details.insert("tombstones_changed".to_owned(), json!(tombstones_changed));
                details.insert("hash".to_owned(), json!(hash_after));
                if let Some(backup) = outcome.backup {
                    details.insert("backup".to_owned(), json!(backup));
                }
                if !outcome.warnings.is_empty() {
                    details.insert("warnings".to_owned(), json!(outcome.warnings));
                }
                return Err(live_error.with_details(Value::Object(details)));
            }
            Err(MergeApplyError::AfterSave { error, tombstoned }) => {
                // Always end live sessions for every newly tombstoned device whenever devices.toml
                // was saved, even if policy or authorized_keys reconciliation failed afterwards.
                for target in &tombstoned {
                    let _ = terminate_device_sessions_for_target(
                        &paths_for_sessions,
                        target,
                        DEFAULT_TERMINATE_GRACE,
                    )
                    .await;
                }
                return Err(error);
            }
        };

        // End live sessions and open SSH connections for any devices removed by the merge.
        for target in &merge_outcome.removed_targets {
            let _ = terminate_device_sessions_for_target(
                &paths_for_sessions,
                target,
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
        if let Some(ref push_audit) = device_registry_push
            && let Some(obj) = audit_args.as_object_mut()
        {
            obj.insert("device_registry_push".to_owned(), json!(push_audit));
        }
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
    /// Optional audit metadata attached during a node-to-node device-registry push after a local
    /// device tool (`device.enrol`, `device.set`, or `device.remove`). Valid only for `devices.toml`
    /// and grants no additional permission.
    #[serde(default)]
    device_registry_push: Option<DeviceRegistryPushAudit>,
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
    let outcome = apply_local_write(
        ctx,
        doc,
        args.content,
        args.expect_hash,
        args.expect_absent,
        args.device_registry_push,
    )
    .await?;
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
    /// Removals (tombstones) were delivered, while live device changes were refused; see `detail`
    /// and `error`.
    Partial,
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
    /// Skipped for this caller (for example `devices.toml` when the caller does not hold the `owner` role); see `detail`.
    Skipped,
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
    /// Optional audit metadata attached by this node during an automatic device-registry push after
    /// `device.enrol`, `device.set`, or `device.remove`. Valid only with `direction="push"` and
    /// `documents=["devices.toml"]` as owner.
    #[serde(default)]
    device_registry_push: Option<DeviceRegistryPushAudit>,
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

    if args.device_registry_push.is_some() {
        if ctx.grant().role() != OWNER {
            return Err(ErrorBody::new(
                ErrorCode::Denied,
                "`device_registry_push` requires the owner role on this node.",
            ));
        }
        if args.direction != SyncDirection::Push
            || explicit_docs.as_deref() != Some(&[SharedDocument::Devices])
        {
            return Err(ErrorBody::new(
                ErrorCode::InvalidArgs,
                "`device_registry_push` may only be used with direction=\"push\" and documents=[\"devices.toml\"].",
            ));
        }
    }

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

    let device_push_ref = args.device_registry_push.as_ref();
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
                    device_push_ref,
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
                            device_push_ref,
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
    device_registry_push: Option<&DeviceRegistryPushAudit>,
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
        device_registry_push,
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
    device_registry_push: Option<&DeviceRegistryPushAudit>,
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
        let outcome = sync_one_document(
            fleet,
            ctx,
            machine,
            &doc,
            direction,
            force,
            dry_run,
            device_registry_push,
        )
        .await;
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

#[allow(clippy::too_many_arguments)]
async fn sync_one_document(
    fleet: &Fleet,
    ctx: &Ctx,
    machine: &str,
    doc: &SharedDocument,
    direction: SyncDirection,
    force: bool,
    dry_run: bool,
    device_registry_push: Option<&DeviceRegistryPushAudit>,
) -> DocSyncOutcome {
    if *doc == SharedDocument::Devices {
        return sync_devices_document(
            fleet,
            ctx,
            machine,
            direction,
            dry_run,
            device_registry_push,
        )
        .await;
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
            match apply_local_write(
                ctx,
                doc.clone(),
                remote_content,
                expect_hash,
                expect_absent,
                None,
            )
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
    device_registry_push: Option<&DeviceRegistryPushAudit>,
) -> DocSyncOutcome {
    if direction != SyncDirection::Status && ctx.grant().role() != OWNER {
        return DocSyncOutcome {
            status: SyncStatus::Skipped,
            local_hash: None,
            remote_hash: None,
            last_synced_hash: None,
            backup: None,
            detail: Some(
                "Syncing devices.toml requires the owner role on this node; skipped for non-owner callers."
                    .to_owned(),
            ),
            warnings: Vec::new(),
            error: None,
        };
    }

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

    let mut remote_for_status = remote_devices.clone();
    rewrite_tombstones_from_local(&local_devices, &mut remote_for_status);
    let merged = merge_devices(&local_devices, &remote_for_status);

    if direction == SyncDirection::Status {
        let (status, detail) = classify_devices_status(
            &local_devices,
            &remote_devices,
            &merged,
            local.exists,
            remote.exists,
            machine,
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

    match direction {
        SyncDirection::Push => {
            // Trust rule: push sends local entries to the peer and imports ONLY tombstones back —
            // never live device entries from the peer.
            //
            // Confirmations (`confirmed_by`/`confirmed_at`) are node-local and only accepted via
            // `device.set confirm=true`, so strip them before sending; the receiving node
            // preserves its own local confirmation when identity and pairing provenance match
            // (and clears it with a warning otherwise).
            let mut outgoing = local_devices.clone();
            for entry in outgoing.devices.values_mut() {
                entry.confirmed_by = None;
                entry.confirmed_at = None;
            }

            // An expiring credential may push ONLY tombstones to peers (never live device
            // entries), so revocations made with an expiring credential still reach the fleet.
            let mut skipped_live_due_to_expiry = false;
            if ctx.credential_expires().is_some() && !outgoing.devices.is_empty() {
                let mut full_preview = outgoing.clone();
                rewrite_tombstones_from_local(&remote_devices, &mut full_preview);
                let mut full_after = merge_devices(&remote_devices, &full_preview);
                let _ = preserve_local_confirmations(&remote_devices, &mut full_after);
                let mut tombs_only = outgoing.clone();
                tombs_only.devices.clear();
                let mut tombs_preview = tombs_only.clone();
                rewrite_tombstones_from_local(&remote_devices, &mut tombs_preview);
                let mut tombs_after = merge_devices(&remote_devices, &tombs_preview);
                let _ = preserve_local_confirmations(&remote_devices, &mut tombs_after);
                skipped_live_due_to_expiry = full_after != tombs_after;
                outgoing = tombs_only;
            }

            let mut remote_tombstones_only = DevicesFile {
                version: DEVICES_VERSION,
                devices: BTreeMap::new(),
                removed: remote_devices.removed.clone(),
            };
            rewrite_tombstones_from_local(&local_devices, &mut remote_tombstones_only);
            let local_after_tombstones = merge_devices(&local_devices, &remote_tombstones_only);
            let local_needs_tombstones = local_after_tombstones != local_devices;

            let mut outgoing_preview = outgoing.clone();
            rewrite_tombstones_from_local(&remote_devices, &mut outgoing_preview);
            let mut remote_after_push = merge_devices(&remote_devices, &outgoing_preview);
            let _ = preserve_local_confirmations(&remote_devices, &mut remote_after_push);
            let outgoing_is_empty = outgoing.devices.is_empty() && outgoing.removed.is_empty();
            let remote_needs_push =
                remote_after_push != remote_devices || (!remote.exists && !outgoing_is_empty);

            if !local_needs_tombstones && !remote_needs_push {
                if skipped_live_due_to_expiry && let Some(limit) = ctx.credential_expires() {
                    let expiry_err = ErrorBody::new(
                        ErrorCode::Denied,
                        format!(
                            "The credential this call came with expires at {}, so it cannot push live paired devices to {machine}; only tombstones are allowed.",
                            limit.strftime("%Y-%m-%dT%H:%M:%SZ")
                        ),
                    )
                    .with_hint(
                        "Use a local shell or a non-expiring credential to push live paired devices.",
                    );
                    if !outgoing.removed.is_empty() {
                        return DocSyncOutcome {
                            status: SyncStatus::Partial,
                            local_hash: local.hash,
                            remote_hash: remote.hash,
                            last_synced_hash,
                            backup: None,
                            detail: Some(format!(
                                "Removals delivered to {machine}; live device changes were refused: {}",
                                expiry_err.message
                            )),
                            warnings: Vec::new(),
                            error: Some(expiry_err),
                        };
                    }
                    return error_outcome(expiry_err);
                }
                if !dry_run
                    && local.hash == remote.hash
                    && let Some(ref lh) = local.hash
                    && last_synced_hash.as_deref() != Some(lh.as_str())
                {
                    let _ = record_sidecar_async(&state_dir, &doc, machine, lh).await;
                }
                let extra_remote = remote_devices != local_after_tombstones;
                let detail = extra_remote.then(|| {
                    format!(
                        "{machine} has additional live device(s) not imported by push; run `legion fleet.sync direction=pull from={machine} documents='[\"devices.toml\"]'` to import them."
                    )
                });
                return DocSyncOutcome {
                    status: SyncStatus::Same,
                    local_hash: local.hash,
                    remote_hash: remote.hash,
                    last_synced_hash,
                    backup: None,
                    detail,
                    warnings: Vec::new(),
                    error: None,
                };
            }

            if dry_run {
                return DocSyncOutcome {
                    status: SyncStatus::Pushed,
                    local_hash: local.hash,
                    remote_hash: remote.hash,
                    last_synced_hash,
                    backup: None,
                    detail: Some(
                        "Would push local devices.toml to remote and import only tombstones back (dry_run)."
                            .to_owned(),
                    ),
                    warnings: Vec::new(),
                    error: None,
                };
            }

            let current_local = local_devices.clone();
            let mut current_local_hash = local.hash.clone().unwrap_or_else(|| {
                hash_bytes(local_devices.to_toml_string().as_bytes()).to_string()
            });
            let mut local_backup = None;
            let mut warnings = Vec::new();
            let mut final_remote_hash = remote.hash.clone();
            let mut remote_has_extra_live = remote_after_push.devices.len()
                > current_local.devices.len()
                || remote_after_push
                    .devices
                    .iter()
                    .any(|(id, dev)| current_local.devices.get(id) != Some(dev));
            let mut tombstones_to_import = remote_tombstones_only;
            let mut partial_live_error: Option<ErrorBody> = None;

            // 1. Push our outgoing entries to the peer FIRST so revocation tombstones leave this
            //    node even if a local step or an unrelated entry fails.
            if remote_needs_push && !outgoing_is_empty {
                let outgoing_toml = outgoing.to_toml_string();
                let mut write_args = json!({
                    "document": "devices.toml",
                    "content": outgoing_toml,
                });
                if let Some(push_audit) = device_registry_push
                    && let Some(obj) = write_args.as_object_mut()
                {
                    obj.insert("device_registry_push".to_owned(), json!(push_audit));
                }
                let write_res = fleet
                    .call_remote(
                        ctx,
                        machine,
                        "config.write",
                        write_args,
                        EventSink::discard(),
                    )
                    .await;
                match write_res {
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
                        final_remote_hash = Some(written.hash.clone());
                        warnings.extend(written.warnings);
                        // If the remote hash differs from ours, read back ONLY tombstones from
                        // the peer (never live device entries!).
                        if written.hash != current_local_hash
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
                            && let Ok(again_devices) = parse::<DevicesFile>(
                                Path::new("devices.toml"),
                                again_content.as_bytes(),
                            )
                        {
                            let mut extra_tombs = DevicesFile {
                                version: DEVICES_VERSION,
                                devices: BTreeMap::new(),
                                removed: again_devices.removed,
                            };
                            rewrite_tombstones_from_local(&current_local, &mut extra_tombs);
                            tombstones_to_import =
                                merge_devices(&tombstones_to_import, &extra_tombs);
                            remote_has_extra_live = again_devices
                                .devices
                                .iter()
                                .any(|(id, dev)| current_local.devices.get(id) != Some(dev));
                        }
                    }
                    Err(error) => {
                        let receiver_delivered_tombstones = error
                            .details
                            .as_ref()
                            .and_then(|d| d.get("tombstones_delivered"))
                            .and_then(Value::as_bool)
                            == Some(true);
                        if receiver_delivered_tombstones {
                            if let Some(hash) = error
                                .details
                                .as_ref()
                                .and_then(|d| d.get("hash"))
                                .and_then(Value::as_str)
                            {
                                final_remote_hash = Some(hash.to_owned());
                            }
                            if let Some(arr) = error
                                .details
                                .as_ref()
                                .and_then(|d| d.get("warnings"))
                                .and_then(Value::as_array)
                            {
                                warnings.extend(
                                    arr.iter().filter_map(Value::as_str).map(str::to_owned),
                                );
                            }
                            partial_live_error = Some(error);
                        } else if !outgoing.removed.is_empty() && !outgoing.devices.is_empty() {
                            // Compatibility fallback for older peers that do not apply tombstones
                            // when refusing a live entry: send tombstones on their own so
                            // revocation still lands.
                            let tombs_only = DevicesFile {
                                version: DEVICES_VERSION,
                                devices: BTreeMap::new(),
                                removed: outgoing.removed.clone(),
                            };
                            let mut retry_args = json!({
                                "document": "devices.toml",
                                "content": tombs_only.to_toml_string(),
                            });
                            if let Some(push_audit) = device_registry_push
                                && let Some(obj) = retry_args.as_object_mut()
                            {
                                obj.insert("device_registry_push".to_owned(), json!(push_audit));
                            }
                            if let Ok(val) = fleet
                                .call_remote(
                                    ctx,
                                    machine,
                                    "config.write",
                                    retry_args,
                                    EventSink::discard(),
                                )
                                .await
                                && let Ok(written) =
                                    serde_json::from_value::<ConfigWriteResult>(val)
                            {
                                warnings.extend(written.warnings);
                                final_remote_hash = Some(written.hash);
                                partial_live_error = Some(error);
                            } else {
                                return DocSyncOutcome {
                                    status: SyncStatus::Error,
                                    local_hash: Some(current_local_hash),
                                    remote_hash: final_remote_hash,
                                    last_synced_hash,
                                    backup: local_backup,
                                    detail: None,
                                    warnings,
                                    error: Some(error),
                                };
                            }
                        } else {
                            return DocSyncOutcome {
                                status: SyncStatus::Error,
                                local_hash: Some(current_local_hash),
                                remote_hash: final_remote_hash,
                                last_synced_hash,
                                backup: local_backup,
                                detail: None,
                                warnings,
                                error: Some(error),
                            };
                        }
                    }
                }
            }

            // 2. Apply ONLY remote tombstones locally (never remote live entries).
            if merge_devices(&current_local, &tombstones_to_import) != current_local {
                match apply_local_write(
                    ctx,
                    SharedDocument::Devices,
                    tombstones_to_import.to_toml_string(),
                    None,
                    false,
                    device_registry_push.cloned(),
                )
                .await
                {
                    Ok(written) => {
                        local_backup = written.backup;
                        current_local_hash = written.hash_after;
                        warnings.extend(written.warnings);
                    }
                    Err(error) => {
                        return DocSyncOutcome {
                            status: SyncStatus::Error,
                            local_hash: Some(current_local_hash),
                            remote_hash: final_remote_hash,
                            last_synced_hash,
                            backup: None,
                            detail: None,
                            warnings,
                            error: Some(error),
                        };
                    }
                }
            }

            if let Some(live_err) = partial_live_error {
                let detail = format!(
                    "Removals delivered to {machine}; live device changes were refused: {}",
                    live_err.message
                );
                return DocSyncOutcome {
                    status: SyncStatus::Partial,
                    local_hash: Some(current_local_hash),
                    remote_hash: final_remote_hash,
                    last_synced_hash,
                    backup: local_backup,
                    detail: Some(detail),
                    warnings,
                    error: Some(live_err),
                };
            }

            if skipped_live_due_to_expiry && let Some(limit) = ctx.credential_expires() {
                let expiry_err = ErrorBody::new(
                    ErrorCode::Denied,
                    format!(
                        "The credential this call came with expires at {}, so it cannot push live paired devices to {machine}; only tombstones are allowed.",
                        limit.strftime("%Y-%m-%dT%H:%M:%SZ")
                    ),
                )
                .with_hint(
                    "Use a local shell or a non-expiring credential to push live paired devices.",
                );
                return DocSyncOutcome {
                    status: SyncStatus::Partial,
                    local_hash: Some(current_local_hash),
                    remote_hash: final_remote_hash,
                    last_synced_hash,
                    backup: local_backup,
                    detail: Some(format!(
                        "Removals delivered to {machine}; live device changes were refused: {}",
                        expiry_err.message
                    )),
                    warnings,
                    error: Some(expiry_err),
                };
            }

            let converged = final_remote_hash.as_deref() == Some(current_local_hash.as_str());
            if converged
                && let Err(error) =
                    record_sidecar_async(&state_dir, &doc, machine, &current_local_hash).await
            {
                return error_outcome(error);
            }

            let detail = remote_has_extra_live.then(|| {
                format!(
                    "{machine} has additional live device(s) not imported by push; run `legion fleet.sync direction=pull from={machine} documents='[\"devices.toml\"]'` to import them."
                )
            });

            DocSyncOutcome {
                status: SyncStatus::Pushed,
                local_hash: Some(current_local_hash.clone()),
                remote_hash: final_remote_hash,
                last_synced_hash: converged.then_some(current_local_hash).or(last_synced_hash),
                backup: local_backup,
                detail,
                warnings,
                error: None,
            }
        }
        SyncDirection::Pull => {
            let remote_legacy_ids = remote
                .content
                .as_deref()
                .map(legion_tools::device::registry::legacy_single_stamp_device_ids)
                .unwrap_or_default();
            if !remote_legacy_ids.is_empty() {
                let legacy_err = legion_tools::device::registry::legacy_single_stamp_incoming_error(
                    &remote_legacy_ids,
                );
                if !dry_run && !remote_devices.removed.is_empty() {
                    let tombs_only = DevicesFile {
                        version: DEVICES_VERSION,
                        devices: BTreeMap::new(),
                        removed: remote_devices.removed.clone(),
                    };
                    if let Ok(written) = apply_local_write(
                        ctx,
                        SharedDocument::Devices,
                        tombs_only.to_toml_string(),
                        None,
                        false,
                        None,
                    )
                    .await
                    {
                        return DocSyncOutcome {
                            status: SyncStatus::Partial,
                            local_hash: Some(written.hash_after),
                            remote_hash: remote.hash,
                            last_synced_hash,
                            backup: written.backup,
                            detail: Some(format!(
                                "Removals imported from {machine}; live device changes were refused: {}",
                                legacy_err.message
                            )),
                            warnings: written.warnings,
                            error: Some(legacy_err),
                        };
                    }
                }
                return DocSyncOutcome {
                    status: SyncStatus::Error,
                    local_hash: local.hash,
                    remote_hash: remote.hash,
                    last_synced_hash,
                    backup: None,
                    detail: None,
                    warnings: Vec::new(),
                    error: Some(legacy_err),
                };
            }
            // Explicit pull: merges the peer's live devices and tombstones into local `devices.toml`
            // without writing back to the peer. Confirmations are node-local (`device.set`), so
            // strip incoming confirmations; `preserve_local_confirmations` keeps this node's own
            // confirmation when identity and pairing provenance match (and clears + warns otherwise).
            let mut sanitized_remote = remote_devices.clone();
            for entry in sanitized_remote.devices.values_mut() {
                entry.confirmed_by = None;
                entry.confirmed_at = None;
            }
            rewrite_tombstones_from_local(&local_devices, &mut sanitized_remote);
            let mut sanitized_merged = merge_devices(&local_devices, &sanitized_remote);
            let _ = preserve_local_confirmations(&local_devices, &mut sanitized_merged);
            let sanitized_empty =
                sanitized_merged.devices.is_empty() && sanitized_merged.removed.is_empty();
            let local_up_to_date =
                local_devices == sanitized_merged && (sanitized_empty || local_file_exists);
            if local_up_to_date {
                if !dry_run
                    && local.hash == remote.hash
                    && let Some(ref lh) = local.hash
                    && last_synced_hash.as_deref() != Some(lh.as_str())
                {
                    let _ = record_sidecar_async(&state_dir, &doc, machine, lh).await;
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

            if remote.content.is_none() {
                return DocSyncOutcome {
                    status: SyncStatus::OnlyHere,
                    local_hash: local.hash,
                    remote_hash: remote.hash,
                    last_synced_hash,
                    backup: None,
                    detail: Some(format!(
                        "devices.toml does not exist on {machine}; run `legion fleet.sync direction=push machines='[\"{machine}\"]' documents='[\"devices.toml\"]'` to push local devices."
                    )),
                    warnings: Vec::new(),
                    error: None,
                };
            }

            if dry_run {
                return DocSyncOutcome {
                    status: SyncStatus::Pulled,
                    local_hash: local.hash,
                    remote_hash: remote.hash,
                    last_synced_hash,
                    backup: None,
                    detail: Some(format!(
                        "Would merge devices.toml from {machine} into this node (dry_run)."
                    )),
                    warnings: Vec::new(),
                    error: None,
                };
            }

            match apply_local_write(
                ctx,
                SharedDocument::Devices,
                sanitized_remote.to_toml_string(),
                None,
                false,
                None,
            )
            .await
            {
                Ok(written) => {
                    let converged = remote.hash.as_deref() == Some(written.hash_after.as_str());
                    if converged
                        && let Err(error) =
                            record_sidecar_async(&state_dir, &doc, machine, &written.hash_after)
                                .await
                    {
                        return error_outcome(error);
                    }
                    DocSyncOutcome {
                        status: SyncStatus::Pulled,
                        local_hash: Some(written.hash_after.clone()),
                        remote_hash: remote.hash,
                        last_synced_hash: converged
                            .then_some(written.hash_after)
                            .or(last_synced_hash),
                        backup: written.backup,
                        detail: None,
                        warnings: written.warnings,
                        error: None,
                    }
                }
                Err(error)
                    if error
                        .details
                        .as_ref()
                        .and_then(|d| d.get("tombstones_delivered"))
                        .and_then(Value::as_bool)
                        == Some(true) =>
                {
                    let new_local_hash = error
                        .details
                        .as_ref()
                        .and_then(|d| d.get("hash"))
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                        .or(local.hash);
                    let backup = error
                        .details
                        .as_ref()
                        .and_then(|d| d.get("backup"))
                        .and_then(Value::as_str)
                        .map(str::to_owned);
                    let warnings = error
                        .details
                        .as_ref()
                        .and_then(|d| d.get("warnings"))
                        .and_then(Value::as_array)
                        .map(|arr| {
                            arr.iter()
                                .filter_map(Value::as_str)
                                .map(str::to_owned)
                                .collect()
                        })
                        .unwrap_or_default();
                    let detail = format!(
                        "Removals imported from {machine}; live device changes were refused: {}",
                        error.message
                    );
                    DocSyncOutcome {
                        status: SyncStatus::Partial,
                        local_hash: new_local_hash,
                        remote_hash: remote.hash,
                        last_synced_hash,
                        backup,
                        detail: Some(detail),
                        warnings,
                        error: Some(error),
                    }
                }
                Err(error) => DocSyncOutcome {
                    status: SyncStatus::Error,
                    local_hash: local.hash,
                    remote_hash: remote.hash,
                    last_synced_hash,
                    backup: None,
                    detail: None,
                    warnings: Vec::new(),
                    error: Some(error),
                },
            }
        }
        SyncDirection::Status => unreachable!(),
    }
}

fn classify_devices_status(
    local: &DevicesFile,
    remote: &DevicesFile,
    merged: &DevicesFile,
    local_exists: bool,
    remote_exists: bool,
    machine: &str,
) -> (SyncStatus, Option<String>) {
    if local == remote {
        return (SyncStatus::Same, None);
    }
    if !local_exists && remote_exists {
        return (
            SyncStatus::OnlyThere,
            Some(format!(
                "Run `legion fleet.sync direction=pull from={machine} documents='[\"devices.toml\"]'` to import devices from {machine}."
            )),
        );
    }
    if local_exists && !remote_exists {
        return (
            SyncStatus::OnlyHere,
            Some(format!(
                "Run `legion fleet.sync direction=push machines='[\"{machine}\"]' documents='[\"devices.toml\"]'` to push local devices to {machine}."
            )),
        );
    }
    if merged == local && merged != remote {
        return (
            SyncStatus::ChangedHere,
            Some(format!(
                "Run `legion fleet.sync direction=push machines='[\"{machine}\"]' documents='[\"devices.toml\"]'` to bring {machine} up to date."
            )),
        );
    }
    if merged == remote && merged != local {
        return (
            SyncStatus::ChangedThere,
            Some(format!(
                "Run `legion fleet.sync direction=pull from={machine} documents='[\"devices.toml\"]'` to import devices from {machine} (push never imports a peer's live devices)."
            )),
        );
    }
    (
        SyncStatus::Both,
        Some(format!(
            "Both machines have device changes: push sends local devices to {machine} and imports only tombstones back; run `legion fleet.sync direction=pull from={machine} documents='[\"devices.toml\"]'` to import {machine}'s live devices here."
        )),
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
            classify_devices_status(&empty, &empty, &empty, false, false, "node-b").0,
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
            paired_by: None,
            confirmed_by: None,
            confirmed_at: None,
            updated_at: "2026-11-24T10:00:00Z".to_owned(),
            updated_on: "node-a".to_owned(),
            name_updated_at: "2026-11-24T10:00:00Z".to_owned(),
            name_updated_on: "node-a".to_owned(),
            role_updated_at: "2026-11-24T10:00:00Z".to_owned(),
            role_updated_on: "node-a".to_owned(),
            conflict: false,
            role_conflict: false,
        };
        let dev_b = FleetDevice {
            name: "phone-b".to_owned(),
            public_key: ed25519_public_key_from_seed(&[2u8; 32]),
            principal: "device:phone-b".to_owned(),
            role: Some("observer".to_owned()),
            paired_at: "2026-11-24T10:01:00Z".to_owned(),
            paired_on: "node-b".to_owned(),
            paired_by: None,
            confirmed_by: None,
            confirmed_at: None,
            updated_at: "2026-11-24T10:01:00Z".to_owned(),
            updated_on: "node-b".to_owned(),
            name_updated_at: "2026-11-24T10:01:00Z".to_owned(),
            name_updated_on: "node-b".to_owned(),
            role_updated_at: "2026-11-24T10:01:00Z".to_owned(),
            role_updated_on: "node-b".to_owned(),
            conflict: false,
            role_conflict: false,
        };

        let mut only_a = DevicesFile::default();
        only_a.devices.insert(id_a.clone(), dev_a.clone());
        let mut only_b = DevicesFile::default();
        only_b.devices.insert(id_b.clone(), dev_b.clone());
        let both = merge_devices(&only_a, &only_b);

        assert_eq!(
            classify_devices_status(&only_a, &empty, &only_a, true, false, "node-b").0,
            SyncStatus::OnlyHere
        );
        assert_eq!(
            classify_devices_status(&empty, &only_b, &only_b, false, true, "node-b").0,
            SyncStatus::OnlyThere
        );
        assert_eq!(
            classify_devices_status(&both, &only_a, &both, true, true, "node-b").0,
            SyncStatus::ChangedHere
        );
        assert_eq!(
            classify_devices_status(&only_a, &both, &both, true, true, "node-b").0,
            SyncStatus::ChangedThere
        );
        let (st, detail) = classify_devices_status(&only_a, &only_b, &both, true, true, "node-b");
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
                paired_by: None,
                confirmed_by: None,
                confirmed_at: None,
                updated_at: "2026-11-24T10:00:00Z".to_owned(),
                updated_on: "box-a".to_owned(),
                name_updated_at: "2026-11-24T10:00:00Z".to_owned(),
                name_updated_on: "box-a".to_owned(),
                role_updated_at: "2026-11-24T10:00:00Z".to_owned(),
                role_updated_on: "box-a".to_owned(),
                conflict: false,
                role_conflict: false,
            },
        );
        local.removed.insert(
            id_tomb.clone(),
            DeviceTombstone {
                name: "revoked-phone".to_owned(),
                principal: "device:revoked-phone".to_owned(),
                fingerprint: None,
                paired_at: Some("2026-11-24T09:00:00Z".to_owned()),
                paired_on: Some("box-a".to_owned()),
                paired_by: None,
                removed_by: None,
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
                paired_by: None,
                confirmed_by: None,
                confirmed_at: None,
                updated_at: "2026-11-24T11:00:00Z".to_owned(),
                updated_on: "box-b".to_owned(),
                name_updated_at: "2026-11-24T11:00:00Z".to_owned(),
                name_updated_on: "box-b".to_owned(),
                role_updated_at: "2026-11-24T11:00:00Z".to_owned(),
                role_updated_on: "box-b".to_owned(),
                conflict: false,
                role_conflict: false,
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
                paired_by: None,
                confirmed_by: None,
                confirmed_at: None,
                updated_at: "2026-11-24T10:10:00Z".to_owned(),
                updated_on: "box-b".to_owned(),
                name_updated_at: "2026-11-24T10:10:00Z".to_owned(),
                name_updated_on: "box-b".to_owned(),
                role_updated_at: "2026-11-24T10:10:00Z".to_owned(),
                role_updated_on: "box-b".to_owned(),
                conflict: false,
                role_conflict: false,
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
                paired_by: None,
                confirmed_by: None,
                confirmed_at: None,
                updated_at: "2026-11-24T10:20:00Z".to_owned(),
                updated_on: "box-b".to_owned(),
                name_updated_at: "2026-11-24T10:20:00Z".to_owned(),
                name_updated_on: "box-b".to_owned(),
                role_updated_at: "2026-11-24T10:20:00Z".to_owned(),
                role_updated_on: "box-b".to_owned(),
                conflict: false,
                role_conflict: false,
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
                fingerprint: None,
                paired_at: None,
                paired_on: None,
                paired_by: None,
                removed_by: None,
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
                paired_by: None,
                confirmed_by: None,
                confirmed_at: None,
                updated_at: "2026-11-24T10:00:00Z".to_owned(),
                updated_on: "box-a".to_owned(),
                name_updated_at: "2026-11-24T10:00:00Z".to_owned(),
                name_updated_on: "box-a".to_owned(),
                role_updated_at: "2026-11-24T10:00:00Z".to_owned(),
                role_updated_on: "box-a".to_owned(),
                conflict: false,
                role_conflict: false,
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
                paired_by: None,
                confirmed_by: None,
                confirmed_at: None,
                updated_at: "2026-11-24T10:05:00Z".to_owned(),
                updated_on: "box-a".to_owned(),
                name_updated_at: "2026-11-24T10:05:00Z".to_owned(),
                name_updated_on: "box-a".to_owned(),
                role_updated_at: "2026-11-24T10:05:00Z".to_owned(),
                role_updated_on: "box-a".to_owned(),
                conflict: false,
                role_conflict: false,
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
        entry.role_updated_at = "2026-11-24T10:06:00Z".to_owned();
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
                fingerprint: None,
                paired_at: Some("2026-11-24T10:00:00Z".to_owned()),
                paired_on: Some("box-a".to_owned()),
                paired_by: None,
                removed_by: None,
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
        node.call(tomb_call)
            .await
            .expect("tombstone should be allowed with expiring credential");

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
                paired_by: None,
                confirmed_by: None,
                confirmed_at: None,
                updated_at: "2026-11-24T10:00:00Z".to_owned(),
                updated_on: "box-a".to_owned(),
                name_updated_at: "2026-11-24T10:00:00Z".to_owned(),
                name_updated_on: "box-a".to_owned(),
                role_updated_at: "2026-11-24T10:00:00Z".to_owned(),
                role_updated_on: "box-a".to_owned(),
                conflict: false,
                role_conflict: false,
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
                paired_by: None,
                confirmed_by: None,
                confirmed_at: None,
                updated_at: "2026-11-24T10:05:00Z".to_owned(),
                updated_on: "box-b".to_owned(),
                name_updated_at: "2026-11-24T10:05:00Z".to_owned(),
                name_updated_on: "box-b".to_owned(),
                role_updated_at: "2026-11-24T10:05:00Z".to_owned(),
                role_updated_on: "box-b".to_owned(),
                conflict: false,
                role_conflict: false,
            },
        );
        let mut hijack_call = Call::new(
            Door::Cli,
            "config.write",
            json!({"document": "devices.toml", "content": hijack.to_toml_string()}),
        );
        hijack_call.pinned = Some("tablet".to_owned());
        let err = node.call(hijack_call).await.unwrap_err();
        assert_eq!(
            err.code,
            ErrorCode::Denied,
            "hijack must be denied: {err:?}"
        );

        // 2. Non-owner admin (`tablet`) cannot plant a tombstone for an id that is not live locally.
        let mut phantom_tomb = DevicesFile::default();
        phantom_tomb.removed.insert(
            "bb".repeat(16),
            DeviceTombstone {
                name: "phantom".to_owned(),
                principal: "tablet".to_owned(),
                fingerprint: None,
                paired_at: None,
                paired_on: None,
                paired_by: None,
                removed_by: None,
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
                paired_by: None,
                confirmed_by: None,
                confirmed_at: None,
                updated_at: "2026-11-24T10:07:00Z".to_owned(),
                updated_on: "box-b".to_owned(),
                name_updated_at: "2026-11-24T10:07:00Z".to_owned(),
                name_updated_on: "box-b".to_owned(),
                role_updated_at: "2026-11-24T10:07:00Z".to_owned(),
                role_updated_on: "box-b".to_owned(),
                conflict: false,
                role_conflict: false,
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

    #[cfg(unix)]
    #[tokio::test]
    async fn devices_merge_writes_tombstone_first_and_ends_sessions_on_failure_after_save() {
        use std::os::unix::fs::PermissionsExt;

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
        let id = "55".repeat(16);
        std::fs::write(
            paths.config_file("policy.toml"),
            format!(
                "default = \"owner\"\n\n[roles.observer]\nallow = [\"effect:read\"]\n\n[principals.\"device:pixel\"]\nrole = \"observer\"\ndevice = \"{id}\"\n"
            ),
        )
        .unwrap();

        let mut reg = legion_tools::registry();
        let fleet = Fleet::new(&paths);
        register(&mut reg, fleet);
        let node = Node::new(paths.clone(), reg);

        let mut initial = DevicesFile::default();
        initial.devices.insert(
            id.clone(),
            FleetDevice {
                name: "pixel".to_owned(),
                public_key: ed25519_public_key_from_seed(&[55u8; 32]),
                principal: "device:pixel".to_owned(),
                role: Some("observer".to_owned()),
                paired_at: "2026-11-24T10:00:00Z".to_owned(),
                paired_on: "box-a".to_owned(),
                paired_by: None,
                confirmed_by: None,
                confirmed_at: None,
                updated_at: "2026-11-24T10:00:00Z".to_owned(),
                updated_on: "box-a".to_owned(),
                name_updated_at: "2026-11-24T10:00:00Z".to_owned(),
                name_updated_on: "box-a".to_owned(),
                role_updated_at: "2026-11-24T10:00:00Z".to_owned(),
                role_updated_on: "box-a".to_owned(),
                conflict: false,
                role_conflict: false,
            },
        );
        std::fs::write(paths.config_file("devices.toml"), initial.to_toml_string()).unwrap();

        let mut revoke = DevicesFile::default();
        revoke.removed.insert(
            id.clone(),
            DeviceTombstone {
                name: "pixel".to_owned(),
                principal: "device:pixel".to_owned(),
                fingerprint: None,
                paired_at: Some("2026-11-24T10:00:00Z".to_owned()),
                paired_on: Some("box-a".to_owned()),
                paired_by: None,
                removed_by: None,
                removed_at: "2026-11-24T10:05:00Z".to_owned(),
                removed_on: "box-b".to_owned(),
            },
        );

        // 1. Unwritable config/ dir fails BEFORE touching policy.toml or authorized_keys.
        let cfg_dir = paths.config_dir().to_path_buf();
        let policy_before = std::fs::read_to_string(paths.config_file("policy.toml")).unwrap();
        std::fs::set_permissions(&cfg_dir, std::fs::Permissions::from_mode(0o500)).unwrap();
        let err_unwritable = node
            .call(Call::new(
                Door::Cli,
                "config.write",
                json!({"document": "devices.toml", "content": revoke.to_toml_string()}),
            ))
            .await;
        std::fs::set_permissions(&cfg_dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert!(err_unwritable.is_err());
        assert_eq!(
            std::fs::read_to_string(paths.config_file("policy.toml")).unwrap(),
            policy_before
        );

        // 2. Failure AFTER saving devices.toml (authorized_keys is a directory):
        //    devices.toml has the tombstone AND session termination/sweep STILL runs.
        let sess_dir = paths.state_dir().join("device-sessions");
        std::fs::create_dir_all(&sess_dir).unwrap();
        let marker_session = sess_dir.join("pixel-999999-1.json");
        std::fs::write(&marker_session, b"{\"stale\":true}\n").unwrap();
        std::fs::create_dir(&auth_keys).unwrap();

        let err_after = node
            .call(Call::new(
                Door::Cli,
                "config.write",
                json!({"document": "devices.toml", "content": revoke.to_toml_string()}),
            ))
            .await;
        let _ = std::fs::remove_dir(&auth_keys);
        assert!(err_after.is_err());
        let saved: DevicesFile = parse(
            &paths.config_file("devices.toml"),
            &std::fs::read(paths.config_file("devices.toml")).unwrap(),
        )
        .unwrap();
        assert!(saved.devices.is_empty());
        assert!(saved.removed.contains_key(&id));
        assert!(
            !marker_session.exists(),
            "session termination/sweep must run after devices.toml save even when authorized_keys fails"
        );
    }

    #[tokio::test]
    async fn config_read_devices_does_not_expose_unsaved_adopted_list_and_size_limit_has_way_out() {
        use legion_core::{Call, Door, Node, Paths};
        use legion_tools::device::authkeys::{ForcedCommandShell, PinnedHome, format_device_line};
        use legion_tools::device::keys::{
            ed25519_public_key_from_seed, validate_ed25519_public_key,
        };
        use legion_tools::device::store::{DeviceRecord, DevicesState, save_state};

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

        // Write a matching legacy line + state/devices.json WITHOUT creating devices.toml.
        let id = "ab".repeat(16);
        let key = ed25519_public_key_from_seed(&[81u8; 32]);
        let fp = validate_ed25519_public_key(&key).unwrap().fingerprint;
        let line = format_device_line(
            &legion_core::current_exe().unwrap(),
            "owner",
            &key,
            "legacy-box",
            &id,
            &PinnedHome::from_paths(&paths),
            None,
            ForcedCommandShell::host().unwrap(),
        )
        .unwrap();
        std::fs::write(&auth_keys, format!("{line}\n")).unwrap();
        let mut st = DevicesState::default();
        st.devices.insert(
            "legacy-box".to_owned(),
            DeviceRecord {
                name: "legacy-box".to_owned(),
                principal: "owner".to_owned(),
                added: "2026-11-24T10:00:00Z".to_owned(),
                fingerprint: fp,
                pairing_id: Some(id),
            },
        );
        save_state(&paths, &st).unwrap();

        let mut reg = legion_tools::registry();
        let fleet = Fleet::new(&paths);
        register(&mut reg, fleet);
        let node = Node::new(paths.clone(), reg);

        // config.read devices.toml must NOT synthesize an unsaved adopted list.
        let read = node
            .call(Call::new(
                Door::Cli,
                "config.read",
                json!({"document": "devices.toml"}),
            ))
            .await
            .unwrap();
        assert_eq!(read["exists"], json!(false), "{read}");
        assert!(read["content"].is_null(), "{read}");

        // Exceeding MAX_DOCUMENT_BYTES on devices.toml gives a clear error explaining that
        // tombstones are permanent.
        let huge = "a".repeat(MAX_DOCUMENT_BYTES + 16);
        let err = node
            .call(Call::new(
                Door::Cli,
                "config.write",
                json!({"document": "devices.toml", "content": huge}),
            ))
            .await
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::InvalidArgs);
        assert!(
            err.message.contains("configuration limit")
                && err.hint.as_deref().is_some_and(|h| h.contains("permanent")
                    && h.contains("device.remove")
                    && !h.to_ascii_lowercase().contains("prune")
                    && !h.to_ascii_lowercase().contains("tidy")),
            "expected clear permanent-tombstone size limit error: {err:?}"
        );
    }

    #[tokio::test]
    async fn m1_quarantine_and_removal_cannot_be_bypassed_via_config_write() {
        use jiff::SignedDuration;
        use legion_core::{Call, Door, Node, Paths};
        use legion_tools::device::keys::ed25519_public_key_from_seed;
        use legion_tools::device::registry::{
            DeviceTombstone, FleetDevice, PairedBy, lock_config, reconcile_device_principals_locked,
        };

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

        let id_d = "d1".repeat(16);
        let id_e = "e1".repeat(16);
        let id_s = "a1".repeat(16);
        let id_child = "c1".repeat(16);
        std::fs::write(
            paths.config_file("policy.toml"),
            format!(
                "\
default = \"owner\"

[roles.ops]
allow = [\"effect:read\", \"effect:admin\"]

[principals.\"device:d\"]
role = \"ops\"
device = \"{id_d}\"

[principals.shared_ops]
role = \"ops\"
"
            ),
        )
        .unwrap();

        let now = Timestamp::now();
        let t0 = (now - SignedDuration::from_mins(30)).to_string();
        let t1 = (now - SignedDuration::from_mins(20)).to_string();
        let t2 = (now - SignedDuration::from_mins(10)).to_string();
        let t_older = (now - SignedDuration::from_mins(25)).to_string();
        let t_backdated = (now - SignedDuration::from_hours(2)).to_string();

        let mut initial = DevicesFile::default();
        initial.devices.insert(
            id_d.clone(),
            FleetDevice {
                name: "d".to_owned(),
                public_key: ed25519_public_key_from_seed(&[0xd1; 32]),
                principal: "device:d".to_owned(),
                role: Some("ops".to_owned()),
                paired_at: t0.clone(),
                paired_on: "box-a".to_owned(),
                paired_by: Some(PairedBy {
                    principal: "owner".to_owned(),
                    device: None,
                }),
                confirmed_by: None,
                confirmed_at: None,
                updated_at: t0.clone(),
                updated_on: "box-a".to_owned(),
                name_updated_at: t0.clone(),
                name_updated_on: "box-a".to_owned(),
                role_updated_at: t0.clone(),
                role_updated_on: "box-a".to_owned(),
                conflict: false,
                role_conflict: false,
            },
        );
        initial.devices.insert(
            id_s.clone(),
            FleetDevice {
                name: "shared-parent".to_owned(),
                public_key: ed25519_public_key_from_seed(&[0xa1; 32]),
                principal: "shared_ops".to_owned(),
                role: None,
                paired_at: t0.clone(),
                paired_on: "box-a".to_owned(),
                paired_by: Some(PairedBy {
                    principal: "owner".to_owned(),
                    device: None,
                }),
                confirmed_by: None,
                confirmed_at: None,
                updated_at: t0.clone(),
                updated_on: "box-a".to_owned(),
                name_updated_at: t0.clone(),
                name_updated_on: "box-a".to_owned(),
                role_updated_at: t0.clone(),
                role_updated_on: "box-a".to_owned(),
                conflict: false,
                role_conflict: false,
            },
        );
        std::fs::write(paths.config_file("devices.toml"), initial.to_toml_string()).unwrap();

        let mut reg = legion_tools::registry();
        let fleet = Fleet::new(&paths);
        register(&mut reg, fleet);
        let node = Node::new(paths.clone(), reg);

        // 1. Reproduced attack: device `d` (admin-capable role `ops` on marked `device:d`) writes
        //    a new live entry `e` with another key, `principal = "device:d"`, `paired_by` omitted.
        //    Must be refused because `device:d` is marked for `id_d`!
        let mut hijack = DevicesFile::default();
        hijack.devices.insert(
            id_e.clone(),
            FleetDevice {
                name: "e".to_owned(),
                public_key: ed25519_public_key_from_seed(&[0xe1; 32]),
                principal: "device:d".to_owned(),
                role: Some("ops".to_owned()),
                paired_at: t1.clone(),
                paired_on: "box-a".to_owned(),
                paired_by: None,
                confirmed_by: None,
                confirmed_at: None,
                updated_at: t1.clone(),
                updated_on: "box-a".to_owned(),
                name_updated_at: t1.clone(),
                name_updated_on: "box-a".to_owned(),
                role_updated_at: t1.clone(),
                role_updated_on: "box-a".to_owned(),
                conflict: false,
                role_conflict: false,
            },
        );
        let mut hijack_call = Call::new(
            Door::Ssh,
            "config.write",
            json!({"document": "devices.toml", "content": hijack.to_toml_string()}),
        );
        hijack_call.pinned = Some("device:d".to_owned());
        hijack_call.device = Some("d".to_owned());
        hijack_call.pairing_id = Some(id_d.clone());
        let err = node.call(hijack_call).await.unwrap_err();
        assert_eq!(
            err.code,
            ErrorCode::Denied,
            "hijack by d must fail: {err:?}"
        );

        // 1b. Even an owner caller cannot assign another pairing id to marked principal `device:d`.
        let owner_hijack_err = node
            .call(Call::new(
                Door::Cli,
                "config.write",
                json!({"document": "devices.toml", "content": hijack.to_toml_string()}),
            ))
            .await
            .unwrap_err();
        assert_eq!(
            owner_hijack_err.code,
            ErrorCode::Denied,
            "owner merge claiming marked device:d for id_e must fail: {owner_hijack_err:?}"
        );

        // 2. Non-owner paired devices cannot write `devices.toml` at all under the two-writers
        //    rule (only the node's own device tools or an owner-authorised write may write it).
        let make_child = |paired_by: Option<PairedBy>| {
            let mut f = DevicesFile::default();
            f.devices.insert(
                id_child.clone(),
                FleetDevice {
                    name: "child".to_owned(),
                    public_key: ed25519_public_key_from_seed(&[0xc1; 32]),
                    principal: "shared_ops".to_owned(),
                    role: None,
                    paired_at: t1.clone(),
                    paired_on: "box-a".to_owned(),
                    paired_by,
                    confirmed_by: None,
                    confirmed_at: None,
                    updated_at: t1.clone(),
                    updated_on: "box-a".to_owned(),
                    name_updated_at: t1.clone(),
                    name_updated_on: "box-a".to_owned(),
                    role_updated_at: t1.clone(),
                    role_updated_on: "box-a".to_owned(),
                    conflict: false,
                    role_conflict: false,
                },
            );
            f
        };
        let mut non_owner_add = Call::new(
            Door::Ssh,
            "config.write",
            json!({
                "document": "devices.toml",
                "content": make_child(Some(PairedBy {
                    principal: "shared_ops".to_owned(),
                    device: Some(id_s.clone()),
                })).to_toml_string()
            }),
        );
        non_owner_add.pinned = Some("shared_ops".to_owned());
        non_owner_add.device = Some("shared-parent".to_owned());
        non_owner_add.pairing_id = Some(id_s.clone());
        assert_eq!(
            node.call(non_owner_add).await.unwrap_err().code,
            ErrorCode::Denied
        );

        // Seed `id_child` through an owner write so we can verify owner-level invariants below.
        node.call(Call::new(
            Door::Cli,
            "config.write",
            json!({
                "document": "devices.toml",
                "content": make_child(Some(PairedBy {
                    principal: "shared_ops".to_owned(),
                    device: Some(id_s.clone()),
                })).to_toml_string()
            }),
        ))
        .await
        .unwrap();

        let saved: DevicesFile = parse(
            &paths.config_file("devices.toml"),
            &std::fs::read(paths.config_file("devices.toml")).unwrap(),
        )
        .unwrap();
        let base_child = saved.devices[&id_child].clone();

        // 3. Non-owner child cannot rewrite its own fields (even with older or newer updated_at).
        for (label, mutate) in [
            (
                "paired_at newer",
                Box::new(|d: &mut FleetDevice| {
                    d.paired_at = t_backdated.clone();
                    d.updated_at = t2.clone();
                }) as Box<dyn Fn(&mut FleetDevice)>,
            ),
            (
                "paired_at older",
                Box::new(|d: &mut FleetDevice| {
                    d.paired_at = t_backdated.clone();
                    d.updated_at = t_older.clone();
                }),
            ),
            (
                "paired_by",
                Box::new(|d: &mut FleetDevice| {
                    d.paired_by = None;
                    d.updated_at = t2.clone();
                }),
            ),
        ] {
            let mut attempt = DevicesFile::default();
            let mut mutated = base_child.clone();
            mutate(&mut mutated);
            attempt.devices.insert(id_child.clone(), mutated);
            let mut call = Call::new(
                Door::Ssh,
                "config.write",
                json!({"document": "devices.toml", "content": attempt.to_toml_string()}),
            );
            call.pinned = Some("shared_ops".to_owned());
            call.device = Some("child".to_owned());
            call.pairing_id = Some(id_child.clone());
            let err = node.call(call).await.unwrap_err();
            assert_eq!(err.code, ErrorCode::Denied, "{label}: {err:?}");
        }

        // 4. Owner `config.write` cannot drop `paired_by` or set `confirmed_by`/`confirmed_at`
        //    on an existing entry (confirmation only through `device.set`).
        let mut owner_drop_pb = DevicesFile::default();
        let mut dropped = base_child.clone();
        dropped.paired_by = None;
        dropped.updated_at = t2.clone();
        owner_drop_pb.devices.insert(id_child.clone(), dropped);
        let drop_err = node
            .call(Call::new(
                Door::Cli,
                "config.write",
                json!({"document": "devices.toml", "content": owner_drop_pb.to_toml_string()}),
            ))
            .await
            .unwrap_err();
        assert_eq!(
            drop_err.code,
            ErrorCode::Denied,
            "owner drop paired_by: {drop_err:?}"
        );

        let confirm_toml = format!(
            "\
version = 1

[devices.{id_child}]
name = \"child\"
public_key = \"{}\"
principal = \"shared_ops\"
paired_at = \"{t1}\"
paired_on = \"box-a\"
paired_by = {{ principal = \"shared_ops\", device = \"{id_s}\" }}
confirmed_by = \"owner\"
confirmed_at = \"{t2}\"
updated_at = \"{t2}\"
updated_on = \"box-a\"
name_updated_at = \"{t1}\"
name_updated_on = \"box-a\"
role_updated_at = \"{t1}\"
role_updated_on = \"box-a\"
",
            base_child.public_key
        );
        let confirm_err = node
            .call(Call::new(
                Door::Cli,
                "config.write",
                json!({"document": "devices.toml", "content": confirm_toml}),
            ))
            .await
            .unwrap_err();
        assert_eq!(
            confirm_err.code,
            ErrorCode::Denied,
            "owner config.write setting confirmed_by/confirmed_at must fail: {confirm_err:?}"
        );

        // 5. After `d` is tombstoned, reconcile removes `device:d` and NEVER re-creates `device:d`
        //    for a different pairing id `e` (nor does owner `config.write` allow adding `e` on `device:d`).
        let mut tomb_d = DevicesFile::default();
        tomb_d.removed.insert(
            id_d.clone(),
            DeviceTombstone {
                name: "d".to_owned(),
                principal: "device:d".to_owned(),
                fingerprint: None,
                paired_at: Some(t0.clone()),
                paired_on: Some("box-a".to_owned()),
                paired_by: None,
                removed_by: None,
                removed_at: t2.clone(),
                removed_on: "box-a".to_owned(),
            },
        );
        node.call(Call::new(
            Door::Cli,
            "config.write",
            json!({"document": "devices.toml", "content": tomb_d.to_toml_string()}),
        ))
        .await
        .unwrap();

        let reuse_err = node
            .call(Call::new(
                Door::Cli,
                "config.write",
                json!({"document": "devices.toml", "content": hijack.to_toml_string()}),
            ))
            .await
            .unwrap_err();
        assert_eq!(
            reuse_err.code,
            ErrorCode::Denied,
            "reusing tombstoned device:d for id_e must fail: {reuse_err:?}"
        );

        // Even if a pre-existing file had `e` claiming `device:d` alongside `removed.d`,
        // reconcile_device_principals_locked must never re-create `device:d` for `e`.
        let mut corrupted: DevicesFile = parse(
            &paths.config_file("devices.toml"),
            &std::fs::read(paths.config_file("devices.toml")).unwrap(),
        )
        .unwrap();
        corrupted
            .devices
            .insert(id_e.clone(), hijack.devices[&id_e].clone());
        {
            let _cfg = lock_config(&paths).unwrap();
            let warnings = reconcile_device_principals_locked(&paths, &corrupted).unwrap();
            assert!(
                warnings.iter().any(|w| w.contains("device:d")),
                "expected warning refusing to re-create device:d: {warnings:?}"
            );
        }
        let pol_after: Policy = parse(
            &paths.config_file("policy.toml"),
            &std::fs::read(paths.config_file("policy.toml")).unwrap(),
        )
        .unwrap();
        assert_eq!(
            pol_after.device_owner("device:d"),
            None,
            "reconcile must never re-create device:d for id_e"
        );
    }

    #[tokio::test]
    async fn two_writers_rule_refuses_non_owner_config_write_and_skips_devices_in_fleet_sync() {
        use jiff::SignedDuration;
        use legion_core::{Call, Door, Node, Paths};
        use legion_tools::device::keys::ed25519_public_key_from_seed;
        use legion_tools::device::registry::{DeviceTombstone, FleetDevice, PairedBy};

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
            paths.config_file("fleet.toml"),
            "[machines.box-b]\ncommand = [\"/nonexistent/legion-does-not-exist\"]\n",
        )
        .unwrap();
        std::fs::write(
            paths.config_file("policy.toml"),
            "\
default = \"owner\"

[roles.ops]
allow = [\"effect:read\", \"effect:admin\"]

[principals.tablet]
role = \"ops\"
",
        )
        .unwrap();
        let id_x = "61".repeat(16);
        let now = Timestamp::now();
        let now_str = now.to_string();
        let backdated = (now - SignedDuration::from_hours(2)).to_string();

        let mut initial = DevicesFile::default();
        initial.devices.insert(
            id_x.clone(),
            FleetDevice {
                name: "x".to_owned(),
                public_key: ed25519_public_key_from_seed(&[0x61; 32]),
                principal: "tablet".to_owned(),
                role: None,
                paired_at: now_str.clone(),
                paired_on: "box-a".to_owned(),
                paired_by: Some(PairedBy {
                    principal: "owner".to_owned(),
                    device: None,
                }),
                confirmed_by: None,
                confirmed_at: None,
                updated_at: now_str.clone(),
                updated_on: "box-a".to_owned(),
                name_updated_at: now_str.clone(),
                name_updated_on: "box-a".to_owned(),
                role_updated_at: now_str.clone(),
                role_updated_on: "box-a".to_owned(),
                conflict: false,
                role_conflict: false,
            },
        );
        std::fs::write(paths.config_file("devices.toml"), initial.to_toml_string()).unwrap();

        let mut reg = legion_tools::registry();
        let fleet = Fleet::new(&paths);
        register(&mut reg, fleet);
        let node = Node::new(paths.clone(), reg);

        // 1. Non-owner with effect:admin cannot write its own live entry (or backdated entry) via config.write.
        let mut add_own = DevicesFile::default();
        add_own.devices.insert(
            "62".repeat(16),
            FleetDevice {
                name: "backdated-child".to_owned(),
                public_key: ed25519_public_key_from_seed(&[0x62; 32]),
                principal: "tablet".to_owned(),
                role: None,
                paired_at: backdated,
                paired_on: "box-a".to_owned(),
                paired_by: Some(PairedBy {
                    principal: "tablet".to_owned(),
                    device: Some(id_x.clone()),
                }),
                confirmed_by: None,
                confirmed_at: None,
                updated_at: now_str.clone(),
                updated_on: "box-a".to_owned(),
                name_updated_at: now_str.clone(),
                name_updated_on: "box-a".to_owned(),
                role_updated_at: now_str.clone(),
                role_updated_on: "box-a".to_owned(),
                conflict: false,
                role_conflict: false,
            },
        );
        let mut add_call = Call::new(
            Door::Ssh,
            "config.write",
            json!({"document": "devices.toml", "content": add_own.to_toml_string()}),
        );
        add_call.pinned = Some("tablet".to_owned());
        add_call.device = Some("x".to_owned());
        add_call.pairing_id = Some(id_x.clone());
        let add_err = node.call(add_call).await.unwrap_err();
        assert_eq!(
            add_err.code,
            ErrorCode::Denied,
            "non-owner config.write devices.toml must always be refused: {add_err:?}"
        );

        // 2. Non-owner cannot tombstone even its own live entry x (or name device:victim) via config.write.
        let mut tomb_x = DevicesFile::default();
        tomb_x.removed.insert(
            id_x.clone(),
            DeviceTombstone {
                name: "victim".to_owned(),
                principal: "device:victim".to_owned(),
                fingerprint: None,
                paired_at: None,
                paired_on: None,
                paired_by: None,
                removed_by: None,
                removed_at: now_str,
                removed_on: "box-a".to_owned(),
            },
        );
        let mut tomb_call = Call::new(
            Door::Ssh,
            "config.write",
            json!({"document": "devices.toml", "content": tomb_x.to_toml_string()}),
        );
        tomb_call.pinned = Some("tablet".to_owned());
        let tomb_err = node.call(tomb_call).await.unwrap_err();
        assert_eq!(
            tomb_err.code,
            ErrorCode::Denied,
            "non-owner tombstone config.write devices.toml must be refused: {tomb_err:?}"
        );

        // 3. Non-owner running fleet.sync (push or pull) reports devices.toml as skipped with reason.
        for (dir_name, args) in [
            (
                "push",
                json!({"direction": "push", "machines": ["box-b"], "documents": ["devices.toml"]}),
            ),
            (
                "pull",
                json!({"direction": "pull", "from": "box-b", "documents": ["devices.toml"]}),
            ),
        ] {
            let mut sync_call = Call::new(Door::Cli, "fleet.sync", args);
            sync_call.pinned = Some("tablet".to_owned());
            let res = node.call(sync_call).await.unwrap();
            let doc_res = &res["results"]["box-b"]["devices.toml"];
            assert_eq!(
                doc_res["status"],
                json!("skipped"),
                "{dir_name}: expected devices.toml to be skipped for non-owner: {res}"
            );
            assert!(
                doc_res["detail"]
                    .as_str()
                    .is_some_and(|d| d.contains("owner")),
                "{dir_name}: expected skip reason mentioning owner: {res}"
            );
        }
    }

    #[tokio::test]
    async fn nit2_pull_and_merge_preserve_local_confirmation_on_matching_provenance_else_clear_and_warn()
     {
        use jiff::SignedDuration;
        use legion_core::{Call, Door, Node, Paths};
        use legion_tools::device::keys::ed25519_public_key_from_seed;
        use legion_tools::device::registry::{DeviceTombstone, FleetDevice, PairedBy};

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

[roles.ops]
allow = [\"effect:read\", \"effect:admin\"]

[principals.tablet]
role = \"ops\"
",
        )
        .unwrap();

        let id_parent = "aa".repeat(16);
        let id_child = "bb".repeat(16);
        let now = Timestamp::now();
        let t0 = (now - SignedDuration::from_mins(20)).to_string();
        let t1 = (now - SignedDuration::from_mins(10)).to_string();
        let t2 = (now - SignedDuration::from_mins(5)).to_string();
        let t3 = (now - SignedDuration::from_mins(1)).to_string();
        let child_key_1 = ed25519_public_key_from_seed(&[0xb1; 32]);
        let child_key_2 = ed25519_public_key_from_seed(&[0xb2; 32]);

        // Seed local node with a tombstoned parent and a locally confirmed child.
        let mut local = DevicesFile::default();
        local.removed.insert(
            id_parent.clone(),
            DeviceTombstone {
                name: "parent".to_owned(),
                principal: "tablet".to_owned(),
                fingerprint: None,
                paired_at: Some(t0.clone()),
                paired_on: Some("box-a".to_owned()),
                paired_by: None,
                removed_by: None,
                removed_at: t0.clone(),
                removed_on: "box-a".to_owned(),
            },
        );
        local.devices.insert(
            id_child.clone(),
            FleetDevice {
                name: "child-v1".to_owned(),
                public_key: child_key_1.clone(),
                principal: "tablet".to_owned(),
                role: None,
                paired_at: t0.clone(),
                paired_on: "box-a".to_owned(),
                paired_by: Some(PairedBy {
                    principal: "tablet".to_owned(),
                    device: Some(id_parent.clone()),
                }),
                confirmed_by: Some("owner".to_owned()),
                confirmed_at: Some(t1.clone()),
                updated_at: t1.clone(),
                updated_on: "box-a".to_owned(),
                name_updated_at: t1.clone(),
                name_updated_on: "box-a".to_owned(),
                role_updated_at: t1.clone(),
                role_updated_on: "box-a".to_owned(),
                conflict: false,
                role_conflict: false,
            },
        );
        std::fs::write(paths.config_file("devices.toml"), local.to_toml_string()).unwrap();

        let mut reg = legion_tools::registry();
        let fleet = Fleet::new(&paths);
        register(&mut reg, fleet);
        let node = Node::new(paths.clone(), reg);

        // 1. Newer edit arrives with `confirmed_by: None`, renaming `child-v1` -> `child-v2` with
        //    IDENTICAL identity and pairing provenance (`public_key`, `principal`, `paired_at`,
        //    `paired_on`, `paired_by`). Local confirmation MUST be preserved with no warnings.
        let mut rename_incoming = DevicesFile::default();
        rename_incoming.devices.insert(
            id_child.clone(),
            FleetDevice {
                name: "child-v2".to_owned(),
                public_key: child_key_1.clone(),
                principal: "tablet".to_owned(),
                role: None,
                paired_at: t0.clone(),
                paired_on: "box-a".to_owned(),
                paired_by: Some(PairedBy {
                    principal: "tablet".to_owned(),
                    device: Some(id_parent.clone()),
                }),
                confirmed_by: None,
                confirmed_at: None,
                updated_at: t2.clone(),
                updated_on: "box-b".to_owned(),
                name_updated_at: t2.clone(),
                name_updated_on: "box-b".to_owned(),
                role_updated_at: t0.clone(),
                role_updated_on: "box-a".to_owned(),
                conflict: false,
                role_conflict: false,
            },
        );
        let res1 = node
            .call(Call::new(
                Door::Cli,
                "config.write",
                json!({"document": "devices.toml", "content": rename_incoming.to_toml_string()}),
            ))
            .await
            .unwrap();
        assert!(
            res1.get("warnings")
                .and_then(|v| v.as_array())
                .is_none_or(|w| w.is_empty()),
            "matching identity/provenance edit must not warn: {res1}"
        );
        let after1: DevicesFile = parse(
            &paths.config_file("devices.toml"),
            &std::fs::read(paths.config_file("devices.toml")).unwrap(),
        )
        .unwrap();
        let dev1 = &after1.devices[&id_child];
        assert_eq!(dev1.name, "child-v2");
        assert_eq!(
            dev1.confirmed_by.as_deref(),
            Some("owner"),
            "local confirmed_by must survive a newer edit with matching identity & provenance: {dev1:?}"
        );
        assert_eq!(dev1.confirmed_at.as_deref(), Some(t1.as_str()));

        // 2. Newer edit arrives changing `public_key` (identity mismatch): local confirmation MUST
        //    be cleared AND the result MUST warn loudly (and the device returns to quarantine!).
        let mut key_change_incoming = DevicesFile::default();
        key_change_incoming.devices.insert(
            id_child.clone(),
            FleetDevice {
                name: "child-v2".to_owned(),
                public_key: child_key_2,
                principal: "tablet".to_owned(),
                role: None,
                paired_at: t0,
                paired_on: "box-a".to_owned(),
                paired_by: Some(PairedBy {
                    principal: "tablet".to_owned(),
                    device: Some(id_parent),
                }),
                confirmed_by: None,
                confirmed_at: None,
                updated_at: t3.clone(),
                updated_on: "box-b".to_owned(),
                name_updated_at: t3.clone(),
                name_updated_on: "box-b".to_owned(),
                role_updated_at: t3,
                role_updated_on: "box-b".to_owned(),
                conflict: false,
                role_conflict: false,
            },
        );
        let res2 = node
            .call(Call::new(
                Door::Cli,
                "config.write",
                json!({"document": "devices.toml", "content": key_change_incoming.to_toml_string()}),
            ))
            .await
            .unwrap();
        let warnings2: Vec<&str> = res2["warnings"]
            .as_array()
            .map(|arr| arr.iter().filter_map(|v| v.as_str()).collect())
            .unwrap_or_default();
        assert!(
            warnings2
                .iter()
                .any(|w| w.to_ascii_lowercase().contains("confirmation") && w.contains(&id_child)),
            "expected warning when local confirmation is cleared on identity change: {res2}"
        );
        let after2: DevicesFile = parse(
            &paths.config_file("devices.toml"),
            &std::fs::read(paths.config_file("devices.toml")).unwrap(),
        )
        .unwrap();
        let dev2 = &after2.devices[&id_child];
        assert!(
            dev2.conflict,
            "immutable public_key mismatch must mark conflict = true: {dev2:?}"
        );
        assert_eq!(
            dev2.confirmed_by, None,
            "local confirmation must be cleared when incoming entry changes public_key: {dev2:?}"
        );
        assert_eq!(dev2.confirmed_at, None);
    }

    #[tokio::test]
    async fn f2_tombstone_only_merge_into_near_full_peer_succeeds_and_live_addition_breaking_reserve_is_refused()
     {
        use legion_core::{Call, Door, Node, Paths};
        use legion_tools::device::keys::ed25519_public_key_from_seed;
        use legion_tools::device::registry::{DeviceTombstone, FleetDevice, PairedBy};

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
        std::fs::write(paths.config_file("policy.toml"), "default = \"owner\"\n").unwrap();

        let mut reg = legion_tools::registry();
        let fleet = Fleet::new(&paths);
        register(&mut reg, fleet);
        let node = Node::new(paths.clone(), reg);

        let ts = "2026-11-24T10:00:00Z";
        let live_id = "aa".repeat(16);
        let unseen_id = "cc".repeat(16);
        let make_tomb = |idx: u32, pad_len: usize| DeviceTombstone {
            name: format!("d{idx:04}-{}", "p".repeat(pad_len)),
            principal: "owner".to_owned(),
            fingerprint: Some("SHA256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_owned()),
            paired_at: Some(ts.to_owned()),
            paired_on: Some("box-a".to_owned()),
            paired_by: None,
            removed_by: None,
            removed_at: ts.to_owned(),
            removed_on: "box-a".to_owned(),
        };

        // 1. Seed peer right under 1 MiB with live device `x`.
        let per_tomb = {
            let mut one = DevicesFile::default();
            one.removed
                .insert(format!("f{:031x}", 0u32), make_tomb(0, 100));
            one.to_toml_string().len() - "version = 1\n".len()
        };
        let mut near_full = DevicesFile::default();
        near_full.devices.insert(
            live_id.clone(),
            FleetDevice {
                name: "x".to_owned(),
                public_key: ed25519_public_key_from_seed(&[0xaa; 32]),
                principal: "owner".to_owned(),
                role: None,
                paired_at: ts.to_owned(),
                paired_on: "a".to_owned(),
                paired_by: None,
                confirmed_by: None,
                confirmed_at: None,
                updated_at: ts.to_owned(),
                updated_on: "a".to_owned(),
                name_updated_at: ts.to_owned(),
                name_updated_on: "a".to_owned(),
                role_updated_at: ts.to_owned(),
                role_updated_on: "a".to_owned(),
                conflict: false,
                role_conflict: false,
            },
        );
        let init_count = ((MAX_DOCUMENT_BYTES - 2500) / per_tomb) as u32;
        for i in 0..init_count {
            near_full
                .removed
                .insert(format!("f{i:031x}"), make_tomb(i, 100));
        }
        let mut idx = init_count;
        loop {
            let mut probe = near_full.clone();
            probe
                .removed
                .insert(format!("f{idx:031x}"), make_tomb(idx, 100));
            if probe.to_toml_string().len() > MAX_DOCUMENT_BYTES - 600 {
                break;
            }
            near_full = probe;
            idx += 1;
        }
        let make_bare_tomb = |i: u32, pad: usize| DeviceTombstone {
            name: format!("b{i:04}-{}", "p".repeat(pad)),
            principal: "owner".to_owned(),
            fingerprint: None,
            paired_at: None,
            paired_on: None,
            paired_by: None,
            removed_by: None,
            removed_at: ts.to_owned(),
            removed_on: "a".to_owned(),
        };
        let bare_len = {
            let mut probe = near_full.clone();
            probe
                .removed
                .insert(format!("f{idx:031x}"), make_bare_tomb(idx, 0));
            probe.to_toml_string().len() - near_full.to_toml_string().len()
        };
        while MAX_DOCUMENT_BYTES - near_full.to_toml_string().len() > bare_len + 115 {
            near_full
                .removed
                .insert(format!("f{idx:031x}"), make_bare_tomb(idx, 10));
            idx += 1;
        }
        let remaining = MAX_DOCUMENT_BYTES - near_full.to_toml_string().len();
        if remaining > bare_len + 10 {
            let pad = (remaining - bare_len - 10).min(115);
            near_full
                .removed
                .insert(format!("f{idx:031x}"), make_bare_tomb(idx, pad));
        }
        std::fs::write(
            paths.config_file("devices.toml"),
            near_full.to_toml_string(),
        )
        .unwrap();

        // Tombstone-only merge turning `x` into a larger tombstone AND adding an unseen tombstone
        // pushes merged bytes strictly > 1 MiB and MUST SUCCEED.
        let mut tomb_only = DevicesFile::default();
        tomb_only.removed.insert(
            live_id.clone(),
            DeviceTombstone {
                name: "x".to_owned(),
                principal: "owner".to_owned(),
                fingerprint: None,
                paired_at: None,
                paired_on: None,
                paired_by: None,
                removed_by: Some(PairedBy {
                    principal: format!("owner-{}", "o".repeat(120)),
                    device: Some("bb".repeat(16)),
                }),
                removed_at: "2026-11-24T10:05:00Z".to_owned(),
                removed_on: "n".repeat(63),
            },
        );
        tomb_only
            .removed
            .insert(unseen_id.clone(), make_tomb(9999, 80));

        node.call(Call::new(
            Door::Cli,
            "config.write",
            json!({
                "document": "devices.toml",
                "content": tomb_only.to_toml_string(),
            }),
        ))
        .await
        .expect("tombstone-only merge into near-full peer MUST never be refused for size");

        let after_bytes = std::fs::read(paths.config_file("devices.toml")).unwrap();
        assert!(
            after_bytes.len() > MAX_DOCUMENT_BYTES,
            "merged tombstone file should exceed 1 MiB (got {})",
            after_bytes.len()
        );
        let after: DevicesFile = parse(&paths.config_file("devices.toml"), &after_bytes).unwrap();
        assert!(after.devices.is_empty());
        assert!(after.removed.contains_key(&live_id));
        assert!(after.removed.contains_key(&unseen_id));

        // 2. Seed peer with ~700 bytes of raw headroom (< worst-case tombstone reserve, > actual
        //    live entry bytes): adding a live entry via config.write MUST be refused!
        let mut reserve_peer = DevicesFile::default();
        let reserve_init = ((MAX_DOCUMENT_BYTES - 2500) / per_tomb) as u32;
        for i in 0..reserve_init {
            reserve_peer
                .removed
                .insert(format!("e{i:031x}"), make_tomb(i, 100));
        }
        let mut r_idx = reserve_init;
        loop {
            let mut probe = reserve_peer.clone();
            probe
                .removed
                .insert(format!("e{r_idx:031x}"), make_tomb(r_idx, 100));
            if probe.to_toml_string().len() > MAX_DOCUMENT_BYTES - 650 {
                break;
            }
            reserve_peer = probe;
            r_idx += 1;
        }
        std::fs::write(
            paths.config_file("devices.toml"),
            reserve_peer.to_toml_string(),
        )
        .unwrap();

        let mut add_live = DevicesFile::default();
        add_live.devices.insert(
            "dd".repeat(16),
            FleetDevice {
                name: "z".to_owned(),
                public_key: ed25519_public_key_from_seed(&[0xdd; 32]),
                principal: "owner".to_owned(),
                role: None,
                paired_at: ts.to_owned(),
                paired_on: "a".to_owned(),
                paired_by: None,
                confirmed_by: None,
                confirmed_at: None,
                updated_at: ts.to_owned(),
                updated_on: "a".to_owned(),
                name_updated_at: ts.to_owned(),
                name_updated_on: "a".to_owned(),
                role_updated_at: ts.to_owned(),
                role_updated_on: "a".to_owned(),
                conflict: false,
                role_conflict: false,
            },
        );
        let err = node
            .call(Call::new(
                Door::Cli,
                "config.write",
                json!({
                    "document": "devices.toml",
                    "content": add_live.to_toml_string(),
                }),
            ))
            .await
            .expect_err("adding a live entry that breaks tombstone reserve MUST fail");
        assert_eq!(err.code, ErrorCode::InvalidArgs, "{err:?}");
    }

    #[tokio::test]
    async fn config_write_rejects_non_canonical_rfc9557_and_offset_timestamps() {
        use legion_core::{Call, Door, Node, Paths};
        use legion_tools::device::keys::ed25519_public_key_from_seed;

        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        std::fs::create_dir_all(home.join(".ssh")).unwrap();
        let auth_keys = dir.path().join("authorized_keys");
        std::fs::write(&auth_keys, "").unwrap();
        let paths = Paths::under(dir.path().join("legion")).with_home(&home);
        std::fs::create_dir_all(paths.config_dir()).unwrap();
        let auth_escaped = auth_keys.display().to_string().replace('\\', "\\\\");
        std::fs::write(
            paths.config_file("node.toml"),
            format!("name = \"box-a\"\n[devices]\nauthorized_keys = \"{auth_escaped}\"\n"),
        )
        .unwrap();
        std::fs::write(paths.config_file("policy.toml"), "default = \"owner\"\n").unwrap();

        let mut reg = legion_tools::registry();
        let fleet = Fleet::new(&paths);
        register(&mut reg, fleet);
        let node = Node::new(paths.clone(), reg);

        let key = ed25519_public_key_from_seed(&[0x44; 32]);
        let id = "44".repeat(16);
        for bad_ts in [
            "2026-11-24T10:00:00Z[UTC]",
            "2026-11-24T10:00:00Z[Etc/UTC][u-ca=iso8601]",
            "2026-11-24T12:00:00+02:00",
            "2026-11-24T10:00:00.000Z",
        ] {
            let live_toml = format!(
                "\
version = 1

[devices.{id}]
name = \"phone\"
public_key = \"{key}\"
principal = \"owner\"
paired_at = \"{bad_ts}\"
paired_on = \"box-a\"
updated_at = \"2026-11-24T10:00:00Z\"
updated_on = \"box-a\"
name_updated_at = \"2026-11-24T10:00:00Z\"
name_updated_on = \"box-a\"
role_updated_at = \"2026-11-24T10:00:00Z\"
role_updated_on = \"box-a\"
"
            );
            let err = node
                .call(Call::new(
                    Door::Cli,
                    "config.write",
                    json!({"document": "devices.toml", "content": live_toml}),
                ))
                .await
                .expect_err("non-canonical live timestamp must be refused");
            assert!(
                err.message.contains("canonical RFC 3339 UTC"),
                "{bad_ts}: {err:?}"
            );

            let tomb_toml = format!(
                "\
version = 1

[removed.{id}]
name = \"phone\"
principal = \"owner\"
removed_at = \"{bad_ts}\"
removed_on = \"box-a\"
"
            );
            let err = node
                .call(Call::new(
                    Door::Cli,
                    "config.write",
                    json!({"document": "devices.toml", "content": tomb_toml}),
                ))
                .await
                .expect_err("non-canonical tombstone timestamp must be refused");
            assert!(
                err.message.contains("canonical RFC 3339 UTC"),
                "{bad_ts}: {err:?}"
            );
        }
    }

    #[tokio::test]
    async fn config_write_refuses_legacy_single_stamp_entries_while_local_load_still_migrates() {
        use legion_core::{Call, Door, Node, Paths};
        use legion_tools::device::keys::ed25519_public_key_from_seed;
        use legion_tools::device::registry::load_devices_raw;

        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        std::fs::create_dir_all(home.join(".ssh")).unwrap();
        let auth_keys = dir.path().join("authorized_keys");
        std::fs::write(&auth_keys, "").unwrap();
        let paths = Paths::under(dir.path().join("legion")).with_home(&home);
        std::fs::create_dir_all(paths.config_dir()).unwrap();
        let auth_escaped = auth_keys.display().to_string().replace('\\', "\\\\");
        std::fs::write(
            paths.config_file("node.toml"),
            format!("name = \"box-a\"\n[devices]\nauthorized_keys = \"{auth_escaped}\"\n"),
        )
        .unwrap();
        std::fs::write(paths.config_file("policy.toml"), "default = \"owner\"\n").unwrap();

        let local_id = "11".repeat(16);
        let local_key = ed25519_public_key_from_seed(&[0x11; 32]);
        let local_legacy = format!(
            "\
version = 1

[devices.{local_id}]
name = \"local-old\"
public_key = \"{local_key}\"
principal = \"owner\"
paired_at = \"2026-11-24T10:00:00Z\"
paired_on = \"box-a\"
updated_at = \"2026-11-24T10:00:00Z\"
updated_on = \"box-a\"
"
        );
        std::fs::write(paths.config_file("devices.toml"), &local_legacy).unwrap();

        // 1. Local load STILL accepts the older single-stamp format for local migration.
        let loaded = load_devices_raw(&paths).unwrap().unwrap();
        assert!(loaded.devices.contains_key(&local_id));
        assert_eq!(
            loaded.devices[&local_id].name_updated_at,
            "2026-11-24T10:00:00Z"
        );
        assert_eq!(
            loaded.devices[&local_id].role_updated_at,
            "2026-11-24T10:00:00Z"
        );

        let mut reg = legion_tools::registry();
        let fleet = Fleet::new(&paths);
        register(&mut reg, fleet);
        let node = Node::new(paths.clone(), reg);

        // 2. Incoming `config.write` carrying a legacy single-stamp live entry (either omitting
        //    both per-field stamps or omitting just role_updated_*) MUST be refused with a hint
        //    to update the sending machine.
        let incoming_id = "22".repeat(16);
        let incoming_key = ed25519_public_key_from_seed(&[0x22; 32]);
        let cases = [
            (
                "missing both name_updated_* and role_updated_*",
                format!(
                    "\
version = 1

[devices.{incoming_id}]
name = \"peer-old\"
public_key = \"{incoming_key}\"
principal = \"owner\"
paired_at = \"2026-11-24T10:00:00Z\"
paired_on = \"box-b\"
updated_at = \"2026-11-24T10:00:00Z\"
updated_on = \"box-b\"
"
                ),
            ),
            (
                "missing role_updated_* only",
                format!(
                    "\
version = 1

[devices.{incoming_id}]
name = \"peer-old\"
public_key = \"{incoming_key}\"
principal = \"owner\"
paired_at = \"2026-11-24T10:00:00Z\"
paired_on = \"box-b\"
updated_at = \"2026-11-24T10:00:00Z\"
updated_on = \"box-b\"
name_updated_at = \"2026-11-24T10:00:00Z\"
name_updated_on = \"box-b\"
"
                ),
            ),
        ];

        for (label, content) in cases {
            let err = node
                .call(Call::new(
                    Door::Cli,
                    "config.write",
                    json!({"document": "devices.toml", "content": content}),
                ))
                .await
                .unwrap_err();
            assert_eq!(err.code, ErrorCode::InvalidArgs, "{label}: {err:?}");
            assert!(
                err.hint
                    .as_deref()
                    .is_some_and(|h| h.to_ascii_lowercase().contains("update")
                        && h.to_ascii_lowercase().contains("sending machine")),
                "{label}: expected update sending machine hint: {err:?}"
            );
        }

        // Local devices.toml stayed untouched and did not absorb incoming_id.
        let still_local = load_devices_raw(&paths).unwrap().unwrap();
        assert!(!still_local.devices.contains_key(&incoming_id));
        assert!(still_local.devices.contains_key(&local_id));
    }

    #[tokio::test]
    async fn tombstones_land_when_live_entries_are_refused_for_any_reason() {
        use jiff::SignedDuration;
        use legion_core::{Call, Door, Node, Paths};
        use legion_tools::device::keys::ed25519_public_key_from_seed;
        use legion_tools::device::registry::{
            DeviceTombstone, FleetDevice, load_devices_raw, next_updated_timestamp,
        };

        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        std::fs::create_dir_all(home.join(".ssh")).unwrap();
        let auth_keys = dir.path().join("authorized_keys");
        std::fs::write(&auth_keys, "").unwrap();
        let paths = Paths::under(dir.path().join("legion")).with_home(&home);
        std::fs::create_dir_all(paths.config_dir()).unwrap();
        let auth_escaped = auth_keys.display().to_string().replace('\\', "\\\\");
        std::fs::write(
            paths.config_file("node.toml"),
            format!("name = \"box-b\"\n[devices]\nauthorized_keys = \"{auth_escaped}\"\n"),
        )
        .unwrap();
        std::fs::write(
            paths.config_file("policy.toml"),
            "default = \"owner\"\n\n[roles.observer]\nallow = [\"effect:read\"]\n",
        )
        .unwrap();

        let mut reg = legion_tools::registry();
        let fleet = Fleet::new(&paths);
        register(&mut reg, fleet);
        let node = Node::new(paths.clone(), reg);

        let t0 = next_updated_timestamp(None).unwrap();
        let t1 = next_updated_timestamp(Some(&t0)).unwrap();

        let make_live = |seed: u8, name: &str, principal: &str, role: Option<&str>| FleetDevice {
            name: name.to_owned(),
            public_key: ed25519_public_key_from_seed(&[seed; 32]),
            principal: principal.to_owned(),
            role: role.map(str::to_owned),
            paired_at: t0.clone(),
            paired_on: "box-b".to_owned(),
            paired_by: None,
            confirmed_by: None,
            confirmed_at: None,
            updated_at: t0.clone(),
            updated_on: "box-b".to_owned(),
            name_updated_at: t0.clone(),
            name_updated_on: "box-b".to_owned(),
            role_updated_at: t0.clone(),
            role_updated_on: "box-b".to_owned(),
            conflict: false,
            role_conflict: false,
        };
        let make_tomb = |name: &str, principal: &str| DeviceTombstone {
            name: name.to_owned(),
            principal: principal.to_owned(),
            fingerprint: None,
            paired_at: Some(t0.clone()),
            paired_on: Some("box-b".to_owned()),
            paired_by: None,
            removed_by: None,
            removed_at: t1.clone(),
            removed_on: "box-a".to_owned(),
        };

        // Seed box-b with live devices: `holder` (`device:phone`), plus x1..x4 to revoke under
        // different live-refusal causes.
        let id_holder = "10".repeat(16);
        let id_x1 = "11".repeat(16);
        let id_x2 = "12".repeat(16);
        let id_x3 = "13".repeat(16);
        let id_x4 = "14".repeat(16);
        let mut seed = DevicesFile::default();
        seed.devices.insert(
            id_holder.clone(),
            make_live(0x10, "phone", "device:phone", Some("observer")),
        );
        seed.devices
            .insert(id_x1.clone(), make_live(0x11, "x1", "owner", None));
        seed.devices
            .insert(id_x2.clone(), make_live(0x12, "x2", "owner", None));
        seed.devices
            .insert(id_x3.clone(), make_live(0x13, "x3", "owner", None));
        seed.devices
            .insert(id_x4.clone(), make_live(0x14, "x4", "owner", None));
        node.call(Call::new(
            Door::Cli,
            "config.write",
            json!({"document": "devices.toml", "content": seed.to_toml_string()}),
        ))
        .await
        .unwrap();
        let ak_before = std::fs::read_to_string(&auth_keys).unwrap();
        for id in [&id_holder, &id_x1, &id_x2, &id_x3, &id_x4] {
            assert!(ak_before.contains(id), "expected {id} active before test");
        }

        // (a) Principal collision (`device:phone` already owned by `id_holder`) + tombstone for x1.
        let id_bad_principal = "a1".repeat(16);
        let mut push_principal = DevicesFile::default();
        push_principal
            .removed
            .insert(id_x1.clone(), make_tomb("x1", "owner"));
        push_principal.devices.insert(
            id_bad_principal.clone(),
            make_live(0xa1, "phone", "device:phone", Some("observer")),
        );
        let err = node
            .call(Call::new(
                Door::Cli,
                "config.write",
                json!({"document": "devices.toml", "content": push_principal.to_toml_string()}),
            ))
            .await
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::Denied, "{err:?}");
        assert_eq!(
            err.details
                .as_ref()
                .and_then(|d| d.get("tombstones_delivered")),
            Some(&json!(true)),
            "principal collision must still deliver tombstones: {err:?}"
        );
        let cur = load_devices_raw(&paths).unwrap().unwrap();
        assert!(cur.removed.contains_key(&id_x1) && !cur.devices.contains_key(&id_x1));
        assert!(!cur.devices.contains_key(&id_bad_principal));
        assert!(cur.devices.contains_key(&id_holder));
        assert!(
            !std::fs::read_to_string(&auth_keys)
                .unwrap()
                .contains(&id_x1)
        );

        // (b) Public-key collision (reusing `id_holder`'s key) + tombstone for x2.
        let id_bad_key = "a2".repeat(16);
        let mut push_key = DevicesFile::default();
        push_key
            .removed
            .insert(id_x2.clone(), make_tomb("x2", "owner"));
        push_key.devices.insert(
            id_bad_key.clone(),
            make_live(0x10, "dup-key", "owner", None),
        );
        let err = node
            .call(Call::new(
                Door::Cli,
                "config.write",
                json!({"document": "devices.toml", "content": push_key.to_toml_string()}),
            ))
            .await
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::Conflict, "{err:?}");
        assert_eq!(
            err.details
                .as_ref()
                .and_then(|d| d.get("tombstones_delivered")),
            Some(&json!(true)),
            "key collision must still deliver tombstones: {err:?}"
        );
        let cur = load_devices_raw(&paths).unwrap().unwrap();
        assert!(cur.removed.contains_key(&id_x2) && !cur.devices.contains_key(&id_x2));
        assert!(!cur.devices.contains_key(&id_bad_key));
        assert!(
            !std::fs::read_to_string(&auth_keys)
                .unwrap()
                .contains(&id_x2)
        );

        // (c) Expiring credential pushing tombstone for x3 + live device z3.
        let id_bad_exp = "a3".repeat(16);
        let mut push_exp = DevicesFile::default();
        push_exp
            .removed
            .insert(id_x3.clone(), make_tomb("x3", "owner"));
        push_exp
            .devices
            .insert(id_bad_exp.clone(), make_live(0xa3, "z3", "owner", None));
        let mut exp_call = Call::new(
            Door::Http,
            "config.write",
            json!({"document": "devices.toml", "content": push_exp.to_toml_string()}),
        );
        exp_call.pinned = Some("owner".to_owned());
        exp_call.credential_expires = Some(Timestamp::now() + SignedDuration::from_hours(1));
        let err = node.call(exp_call).await.unwrap_err();
        assert_eq!(err.code, ErrorCode::Denied, "{err:?}");
        assert_eq!(
            err.details
                .as_ref()
                .and_then(|d| d.get("tombstones_delivered")),
            Some(&json!(true)),
            "expiring credential refusal must still deliver tombstones: {err:?}"
        );
        let cur = load_devices_raw(&paths).unwrap().unwrap();
        assert!(cur.removed.contains_key(&id_x3) && !cur.devices.contains_key(&id_x3));
        assert!(!cur.devices.contains_key(&id_bad_exp));
        assert!(
            !std::fs::read_to_string(&auth_keys)
                .unwrap()
                .contains(&id_x3)
        );

        // (d) Legacy single-stamp live entry + valid tombstone for x4.
        let id_legacy = "a4".repeat(16);
        let legacy_key = ed25519_public_key_from_seed(&[0xa4; 32]);
        let legacy_with_tomb = format!(
            "\
version = 1

[devices.{id_legacy}]
name = \"old-format\"
public_key = \"{legacy_key}\"
principal = \"owner\"
paired_at = \"{t0}\"
paired_on = \"box-a\"
updated_at = \"{t0}\"
updated_on = \"box-a\"

[removed.{id_x4}]
name = \"x4\"
principal = \"owner\"
removed_at = \"{t1}\"
removed_on = \"box-a\"
"
        );
        let err = node
            .call(Call::new(
                Door::Cli,
                "config.write",
                json!({"document": "devices.toml", "content": legacy_with_tomb}),
            ))
            .await
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::InvalidArgs, "{err:?}");
        assert_eq!(
            err.details
                .as_ref()
                .and_then(|d| d.get("tombstones_delivered")),
            Some(&json!(true)),
            "legacy single-stamp refusal must still deliver tombstones: {err:?}"
        );
        let cur = load_devices_raw(&paths).unwrap().unwrap();
        assert!(cur.removed.contains_key(&id_x4) && !cur.devices.contains_key(&id_x4));
        assert!(!cur.devices.contains_key(&id_legacy));
        assert!(
            !std::fs::read_to_string(&auth_keys)
                .unwrap()
                .contains(&id_x4)
        );

        // (e) Size-limit / tombstone-reserve refusal + valid tombstone for `id_holder`.
        let mut near_full = cur.clone();
        let make_pad_tomb = |idx: u32| DeviceTombstone {
            name: format!("pad-{idx:04}-{}", "p".repeat(100)),
            principal: "owner".to_owned(),
            fingerprint: Some("SHA256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_owned()),
            paired_at: Some(t0.clone()),
            paired_on: Some("box-b".to_owned()),
            paired_by: None,
            removed_by: None,
            removed_at: t0.clone(),
            removed_on: "box-b".to_owned(),
        };
        let per_tomb = {
            let mut one = DevicesFile::default();
            one.removed
                .insert(format!("f{:031x}", 0u32), make_pad_tomb(0));
            one.to_toml_string().len() - "version = 1\n".len()
        };
        let init_n = ((MAX_DOCUMENT_BYTES - 3500) / per_tomb) as u32;
        for i in 0..init_n {
            near_full
                .removed
                .insert(format!("f{i:031x}"), make_pad_tomb(i));
        }
        let mut idx = init_n;
        loop {
            let mut probe = near_full.clone();
            probe
                .removed
                .insert(format!("f{idx:031x}"), make_pad_tomb(idx));
            if probe.to_toml_string().len() > MAX_DOCUMENT_BYTES - 150 {
                break;
            }
            near_full = probe;
            idx += 1;
        }
        std::fs::write(
            paths.config_file("devices.toml"),
            near_full.to_toml_string(),
        )
        .unwrap();

        let id_big_z = "a5".repeat(16);
        let mut push_size = DevicesFile::default();
        push_size
            .removed
            .insert(id_holder.clone(), make_tomb("phone", "device:phone"));
        push_size
            .devices
            .insert(id_big_z.clone(), make_live(0xa5, "z5", "owner", None));
        assert!(
            legion_tools::device::registry::devices_reserved_bytes(&merge_devices(
                &near_full, &push_size
            )) > MAX_DOCUMENT_BYTES,
            "fixture must exceed tombstone reserve when adding z5"
        );
        let err = node
            .call(Call::new(
                Door::Cli,
                "config.write",
                json!({"document": "devices.toml", "content": push_size.to_toml_string()}),
            ))
            .await
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::InvalidArgs, "{err:?}");
        assert_eq!(
            err.details
                .as_ref()
                .and_then(|d| d.get("tombstones_delivered")),
            Some(&json!(true)),
            "size-limit refusal must still deliver tombstones: {err:?}"
        );
        let after_size = load_devices_raw(&paths).unwrap().unwrap();
        assert!(
            after_size.removed.contains_key(&id_holder)
                && !after_size.devices.contains_key(&id_holder),
            "holder must be tombstoned even when live addition z5 exceeded size limit"
        );
        assert!(
            !after_size.devices.contains_key(&id_big_z),
            "live addition z5 must remain refused"
        );
        assert!(
            !std::fs::read_to_string(&auth_keys)
                .unwrap()
                .contains(&id_holder),
            "holder must be stripped from authorized_keys"
        );
    }
}
