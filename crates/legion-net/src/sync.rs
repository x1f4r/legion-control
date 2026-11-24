//! Shared configuration synchronisation across the fleet (`fleet.sync`, `config.list`,
//! `config.read`, `config.write`).
//!
//! Only the shared configuration documents (`fleet.toml`, `policy.toml`, `projects.toml`, and
//! validated files under `tools.d/<name>.toml`) may be read, written, or synchronised here;
//! machine-local files such as `node.toml` and `tokens.toml` are always refused.
//!
//! A revision counter cannot distinguish "newer" from "diverged", so each node keeps a small JSON
//! sidecar under `<state_dir>/sync/sidecars/` recording the BLAKE3 content hash last synchronised
//! with each peer. Writes are validated by the receiving node's own strict loader before replacing
//! anything, backed up to `<state_dir>/sync/backups/`, written atomically, and recorded in the
//! audit log with hashes before and after.

use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::marker::PhantomData;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use jiff::Timestamp;
use legion_core::config::{parse, write_atomically};
use legion_core::policy::Policy;
use legion_core::{
    BoxFuture, Ctx, EventSink, NoArgs, Registry, Tool, ToolError, input_schema, parse_args,
};
use legion_os::OsError;
use legion_os::lock::{FileLock, LockMode};
use legion_proto::{Effect, ErrorBody, ErrorCode, ToolSpec};
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
pub const FIXED_SHARED_DOCUMENTS: [&str; 3] = ["fleet.toml", "policy.toml", "projects.toml"];

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
        "List the shared configuration documents present on this node (fleet.toml, policy.toml, projects.toml, and files in tools.d/). Machine-local files (node.toml, tokens.toml) are never included.",
        config_list,
    );
    registry.add_fn(
        "config.read",
        Effect::Admin,
        "Read one shared configuration document (fleet.toml, policy.toml, projects.toml, or tools.d/<name>.toml) and its BLAKE3 content hash. Refuses machine-local files such as node.toml and tokens.toml.",
        config_read,
    );
    registry.add_fn(
        "config.write",
        Effect::Admin,
        "Validate and atomically replace one shared configuration document (fleet.toml, policy.toml, projects.toml, or tools.d/<name>.toml), keeping the previous version as a backup in the state directory and recording the hash before and after in the audit log. Writing policy.toml changes who may do what on this machine. Refuses invalid documents before replacing anything, refuses mismatched expect_hash with conflict, and refuses machine-local files such as node.toml and tokens.toml.",
        config_write,
    );
    registry.add(SyncTool::<SyncArgs, SyncResult, _>::new(
        "fleet.sync",
        Effect::Admin,
        "Compare, push, or pull shared configuration documents (fleet.toml, policy.toml, projects.toml, and tools.d/*.toml) across machines in fleet.toml using per-peer BLAKE3 sync sidecars to detect conflicts. Syncing policy.toml is powerful because it changes who may do what on the receiving machine; every replacement is strictly validated by the receiver before installing, backed up, and recorded in the audit log with its hash before and after. Never syncs node.toml or tokens.toml.",
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
                "Only fleet.toml, policy.toml, projects.toml, and tools.d/<name>.toml can be synced.",
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
                    "Allowed shared documents are fleet.toml, policy.toml, projects.toml, and tools.d/<name>.toml.",
                ))
            }
        }
    }

    /// Canonical wire name (always using forward slash for `tools.d/<name>.toml`).
    pub fn canonical_name(&self) -> String {
        match self {
            Self::Fleet => "fleet.toml".to_owned(),
            Self::Policy => "policy.toml".to_owned(),
            Self::Projects => "projects.toml".to_owned(),
            Self::ToolExt(bare) => format!("{TOOLS_D_PREFIX}{bare}"),
        }
    }

    /// Location of this document under `config_dir`.
    pub fn config_path(&self, config_dir: &Path) -> PathBuf {
        match self {
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
}

/// Validates, backs up, and atomically writes one shared configuration document.
fn write_local_document_blocking(
    config_dir: &Path,
    state_dir: &Path,
    doc: &SharedDocument,
    content: &str,
    expect_hash: Option<&str>,
    is_builtin: &dyn Fn(&str) -> bool,
) -> Result<WriteOutcome, ErrorBody> {
    if let Some(expected) = expect_hash {
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

    if let Some(expected) = expect_hash
        && hash_before.as_deref() != Some(expected)
    {
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
    })
}

async fn apply_local_write(
    ctx: &Ctx,
    doc: SharedDocument,
    content: String,
    expect_hash: Option<String>,
) -> Result<WriteOutcome, ErrorBody> {
    let started = Instant::now();
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
        write_local_document_blocking(
            &config_dir,
            &state_dir,
            &doc,
            &content,
            expect_hash.as_deref(),
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
        .record_audit(
            ctx.node_name().to_owned(),
            Some(ctx.principal().to_owned()),
            ctx.door(),
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
    /// Shared document name: `fleet.toml`, `policy.toml`, `projects.toml`, or `tools.d/<name>.toml`.
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
    let config_dir = ctx.paths().config_dir().to_path_buf();
    tokio::task::spawn_blocking(move || read_local_document(&config_dir, &doc))
        .await
        .map_err(|error| {
            ErrorBody::new(
                ErrorCode::Internal,
                format!("config.read stopped unexpectedly: {error}."),
            )
        })?
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct ConfigWriteArgs {
    /// Shared document name: `fleet.toml`, `policy.toml`, `projects.toml`, or `tools.d/<name>.toml`.
    document: String,
    /// Full UTF-8 TOML text to validate and write atomically.
    content: String,
    /// Expected BLAKE3 hash (`blake3:<hex>`) of the existing file; fails with `conflict` if different.
    #[serde(default)]
    expect_hash: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
struct ConfigWriteResult {
    document: String,
    hash: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    backup: Option<String>,
}

async fn config_write(ctx: &Ctx, args: ConfigWriteArgs) -> Result<ConfigWriteResult, ToolError> {
    let doc = SharedDocument::parse(&args.document)?;
    let outcome = apply_local_write(ctx, doc, args.content, args.expect_hash).await?;
    Ok(ConfigWriteResult {
        document: outcome.document,
        hash: outcome.hash_after,
        backup: outcome.backup,
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
#[serde(rename_all = "lowercase")]
pub enum SyncStatus {
    /// Local and remote bytes have the same hash (or both are absent).
    Same,
    /// Local document was pushed to the remote (or would be under `dry_run`).
    Pushed,
    /// Remote document was pulled locally (or would be under `dry_run`).
    Pulled,
    /// Local and remote hashes differ (reported by `status`, or when the source lacks the file).
    Differs,
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
    /// Shared documents to sync; defaults to fleet.toml, policy.toml, projects.toml, and any tools.d/*.toml files.
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
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<ErrorBody>,
}

async fn fleet_sync(fleet: &Fleet, ctx: &Ctx, args: SyncArgs) -> Result<SyncResult, ToolError> {
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
                let per_doc = sync_one_machine(
                    fleet,
                    ctx,
                    &machine,
                    args.direction,
                    explicit_docs.as_deref(),
                    &local_docs,
                    args.force,
                    args.dry_run,
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
                        let per_doc = sync_one_machine(
                            fleet,
                            ctx,
                            &machine,
                            args.direction,
                            explicit_docs,
                            local_docs,
                            args.force,
                            args.dry_run,
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
                error: None,
            }
        }
        SyncDecision::Differs(reason) => DocSyncOutcome {
            status: SyncStatus::Differs,
            local_hash: local.hash,
            remote_hash: remote.hash,
            last_synced_hash,
            backup: None,
            detail: reason,
            error: None,
        },
        SyncDecision::Conflict(reason) => DocSyncOutcome {
            status: SyncStatus::Conflict,
            local_hash: local.hash,
            remote_hash: remote.hash,
            last_synced_hash,
            backup: None,
            detail: Some(reason),
            error: None,
        },
        SyncDecision::Push => {
            let Some(local_content) = local.content else {
                return DocSyncOutcome {
                    status: SyncStatus::Differs,
                    local_hash: local.hash,
                    remote_hash: remote.hash,
                    last_synced_hash,
                    backup: None,
                    detail: Some("Document does not exist locally.".to_owned()),
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
                    error: None,
                };
            }
            let mut write_args = json!({
                "document": doc.canonical_name(),
                "content": local_content,
            });
            if !force
                && let Some(ref rh) = remote.hash
                && let Some(obj) = write_args.as_object_mut()
            {
                obj.insert("expect_hash".to_owned(), json!(rh));
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
                    error: Some(error),
                },
                Err(error) => DocSyncOutcome {
                    status: SyncStatus::Error,
                    local_hash: Some(local_hash),
                    remote_hash: remote.hash,
                    last_synced_hash,
                    backup: None,
                    detail: None,
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
                    error: None,
                };
            }
            let expect = if force { None } else { local.hash.clone() };
            match apply_local_write(ctx, doc.clone(), remote_content, expect).await {
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
                    error: Some(error),
                },
                Err(error) => DocSyncOutcome {
                    status: SyncStatus::Error,
                    local_hash: local.hash,
                    remote_hash: Some(remote_hash),
                    last_synced_hash,
                    backup: None,
                    detail: None,
                    error: Some(error),
                },
            }
        }
    }
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
        error: Some(error),
    }
}

/// Pure sync decision for one document between local and remote hashes and the last synced hash.
#[derive(Debug, PartialEq, Eq)]
pub enum SyncDecision {
    /// Local and remote match (or both are absent).
    Same,
    /// Hashes differ (status mode, or source side does not have the file).
    Differs(Option<String>),
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
        SyncDirection::Status => SyncDecision::Differs(None),
        SyncDirection::Push => {
            if local_hash.is_none() {
                return SyncDecision::Differs(Some(
                    "Document does not exist locally; push does not delete remote files."
                        .to_owned(),
                ));
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
                return SyncDecision::Differs(Some(
                    "Document does not exist on the remote; pull does not delete local files."
                        .to_owned(),
                ));
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
            decide_sync(SyncDirection::Status, Some("h1"), Some("h2"), None, false),
            SyncDecision::Differs(None)
        );
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
            None,
            &no_builtins,
        );
        assert!(bad.is_err());
        assert!(!cfg.join("policy.toml").exists());

        // First valid write creates the file with no backup.
        let v1 = "default = \"owner\"\n";
        let first = write_local_document_blocking(
            &cfg,
            &state,
            &SharedDocument::Policy,
            v1,
            None,
            &no_builtins,
        )
        .unwrap();
        assert!(first.backup.is_none());
        assert!(first.hash_before.is_none());

        // Second valid write with matching expect_hash backs up v1 and replaces atomically.
        let v2 = "default = \"owner\"\n\n[roles.viewer]\nallow = [\"effect:read\"]\n";
        let second = write_local_document_blocking(
            &cfg,
            &state,
            &SharedDocument::Policy,
            v2,
            Some(&first.hash_after),
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
            Some(&first.hash_after),
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
            write_local_document_blocking(&cfg, &state, &doc, &unknown, None, &is_builtin).is_err()
        );

        // Reserved family is refused.
        let reserved_doc = SharedDocument::parse("tools.d/meta.evil.toml").unwrap();
        let reserved =
            "name = \"meta.evil\"\ndescription = \"Evil.\"\neffect = \"read\"\ncmd = \"echo x\"\n";
        let err =
            write_local_document_blocking(&cfg, &state, &reserved_doc, reserved, None, &is_builtin)
                .err()
                .unwrap();
        assert!(err.message.contains("reserved"), "{err:?}");

        // Built-in shadowing is refused.
        let shadow_doc = SharedDocument::parse("tools.d/agent.list.toml").unwrap();
        let shadow = "name = \"agent.list\"\ndescription = \"Shadow.\"\neffect = \"read\"\ncmd = \"echo x\"\n";
        let err =
            write_local_document_blocking(&cfg, &state, &shadow_doc, shadow, None, &is_builtin)
                .err()
                .unwrap();
        assert!(err.message.contains("built-in"), "{err:?}");

        // File stem mismatch is refused.
        let wrong_stem = SharedDocument::parse("tools.d/other.toml").unwrap();
        let err =
            write_local_document_blocking(&cfg, &state, &wrong_stem, valid, None, &is_builtin)
                .err()
                .unwrap();
        assert!(
            err.message.contains("does not match file name stem"),
            "{err:?}"
        );

        // Duplicate tool name already defined in another tools.d file is refused.
        std::fs::create_dir_all(cfg.join("tools.d")).unwrap();
        std::fs::write(cfg.join("tools.d").join("legacy.toml"), valid).unwrap();
        let err = write_local_document_blocking(&cfg, &state, &doc, valid, None, &is_builtin)
            .err()
            .unwrap();
        assert!(err.message.contains("already defined"), "{err:?}");

        // Removing the conflicting file allows writing and updating tools.d/demo.hello.toml.
        std::fs::remove_file(cfg.join("tools.d").join("legacy.toml")).unwrap();
        let first =
            write_local_document_blocking(&cfg, &state, &doc, valid, None, &is_builtin).unwrap();
        let updated = "name = \"demo.hello\"\ndescription = \"Say hi.\"\neffect = \"read\"\ncmd = \"echo hi2\"\n";
        write_local_document_blocking(
            &cfg,
            &state,
            &doc,
            updated,
            Some(&first.hash_after),
            &is_builtin,
        )
        .unwrap();
    }
}
