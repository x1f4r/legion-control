//! `agent`: register, list, and remove the Legion MCP door in AI coding agents.
//!
//! Every operation delegates to each agent's own command-line program (`claude`, `codex`, `agy`,
//! `opencode`) with an explicit argument vector and timeout, and never reads or edits any agent's
//! configuration files directly.

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use legion_core::policy::{OWNER, Policy, Principal};
use legion_core::proto::{Effect, ErrorBody, ErrorCode};
use legion_core::{Ctx, NoArgs, Registry, ToolError};
use legion_os::OsError;
use legion_os::run::{RunOptions, run};
use legion_os::shell::CommandSpec;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tokio::task::JoinSet;

/// Name under which Legion registers its MCP server in every supported agent.
pub const SERVER_NAME: &str = "legion";

/// Bound for querying an agent's `mcp list` output.
const LIST_TIMEOUT: Duration = Duration::from_secs(20);

/// Bound for running an agent's `mcp add` or `mcp remove` command.
const MUTATE_TIMEOUT: Duration = Duration::from_secs(30);

/// Supported AI coding agents whose own CLIs manage MCP server registrations.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Deserialize, Serialize, JsonSchema,
)]
#[serde(rename_all = "lowercase")]
pub enum AgentKind {
    /// Claude Code (`claude`).
    Claude,
    /// OpenAI Codex CLI (`codex`).
    Codex,
    /// Antigravity CLI (`agy`).
    Antigravity,
    /// OpenCode (`opencode`).
    Opencode,
}

impl AgentKind {
    /// All known agents in display order.
    pub const ALL: [AgentKind; 4] = [
        AgentKind::Claude,
        AgentKind::Codex,
        AgentKind::Antigravity,
        AgentKind::Opencode,
    ];

    /// Canonical name accepted by `agent.add` and `agent.remove`.
    pub const fn as_str(self) -> &'static str {
        match self {
            AgentKind::Claude => "claude",
            AgentKind::Codex => "codex",
            AgentKind::Antigravity => "antigravity",
            AgentKind::Opencode => "opencode",
        }
    }

    /// Executable name searched on `PATH`.
    pub const fn program(self) -> &'static str {
        match self {
            AgentKind::Claude => "claude",
            AgentKind::Codex => "codex",
            AgentKind::Antigravity => "agy",
            AgentKind::Opencode => "opencode",
        }
    }

    /// Human-friendly hint shown when the agent's CLI is not on `PATH`.
    const fn install_hint(self) -> &'static str {
        match self {
            AgentKind::Claude => "Install Claude Code so `claude` is on PATH, then try again.",
            AgentKind::Codex => "Install the Codex CLI so `codex` is on PATH, then try again.",
            AgentKind::Antigravity => {
                "Install the Antigravity CLI so `agy` is on PATH, then try again."
            }
            AgentKind::Opencode => "Install OpenCode so `opencode` is on PATH, then try again.",
        }
    }
}

/// Adds the `agent` tools to `registry`.
pub fn register(registry: &mut Registry) {
    registry.add_fn(
        "agent.list",
        Effect::Admin,
        "Check each known AI coding agent (claude, codex, antigravity, opencode) to see whether its CLI is on PATH and whether an MCP server named `legion` is registered via its own `mcp list` command.",
        list,
    );
    registry.add_fn(
        "agent.add",
        Effect::Admin,
        "Register this `legion` binary as an MCP server named `legion` with an AI coding agent using the agent's own CLI command, never by editing its configuration files directly. `compact` defaults to true because every registered tool schema costs the agent context on every conversation; pass `compact=false` to expose every tool individually. A named, limited principal from policy.toml (such as `agent:claude` with the `operator` or `observer` role) is safer than `owner` because it narrows what the agent may do and identifies its calls in the audit log; registering as `owner` requires `allow_owner=true`.",
        add,
    );
    registry.add_fn(
        "agent.remove",
        Effect::Admin,
        "Remove the `legion` MCP server registration from an AI coding agent using the agent's own CLI command, never by editing its configuration files directly.",
        remove,
    );
}

/// Whether `legion` is registered in an agent, parsed defensively from `mcp list`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Registered {
    /// A server entry named `legion` was clearly present in `mcp list`.
    Yes,
    /// `mcp list` clearly reported servers (or none) and `legion` was not among them.
    No,
    /// The agent is not installed, `mcp list` failed or timed out, or its output format was not recognised.
    Unknown,
}

/// What `agent.list` returns.
#[derive(Debug, Serialize)]
pub struct ListResult {
    /// Status of each known AI coding agent.
    pub agents: Vec<AgentStatus>,
}

/// Installation and registration status of one known agent.
#[derive(Debug, Serialize)]
pub struct AgentStatus {
    /// Agent identifier (`claude`, `codex`, `antigravity`, `opencode`).
    pub agent: AgentKind,
    /// CLI program name looked up on `PATH`.
    pub program: &'static str,
    /// Whether the program was found on `PATH`.
    pub installed: bool,
    /// Resolved executable path when found on `PATH`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// Whether an MCP server named `legion` is registered (`yes`, `no`, or `unknown`).
    pub registered: Registered,
    /// Explanation when registration status is `unknown`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

async fn list(ctx: &Ctx, _args: NoArgs) -> Result<ListResult, ToolError> {
    let mut set = JoinSet::new();
    for (index, agent) in AgentKind::ALL.into_iter().enumerate() {
        let cancel = ctx.cancel_token().child_token();
        set.spawn(async move {
            let status = inspect_agent(agent, cancel).await;
            (index, status)
        });
    }

    let mut ordered = BTreeMap::new();
    while let Some(joined) = set.join_next().await {
        if let Ok((index, status)) = joined {
            ordered.insert(index, status);
        }
    }
    if ctx.is_cancelled() {
        return Err(ErrorBody::new(
            ErrorCode::Cancelled,
            "The call to agent.list was cancelled.",
        ));
    }

    Ok(ListResult {
        agents: ordered.into_values().collect(),
    })
}

async fn inspect_agent(agent: AgentKind, cancel: legion_core::CancellationToken) -> AgentStatus {
    let found = find_program_on_env_path(agent.program());
    let Some(path) = found else {
        return AgentStatus {
            agent,
            program: agent.program(),
            installed: false,
            path: None,
            registered: Registered::Unknown,
            detail: None,
        };
    };

    let (registered, detail) = query_registered(agent, &path, cancel).await;
    AgentStatus {
        agent,
        program: agent.program(),
        installed: true,
        path: Some(path.display().to_string()),
        registered,
        detail,
    }
}

async fn query_registered(
    agent: AgentKind,
    program_path: &Path,
    cancel: legion_core::CancellationToken,
) -> (Registered, Option<String>) {
    let argv = build_list_command(agent);
    let spec = CommandSpec::argv(program_path.as_os_str(), &argv[1..]);
    let options = RunOptions::new(spec)
        .c_locale()
        .timeout(LIST_TIMEOUT)
        .cancel(cancel);
    match run(options).await {
        Ok(output) if output.cancelled => (Registered::Unknown, Some("Cancelled.".to_owned())),
        Ok(output) if output.timed_out => (
            Registered::Unknown,
            Some(format!(
                "`{} mcp list` timed out after {}s.",
                agent.program(),
                LIST_TIMEOUT.as_secs()
            )),
        ),
        Ok(output) if !output.success() => {
            let stderr = output.stderr.text.trim();
            let detail = if stderr.is_empty() {
                format!(
                    "`{} mcp list` exited with {:?}.",
                    agent.program(),
                    output.exit_code
                )
            } else {
                format!("`{} mcp list` failed: {stderr}", agent.program())
            };
            (Registered::Unknown, Some(detail))
        }
        Ok(output) => {
            let state = parse_mcp_list(agent, &output.stdout.text);
            let detail = (state == Registered::Unknown).then(|| {
                format!(
                    "Could not recognise `{}` output from `{} mcp list`.",
                    SERVER_NAME,
                    agent.program()
                )
            });
            (state, detail)
        }
        Err(error) => (
            Registered::Unknown,
            Some(format!(
                "Could not run `{} mcp list`: {error}.",
                agent.program()
            )),
        ),
    }
}

/// Arguments of `agent.add`.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct AddArgs {
    /// Which AI coding agent to register Legion with (`claude`, `codex`, `antigravity`, `opencode`).
    agent: AgentKind,
    /// Principal every MCP call from this agent acts as. Must be named in policy.toml, or `owner`
    /// with `allow_owner=true`. Prefer a named, limited principal so the agent is confined by its
    /// role and clearly identified in the audit log.
    #[serde(rename = "as")]
    as_principal: String,
    /// Pass `--compact` to `legion mcp` (exposes three meta-tools instead of one per registered
    /// tool). Defaults to true because every registered tool schema costs context on every
    /// conversation.
    #[serde(default = "default_true")]
    compact: bool,
    /// Scope for agents that support one (`local`, `user`, or `project` for `claude`, default
    /// `user`; `global`/`user` or `project`/`local` for `opencode`, default `global`).
    #[serde(default)]
    scope: Option<String>,
    /// Allow registering the agent as `owner`, which grants unrestricted access. Off unless true.
    #[serde(default)]
    allow_owner: bool,
    /// Return the exact registration command without running it.
    #[serde(default)]
    dry_run: bool,
}

fn default_true() -> bool {
    true
}

/// What `agent.add` returns.
#[derive(Debug, Serialize)]
pub struct AddResult {
    /// Agent that was configured.
    pub agent: AgentKind,
    /// Exact argument vector built (and executed unless `dry_run` was true).
    pub command: Vec<String>,
    /// Whether the registration command was executed and succeeded.
    pub registered: bool,
    /// Set to `true` when an existing `legion` registration was replaced.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub replaced: Option<bool>,
}

async fn add(ctx: &Ctx, args: AddArgs) -> Result<AddResult, ToolError> {
    let principal = Principal::parse(&args.as_principal)?;
    let principal = principal.as_str().to_owned();
    if principal == OWNER {
        if !args.allow_owner {
            return Err(ErrorBody::new(
                ErrorCode::Denied,
                "Registering an AI agent as `owner` gives it unrestricted access to everything this account can do, so it is refused unless `allow_owner=true`.",
            )
            .with_hint(
                "Add a named principal with a narrower role in policy.toml (such as `[principals.\"agent:claude\"]` with `role = \"operator\"`), or pass `allow_owner=true` if you really want unrestricted access.",
            ));
        }
    } else {
        let policy = ctx.config().get::<Policy>()?;
        if !policy
            .principals
            .keys()
            .any(|known| known.as_str() == principal)
        {
            return Err(ErrorBody::new(
                ErrorCode::NotFound,
                format!(
                    "policy.toml does not name {principal}, so an MCP server acting as it could do nothing."
                ),
            )
            .with_hint(format!(
                "Add [principals.\"{principal}\"] with a role to policy.toml first."
            )));
        }
    }

    let exe = current_legion_exe()?;
    let command = build_add_command(
        args.agent,
        &exe,
        &principal,
        args.compact,
        args.scope.as_deref(),
    )?;

    if args.dry_run {
        return Ok(AddResult {
            agent: args.agent,
            command,
            registered: false,
            replaced: None,
        });
    }

    let program_path = find_program_on_env_path(args.agent.program()).ok_or_else(|| {
        ErrorBody::new(
            ErrorCode::Unsupported,
            format!(
                "Cannot register with {}: `{}` is not installed or not on PATH.",
                args.agent.as_str(),
                args.agent.program()
            ),
        )
        .with_hint(args.agent.install_hint())
    })?;

    // Check best-effort whether `legion` is already registered so we can report `replaced`.
    let (existing, _) =
        query_registered(args.agent, &program_path, ctx.cancel_token().child_token()).await;
    let mut replaced = existing == Registered::Yes;

    let first_attempt = run_agent_command(
        args.agent,
        &program_path,
        &command[1..],
        ctx.cancel_token().child_token(),
    )
    .await?;

    if !first_attempt.success() {
        let combined = format!(
            "{}\n{}",
            first_attempt.stdout.text, first_attempt.stderr.text
        );
        // Claude Code refuses `mcp add` when a server with the same name already exists in that scope.
        if args.agent == AgentKind::Claude && looks_like_already_exists(&combined) {
            let remove_cmd = build_remove_command(args.agent, args.scope.as_deref())?;
            let remove_out = run_agent_command(
                args.agent,
                &program_path,
                &remove_cmd[1..],
                ctx.cancel_token().child_token(),
            )
            .await?;
            if !remove_out.success() {
                return Err(command_failed(args.agent, "mcp remove", &remove_out));
            }
            let retry = run_agent_command(
                args.agent,
                &program_path,
                &command[1..],
                ctx.cancel_token().child_token(),
            )
            .await?;
            if !retry.success() {
                return Err(command_failed(args.agent, "mcp add", &retry));
            }
            replaced = true;
        } else {
            return Err(command_failed(args.agent, "mcp add", &first_attempt));
        }
    }

    Ok(AddResult {
        agent: args.agent,
        command,
        registered: true,
        replaced: replaced.then_some(true),
    })
}

/// Arguments of `agent.remove`.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct RemoveArgs {
    /// Which AI coding agent to remove Legion from (`claude`, `codex`, `antigravity`, `opencode`).
    agent: AgentKind,
    /// Scope for `claude` (`local`, `user`, or `project`; defaults to `user`).
    #[serde(default)]
    scope: Option<String>,
    /// Return the exact removal command without running it.
    #[serde(default)]
    dry_run: bool,
}

/// What `agent.remove` returns.
#[derive(Debug, Serialize)]
pub struct RemoveResult {
    /// Agent from which Legion was removed.
    pub agent: AgentKind,
    /// Exact argument vector built (and executed unless `dry_run` was true).
    pub command: Vec<String>,
    /// Whether the removal command was executed and succeeded.
    pub removed: bool,
}

async fn remove(ctx: &Ctx, args: RemoveArgs) -> Result<RemoveResult, ToolError> {
    let command = build_remove_command(args.agent, args.scope.as_deref())?;

    if args.dry_run {
        return Ok(RemoveResult {
            agent: args.agent,
            command,
            removed: false,
        });
    }

    let program_path = find_program_on_env_path(args.agent.program()).ok_or_else(|| {
        ErrorBody::new(
            ErrorCode::Unsupported,
            format!(
                "Cannot remove from {}: `{}` is not installed or not on PATH.",
                args.agent.as_str(),
                args.agent.program()
            ),
        )
        .with_hint(args.agent.install_hint())
    })?;

    let output = run_agent_command(
        args.agent,
        &program_path,
        &command[1..],
        ctx.cancel_token().child_token(),
    )
    .await?;

    if !output.success() {
        return Err(command_failed(args.agent, "mcp remove", &output));
    }

    Ok(RemoveResult {
        agent: args.agent,
        command,
        removed: true,
    })
}

async fn run_agent_command(
    agent: AgentKind,
    program_path: &Path,
    args: &[String],
    cancel: legion_core::CancellationToken,
) -> Result<legion_os::run::RunOutput, ErrorBody> {
    let spec = CommandSpec::argv(program_path.as_os_str(), args);
    let options = RunOptions::new(spec)
        .c_locale()
        .timeout(MUTATE_TIMEOUT)
        .cancel(cancel);
    let output = run(options).await.map_err(|error| match error {
        OsError::ProgramNotFound { .. } => ErrorBody::new(
            ErrorCode::Unsupported,
            format!("`{}` is not installed or not on PATH.", agent.program()),
        )
        .with_hint(agent.install_hint()),
        other => ErrorBody::new(
            ErrorCode::Failed,
            format!("Could not run `{}`: {other}.", agent.program()),
        ),
    })?;
    if output.cancelled {
        return Err(ErrorBody::new(
            ErrorCode::Cancelled,
            format!("Running `{}` was cancelled.", agent.program()),
        ));
    }
    if output.timed_out {
        return Err(ErrorBody::new(
            ErrorCode::Timeout,
            format!(
                "Running `{}` timed out after {}s.",
                agent.program(),
                MUTATE_TIMEOUT.as_secs()
            ),
        ));
    }
    Ok(output)
}

fn command_failed(
    agent: AgentKind,
    subcommand: &str,
    output: &legion_os::run::RunOutput,
) -> ErrorBody {
    let stderr = output.stderr.text.trim();
    let stdout = output.stdout.text.trim();
    let detail = if !stderr.is_empty() {
        stderr
    } else if !stdout.is_empty() {
        stdout
    } else {
        ""
    };
    let message = if detail.is_empty() {
        format!(
            "`{} {subcommand}` failed (exit {:?}).",
            agent.program(),
            output.exit_code
        )
    } else {
        format!("`{} {subcommand}` failed: {detail}", agent.program())
    };
    ErrorBody::new(ErrorCode::Failed, message)
}

fn looks_like_already_exists(output: &str) -> bool {
    let lower = output.to_ascii_lowercase();
    lower.contains("already exists") || lower.contains("already configured")
}

/// Resolves the absolute path of the running `legion` binary so registered MCP configs remain
/// valid even when the agent spawns subprocesses with a minimal `PATH`.
fn current_legion_exe() -> Result<String, ErrorBody> {
    let exe = legion_core::current_exe()?;
    let resolved = dunce::canonicalize(&exe).unwrap_or(exe);
    resolved.into_os_string().into_string().map_err(|path| {
        ErrorBody::new(
            ErrorCode::Failed,
            format!(
                "The legion executable path {} is not valid Unicode.",
                path.to_string_lossy()
            ),
        )
    })
}

/// Builds the exact argv for `agent`'s `mcp list` command.
pub fn build_list_command(agent: AgentKind) -> Vec<String> {
    vec![
        agent.program().to_owned(),
        "mcp".to_owned(),
        "list".to_owned(),
    ]
}

/// Builds the exact argv to register `legion mcp --as <principal> [--compact]` with `agent`.
pub fn build_add_command(
    agent: AgentKind,
    legion_exe: &str,
    principal: &str,
    compact: bool,
    scope: Option<&str>,
) -> Result<Vec<String>, ErrorBody> {
    let mut mcp_args = vec![
        legion_exe.to_owned(),
        "mcp".to_owned(),
        "--as".to_owned(),
        principal.to_owned(),
    ];
    if compact {
        mcp_args.push("--compact".to_owned());
    }

    let mut argv = match agent {
        AgentKind::Claude => {
            let scope = match scope {
                None => "user",
                Some(s @ ("local" | "user" | "project")) => s,
                Some(other) => {
                    return Err(ErrorBody::new(
                        ErrorCode::InvalidArgs,
                        format!("Invalid scope {other:?} for claude."),
                    )
                    .with_hint("Use scope `local`, `user`, or `project` (default `user`)."));
                }
            };
            vec![
                "claude".to_owned(),
                "mcp".to_owned(),
                "add".to_owned(),
                "-s".to_owned(),
                scope.to_owned(),
                SERVER_NAME.to_owned(),
                "--".to_owned(),
            ]
        }
        AgentKind::Codex => {
            if let Some(other) = scope {
                return Err(ErrorBody::new(
                    ErrorCode::InvalidArgs,
                    format!("Codex does not take a scope option ({other:?} was given)."),
                )
                .with_hint("Omit `scope` when registering with codex."));
            }
            vec![
                "codex".to_owned(),
                "mcp".to_owned(),
                "add".to_owned(),
                SERVER_NAME.to_owned(),
                "--".to_owned(),
            ]
        }
        AgentKind::Antigravity => {
            if let Some(other) = scope {
                return Err(ErrorBody::new(
                    ErrorCode::InvalidArgs,
                    format!("Antigravity does not take a scope option ({other:?} was given)."),
                )
                .with_hint("Omit `scope` when registering with antigravity."));
            }
            // `agy mcp add` requires flags before `<name>` and recommends `--` before a command
            // whose arguments begin with `-` (such as `--as` and `--compact`).
            vec![
                "agy".to_owned(),
                "mcp".to_owned(),
                "add".to_owned(),
                SERVER_NAME.to_owned(),
                "--".to_owned(),
            ]
        }
        AgentKind::Opencode => {
            let global = match scope {
                None | Some("global" | "user") => true,
                Some("project" | "local") => false,
                Some(other) => {
                    return Err(ErrorBody::new(
                        ErrorCode::InvalidArgs,
                        format!("Invalid scope {other:?} for opencode."),
                    )
                    .with_hint("Use scope `global` (or `user`) or `project` (or `local`)."));
                }
            };
            let mut base = vec![
                "opencode".to_owned(),
                "mcp".to_owned(),
                "add".to_owned(),
                SERVER_NAME.to_owned(),
            ];
            if global {
                base.push("--global".to_owned());
            }
            base.push("--".to_owned());
            base
        }
    };
    argv.extend(mcp_args);
    Ok(argv)
}

/// Builds the exact argv to remove the `legion` MCP registration from `agent`.
pub fn build_remove_command(
    agent: AgentKind,
    scope: Option<&str>,
) -> Result<Vec<String>, ErrorBody> {
    match agent {
        AgentKind::Claude => {
            let scope = match scope {
                None => "user",
                Some(s @ ("local" | "user" | "project")) => s,
                Some(other) => {
                    return Err(ErrorBody::new(
                        ErrorCode::InvalidArgs,
                        format!("Invalid scope {other:?} for claude."),
                    )
                    .with_hint("Use scope `local`, `user`, or `project` (default `user`)."));
                }
            };
            Ok(vec![
                "claude".to_owned(),
                "mcp".to_owned(),
                "remove".to_owned(),
                "-s".to_owned(),
                scope.to_owned(),
                SERVER_NAME.to_owned(),
            ])
        }
        AgentKind::Codex => {
            if let Some(other) = scope {
                return Err(ErrorBody::new(
                    ErrorCode::InvalidArgs,
                    format!("Codex does not take a scope option ({other:?} was given)."),
                )
                .with_hint("Omit `scope` when removing from codex."));
            }
            Ok(vec![
                "codex".to_owned(),
                "mcp".to_owned(),
                "remove".to_owned(),
                SERVER_NAME.to_owned(),
            ])
        }
        AgentKind::Antigravity => {
            if let Some(other) = scope {
                return Err(ErrorBody::new(
                    ErrorCode::InvalidArgs,
                    format!("Antigravity does not take a scope option ({other:?} was given)."),
                )
                .with_hint("Omit `scope` when removing from antigravity."));
            }
            Ok(vec![
                "agy".to_owned(),
                "mcp".to_owned(),
                "remove".to_owned(),
                SERVER_NAME.to_owned(),
            ])
        }
        AgentKind::Opencode => Err(ErrorBody::new(
            ErrorCode::Unsupported,
            "OpenCode's CLI does not provide an `opencode mcp remove` subcommand.",
        )
        .with_hint(
            "Legion never edits agent configuration files directly; remove the `legion` entry from your OpenCode configuration manually.",
        )),
    }
}

/// Parses an agent's `mcp list` stdout defensively, returning [`Registered::Unknown`] whenever the
/// output format is not recognised rather than guessing.
pub fn parse_mcp_list(agent: AgentKind, stdout: &str) -> Registered {
    let cleaned = strip_ansi(stdout);
    let trimmed = cleaned.trim();
    if trimmed.is_empty() {
        return Registered::Unknown;
    }

    // Some versions of codex/other tools may emit JSON arrays when configured to do so.
    if trimmed.starts_with('[')
        && let Ok(serde_json::Value::Array(items)) =
            serde_json::from_str::<serde_json::Value>(trimmed)
    {
        let mut recognised = items.is_empty();
        for item in &items {
            if let Some(name) = item.get("name").and_then(serde_json::Value::as_str) {
                recognised = true;
                if name == SERVER_NAME {
                    return Registered::Yes;
                }
            }
        }
        return if recognised {
            Registered::No
        } else {
            Registered::Unknown
        };
    }

    let lines: Vec<&str> = cleaned
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect();

    if lines
        .iter()
        .any(|line| line.to_ascii_lowercase().contains("no mcp servers"))
    {
        return Registered::No;
    }

    match agent {
        AgentKind::Claude => parse_claude_list(&lines),
        AgentKind::Codex => parse_table_list(&lines, &["name"], &["command", "url", "status"]),
        AgentKind::Antigravity => parse_table_list(&lines, &["name"], &["type", "status"]),
        AgentKind::Opencode => parse_opencode_list(&lines),
    }
}

fn parse_claude_list(lines: &[&str]) -> Registered {
    // Claude Code prints lines like:
    //   Checking MCP server health...
    //   legion: /home/me/.local/bin/legion mcp --as agent:claude --compact - ✓ Connected
    let mut saw_server = false;
    for line in lines {
        if line.to_ascii_lowercase().starts_with("checking mcp server") {
            continue;
        }
        if let Some((name, rest)) = line.split_once(':') {
            let name = name.trim();
            if is_server_name_token(name) && !rest.trim().is_empty() {
                saw_server = true;
                if name == SERVER_NAME {
                    return Registered::Yes;
                }
            }
        }
    }
    if saw_server {
        Registered::No
    } else {
        Registered::Unknown
    }
}

fn parse_table_list(lines: &[&str], first_cols: &[&str], other_cols: &[&str]) -> Registered {
    let mut header_seen = false;
    for line in lines {
        let lower = line.to_ascii_lowercase();
        let mut parts = lower.split_whitespace();
        let first = parts.next().unwrap_or_default();
        if !header_seen {
            let matches_first = first_cols.contains(&first);
            let matches_other =
                parts.any(|col| other_cols.iter().any(|want| col.starts_with(want)));
            if matches_first && matches_other {
                header_seen = true;
            }
            continue;
        }
        // Skip decorative separator rows like `----  ----`.
        if line.chars().all(|c| matches!(c, '-' | '=' | '─' | ' ')) {
            continue;
        }
        if let Some(name) = line.split_whitespace().next()
            && name == SERVER_NAME
        {
            return Registered::Yes;
        }
    }
    if header_seen {
        Registered::No
    } else {
        Registered::Unknown
    }
}

fn parse_opencode_list(lines: &[&str]) -> Registered {
    // OpenCode prints status lines with glyphs (`✓ legion ...`, `✗ other ...`, `○ ...`),
    // `name: ...`, or a NAME/STATUS table.
    if let state @ (Registered::Yes | Registered::No) =
        parse_table_list(lines, &["name"], &["status", "type", "command"])
    {
        return state;
    }

    let mut saw_entry = false;
    for line in lines {
        // Strip common box-drawing borders or status symbols used by CLI tables.
        let stripped = line.trim_start_matches(|c: char| {
            matches!(
                c,
                '✓' | '✔'
                    | '✗'
                    | '✘'
                    | '×'
                    | '○'
                    | '●'
                    | '•'
                    | '◉'
                    | '⚠'
                    | '│'
                    | '-'
                    | '*'
                    | ' '
            )
        });
        let had_prefix = stripped.len() < line.len();
        let Some(first) = stripped.split_whitespace().next() else {
            continue;
        };
        let token = first.trim_end_matches(':');
        let had_colon = first.ends_with(':');
        let rest_lower = stripped[first.len()..].to_ascii_lowercase();
        let has_status_word = [
            "connected",
            "disabled",
            "failed",
            "needs_auth",
            "enabled",
            "local",
            "remote",
            "stdio",
            "http",
        ]
        .iter()
        .any(|word| rest_lower.contains(word));

        if is_server_name_token(token) && (had_prefix || had_colon || has_status_word) {
            saw_entry = true;
            if token == SERVER_NAME {
                return Registered::Yes;
            }
        }
    }
    if saw_entry {
        Registered::No
    } else {
        Registered::Unknown
    }
}

fn is_server_name_token(token: &str) -> bool {
    !token.is_empty()
        && token
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

/// Removes ANSI CSI escape sequences so colourised CLI output parses reliably.
fn strip_ansi(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\x1b' && chars.peek() == Some(&'[') {
            chars.next();
            for next in chars.by_ref() {
                if ('@'..='~').contains(&next) {
                    break;
                }
            }
        } else {
            out.push(ch);
        }
    }
    out
}

/// Locates `program` on the process's current `PATH`.
pub(crate) fn find_program_on_env_path(program: &str) -> Option<PathBuf> {
    let path_var = std::env::var_os("PATH");
    let pathext = std::env::var_os("PATHEXT");
    find_program_in_path(
        program,
        path_var.as_deref(),
        pathext.as_deref(),
        cfg!(windows),
    )
}

/// Locates `program` inside an explicit `PATH` value (and `PATHEXT` on Windows).
pub(crate) fn find_program_in_path(
    program: &str,
    path_var: Option<&OsStr>,
    pathext: Option<&OsStr>,
    windows: bool,
) -> Option<PathBuf> {
    let path_var = path_var?;
    let has_ext = Path::new(program).extension().is_some();
    let extensions: Vec<String> = if windows && !has_ext {
        parse_pathext(pathext)
    } else {
        Vec::new()
    };

    for dir in std::env::split_paths(path_var) {
        if dir.as_os_str().is_empty() {
            continue;
        }
        if !windows || has_ext {
            let candidate = dir.join(program);
            if is_executable_file(&candidate, windows) {
                return Some(candidate);
            }
        }
        for ext in &extensions {
            let candidate = dir.join(format!("{program}{ext}"));
            if is_executable_file(&candidate, windows) {
                return Some(candidate);
            }
        }
    }
    None
}

fn parse_pathext(pathext: Option<&OsStr>) -> Vec<String> {
    const DEFAULTS: &[&str] = &[".EXE", ".COM", ".BAT", ".CMD"];
    let parsed: Vec<String> = pathext
        .and_then(OsStr::to_str)
        .map(|raw| {
            raw.split(';')
                .map(str::trim)
                .filter(|ext| !ext.is_empty())
                .map(|ext| {
                    if ext.starts_with('.') {
                        ext.to_owned()
                    } else {
                        format!(".{ext}")
                    }
                })
                .collect()
        })
        .unwrap_or_default();
    if parsed.is_empty() {
        DEFAULTS.iter().map(|ext| (*ext).to_owned()).collect()
    } else {
        parsed
    }
}

fn is_executable_file(path: &Path, windows: bool) -> bool {
    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    if !meta.is_file() {
        return false;
    }
    if windows {
        return true;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        meta.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_builders_match_each_agents_verified_syntax() {
        let exe = "/home/me/.local/share/legion/legion";

        assert_eq!(
            build_add_command(AgentKind::Claude, exe, "agent:claude", true, None).unwrap(),
            vec![
                "claude",
                "mcp",
                "add",
                "-s",
                "user",
                "legion",
                "--",
                exe,
                "mcp",
                "--as",
                "agent:claude",
                "--compact"
            ]
        );
        assert_eq!(
            build_add_command(
                AgentKind::Claude,
                exe,
                "agent:claude",
                false,
                Some("project")
            )
            .unwrap(),
            vec![
                "claude",
                "mcp",
                "add",
                "-s",
                "project",
                "legion",
                "--",
                exe,
                "mcp",
                "--as",
                "agent:claude"
            ]
        );
        assert!(
            build_add_command(AgentKind::Claude, exe, "agent:claude", true, Some("global"))
                .is_err()
        );

        assert_eq!(
            build_add_command(AgentKind::Codex, exe, "agent:codex", true, None).unwrap(),
            vec![
                "codex",
                "mcp",
                "add",
                "legion",
                "--",
                exe,
                "mcp",
                "--as",
                "agent:codex",
                "--compact"
            ]
        );
        assert!(
            build_add_command(AgentKind::Codex, exe, "agent:codex", true, Some("user")).is_err()
        );

        assert_eq!(
            build_add_command(AgentKind::Antigravity, exe, "agent:agy", true, None).unwrap(),
            vec![
                "agy",
                "mcp",
                "add",
                "legion",
                "--",
                exe,
                "mcp",
                "--as",
                "agent:agy",
                "--compact"
            ]
        );

        assert_eq!(
            build_add_command(AgentKind::Opencode, exe, "agent:oc", true, None).unwrap(),
            vec![
                "opencode",
                "mcp",
                "add",
                "legion",
                "--global",
                "--",
                exe,
                "mcp",
                "--as",
                "agent:oc",
                "--compact"
            ]
        );
        assert_eq!(
            build_add_command(AgentKind::Opencode, exe, "agent:oc", false, Some("project"))
                .unwrap(),
            vec![
                "opencode", "mcp", "add", "legion", "--", exe, "mcp", "--as", "agent:oc"
            ]
        );

        assert_eq!(
            build_remove_command(AgentKind::Claude, None).unwrap(),
            vec!["claude", "mcp", "remove", "-s", "user", "legion"]
        );
        assert_eq!(
            build_remove_command(AgentKind::Codex, None).unwrap(),
            vec!["codex", "mcp", "remove", "legion"]
        );
        assert_eq!(
            build_remove_command(AgentKind::Antigravity, None).unwrap(),
            vec!["agy", "mcp", "remove", "legion"]
        );
        let opencode_err = build_remove_command(AgentKind::Opencode, None).unwrap_err();
        assert_eq!(opencode_err.code, ErrorCode::Unsupported);
    }

    #[test]
    fn mcp_list_parsers_handle_canned_outputs_and_report_unknown_defensively() {
        // Claude
        assert_eq!(
            parse_mcp_list(
                AgentKind::Claude,
                "Checking MCP server health...\n\nlegion: /home/me/.local/legion mcp --as agent:claude --compact - ✓ Connected\n"
            ),
            Registered::Yes
        );
        assert_eq!(
            parse_mcp_list(AgentKind::Claude, "other: /usr/bin/other - ✓ Connected\n"),
            Registered::No
        );
        assert_eq!(
            parse_mcp_list(
                AgentKind::Claude,
                "No MCP servers configured. Use `claude mcp add` to add a server.\n"
            ),
            Registered::No
        );
        assert_eq!(
            parse_mcp_list(AgentKind::Claude, "unexpected banner text\n"),
            Registered::Unknown
        );

        // Codex
        assert_eq!(
            parse_mcp_list(
                AgentKind::Codex,
                "Name    Command                     Args  Env  Cwd  Status   Auth\nlegion  /home/me/legion             mcp   -    -    enabled  Unsupported\n"
            ),
            Registered::Yes
        );
        assert_eq!(
            parse_mcp_list(
                AgentKind::Codex,
                "Name   Command   Status\nother  /bin/x    enabled\n"
            ),
            Registered::No
        );
        assert_eq!(
            parse_mcp_list(
                AgentKind::Codex,
                "No MCP servers configured yet. Try `codex mcp add my-tool -- my-command`.\n"
            ),
            Registered::No
        );
        assert_eq!(
            parse_mcp_list(AgentKind::Codex, r#"[{"name":"legion"}]"#),
            Registered::Yes
        );
        assert_eq!(
            parse_mcp_list(AgentKind::Codex, r#"[{"name":"other"}]"#),
            Registered::No
        );
        assert_eq!(
            parse_mcp_list(AgentKind::Codex, "garbage output"),
            Registered::Unknown
        );

        // Antigravity
        assert_eq!(
            parse_mcp_list(
                AgentKind::Antigravity,
                "NAME    TYPE   STATUS   COMMAND/URL\nlegion  stdio  enabled  /home/me/legion mcp --as agent:agy --compact\n"
            ),
            Registered::Yes
        );
        assert_eq!(
            parse_mcp_list(
                AgentKind::Antigravity,
                "NAME    TYPE   STATUS   COMMAND/URL\nother   http   enabled  https://example.net/mcp\n"
            ),
            Registered::No
        );
        assert_eq!(
            parse_mcp_list(AgentKind::Antigravity, "No MCP servers configured.\n"),
            Registered::No
        );
        assert_eq!(
            parse_mcp_list(AgentKind::Antigravity, ""),
            Registered::Unknown
        );

        // OpenCode (including ANSI colour codes)
        assert_eq!(
            parse_mcp_list(
                AgentKind::Opencode,
                "\x1b[32m✓\x1b[0m legion  connected\n\x1b[31m✗\x1b[0m docs    disabled\n"
            ),
            Registered::Yes
        );
        assert_eq!(
            parse_mcp_list(AgentKind::Opencode, "✓ docs  connected\n"),
            Registered::No
        );
        assert_eq!(
            parse_mcp_list(AgentKind::Opencode, "No MCP servers found\n"),
            Registered::No
        );
        assert_eq!(
            parse_mcp_list(AgentKind::Opencode, "some unrecognised message"),
            Registered::Unknown
        );
    }
}
