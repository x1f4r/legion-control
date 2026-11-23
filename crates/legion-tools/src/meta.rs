//! `meta`: which build this is, who the caller is, and whether the node is healthy.

use std::fs::OpenOptions;
use std::io;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use legion_core::proto::{Effect, PROTOCOL};
use legion_core::{Ctx, Door, NoArgs, Registry, ToolError, VERSION};
use serde::Serialize;

/// Adds the `meta` tools.
pub fn register(registry: &mut Registry) {
    registry.add_fn(
        "meta.version",
        Effect::Read,
        "Say which build of legion this node runs, the protocol it speaks, and its operating system and architecture.",
        version,
    );
    registry.add_fn(
        "meta.whoami",
        Effect::Read,
        "Say who the caller is on this node: the principal and role it acts as, the door it came through, the directories file tools are confined to, and the tools it may use.",
        whoami,
    );
    registry.add_fn(
        "meta.doctor",
        Effect::Read,
        "Check that this node is healthy: its configuration and state directories exist and are writable, and every configuration file parses. Each check says what it found and, when something is wrong, what to do.",
        doctor,
    );
}

/// What `meta.version` returns.
#[derive(Debug, Serialize)]
pub struct Version {
    /// Always `legion`.
    pub name: &'static str,
    /// The version of legion.
    pub version: &'static str,
    /// The protocol it speaks.
    pub protocol: u32,
    /// The node's name.
    pub node: String,
    /// The operating system.
    pub os: &'static str,
    /// The CPU architecture.
    pub arch: &'static str,
}

async fn version(ctx: &Ctx, _args: NoArgs) -> Result<Version, ToolError> {
    Ok(Version {
        name: "legion",
        version: VERSION,
        protocol: PROTOCOL,
        node: ctx.node_name().to_owned(),
        os: ctx.os(),
        arch: ctx.arch(),
    })
}

/// What `meta.whoami` returns.
#[derive(Debug, Serialize)]
pub struct WhoAmI {
    /// The principal the call acted as.
    pub principal: String,
    /// Its role.
    pub role: String,
    /// The door the call came through.
    pub door: Door,
    /// The node that answered.
    pub node: String,
    /// Whether the principal may act as someone else.
    pub delegate: bool,
    /// The directories file tools are confined to, or `null` when they are not.
    pub paths: Option<Vec<String>>,
    /// The tools this principal may call here, sorted.
    pub tools: Vec<String>,
}

async fn whoami(ctx: &Ctx, _args: NoArgs) -> Result<WhoAmI, ToolError> {
    let grant = ctx.grant();
    Ok(WhoAmI {
        principal: grant.principal().to_owned(),
        role: grant.role().to_owned(),
        door: ctx.door(),
        node: ctx.node_name().to_owned(),
        delegate: grant.delegates(),
        paths: grant.roots(),
        tools: ctx
            .tools()
            .into_iter()
            .filter(|spec| grant.allows(spec))
            .map(|spec| spec.name)
            .collect(),
    })
}

/// What `meta.doctor` returns.
#[derive(Debug, Serialize)]
pub struct Doctor {
    /// Whether every check passed.
    pub ok: bool,
    /// The node that was checked.
    pub node: String,
    /// Each check, in the order it ran.
    pub checks: Vec<Check>,
}

/// One thing `meta.doctor` looked at.
#[derive(Debug, Serialize)]
pub struct Check {
    /// What was checked.
    pub name: String,
    /// Whether it is as it should be.
    pub ok: bool,
    /// What was found.
    pub detail: String,
    /// What to do about it, when something is wrong.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
}

async fn doctor(ctx: &Ctx, _args: NoArgs) -> Result<Doctor, ToolError> {
    let paths = ctx.paths();
    let mut checks = vec![
        directory_check("configuration directory", paths.config_dir(), false),
        directory_check("state directory", paths.state_dir(), true),
    ];
    for document in ctx.documents() {
        let file = paths.config_file(document.file());
        let check = match document.check(ctx.config()) {
            Ok(()) if file.exists() => Check {
                name: document.file().to_owned(),
                ok: true,
                detail: format!("{} parses.", file.display()),
                hint: None,
            },
            Ok(()) => Check {
                name: document.file().to_owned(),
                ok: true,
                detail: format!("{} does not exist, so its defaults apply.", file.display()),
                hint: None,
            },
            Err(error) => Check {
                name: document.file().to_owned(),
                ok: false,
                detail: error.message,
                hint: error.hint,
            },
        };
        checks.push(check);
    }
    Ok(Doctor {
        ok: checks.iter().all(|check| check.ok),
        node: ctx.node_name().to_owned(),
        checks,
    })
}

/// Whether a directory exists and can be written to. The configuration directory may be missing,
/// since every file then takes its default; the state directory must be there, since every call
/// writes its audit line into it.
fn directory_check(name: &str, dir: &Path, required: bool) -> Check {
    let check = |ok: bool, detail: String, hint: Option<&str>| Check {
        name: name.to_owned(),
        ok,
        detail,
        hint: hint.map(str::to_owned),
    };
    match std::fs::metadata(dir) {
        Err(error) if error.kind() == io::ErrorKind::NotFound && !required => check(
            true,
            format!(
                "{} does not exist, so every configuration file takes its default.",
                dir.display()
            ),
            None,
        ),
        Err(error) if error.kind() == io::ErrorKind::NotFound => check(
            false,
            format!("{} does not exist.", dir.display()),
            Some("Create it, or check that legion may create directories there."),
        ),
        Err(error) => check(
            false,
            format!("{} cannot be read: {error}.", dir.display()),
            Some("Give the account legion runs as access to it."),
        ),
        Ok(meta) if !meta.is_dir() => check(
            false,
            format!("{} is not a directory.", dir.display()),
            Some("Move the file that is in its place."),
        ),
        Ok(_) => match probe_write(dir) {
            Ok(()) => check(
                true,
                format!("{} exists and is writable.", dir.display()),
                None,
            ),
            Err(error) => check(
                false,
                format!(
                    "{} exists but cannot be written to: {error}.",
                    dir.display()
                ),
                Some("Give the account legion runs as write access to it."),
            ),
        },
    }
}

/// Creates and removes an empty file, which is the one way to know a directory is writable on every
/// platform. It leaves nothing behind.
fn probe_write(dir: &Path) -> io::Result<()> {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.subsec_nanos());
    let probe = dir.join(format!(".legion-doctor-{}-{nanos}", std::process::id()));
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&probe)?;
    std::fs::remove_file(&probe)
}
