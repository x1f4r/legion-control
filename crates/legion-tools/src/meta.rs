//! `meta`: which build this is, who the caller is, and whether the node is healthy.

mod doctor;
mod init;

use std::io;
use std::path::Path;
use std::time::Instant;

use legion_core::policy::OWNER;
use legion_core::proto::{Effect, ErrorBody, ErrorCode, PROTOCOL};
use legion_core::{BUILD, Ctx, Door, NoArgs, Registry, ToolError, VERSION};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::json;

pub use doctor::{
    FleetPingEntry, check_executable_on_path, check_installation, check_linux_lingering,
    check_policy_exec_roles, check_self_in_fleet, check_ssh_on_path, extract_login_path,
    fleet_ping_checks, parse_linger_output,
};
pub use init::{InitResult, render_node_toml, render_policy_toml};

/// Adds the `meta` tools.
pub fn register(registry: &mut Registry) {
    registry.add_fn(
        "meta.version",
        Effect::Read,
        "Say which build of legion this node runs, how it was installed, the protocol it speaks, and its operating system and architecture.",
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
        "Check that this node is healthy: its configuration directory can be read, its state directory exists and can be written to, every configuration file parses, how legion was installed, the executable directory and ssh are on PATH, Linux systemd lingering is enabled, non-owner roles that allow exec are flagged as a warning, this node's own name appears in fleet.toml when fleet.toml lists machines, and each machine in fleet.toml is checked (unless quick=true; unreachable machines and node name mismatches are reported as warnings while protocol or reply errors fail). Each check says what it found and, when something is wrong, what to do. Passing acknowledge=\"devices-rollback\" (owner role only) clears a recorded configuration rollback failure in the state directory and audits who acknowledged it; otherwise it writes nothing but a probe file in the state directory, which it removes.",
        doctor,
    );
    init::register(registry);
    crate::update::register(registry);
}

/// What `meta.version` returns.
#[derive(Debug, Serialize)]
pub struct Version {
    /// Always `legion`.
    pub name: &'static str,
    /// The version of legion.
    pub version: &'static str,
    /// The source revision of legion, when known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub build: Option<&'static str>,
    /// How this binary was installed (`deb`, `pacman`, or `standalone`).
    pub install: String,
    /// The protocol it speaks.
    pub protocol: u32,
    /// The node's name.
    pub node: String,
    /// Whether `node` was set explicitly in `node.toml` (`Some(true)`), derived from the host name
    /// (`Some(false)`), or unknown because `node.toml` failed to parse (`None`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node_explicit: Option<bool>,
    /// The operating system.
    pub os: &'static str,
    /// The CPU architecture.
    pub arch: &'static str,
}

async fn version(ctx: &Ctx, _args: NoArgs) -> Result<Version, ToolError> {
    let exe = legion_core::current_exe().ok();
    Ok(Version {
        name: "legion",
        version: VERSION,
        build: BUILD,
        install: crate::package::install_method(exe.as_deref()),
        protocol: PROTOCOL,
        node: ctx.node_name().to_owned(),
        node_explicit: ctx.node_name_explicit(),
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

/// Arguments of `meta.doctor`.
#[derive(Debug, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct DoctorArgs {
    /// Skip checking whether remote machines in fleet.toml answer.
    #[serde(default)]
    quick: bool,
    /// Acknowledge and clear a recorded health failure (owner role only). Pass
    /// `devices-rollback` after verifying policy.toml and devices.toml to clear
    /// `state/devices-rollback-error.json` and audit who acknowledged it.
    #[serde(default)]
    acknowledge: Option<String>,
}

/// What `meta.doctor` returns.
#[derive(Debug, Serialize)]
pub struct Doctor {
    /// Whether every check passed.
    pub ok: bool,
    /// The node that was checked.
    pub node: String,
    /// Recorded failure acknowledged during this call, when `acknowledge` was passed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub acknowledged: Option<String>,
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
    /// Advisory warning that does not make the node unhealthy on its own.
    #[serde(default, skip_serializing_if = "is_false")]
    pub warning: bool,
    /// What was found.
    pub detail: String,
    /// What to do about it, when something is wrong.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
}

fn is_false(value: &bool) -> bool {
    !*value
}

async fn doctor(ctx: &Ctx, args: DoctorArgs) -> Result<Doctor, ToolError> {
    let acknowledged = if let Some(target) = args.acknowledge {
        let started = Instant::now();
        if ctx.grant().role() != OWNER {
            return Err(ErrorBody::new(
                ErrorCode::Denied,
                "Only a caller with the owner role may acknowledge recorded health failures.",
            )
            .with_hint(
                "Run `legion meta.doctor acknowledge=devices-rollback` as the owner of this machine.",
            ));
        }
        if target != "devices-rollback" {
            return Err(ErrorBody::new(
                ErrorCode::InvalidArgs,
                format!("Unsupported acknowledge target {target:?}."),
            )
            .with_hint("Supported value: `acknowledge=devices-rollback`."));
        }
        let cleared = crate::device::registry::clear_rollback_failures(ctx.paths())?;
        let audit_args = json!({
            "acknowledge": "devices-rollback",
            "cleared_count": cleared,
        });
        ctx.node()
            .record_audit_with_device(
                ctx.node_name().to_owned(),
                Some(ctx.principal().to_owned()),
                ctx.door(),
                ctx.device().map(str::to_owned),
                "meta.doctor.acknowledge".to_owned(),
                &audit_args,
                started,
                &Ok(json!({})),
            )
            .await;
        Some("devices-rollback".to_owned())
    } else {
        None
    };

    let paths = ctx.paths();
    let mut checks = vec![
        directory_check("configuration directory", paths.config_dir(), Use::Read),
        directory_check("state directory", paths.state_dir(), Use::Write),
    ];
    for document in ctx.documents() {
        let file = paths.config_file(document.file());
        let check = match document.check(ctx.config()) {
            Ok(()) if file.exists() => Check {
                name: document.file().to_owned(),
                ok: true,
                warning: false,
                detail: format!("{} parses.", file.display()),
                hint: None,
            },
            Ok(()) => Check {
                name: document.file().to_owned(),
                ok: true,
                warning: false,
                detail: format!("{} does not exist, so its defaults apply.", file.display()),
                hint: None,
            },
            Err(error) => Check {
                name: document.file().to_owned(),
                ok: false,
                warning: false,
                detail: error.message,
                hint: error.hint,
            },
        };
        checks.push(check);
    }
    for source in ctx.source_checks() {
        checks.push(Check {
            name: source.name,
            ok: source.ok,
            warning: false,
            detail: source.detail,
            hint: source.hint,
        });
    }
    doctor::append_extra_checks(ctx, args.quick, &mut checks).await;
    Ok(Doctor {
        ok: checks.iter().all(|check| check.ok),
        node: ctx.node_name().to_owned(),
        acknowledged,
        checks,
    })
}

/// What the node does with a directory, and so what its check asks of it.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Use {
    /// Only read: the configuration directory, which may be missing, since every file then takes
    /// its default, and which an owner may keep read-only, so nothing is written to it to check.
    Read,
    /// Written: the state directory, which must be there, since every call writes its audit line
    /// into it.
    Write,
}

/// Whether a directory is there and does what the node needs of it.
fn directory_check(name: &str, dir: &Path, needed: Use) -> Check {
    let required = needed == Use::Write;
    let check = |ok: bool, detail: String, hint: Option<&str>| Check {
        name: name.to_owned(),
        ok,
        warning: false,
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
        Ok(_) if needed == Use::Read => match std::fs::read_dir(dir) {
            Ok(_) => check(
                true,
                format!("{} exists and is readable.", dir.display()),
                None,
            ),
            Err(error) => check(
                false,
                format!("{} exists but cannot be read: {error}.", dir.display()),
                Some("Give the account legion runs as read access to it."),
            ),
        },
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
/// platform. Its name is random and taken only if free, so concurrent checks never meet, and it is
/// removed whether or not the check passes.
fn probe_write(dir: &Path) -> io::Result<()> {
    tempfile::Builder::new()
        .prefix(".legion-doctor-")
        .tempfile_in(dir)?
        .close()
}
