//! `meta.init`: idempotent first-run setup creating `node.toml` and `policy.toml` only when missing.

use std::io;
use std::path::Path;
use std::time::Duration;

use legion_core::config::{Document, NodeFile, NodeName, parse, write_atomically};
use legion_core::policy::Policy;
use legion_core::proto::{Effect, ErrorBody, ErrorCode};
use legion_core::{Ctx, Registry, ToolError};
use legion_os::OsError;
use legion_os::lock::{FileLock, LockMode};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// How long `meta.init` waits if another call is initialising this node at the same time.
const LOCK_TIMEOUT: Duration = Duration::from_secs(10);

/// Adds `meta.init` to `registry`.
pub(super) fn register(registry: &mut Registry) {
    registry.add_fn(
        "meta.init",
        Effect::Admin,
        "Create starter node.toml and policy.toml files where they do not already exist, leaving every existing file untouched so it is safe to run again.",
        init,
    );
}

/// Arguments of `meta.init`.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct InitArgs {
    /// Node name to write into node.toml when creating it; defaults to the name derived from this machine's host name.
    #[serde(default)]
    name: Option<String>,
    /// Create policy.toml with ready-made observer, editor and operator roles when missing.
    #[serde(default = "default_true")]
    roles: bool,
    /// Report what would be created or kept without writing any files.
    #[serde(default)]
    dry_run: bool,
}

fn default_true() -> bool {
    true
}

/// What `meta.init` returns.
#[derive(Debug, Serialize)]
pub struct InitResult {
    /// The node's name (existing when node.toml was kept, or chosen when created).
    pub node: String,
    /// Configuration files created (or that would be created under `dry_run`).
    pub created: Vec<String>,
    /// Existing configuration files kept untouched.
    pub kept: Vec<String>,
    /// Plain-language next steps after initialising the node.
    pub next: Vec<String>,
}

async fn init(ctx: &Ctx, args: InitArgs) -> Result<InitResult, ToolError> {
    let requested_name = match args.name {
        Some(raw) => Some(NodeName::try_from(raw).map_err(|message| {
            ErrorBody::new(ErrorCode::InvalidArgs, format!("name: {message}.")).with_hint(
                "Use lower-case letters, digits, - and _, starting with a letter or digit.",
            )
        })?),
        None => None,
    };

    let current_node_name = ctx.node_name().to_owned();
    let config_dir = ctx.paths().config_dir().to_path_buf();
    let lock_path = ctx.paths().state_dir().join("init.lock");
    let roles = args.roles;
    let dry_run = args.dry_run;

    tokio::task::spawn_blocking(move || {
        run_init(
            &config_dir,
            &lock_path,
            &current_node_name,
            requested_name,
            roles,
            dry_run,
        )
    })
    .await
    .map_err(|error| {
        ErrorBody::new(
            ErrorCode::Internal,
            format!("meta.init stopped unexpectedly: {error}."),
        )
    })?
}

fn run_init(
    config_dir: &Path,
    lock_path: &Path,
    current_node_name: &str,
    requested_name: Option<NodeName>,
    roles: bool,
    dry_run: bool,
) -> Result<InitResult, ErrorBody> {
    let failed = |what: String, error: &dyn std::fmt::Display| {
        ErrorBody::new(ErrorCode::Failed, format!("{what}: {error}."))
    };

    // Hold a lock only when writing so dry_run never touches the state directory either.
    let _held = if !dry_run {
        if let Some(dir) = lock_path.parent() {
            std::fs::create_dir_all(dir)
                .map_err(|error| failed(format!("{} cannot be created", dir.display()), &error))?;
        }
        Some(
            FileLock::acquire_timeout(lock_path, LockMode::Exclusive, LOCK_TIMEOUT).map_err(
                |error| match error {
                    OsError::TimedOut { .. } => ErrorBody::new(
                        ErrorCode::Busy,
                        "Another call has been initialising this node for too long.",
                    )
                    .with_hint("Try again in a moment."),
                    other => failed(format!("{} cannot be locked", lock_path.display()), &other),
                },
            )?,
        )
    } else {
        None
    };

    let mut created = Vec::new();
    let mut kept = Vec::new();

    let node_path = config_dir.join(NodeFile::FILE);
    let node_exists = path_exists(&node_path)?;
    let chosen_name = requested_name
        .as_ref()
        .map_or(current_node_name, NodeName::as_str);

    let effective_node = if node_exists {
        kept.push(display_path(&node_path));
        current_node_name.to_owned()
    } else {
        let text = render_node_toml(chosen_name);
        parse::<NodeFile>(&node_path, text.as_bytes())?;
        if !dry_run {
            std::fs::create_dir_all(config_dir).map_err(|error| {
                failed(
                    format!("{} cannot be created", config_dir.display()),
                    &error,
                )
            })?;
            write_atomically(&node_path, text.as_bytes()).map_err(|error| {
                failed(format!("{} cannot be written", node_path.display()), &error)
            })?;
        }
        created.push(display_path(&node_path));
        chosen_name.to_owned()
    };

    if roles {
        let policy_path = config_dir.join(Policy::FILE);
        if path_exists(&policy_path)? {
            kept.push(display_path(&policy_path));
        } else {
            let text = render_policy_toml();
            parse::<Policy>(&policy_path, text.as_bytes())?;
            if !dry_run {
                std::fs::create_dir_all(config_dir).map_err(|error| {
                    failed(
                        format!("{} cannot be created", config_dir.display()),
                        &error,
                    )
                })?;
                write_atomically(&policy_path, text.as_bytes()).map_err(|error| {
                    failed(
                        format!("{} cannot be written", policy_path.display()),
                        &error,
                    )
                })?;
            }
            created.push(display_path(&policy_path));
        }
    }

    Ok(InitResult {
        node: effective_node,
        created,
        kept,
        next: next_steps(),
    })
}

/// Checks whether a file or symlink is already at `path` so a broken symlink is never clobbered.
fn path_exists(path: &Path) -> Result<bool, ErrorBody> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(ErrorBody::new(
            ErrorCode::Failed,
            format!("{} cannot be inspected: {error}.", path.display()),
        )),
    }
}

fn display_path(path: &Path) -> String {
    path.display().to_string()
}

/// Starter `node.toml` content with the chosen node name and commented examples.
pub fn render_node_toml(name: &str) -> String {
    format!(
        "\
# Configuration for this Legion node. This file belongs to this machine and is not synced.
name = \"{name}\"

# Busy detection protects running work from unattended power changes (sleep, reboot, shutdown).
# Without any busy probes configured, a node is \"unmonitored\" rather than idle, so unattended
# power changes are blocked until you configure probes or declare explicitly that this machine
# has no work worth protecting:
# busy = \"none\"

# Allow system-scope service control over non-interactive sudo on Unix (needs narrow sudoers rules):
# [svc]
# sudo = true

# Power and dual-boot targets for this machine:
# [power]
# sleep = [\"systemctl\", \"suspend\"]

# Extra host names and browser origins accepted by `legion serve` (for example behind Tailscale Serve):
# [serve]
# hosts = [\"tower.example.ts.net\"]
# origins = [\"https://app.example.net\"]
"
    )
}

/// Starter `policy.toml` content preserving `default = "owner"` and defining the three ready-made roles.
pub fn render_policy_toml() -> &'static str {
    "\
# Who may call which Legion tools on this machine.
# Keeping default = \"owner\" preserves local shell behaviour until you assign narrower principals.
default = \"owner\"

# Observer: may look without changing anything. Besides system, service, job, and project status
# and files under `paths` (never Legion's audit log), it can see every process's command line
# (with secret-looking values replaced), service logs, every principal's job commands and output,
# the fleet's layout, and the definitions of extension tools, and it can run any extension tool
# declared `read`. This is a real boundary when the caller arrives through a door that pins its
# principal (an ssh key forced to `legion gate`, or an HTTP token).
[roles.observer]
allow = [\"effect:read\"]
deny = [\"audit.*\"]
paths = [\"~/projects/**\"]

# Editor: may read everything `observer` sees and change files inside `paths` (plus run any
# extension tool declared `read` or `write`), without altering project registrations (`proj.add`,
# `proj.remove`), sending files across machines (`fs.transfer`), or reading Legion's audit log.
# This is a real boundary through the same pinned doors (`legion gate` or an HTTP token); because
# it has no `exec`, it cannot run commands, project tasks, or tests.
[roles.editor]
allow = [\"effect:read\", \"effect:write\"]
deny = [\"audit.*\", \"proj.add\", \"proj.remove\", \"fs.transfer\"]
paths = [\"~/projects/**\"]

# Operator: may do everything except power and administration.
# Because it can run commands (`effect:exec`), it is as strong as your account and could change
# Legion's own files (including policy.toml and tools.d/) through a shell; the denials are a guard
# rail for a cooperating agent and a clear audit trail, not a wall. Give it only to agents you
# would hand a terminal.
[roles.operator]
allow = [\"effect:read\", \"effect:write\", \"effect:exec\", \"effect:destroy\"]
deny = [\"effect:power\", \"effect:admin\"]

# Assign principals to roles. Uncomment and adjust before registering an agent or issuing a token:
# [principals.\"agent:claude\"]
# role = \"editor\"
#
# [principals.phone]
# role = \"observer\"
"
}

fn next_steps() -> Vec<String> {
    vec![
        "Edit policy.toml to assign principals to a role (`observer`, `editor`, or `operator`) before connecting an AI agent or remote device.".to_owned(),
        "Add your other machines to fleet.toml with `legion fleet.discover` (or `legion fleet.discover add=true`, or `legion fleet.add`).".to_owned(),
        "Register an AI coding agent with `legion agents add claude --as agent:claude` (also supports codex, antigravity, and opencode).".to_owned(),
        "Check that everything is configured and reachable with `legion meta.doctor`.".to_owned(),
    ]
}

#[cfg(test)]
mod tests {
    use legion_core::{Access, Paths};
    use legion_proto::ToolSpec;
    use serde_json::json;

    use super::*;

    fn spec(name: &str, effect: Effect) -> ToolSpec {
        ToolSpec {
            name: name.to_owned(),
            description: String::new(),
            effect,
            input_schema: json!({"type": "object"}),
        }
    }

    #[test]
    fn templates_parse_strictly_and_roles_behave_as_documented() {
        let node = parse::<NodeFile>(Path::new("node.toml"), render_node_toml("tower").as_bytes())
            .unwrap();
        assert_eq!(node.name_or_host().as_str(), "tower");

        let mut policy_text = render_policy_toml().to_owned();
        let base_policy =
            parse::<Policy>(Path::new("policy.toml"), policy_text.as_bytes()).unwrap();
        assert_eq!(base_policy.default_principal(), "owner");
        assert!(base_policy.roles.contains_key("observer"));
        assert!(base_policy.roles.contains_key("editor"));
        assert!(base_policy.roles.contains_key("operator"));
        assert!(
            !base_policy.roles.contains_key("maintainer"),
            "maintainer role must not be generated"
        );
        assert!(
            policy_text.contains("as strong as your account") && policy_text.contains("not a wall"),
            "operator comment must state plainly that exec is as strong as the account"
        );
        for required_observer_fact in [
            "every process's command line",
            "service logs",
            "every principal's job commands and output",
            "the fleet's layout",
            "definitions of extension tools",
            "declared `read`",
        ] {
            assert!(
                policy_text.contains(required_observer_fact),
                "observer comment missing {required_observer_fact:?}"
            );
        }

        policy_text.push_str(
            "\n[principals.viewer]\nrole = \"observer\"\n\
             [principals.writer]\nrole = \"editor\"\n\
             [principals.bot]\nrole = \"operator\"\n",
        );
        let policy = parse::<Policy>(Path::new("policy.toml"), policy_text.as_bytes()).unwrap();

        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        std::fs::create_dir_all(home.join("projects").join("demo")).unwrap();
        std::fs::create_dir_all(home.join("private")).unwrap();
        let paths = Paths::under(dir.path().join("legion")).with_home(&home);

        // Observer: reads allowed and confined to ~/projects/**, audit log and writes denied.
        let obs_grant = policy
            .authorize("viewer", &spec("fs.read", Effect::Read), &paths)
            .unwrap();
        assert!(
            obs_grant
                .check_path("~/projects/demo/a.txt", Access::Read)
                .is_ok()
        );
        assert!(
            obs_grant
                .check_path("~/private/secret.txt", Access::Read)
                .is_err(),
            "observer must be confined to ~/projects/**"
        );
        assert!(
            policy
                .authorize("viewer", &spec("audit.tail", Effect::Read), &paths)
                .is_err()
        );
        assert!(
            policy
                .authorize("viewer", &spec("fs.write", Effect::Write), &paths)
                .is_err()
        );

        // Editor: read and write confined to ~/projects/**, no exec, destroy, power, or admin.
        let ed_grant = policy
            .authorize("writer", &spec("fs.write", Effect::Write), &paths)
            .unwrap();
        assert!(
            ed_grant
                .check_path("~/projects/demo/a.txt", Access::Write)
                .is_ok()
        );
        assert!(
            ed_grant
                .check_path("~/private/secret.txt", Access::Read)
                .is_err()
        );
        for (name, effect) in [
            ("audit.tail", Effect::Read),
            ("proj.add", Effect::Write),
            ("proj.remove", Effect::Write),
            ("fs.transfer", Effect::Write),
            ("exec.run", Effect::Exec),
            ("fs.remove", Effect::Destroy),
            ("power.reboot", Effect::Power),
            ("token.add", Effect::Admin),
        ] {
            assert!(
                policy
                    .authorize("writer", &spec(name, effect), &paths)
                    .is_err(),
                "editor must deny {name}"
            );
        }

        // Operator: read/write/exec/destroy allowed, power and admin denied.
        for (name, effect) in [
            ("fs.read", Effect::Read),
            ("fs.write", Effect::Write),
            ("exec.run", Effect::Exec),
            ("fs.remove", Effect::Destroy),
            ("audit.tail", Effect::Read),
        ] {
            assert!(
                policy.authorize("bot", &spec(name, effect), &paths).is_ok(),
                "operator should allow {name}"
            );
        }
        assert!(
            policy
                .authorize("bot", &spec("power.reboot", Effect::Power), &paths)
                .is_err()
        );
        assert!(
            policy
                .authorize("bot", &spec("token.add", Effect::Admin), &paths)
                .is_err()
        );
        assert!(
            policy
                .authorize("bot", &spec("fleet.sync", Effect::Admin), &paths)
                .is_err()
        );
    }

    #[test]
    fn run_init_is_idempotent_and_honours_dry_run_and_roles_flag() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("config");
        let lock = dir.path().join("state").join("init.lock");

        let dry = run_init(
            &config,
            &lock,
            "laptop",
            Some(NodeName::try_from("tower".to_owned()).unwrap()),
            true,
            true,
        )
        .unwrap();
        assert_eq!(dry.node, "tower");
        assert_eq!(dry.created.len(), 2);
        assert!(dry.kept.is_empty());
        assert!(!config.exists());

        let first = run_init(
            &config,
            &lock,
            "laptop",
            Some(NodeName::try_from("tower".to_owned()).unwrap()),
            true,
            false,
        )
        .unwrap();
        assert_eq!(first.node, "tower");
        assert_eq!(first.created.len(), 2);
        assert!(first.kept.is_empty());

        let custom_node = "# custom\nname = \"tower\"\n";
        std::fs::write(config.join("node.toml"), custom_node).unwrap();
        let policy_before = std::fs::read(config.join("policy.toml")).unwrap();

        let second = run_init(&config, &lock, "tower", None, true, false).unwrap();
        assert!(second.created.is_empty());
        assert_eq!(second.kept.len(), 2);
        assert_eq!(
            std::fs::read_to_string(config.join("node.toml")).unwrap(),
            custom_node
        );
        assert_eq!(
            std::fs::read(config.join("policy.toml")).unwrap(),
            policy_before
        );

        // roles = false skips policy.toml when missing.
        let dir2 = tempfile::tempdir().unwrap();
        let no_roles = run_init(
            &dir2.path().join("config"),
            &dir2.path().join("state").join("init.lock"),
            "pi",
            None,
            false,
            false,
        )
        .unwrap();
        assert_eq!(no_roles.created.len(), 1);
        assert!(!dir2.path().join("config").join("policy.toml").exists());
    }
}
