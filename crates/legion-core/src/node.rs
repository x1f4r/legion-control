//! The node: the one place a call is resolved, authorised, run and audited, whichever door it came
//! through.

use std::collections::BTreeMap;
use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hasher};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use legion_os::lock::{FileLock, LockMode};
pub use legion_proto::{CANCEL_GRACE, VERSION};
use legion_proto::{ErrorBody, ErrorCode, HelloParams, HelloResult, PROTOCOL, ToolSpec};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::audit::{self, AuditLog, Entry, Outcome};
use crate::config::{ConfigStore, NodeFile, NodeName, write_atomically};
use crate::ctx::{Ctx, Door, EventSink};
use crate::paths::Paths;
use crate::policy::Policy;
use crate::registry::{Registry, SourceCheck};
use crate::tool::Tool;

/// The source revision this binary was built from (for example `a59ec0d12345` or
/// `a59ec0d12345, modified`), or `None` when built outside git without `LEGION_BUILD_REV`.
pub const BUILD: Option<&str> = option_env!("LEGION_BUILD_ID");

/// Formats the `legion --version` line for `version` and optional `build`.
pub fn format_version_line(version: &str, build: Option<&str>) -> String {
    match build.map(str::trim).filter(|b| !b.is_empty()) {
        Some(rev) => format!("legion {version} ({rev})"),
        None => format!("legion {version}"),
    }
}

/// One call, as a door hands it to [`Node::call`].
#[derive(Debug)]
pub struct Call {
    /// The tool's name.
    pub tool: String,
    /// The tool's arguments, as the caller sent them.
    pub args: Value,
    /// The principal the caller asked to act as (`--as`, or `as` in the request).
    pub as_principal: Option<String>,
    /// The door it came through.
    pub door: Door,
    /// The principal the door pinned, when it pinned one: `legion rpc --as`, an ssh key forced to
    /// `legion gate --as`, an HTTP token.
    pub pinned: Option<String>,
    /// The paired device name when the call arrived through `legion gate --as ... --device <name>`.
    pub device: Option<String>,
    /// The paired device's 32-hex pairing identifier when the call arrived through
    /// `legion gate --as ... --pairing <id>`.
    pub pairing_id: Option<String>,
    /// Cancelled by the door when the caller cancels.
    pub cancel: CancellationToken,
    /// Where the call's events go.
    pub events: EventSink,
    /// How many nodes have already forwarded this call; 0 when it started on this node.
    pub hops: u32,
    /// When the credential the call arrived with stops working, when it is one that does: an
    /// HTTP token with an expiry. Whatever the call makes on the caller's behalf, such as another
    /// token, may last no longer.
    pub credential_expires: Option<jiff::Timestamp>,
}

impl Call {
    /// A call acting as whoever the door and the policy say, never cancelled, whose events are
    /// dropped; set the other fields to change that.
    pub fn new(door: Door, tool: impl Into<String>, args: Value) -> Call {
        Call {
            tool: tool.into(),
            args,
            as_principal: None,
            door,
            pinned: None,
            device: None,
            pairing_id: None,
            cancel: CancellationToken::new(),
            events: EventSink::discard(),
            hops: 0,
            credential_expires: None,
        }
    }
}

/// A running node. Cloning it is cheap and every clone is the same node, so each concurrent call
/// can hold its own.
#[derive(Clone, Debug)]
pub struct Node {
    inner: Arc<Inner>,
}

#[derive(Debug)]
struct Inner {
    paths: Paths,
    registry: Registry,
    config: ConfigStore,
    audit: AuditLog,
    host_name: NodeName,
    node_id: OnceLock<String>,
}

impl Node {
    /// A node over these directories offering these tools.
    ///
    /// The state directory is created now if it can be, so the first call has somewhere to write
    /// its audit line; if it cannot, every call says so on stderr and `meta.doctor` says why.
    pub fn new(paths: Paths, registry: Registry) -> Node {
        let _ = std::fs::create_dir_all(paths.state_dir());
        Node {
            inner: Arc::new(Inner {
                config: ConfigStore::new(paths.config_dir()),
                audit: AuditLog::new(&paths),
                host_name: NodeName::from_host(),
                node_id: OnceLock::new(),
                paths,
                registry,
            }),
        }
    }

    /// Where the node keeps its files.
    pub fn paths(&self) -> &Paths {
        &self.inner.paths
    }

    /// The tools it offers.
    pub fn registry(&self) -> &Registry {
        &self.inner.registry
    }

    /// Its configuration files.
    pub fn config(&self) -> &ConfigStore {
        &self.inner.config
    }

    /// Its audit log.
    pub fn audit_log(&self) -> &AuditLog {
        &self.inner.audit
    }

    /// The node's name: the one `node.toml` gives, or the host's.
    ///
    /// A `node.toml` that does not parse fails the calls that read its other settings, and
    /// `meta.doctor` reports it; the name falls back to the host's meanwhile, because every call
    /// needs a name to record and a wrong one grants nothing.
    pub fn name(&self) -> String {
        match self.inner.config.get::<NodeFile>() {
            Ok(file) => file
                .name
                .as_ref()
                .map_or_else(|| self.inner.host_name.to_string(), NodeName::to_string),
            Err(_) => self.inner.host_name.to_string(),
        }
    }

    /// Whether `node.toml` sets `name` explicitly (`Some(true)`), omits `name` / does not exist
    /// (`Some(false)`), or could not be parsed (`None`, so callers never overwrite a broken file).
    pub fn name_explicit(&self) -> Option<bool> {
        self.inner
            .config
            .get::<NodeFile>()
            .ok()
            .map(|file| file.name.is_some())
    }

    /// Every tool's spec, sorted by name: built-in tools first, then any reloadable sources.
    /// Built-in tools always win if a source offers the same name.
    pub fn specs(&self) -> Vec<ToolSpec> {
        let mut by_name: BTreeMap<String, ToolSpec> = self
            .inner
            .registry
            .specs()
            .into_iter()
            .map(|spec| (spec.name.clone(), spec))
            .collect();
        for source in self.inner.registry.sources() {
            for spec in source.specs(&self.inner.paths, &self.inner.registry) {
                by_name.entry(spec.name.clone()).or_insert(spec);
            }
        }
        by_name.into_values().collect()
    }

    /// One tool's spec, or the `unknown_tool` error a call to it would get.
    pub fn spec(&self, tool: &str) -> Result<ToolSpec, ErrorBody> {
        let name = self.name();
        match self.resolve_tool(tool) {
            Some(found) => Ok(found.spec().clone()),
            None => Err(self.unknown_tool(tool, &name).with_machine(name)),
        }
    }

    /// Health checks reported by reloadable tool sources, for `meta.doctor`.
    pub fn source_checks(&self) -> Vec<SourceCheck> {
        let mut checks = Vec::new();
        for source in self.inner.registry.sources() {
            checks.extend(source.checks(&self.inner.paths, &self.inner.registry));
        }
        checks
    }

    /// Looks up a tool in the built-in registry first, then in reloadable sources.
    fn resolve_tool(&self, name: &str) -> Option<Arc<dyn Tool>> {
        if let Some(found) = self.inner.registry.get(name) {
            return Some(Arc::clone(found));
        }
        for source in self.inner.registry.sources() {
            if let Some(found) = source.get(&self.inner.paths, &self.inner.registry, name) {
                return Some(found);
            }
        }
        None
    }

    /// Answers a `hello`, refusing a client that speaks another protocol.
    pub fn hello(&self, params: &HelloParams) -> Result<HelloResult, ErrorBody> {
        if params.protocol != PROTOCOL {
            return Err(ErrorBody::new(
                ErrorCode::Unsupported,
                format!(
                    "This node speaks protocol {PROTOCOL}, and {} {} asked for protocol {}.",
                    params.client, params.version, params.protocol
                ),
            )
            .with_hint("Update legion on whichever side is older, so both speak the same protocol.")
            .with_machine(self.name())
            .with_details(serde_json::json!({"supported": [PROTOCOL]})));
        }
        let file = self.inner.config.get::<NodeFile>().ok();
        let system = file
            .as_ref()
            .and_then(|file| file.system.as_ref())
            .map(NodeName::to_string);
        // This system and every one it can restart into, as node.toml names them.
        let mut systems: Vec<String> = system.iter().cloned().collect();
        for target in file.iter().flat_map(|file| file.boot.keys()) {
            if !systems.contains(target) {
                systems.push(target.clone());
            }
        }
        let node_explicit = file.as_ref().map(|file| file.name.is_some());
        Ok(HelloResult {
            node: self.name(),
            node_explicit,
            node_id: self.node_id(),
            version: VERSION.to_owned(),
            build: BUILD.map(str::to_owned),
            protocol: PROTOCOL,
            os: std::env::consts::OS.to_owned(),
            arch: std::env::consts::ARCH.to_owned(),
            systems,
            system,
            boot_id: self.boot_id(),
        })
    }

    /// Persistent random identifier for this node's state directory (`<state_dir>/node-id`),
    /// created race-safely on first use. Returns `None` if the state directory is not writable.
    pub fn node_id(&self) -> Option<String> {
        if let Some(id) = self.inner.node_id.get() {
            return Some(id.clone());
        }
        let state_dir = self.inner.paths.state_dir();
        let id = load_or_create_node_id(state_dir)?;
        let _ = self.inner.node_id.set(id.clone());
        Some(id)
    }

    /// An identifier of the machine's current boot, which changes on every restart; `None` when
    /// the platform will not say.
    pub fn boot_id(&self) -> Option<String> {
        #[cfg(feature = "testing")]
        if let Some(file) = std::env::var_os("LEGION_TEST_BOOT_ID_FILE") {
            // Read on every call, so a test can change it under a running node.
            return std::fs::read_to_string(file)
                .ok()
                .map(|text| text.trim().to_owned());
        }
        legion_os::procid::boot_id().ok()
    }

    /// Runs one call on this node: decides who it acts as, checks that the policy allows it, runs
    /// the tool, and appends the call to the audit log, whatever came of it.
    ///
    /// `policy.toml` is read for every call, so an edit applies to the next one. A file that does
    /// not parse fails the call; nothing falls back to a default that could allow more.
    ///
    /// Cancelling `call.cancel` cancels the tool's context, and the call ends with what the tool
    /// returns within [`CANCEL_GRACE`], or as `cancelled` when it has not returned by then.
    pub async fn call(&self, call: Call) -> Result<Value, ErrorBody> {
        let started = Instant::now();
        let name = self.name();
        let recorded_args = audit::redact(&call.args);
        let tool = call.tool.clone();
        let door = call.door;
        let device = call.device.clone();

        let (principal, result) = self.run(&name, call).await;
        let result = result.map_err(|error| match error.machine {
            Some(_) => error,
            None => error.with_machine(name.clone()),
        });

        self.append_audit_entry(Entry {
            time: audit::now(),
            principal,
            door,
            device,
            tool,
            target: name,
            outcome: match &result {
                Ok(_) => Outcome::Ok,
                Err(error) => Outcome::Error(error.code),
            },
            duration: (started.elapsed().as_secs_f64() * 1e6).round() / 1e6,
            args: recorded_args,
            message: result
                .as_ref()
                .err()
                .map(|error| audit::redact_message(&error.message)),
        })
        .await;
        result
    }

    /// Appends one routed or forwarded call to this node's audit log, applying the same redaction
    /// and stderr fallback as [`Node::call`].
    #[allow(clippy::too_many_arguments)]
    pub async fn record_audit(
        &self,
        target: String,
        principal: Option<String>,
        door: Door,
        tool: String,
        args: &Value,
        started: Instant,
        result: &Result<Value, ErrorBody>,
    ) {
        self.record_audit_with_device(target, principal, door, None, tool, args, started, result)
            .await;
    }

    /// Appends one call to this node's audit log, optionally recording the paired `device` name.
    #[allow(clippy::too_many_arguments)]
    pub async fn record_audit_with_device(
        &self,
        target: String,
        principal: Option<String>,
        door: Door,
        device: Option<String>,
        tool: String,
        args: &Value,
        started: Instant,
        result: &Result<Value, ErrorBody>,
    ) {
        self.append_audit_entry(Entry {
            time: audit::now(),
            principal,
            door,
            device,
            tool,
            target,
            outcome: match result {
                Ok(_) => Outcome::Ok,
                Err(error) => Outcome::Error(error.code),
            },
            duration: (started.elapsed().as_secs_f64() * 1e6).round() / 1e6,
            args: audit::redact(args),
            message: result
                .as_ref()
                .err()
                .map(|error| audit::redact_message(&error.message)),
        })
        .await;
    }

    async fn append_audit_entry(&self, entry: Entry) {
        let log = self.inner.audit.clone();
        let written = tokio::task::spawn_blocking(move || log.append(&entry)).await;
        // stderr is for people; a call whose audit line is lost still happened, so it is reported
        // there rather than turned into a failure of the call.
        let problem = match written {
            Ok(Ok(())) => None,
            Ok(Err(error)) => Some(format!(
                "legion: the call ran, but it could not be written to the audit log {}: {error}\n",
                self.inner.audit.path().display()
            )),
            Err(error) => Some(format!("legion: writing the audit log failed: {error}\n")),
        };
        if let Some(problem) = problem {
            let _ = std::io::Write::write_all(&mut std::io::stderr(), problem.as_bytes());
        }
    }

    /// Everything [`Node::call`] does but the audit: the principal, once known, and the outcome.
    async fn run(&self, name: &str, call: Call) -> (Option<String>, Result<Value, ErrorBody>) {
        let policy = match self.inner.config.get::<Policy>() {
            Ok(policy) => policy,
            Err(error) => return (call.pinned, Err(error)),
        };
        let principal = match policy.resolve(call.as_principal.as_deref(), call.pinned.as_deref()) {
            Ok(principal) => principal,
            // Recorded as the caller it is, not as whoever it asked to be.
            Err(error) => {
                let caller = call
                    .pinned
                    .unwrap_or_else(|| policy.default_principal().to_owned());
                return (Some(caller), Err(error));
            }
        };
        let Some(tool) = self.resolve_tool(&call.tool) else {
            return (Some(principal), Err(self.unknown_tool(&call.tool, name)));
        };
        let grant = match policy.authorize(&principal, tool.spec(), &self.inner.paths) {
            Ok(grant) => grant,
            Err(error) => return (Some(principal), Err(error)),
        };
        if call.cancel.is_cancelled() {
            return (Some(principal), Err(cancelled(&call.tool)));
        }

        let ctx = Ctx {
            node: self.clone(),
            name: name.to_owned(),
            door: call.door,
            device: call.device,
            pairing_id: call.pairing_id,
            grant,
            cancel: call.cancel.clone(),
            events: call.events,
            hops: call.hops,
            credential_expires: call.credential_expires,
        };
        let work = tool.call(&ctx, call.args);
        tokio::pin!(work);
        let result = tokio::select! {
            result = &mut work => result,
            () = call.cancel.cancelled() => {
                // The tool sees the same token through its context. A result it had ready when the
                // cancel came, or returns after cleaning up, is the call's result.
                match tokio::time::timeout(CANCEL_GRACE, &mut work).await {
                    Ok(result) => result,
                    Err(_) => Err(cancelled(&call.tool)),
                }
            }
        };
        (Some(principal), result)
    }

    fn unknown_tool(&self, tool: &str, name: &str) -> ErrorBody {
        let family = tool.split('.').next().unwrap_or_default();
        let siblings: Vec<String> = self
            .specs()
            .into_iter()
            .map(|spec| spec.name)
            .filter(|known| known.split('.').next() == Some(family))
            .collect();
        let hint = if siblings.is_empty() {
            "`legion tools` lists the tools there are.".to_owned()
        } else {
            format!("The {family} tools here are {}.", siblings.join(", "))
        };
        ErrorBody::new(
            ErrorCode::UnknownTool,
            format!("There is no tool called {tool} on {name}."),
        )
        .with_hint(hint)
    }
}

fn cancelled(tool: &str) -> ErrorBody {
    ErrorBody::new(
        ErrorCode::Cancelled,
        format!("The call to {tool} was cancelled before it finished."),
    )
}

/// Persistent node identifier stored in `<state_dir>/node-id` (32 lower-case hex characters).
const NODE_ID_FILE: &str = "node-id";
const NODE_ID_LOCK_FILE: &str = "node-id.lock";
const NODE_ID_LOCK_TIMEOUT: Duration = Duration::from_secs(5);

fn valid_node_id(text: &str) -> bool {
    text.len() == 32
        && text
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn generate_node_id(state_dir: &std::path::Path) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"legion-node-id-v1:");
    for salt in 0u64..4 {
        let mut h = RandomState::new().build_hasher();
        h.write_u64(salt);
        hasher.update(&h.finish().to_le_bytes());
    }
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0u128, |d| d.as_nanos());
    hasher.update(&nanos.to_le_bytes());
    hasher.update(&std::process::id().to_le_bytes());
    hasher.update(state_dir.as_os_str().as_encoded_bytes());
    let hex = hasher.finalize().to_hex();
    hex.as_str()[..32].to_owned()
}

fn load_or_create_node_id(state_dir: &std::path::Path) -> Option<String> {
    let id_path = state_dir.join(NODE_ID_FILE);
    if let Ok(raw) = std::fs::read_to_string(&id_path) {
        let trimmed = raw.trim();
        if valid_node_id(trimmed) {
            return Some(trimmed.to_owned());
        }
    }
    std::fs::create_dir_all(state_dir).ok()?;
    let lock_path = state_dir.join(NODE_ID_LOCK_FILE);
    let _held =
        FileLock::acquire_timeout(&lock_path, LockMode::Exclusive, NODE_ID_LOCK_TIMEOUT).ok()?;
    if let Ok(raw) = std::fs::read_to_string(&id_path) {
        let trimmed = raw.trim();
        if valid_node_id(trimmed) {
            return Some(trimmed.to_owned());
        }
    }
    let generated = generate_node_id(state_dir);
    let content = format!("{generated}\n");
    write_atomically(&id_path, content.as_bytes()).ok()?;
    Some(generated)
}

#[cfg(test)]
mod tests {
    use legion_proto::HelloParams;

    use super::*;

    fn hello_params() -> HelloParams {
        HelloParams {
            client: "test".to_owned(),
            version: "0".to_owned(),
            protocol: PROTOCOL,
        }
    }

    #[test]
    fn hello_names_the_system_its_targets_and_the_boot() {
        let dir = tempfile::tempdir().unwrap();
        let node = Node::new(Paths::under(dir.path()), Registry::new());
        let bare = node.hello(&hello_params()).unwrap();
        assert_eq!(bare.node_explicit, Some(false));
        assert!(bare.node_id.as_deref().is_some_and(valid_node_id));
        let second_node = Node::new(Paths::under(dir.path()), Registry::new());
        assert_eq!(second_node.node_id(), bare.node_id);
        let other_dir = tempfile::tempdir().unwrap();
        let other_node = Node::new(Paths::under(other_dir.path()), Registry::new());
        assert_ne!(other_node.node_id(), bare.node_id);
        assert_eq!(bare.build.as_deref(), BUILD);
        assert_eq!(bare.system, None);
        assert!(bare.systems.is_empty());
        assert_eq!(bare.boot_id, node.boot_id());
        if cfg!(any(target_os = "linux", target_os = "macos", windows)) {
            assert!(bare.boot_id.is_some_and(|id| !id.is_empty()));
        }

        assert_eq!(format_version_line("0.1.0", None), "legion 0.1.0");
        assert_eq!(
            format_version_line("0.1.0", Some("a59ec0d12345")),
            "legion 0.1.0 (a59ec0d12345)"
        );
        assert_eq!(
            format_version_line("0.1.0", Some("a59ec0d12345, modified")),
            "legion 0.1.0 (a59ec0d12345, modified)"
        );

        std::fs::create_dir_all(dir.path().join("config")).unwrap();
        std::fs::write(
            dir.path().join("config").join("node.toml"),
            "system = \"linux\"\nbusy = \"none\"\n[boot.windows]\nefi_label = \"Windows Boot Manager\"\n[boot.linux]\narm = [\"a\"]\nverify = [\"v\"]\n",
        )
        .unwrap();
        let named = node.hello(&hello_params()).unwrap();
        assert_eq!(named.node_explicit, Some(false));
        assert_eq!(named.system.as_deref(), Some("linux"));
        assert_eq!(named.systems, ["linux", "windows"]);

        std::fs::write(
            dir.path().join("config").join("node.toml"),
            "name = \"studio\"\n",
        )
        .unwrap();
        let explicit = node.hello(&hello_params()).unwrap();
        assert_eq!(explicit.node, "studio");
        assert_eq!(explicit.node_explicit, Some(true));

        std::fs::write(dir.path().join("config").join("node.toml"), "name = 3\n").unwrap();
        let broken = node.hello(&hello_params()).unwrap();
        assert_eq!(broken.node_explicit, None);
    }
}
